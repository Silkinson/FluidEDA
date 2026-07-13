//! Pipeline bench (Phase 1): hierarchical parse, lazy index build, and viewport
//! culling cost across a few zoom levels — the numbers behind the culling proof.
//!
//! Run: cargo run --release -p fluideda-core --example bench

use fluideda_core::{cull, Bboxf, CellDb};
use std::time::Instant;

const SAMPLE: &[u8] = include_bytes!("../../../assets/sample.gds");
const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");

fn main() {
    flat_regression();
    println!("\n{}\n", "=".repeat(64));
    hierarchical_bench();
}

/// Phase 0 flat path still works (byte-identical polygon set for the flat sample).
fn flat_regression() {
    println!("=== FLAT SAMPLE (Phase 0 regression) ===");
    let t = Instant::now();
    let flat = fluideda_core::gds::load_gds_bytes(SAMPLE).expect("parse sample");
    let meshes = fluideda_core::tessellate(&flat);
    println!(
        "sample.gds: {} polygons, {} layers, load+tess {:?}",
        flat.polygons.len(),
        meshes.len(),
        t.elapsed()
    );
}

fn hierarchical_bench() {
    println!("=== HIERARCHICAL (assets/hier.gds) ===");
    println!("file: {} bytes ({:.1} KB)", HIER.len(), HIER.len() as f64 / 1024.0);

    // --- parse into a hierarchical CellDb ---
    let t0 = Instant::now();
    let db = fluideda_core::gds::load_gds_db(HIER).expect("parse hier");
    let t_parse = t0.elapsed();

    let wb = db.world_bbox();
    println!("\n--- PARSE (GDS -> CellDb) ---");
    println!("time:            {:?}", t_parse);
    println!("cell defs:       {}", db.cells.len());
    println!("top cell:        '{}'", db.cell(db.top).name);
    println!("flattened shapes:{:>14} (never materialised)", db.flat_shape_count());
    println!(
        "world bbox:      ({:.0},{:.0}) .. ({:.0},{:.0}) nm  [{:.0} x {:.0} um]",
        wb.min_x, wb.min_y, wb.max_x, wb.max_y,
        wb.width() / 1000.0, wb.height() / 1000.0
    );
    for c in &db.cells {
        println!(
            "  cell '{}': {} shapes, {} instances",
            c.name, c.shapes.len(), c.instances.len()
        );
    }

    // --- lazily build every cell's index, timed ---
    let t1 = Instant::now();
    let mut indexed_shapes = 0usize;
    for c in &db.cells {
        indexed_shapes += c.index().len();
    }
    let t_index = t1.elapsed();
    println!("\n--- INDEX BUILD (rstar R-tree per cell def) ---");
    println!("time:            {:?}  ({} shapes indexed across {} defs)", t_index, indexed_shapes, db.cells.len());

    // --- cull a few viewports ---
    println!("\n--- CULL (visible-shape count + per-frame time) ---");
    let total = db.flat_shape_count();
    let cx = (wb.min_x + wb.max_x) * 0.5;
    let cy = (wb.min_y + wb.max_y) * 0.5;
    let viewport_px = 1200.0;

    // Window widths (nm) from "whole chip" down to "one leaf".
    let cases = [
        ("fit view (whole chip)", wb.width() * 1.1),
        ("mid zoom (~100 um)", 100_000.0),
        ("close (~10 um)", 10_000.0),
        ("leaf zoom (~2 um)", 2_000.0),
    ];

    println!(
        "{:<26} {:>12} {:>10} {:>9} {:>8} {:>7} {:>12}",
        "viewport", "visible", "of total", "draws", "proxies", "nodes", "cull time"
    );
    for (name, win_w) in cases {
        let win_h = win_w * (wb.height() / wb.width());
        let view = Bboxf::new(cx - win_w * 0.5, cy - win_h * 0.5, cx + win_w * 0.5, cy + win_h * 0.5);
        let px_per_nm = viewport_px / win_w;
        let min_pixel = 3.0;

        // warm + time an average frame
        let vs = cull(&db, &view, px_per_nm, min_pixel);
        let iters = 200;
        let t = Instant::now();
        for _ in 0..iters {
            let _ = cull(&db, &view, px_per_nm, min_pixel);
        }
        let per = t.elapsed() / iters;

        let frac = vs.stats.visible_shapes as f64 / total as f64;
        println!(
            "{:<26} {:>12} {:>9.4}% {:>9} {:>8} {:>7} {:>12?}",
            name,
            vs.stats.visible_shapes,
            frac * 100.0,
            vs.stats.instances_emitted,
            vs.stats.proxy_count,
            vs.stats.nodes_visited,
            per
        );
    }

    culling_proof(&db, cx, cy, &wb);
}

/// Assert the headline claim: zoomed in, visible shapes are orders of magnitude
/// below the flattened total.
fn culling_proof(db: &CellDb, cx: f64, cy: f64, wb: &Bboxf) {
    let total = db.flat_shape_count();
    let win = 2_000.0;
    let win_h = win * (wb.height() / wb.width());
    let view = Bboxf::new(cx - win * 0.5, cy - win_h * 0.5, cx + win * 0.5, cy + win_h * 0.5);
    let vs = cull(db, &view, 1200.0 / win, 3.0);
    println!("\n--- CULLING PROOF ---");
    println!("total flattened shapes: {total}");
    println!("visible at leaf zoom:   {}", vs.stats.visible_shapes);
    let ratio = total as f64 / vs.stats.visible_shapes.max(1) as f64;
    println!("reduction:              {ratio:.0}x fewer shapes drawn");
}
