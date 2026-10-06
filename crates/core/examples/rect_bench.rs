//! Phase 7.7 A/B: compact rectangles on vs off, same build.
//!
//!   cargo run --release -p fluideda-core --example rect_bench -- <design.gds>
//!
//! For each mode: parse time, the shape stores' heap bytes, tessellating every
//! chunk of every cell, and the GDS export's SHA-256 — which must be the same
//! (compaction is lossless: every rectangle re-expands to its exact ring). Also
//! compares every shape's ring between the two loads.

use sha2::{Digest, Sha256};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: rect_bench <design.gds>");
    let bytes = std::fs::read(&path).expect("read design");
    let mut loads = Vec::new();
    for compact in [false, true] {
        fluideda_core::set_compact_rects(compact);
        let t = Instant::now();
        let db = fluideda_core::load_layout_db(&bytes).expect("parse");
        let parse_ms = t.elapsed().as_secs_f64() * 1e3;
        let heap: usize = db.cells.iter().map(|c| c.shapes.heap_bytes()).sum();
        let rects: usize = db.cells.iter().map(|c| (0..c.shapes.len()).filter(|&i| c.shapes.rect(i).is_some()).count()).sum();
        let t = Instant::now();
        let mut tris = 0usize;
        for c in &db.cells {
            for ch in &c.chunks {
                tris += fluideda_core::tessellate_chunk(&c.shapes, ch).iter().map(|m| m.indices.len() / 3).sum::<usize>();
            }
        }
        let tess_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let gds = fluideda_core::write_gds(&db, &fluideda_core::EditOverlay::new());
        let write_ms = t.elapsed().as_secs_f64() * 1e3;
        let sha: String = Sha256::digest(&gds).iter().map(|b| format!("{b:02x}")).collect();
        println!(
            "compact {compact:5}: parse {parse_ms:6.0} ms, shape stores {:6.1} MB ({rects} rectangles stored compactly), \
             tessellate all {tess_ms:6.0} ms ({tris} triangles), export {write_ms:5.0} ms, {} B, sha {}",
            heap as f64 / 1e6,
            gds.len(),
            &sha[..16]
        );
        loads.push((db, sha));
    }
    let (a, b) = (&loads[0].0, &loads[1].0);
    let mut diff = 0usize;
    for (ca, cb) in a.cells.iter().zip(&b.cells) {
        for i in 0..ca.shapes.len() {
            if *ca.shapes.points(i) != *cb.shapes.points(i) || ca.shapes.layer(i) != cb.shapes.layer(i) {
                diff += 1;
            }
        }
    }
    println!(
        "same shapes: {} ({diff} differ); same export: {}",
        diff == 0,
        loads[0].1 == loads[1].1
    );
}
