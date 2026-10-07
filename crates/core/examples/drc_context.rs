//! Phase 7.5: a whole design checked in context, natively.
//!
//! * intra-cell: every cell on its own (Phase 5b), timed;
//! * in context: world tiles with geometry gathered through the hierarchy,
//!   on every core, timed;
//! * the intra-cell flags, placed in the world, sorted against the
//!   in-context result: genuine (an in-context violation of the same rule
//!   overlaps) or context-only; and what only the in-context check finds;
//! * the in-context violations by rule. `--out <file>` writes them as JSON
//!   lines (`kind`, `layer`, `datatype`, `bbox`), for the KLayout comparison
//!   (`scripts/oracle/compare_drc.py`).
//!
//! `--inject` first draws known violations below the die through the edit
//! overlay — a 100 nm met1 sliver (m1.1 is 140) and two met2 squares 100 nm
//! apart (m2.2 is 140) — so a clean result is shown to be able to fail.
//! `--inject-random <n>` draws `n` random rectangles on li1/met1/met2 inside
//! the die, among the real geometry (some merge harmlessly, some make width
//! or space violations with their neighbours), and `--write <file.gds>`
//! writes the design with them — what KLayout then checks
//! (`scripts/oracle/sky130_deck.drc`).
//!
//! Run: cargo run --release -p fluideda-core --example drc_context -- <file> [sky130|blanket] [--out f] [--threads n]

use fluideda_core::{check_cell, check_world, review_flags, DrcKind, DrcRules, EditOverlay, Layer, Point, Shape, Violation};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: drc_context <file> [deck] [--out f] [--threads n]");
    let deck = args.get(2).filter(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "sky130".into());
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let threads: usize = flag("--threads")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let bytes = std::fs::read(path).expect("read");
    let t = Instant::now();
    let db = fluideda_core::load_layout_db(&bytes).expect("parse");
    drop(bytes);
    println!("{path}: {} cells, parsed in {:.1} s", db.cells.len(), t.elapsed().as_secs_f64());
    let rules = fluideda_core::Deck::from_name(&deck).map(|d| d.rules()).unwrap_or_else(DrcRules::default_table);
    let mut none = EditOverlay::new();
    if args.iter().any(|a| a == "--inject") {
        let w = db.world_bbox();
        let (x, y) = (w.min_x as i32 + 50_000, w.min_y as i32 - 20_000);
        let rect = |x0: i32, y0: i32, x1: i32, y1: i32| vec![Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)];
        let shapes = [
            (Layer::new(68, 20), rect(x, y, x + 100, y + 2_000)),
            (Layer::new(69, 20), rect(x + 5_000, y, x + 6_000, y + 1_000)),
            (Layer::new(69, 20), rect(x + 6_100, y, x + 7_100, y + 1_000)),
        ];
        for (k, (layer, points)) in shapes.into_iter().enumerate() {
            none.add((9, k as u32), db.top, Shape { layer, points });
        }
        println!("injected a met1 sliver and a met2 gap below the die at ({x}, {y})");
    }
    if let Some(n) = flag("--inject-random").and_then(|s| s.parse::<u32>().ok()) {
        let w = db.world_bbox();
        let mut r = 0x2545_f491_4f6c_dd1du64;
        let mut next = |lo: i32, hi: i32| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            lo + (r % (hi - lo) as u64) as i32
        };
        let layers = [Layer::new(67, 20), Layer::new(68, 20), Layer::new(69, 20)];
        for k in 0..n {
            let layer = layers[next(0, 3) as usize];
            let (x, y) = (next(w.min_x as i32, w.max_x as i32), next(w.min_y as i32, w.max_y as i32));
            let (dx, dy) = (next(60, 400), next(60, 2_000));
            let (dx, dy) = if next(0, 2) == 0 { (dx, dy) } else { (dy, dx) };
            let points = vec![Point::new(x, y), Point::new(x + dx, y), Point::new(x + dx, y + dy), Point::new(x, y + dy)];
            none.add((10, k), db.top, Shape { layer, points });
        }
        println!("injected {n} random rectangles inside the die");
    }
    if let Some(out) = flag("--write") {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&out).expect("create"));
        fluideda_core::write_gds_to(&db, &none, &mut f).expect("write");
        println!("wrote the design with its edits to {out}");
    }

    let t = Instant::now();
    let mut flags: HashMap<usize, Vec<Violation>> = HashMap::new();
    for c in 0..db.cells.len() {
        let v = check_cell(db.cell(c), c, &none, &rules);
        if !v.is_empty() {
            flags.insert(c, v);
        }
    }
    let intra: usize = flags.values().map(Vec::len).sum();
    println!(
        "intra-cell: {intra} flags in {} cells, {:.1} s single-threaded",
        flags.len(),
        t.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    for c in &db.cells {
        let _ = c.index();
    }
    let index_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let report = check_world(&db, &none, &rules, threads);
    let world_s = t.elapsed().as_secs_f64();
    println!(
        "in context: {} violations; {} tiles ({} with geometry, {} shared a checked tile's content), \
         {:.1} M shapes gathered; {world_s:.1} s on {threads} threads (+ {index_s:.1} s building cell indexes)",
        report.violations.len(),
        report.tiles,
        report.nonempty,
        report.shared,
        report.shapes as f64 / 1e6
    );

    let t = Instant::now();
    let review = review_flags(&db, &flags, &report.violations);
    println!(
        "intra-cell flags placed in the world: {} — genuine {} · context-only {}; only in context: {} ({:.1} s)",
        review.placed,
        review.genuine,
        review.context_only,
        review.only_in_context,
        t.elapsed().as_secs_f64()
    );

    let mut by_rule: BTreeMap<String, usize> = BTreeMap::new();
    for v in &report.violations {
        let name = fluideda_core::Deck::from_name(&deck)
            .and_then(|d| d.entry(v.layer))
            .map_or(format!("{}/{}", v.layer.layer, v.layer.datatype), |e| e.name.to_string());
        let kind = if v.kind == DrcKind::Width { "width" } else { "spacing" };
        *by_rule.entry(format!("{name} {kind}")).or_default() += 1;
    }
    println!("in context by rule: {by_rule:?}");

    if let Some(out) = flag("--out") {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&out).expect("create"));
        for v in &report.violations {
            let kind = if v.kind == DrcKind::Width { "width" } else { "space" };
            writeln!(
                f,
                "{{\"kind\":\"{kind}\",\"layer\":{},\"datatype\":{},\"bbox\":[{},{},{},{}]}}",
                v.layer.layer, v.layer.datatype, v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y
            )
            .unwrap();
        }
        println!("wrote {} markers to {out}", report.violations.len());
    }
}
