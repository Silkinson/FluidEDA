//! Convert a layout between GDSII and OASIS with FluidEDA's own readers and
//! writers (Phase 6): the output format follows the output extension.
//!
//! Run: cargo run --release -p fluideda-core --example convert -- <in.gds|.oas> <out.oas|.gds>

use std::time::Instant;

fn main() {
    let src = std::env::args().nth(1).expect("usage: convert <in> <out>");
    let dst = std::env::args().nth(2).expect("usage: convert <in> <out>");
    let bytes = std::fs::read(&src).expect("read");
    let t = Instant::now();
    let db = fluideda_core::load_layout_db(&bytes).expect("parse");
    let read_ms = t.elapsed().as_secs_f64() * 1e3;
    let overlay = fluideda_core::EditOverlay::new();
    let t = Instant::now();
    let out = if dst.ends_with(".oas") || dst.ends_with(".oasis") {
        fluideda_core::write_oasis(&db, &overlay)
    } else {
        fluideda_core::write_gds(&db, &overlay)
    };
    let write_ms = t.elapsed().as_secs_f64() * 1e3;
    std::fs::write(&dst, &out).expect("write");
    println!(
        "{src} ({} B) → {dst} ({} B): read {read_ms:.0} ms, write {write_ms:.0} ms",
        bytes.len(),
        out.len()
    );
}
