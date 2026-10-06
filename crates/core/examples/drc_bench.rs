//! Native DRC timing (Phase 5b — merge-first, tiled): load a GDS, check its
//! biggest cell tile by tile, and report
//!   * grid build time (the budgeted replacement for the atomic R-tree build),
//!   * total check time single-threaded, and the per-tile distribution (p50 /
//!     p99 / max — the unit the inline budget and the worker pool schedule),
//!   * the incremental re-check cost of one edit (the tiles near one shape),
//!   * violations by kind and layer.
//!
//! With `--all`, also sweeps EVERY cell of the design (intra-cell) and reports
//! flags by rule, the worst cells, and how many flags sit on a child instance's
//! same-layer metal (the scope limitation, measured).
//!
//! Run: cargo run --release -p fluideda-core --example drc_bench -- <file.gds> [blanket|sky130] [--all]

use fluideda_core::{
    build_grid, cell_tiles, check_tile, gather_tile, point_in_polygon, CellDb, CellId, DrcKind, DrcRules, EditOverlay,
    InstanceKind, Point, Violation,
};
use std::collections::HashMap;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: drc_bench <file.gds> [blanket|sky130]");
    let deck = std::env::args().nth(2).unwrap_or_else(|| "blanket".into());
    let sweep_all = std::env::args().any(|a| a == "--all");
    let bytes = std::fs::read(&path).expect("read gds");
    let t = Instant::now();
    let db = fluideda_core::load_layout_db(&bytes).expect("parse");
    println!("parsed {} in {:.1} s", path, t.elapsed().as_secs_f64());

    let rules = fluideda_core::Deck::from_name(&deck).map(|d| d.rules()).unwrap_or_else(DrcRules::default_table);
    println!("deck: {deck} (max reach {} nm)", rules.max_reach());

    let biggest = (0..db.cells.len()).max_by_key(|&c| db.cell(c).shapes.len()).unwrap();
    let cell = db.cell(biggest);
    println!("biggest cell: '{}' ({} shapes)", cell.name, cell.shapes.len());
    let overlay = EditOverlay::new();

    let tg = Instant::now();
    let grid = build_grid(cell, &rules);
    let grid_ms = tg.elapsed().as_secs_f64() * 1e3;
    let tiles = cell_tiles(&grid, biggest, &overlay, &rules);
    println!(
        "grid: {}×{} tiles of {:.1} µm ({} non-empty), {} bucket entries, built in {grid_ms:.0} ms",
        grid.nx,
        grid.ny,
        grid.side as f64 / 1000.0,
        tiles.len(),
        grid.entry_count()
    );

    let mut tile_ms: Vec<f64> = Vec::with_capacity(tiles.len());
    let mut gather_ms = 0.0;
    let mut viol = Vec::new();
    let mut max_in = 0usize;
    let t0 = Instant::now();
    for &tk in &tiles {
        let tg = Instant::now();
        let input = gather_tile(cell, biggest, &overlay, &grid, &rules, tk);
        gather_ms += tg.elapsed().as_secs_f64() * 1e3;
        max_in = max_in.max(input.len());
        let tt = Instant::now();
        viol.extend(check_tile(grid.tile_core(tk), &rules, &input));
        tile_ms.push(tt.elapsed().as_secs_f64() * 1e3);
    }
    let total = t0.elapsed().as_secs_f64() * 1e3;
    tile_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| tile_ms[((tile_ms.len() as f64 - 1.0) * p).round() as usize];
    println!(
        "full check: {total:.0} ms single-threaded (gather {gather_ms:.0} ms) over {} tiles; per-tile check p50 {:.2} / p99 {:.2} / max {:.2} ms; biggest tile input {} shapes",
        tiles.len(),
        pct(0.5),
        pct(0.99),
        pct(1.0),
        max_in
    );

    // One edit's incremental cost: re-check the tiles near shape 0.
    let pts = cell.shapes.points(0);
    let mut b = fluideda_core::Bbox::empty();
    for p in pts.iter() {
        b.expand(*p);
    }
    let dirty = grid.tiles_near(&b);
    let te = Instant::now();
    for &tk in &dirty {
        let input = gather_tile(cell, biggest, &overlay, &grid, &rules, tk);
        let _ = check_tile(grid.tile_core(tk), &rules, &input);
    }
    println!("incremental re-check after one edit: {} tile(s) in {:.1} ms", dirty.len(), te.elapsed().as_secs_f64() * 1e3);

    let widths = viol.iter().filter(|v| v.kind == DrcKind::Width).count();
    println!("violations: {} ({} width, {} spacing)", viol.len(), widths, viol.len() - widths);
    // Scope check: how many flags sit where a CHILD instance has same-layer
    // geometry — spots where the merged-in-context layout differs from what an
    // intra-cell check sees (the deferred hierarchical-DRC limitation).
    let covered = viol.iter().filter(|v| covered_by_child(&db, biggest, v)).count();
    println!("  of which {covered} lie on same-layer geometry of a child instance (intra-cell scope artifacts)");
    let mut by_layer: HashMap<(u16, u16, DrcKind), usize> = HashMap::new();
    for v in &viol {
        *by_layer.entry((v.layer.layer, v.layer.datatype, v.kind)).or_default() += 1;
    }
    let mut top: Vec<_> = by_layer.into_iter().collect();
    top.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    for ((l, d, k), n) in top.into_iter().take(8) {
        println!("  L{l}/{d} {k:?}: {n}");
    }

    if sweep_all {
        let t = Instant::now();
        let (mut total, mut flagged, mut covered) = (0usize, 0usize, 0usize);
        let mut by_rule: std::collections::BTreeMap<String, usize> = Default::default();
        let mut worst: Vec<(usize, String)> = Vec::new();
        for c in 0..db.cells.len() {
            let v = fluideda_core::check_cell(db.cell(c), c, &overlay, &rules);
            if v.is_empty() {
                continue;
            }
            flagged += 1;
            total += v.len();
            for x in &v {
                let name = match fluideda_core::Deck::from_name(&deck).and_then(|d| d.entry(x.layer)) {
                    Some(e) => format!("{} {:?}", e.name, x.kind),
                    None => format!("L{}/{} {:?}", x.layer.layer, x.layer.datatype, x.kind),
                };
                *by_rule.entry(name).or_default() += 1;
                covered += covered_by_child(&db, c, x) as usize;
            }
            worst.push((v.len(), db.cell(c).name.clone()));
        }
        worst.sort_by(|a, b| b.cmp(a));
        println!(
            "ALL CELLS: {} cells in {:.1} s — {flagged} with flags, {total} flags, {covered} on a child's same-layer metal",
            db.cells.len(),
            t.elapsed().as_secs_f64()
        );
        println!("  by rule: {by_rule:?}");
        println!("  worst cells: {:?}", &worst[..worst.len().min(8)]);
    }

    // A typical mid-size cell too (median-by-shapes of cells > 1k shapes).
    let mut mids: Vec<usize> = (0..db.cells.len()).filter(|&c| db.cell(c).shapes.len() > 1_000).collect();
    mids.sort_by_key(|&c| db.cell(c).shapes.len());
    if let Some(&mid) = mids.get(mids.len() / 2) {
        let t2 = Instant::now();
        let vm = fluideda_core::check_cell(db.cell(mid), mid, &overlay, &rules);
        let ms = t2.elapsed().as_secs_f64() * 1e3;
        println!(
            "median >1k cell: '{}' ({} shapes) in {ms:.1} ms, {} violations",
            db.cell(mid).name,
            db.cell(mid).shapes.len(),
            vm.len()
        );
    }
}

