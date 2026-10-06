//! Cull cost with and without placement chunks (Phase 7.3), same binary, same
//! session: the design as built (big cells' placements in spatial chunks),
//! then the same design with every cell's placements collapsed into one chunk
//! (the pre-7.3 walk). Both must select identical draws and proxies wherever
//! no placement chunk is too dense for its size on screen (Phase 7.6: such a
//! chunk is one proxy instead of its placements); the difference is the work
//! — nodes visited and wall time.
//!
//! Views: the fit view, then a scan of the die in 50 µm windows (leaf-ish
//! zoom) and in 400 µm windows (mid zoom).
//!
//! Run: cargo run --release -p fluideda-core --example cull_bench -- testdata/caravel/gds/caravel.gds

use fluideda_core::chunk::Chunk;
use fluideda_core::Bboxf;
use fluideda_core::{cull, load_layout_db, CellDb};
use std::time::Instant;

const VIEWPORT_PX: f64 = 1280.0;

fn views(db: &CellDb, span: f64) -> Vec<Bboxf> {
    let w = db.world_bbox();
    let mut out = Vec::new();
    let mut y = w.min_y;
    while y < w.max_y {
        let mut x = w.min_x;
        while x < w.max_x {
            out.push(Bboxf { min_x: x, min_y: y, max_x: x + span, max_y: y + span * 0.625 });
            x += span * 2.5;
        }
        y += span * 2.5;
    }
    out
}

/// Total wall ms, nodes visited, draws + proxies, and placement chunks drawn
/// as one proxy for density, over `views` (3 rounds, best).
fn run(db: &CellDb, views: &[Bboxf]) -> (f64, u64, usize, u64) {
    let mut best = f64::MAX;
    let (mut nodes, mut emitted, mut dense) = (0, 0, 0);
    for _ in 0..3 {
        let t = Instant::now();
        nodes = 0;
        emitted = 0;
        dense = 0;
        for v in views {
            let vs = cull(db, v, VIEWPORT_PX / (v.max_x - v.min_x), 4.0);
            nodes += vs.stats.nodes_visited;
            emitted += vs.draws.len() + vs.proxies.len();
            dense += vs.stats.dense_inst_chunks;
        }
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    (best, nodes, emitted, dense)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: cull_bench <layout file>");
    let bytes = std::fs::read(&path).expect("read layout");
    let mut db = load_layout_db(&bytes).expect("parse");
    drop(bytes);
    let w = db.world_bbox();
    let chunked_cells = db.cells.iter().filter(|c| c.inst_chunks.len() > 1).count();
    let chunks: usize = db.cells.iter().map(|c| c.inst_chunks.len()).sum();
    println!("{path}: {} cells, {chunked_cells} with placement grids, {chunks} placement chunks", db.cells.len());

    let fit = vec![Bboxf { min_x: w.min_x, min_y: w.min_y, max_x: w.max_x, max_y: w.max_y }];
    let cases = [("fit", fit), ("400 µm scan", views(&db, 400_000.0)), ("50 µm scan", views(&db, 50_000.0))];
    let mut chunked = Vec::new();
    for (_, v) in &cases {
        chunked.push(run(&db, v));
    }
    // The pre-7.3 walk: one chunk per cell.
    for id in 0..db.cells.len() {
        let n = db.cells[id].instances.len() as u32;
        if n > 0 {
            let bbox = db.local_bbox(id);
            db.cells[id].inst_chunks = vec![Chunk { start: 0, end: n, bbox }];
        }
    }
    for ((name, v), (ms, nodes, emitted, dense)) in cases.iter().zip(chunked) {
        let (ms0, nodes0, emitted0, _) = run(&db, v);
        if dense == 0 {
            assert_eq!(emitted, emitted0, "{name}: chunked and walked culls must select the same");
        }
        println!(
            "{name:>12} ({:>4} views): walk-all {ms0:>8.1} ms, {nodes0:>10} nodes, {emitted0:>7} drawn | chunked {ms:>8.1} ms, {nodes:>10} nodes, {emitted:>7} drawn ({dense} dense placement chunks) | {:.1}× less time",
            v.len(),
            ms0 / ms.max(1e-9)
        );
    }
}
