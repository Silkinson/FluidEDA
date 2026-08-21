//! Native DRC timing (Phase 5 SLICE 3): load a GDS, run the intra-cell check on
//! its biggest cell twice — COLD (includes the one-time lazy R-tree build) and
//! WARM (the honest re-check-after-one-edit cost: edits never mutate the base,
//! so the index stays valid and an edit re-checks the whole cell warm).
//!
//! Run: cargo run --release -p fluideda-core --example drc_bench -- <file.gds>

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: drc_bench <file.gds>");
    let bytes = std::fs::read(&path).expect("read gds");
    let t = Instant::now();
    let db = fluideda_core::gds::load_gds_db(&bytes).expect("parse");
    println!("parsed {} in {:.1} s", path, t.elapsed().as_secs_f64());

    let biggest = (0..db.cells.len()).max_by_key(|&c| db.cell(c).shapes.len()).unwrap();
    let shapes = db.cell(biggest).shapes.len();
    println!("biggest cell: '{}' ({} shapes)", db.cell(biggest).name, shapes);

    let rules = fluideda_core::DrcRules::default_table();
    let overlay = fluideda_core::EditOverlay::new();

    let t0 = Instant::now();
    let v1 = fluideda_core::check_cell(db.cell(biggest), biggest, &overlay, &rules);
    let cold = t0.elapsed().as_secs_f64() * 1e3;
    let t1 = Instant::now();
    let v2 = fluideda_core::check_cell(db.cell(biggest), biggest, &overlay, &rules);
    let warm = t1.elapsed().as_secs_f64() * 1e3;
    assert_eq!(v1.len(), v2.len());
    println!("cold (incl. index build): {cold:.0} ms");
    println!("warm (re-check after an edit): {warm:.0} ms");
    let widths = v1.iter().filter(|v| v.kind == fluideda_core::DrcKind::Width).count();
    println!("violations: {} ({} width, {} spacing)", v1.len(), widths, v1.len() - widths);
    // Which layers dominate (top 5) — tells us whether the blanket default rule
    // is flagging real drawing layers or marker/pin datatypes.
    let mut by_layer: std::collections::HashMap<(u16, u16), usize> = std::collections::HashMap::new();
    for v in &v1 {
        *by_layer.entry((v.layer.layer, v.layer.datatype)).or_default() += 1;
    }
    let mut top: Vec<_> = by_layer.into_iter().collect();
    top.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    for ((l, d), n) in top.into_iter().take(5) {
        println!("  L{l}/{d}: {n}");
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