/// Does a direct child instance of `cell` have a same-layer shape under the
/// violation marker's centre?
fn covered_by_child(db: &CellDb, cell: CellId, v: &Violation) -> bool {
    let (cx, cy) = ((v.bbox.min.x as f64 + v.bbox.max.x as f64) / 2.0, (v.bbox.min.y as f64 + v.bbox.max.y as f64) / 2.0);
    for inst in &db.cell(cell).instances {
        let (cols, rows) = match inst.kind {
            InstanceKind::Array { cols, rows, .. } => (cols, rows),
            InstanceKind::Single => (1, 1),
        };
        for r in 0..rows {
            for c in 0..cols {
                let t = inst.placement(c, r);
                let bb = t.apply_bbox(&db.local_bbox(inst.child));
                if cx < bb.min_x || cx > bb.max_x || cy < bb.min_y || cy > bb.max_y {
                    continue;
                }
                let (lx, ly) = t.inverse().apply(cx, cy);
                let p = Point::new(lx.round() as i32, ly.round() as i32);
                let child = db.cell(inst.child);
                if (0..child.shapes.len())
                    .any(|i| child.shapes.layer(i) == v.layer && point_in_polygon(&child.shapes.points(i), p))
                {
                    return true;
                }
            }
        }
    }
    false
}
