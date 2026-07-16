//! Phase 1.5 native measurement harness (CPU + memory; no GPU).
//!
//! Reports, per file: size, parse time + throughput, distinct cell defs /
//! polygons, flattened count, per-cell R-tree build time (total + p99),
//! tessellation time + vertex/index memory, peak process RSS, and cull time +
//! visible count at fit / mid / leaf-zoom viewports.
//!
//! Run:  cargo run --release -p fluideda-core --example bench [-- FILE.gds ...]
//! With no file arg it uses the embedded hier.gds.

use fluideda_core::{cull, cull_counted, Bboxf};
use std::time::{Duration, Instant};

const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("# no file arg -> embedded hier.gds (pass paths to measure real chips)\n");
        measure("hier.gds (embedded)", HIER.to_vec());
    } else {
        for (i, path) in args.iter().enumerate() {
            if i > 0 {
                println!("\n{}\n", "#".repeat(72));
            }
            match std::fs::read(path) {
                Ok(bytes) => measure(path, bytes),
                Err(e) => println!("!! {path}: read failed: {e}"),
            }
        }
    }
}

fn measure(name: &str, bytes: Vec<u8>) {
    let size = bytes.len();
    println!("========================================================================");
    println!("FILE: {name}");
    println!("  size:            {}", human_bytes(size));
    let rss0 = peak_rss();

    // --- parse -> CellDb ---
    let t = Instant::now();
    let db = match fluideda_core::gds::load_gds_db(&bytes) {
        Ok(db) => db,
        Err(e) => {
            println!("  !! parse failed: {e}");
            return;
        }
    };
    let t_parse = t.elapsed();
    drop(bytes); // free the raw file buffer before measuring geometry memory

    let cell_defs = db.cells.len();
    let distinct_polys: usize = db.cells.iter().map(|c| c.shapes.len()).sum();
    let flat = db.flat_shape_count();
    let wb = db.world_bbox();
    println!("\n  PARSE");
    println!("    time:          {:?}  ({:.1} MB/s)", t_parse, mb_per_s(size, t_parse));
    println!("    cell defs:     {cell_defs}");
    println!("    distinct polys:{distinct_polys}  (sum of per-cell shapes)");
    println!("    flattened:     {flat}");
    println!("    world bbox:    {:.0} x {:.0} um", wb.width() / 1000.0, wb.height() / 1000.0);
    let chunked: Vec<_> = db.cells.iter().filter(|c| c.chunks.len() > 1).collect();
    let nchunks: usize = db.cells.iter().map(|c| c.chunks.len()).sum();
    println!("    shape store:   {} (columnar: coords+offsets+layers)", human_bytes(db.store_bytes()));
    println!("    chunks:        {nchunks} total; {} cells chunked", chunked.len());
    for c in chunked.iter() {
        let (n, mean, worst) = fluideda_core::chunk::chunk_stats(&c.chunks, &c.shapes.total_bbox());
        println!(
            "      {:<26} {:>4} chunks, mean {:>6.0} shapes, worst loose bbox {:>5.1}x grid sq",
            c.name, n, mean, worst
        );
    }
    println!(
        "    peak RSS:      {} (+{} since start)",
        human_bytes(peak_rss()),
        human_bytes(peak_rss().saturating_sub(rss0))
    );

    // --- per-cell R-tree build (total + p99) ---
    let mut durs: Vec<Duration> = Vec::with_capacity(cell_defs);
    let t = Instant::now();
    let mut indexed = 0usize;
    for c in &db.cells {
        let tc = Instant::now();
        indexed += c.index().len();
        durs.push(tc.elapsed());
    }
    let t_index = t.elapsed();
    durs.sort();
    println!("\n  INDEX (rstar R-tree per cell def, lazy)");
    println!("    total:         {:?}  ({} shapes indexed)", t_index, indexed);
    println!("    per-cell p50:  {:?}", pct(&durs, 0.50));
    println!("    per-cell p99:  {:?}", pct(&durs, 0.99));
    println!("    per-cell max:  {:?}", durs.last().copied().unwrap_or_default());

    // --- tessellation (every chunk of every cell def = the whole-file cost) ---
    let t = Instant::now();
    let mut vbytes = 0usize;
    let mut ibytes = 0usize;
    let mut tris = 0usize;
    for c in &db.cells {
        let meshes: Vec<_> = c
            .chunks
            .iter()
            .flat_map(|ch| fluideda_core::tessellate_chunk(&c.shapes, ch))
            .collect();
        for m in &meshes {
            vbytes += m.vertices.len() * std::mem::size_of::<fluideda_core::Vertex>();
            ibytes += m.indices.len() * 4;
            tris += m.indices.len() / 3;
        }
    }
    let t_tess = t.elapsed();
    println!("\n  TESSELLATE (eager: every chunk of all {cell_defs} cell defs, cell-local)");
    println!("    time:          {:?}", t_tess);
    println!("    triangles:     {tris}");
    println!("    vertex mem:    {}", human_bytes(vbytes));
    println!("    index mem:     {}", human_bytes(ibytes));
    println!("    GPU geo total: {}", human_bytes(vbytes + ibytes));
    println!("    peak RSS:      {}", human_bytes(peak_rss()));

    // --- cull at three viewports ---
    // Two timings: the render-path cull (no visible-count, what a frame pays) and
    // the counted cull (with the visible-shape metric). The gap is the cost we
    // took off the hot path in Phase 2a STEP 2.
    println!("\n  CULL (per-frame time: hot path vs counted)");
    println!(
        "    {:<12} {:>14} {:>10} {:>9} {:>8} {:>12} {:>12}",
        "viewport", "visible", "of total", "instances", "proxies", "cull(hot)", "cull(counted)"
    );
    let (cx, cy) = ((wb.min_x + wb.max_x) * 0.5, (wb.min_y + wb.max_y) * 0.5);
    let cases = [
        ("fit", wb.width().max(wb.height()) * 1.05),
        ("mid", (wb.width() * 0.03).clamp(20_000.0, 300_000.0)),
        ("leaf-zoom", 5_000.0),
    ];
    for (label, win_w) in cases {
        let aspect = if wb.width() > 0.0 { wb.height() / wb.width() } else { 1.0 };
        let win_h = win_w * aspect;
        let view = Bboxf::new(cx - win_w * 0.5, cy - win_h * 0.5, cx + win_w * 0.5, cy + win_h * 0.5);
        let px_per_nm = 1200.0 / win_w;
        let vs = cull_counted(&db, &view, px_per_nm, 4.0);
        let iters = 60;
        let t = Instant::now();
        for _ in 0..iters {
            let _ = cull(&db, &view, px_per_nm, 4.0);
        }
        let per_hot = t.elapsed() / iters;
        let t = Instant::now();
        for _ in 0..iters {
            let _ = cull_counted(&db, &view, px_per_nm, 4.0);
        }
        let per_counted = t.elapsed() / iters;
        println!(
            "    {:<12} {:>14} {:>9.4}% {:>9} {:>8} {:>12?} {:>12?}",
            label,
            vs.stats.visible_shapes,
            vs.stats.visible_shapes as f64 / flat.max(1) as f64 * 100.0,
            vs.stats.instances_emitted,
            vs.stats.proxy_count,
            per_hot,
            per_counted
        );
    }
    println!("\n  FINAL peak RSS: {}", human_bytes(peak_rss()));
}

fn mb_per_s(size: usize, d: Duration) -> f64 {
    if size == 0 || d.as_secs_f64() == 0.0 {
        return f64::NAN;
    }
    (size as f64 / 1e6) / d.as_secs_f64()
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn human_bytes(b: usize) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Process peak resident set size. macOS `ru_maxrss` is in **bytes**; Linux KB.
fn peak_rss() -> usize {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0;
        }
        let m = ru.ru_maxrss as usize;
        if cfg!(target_os = "macos") {
            m
        } else {
            m * 1024
        }
    }
}
