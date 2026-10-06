//! Phase 7.8: what placements cost, before and after compaction.
//!
//!   cargo run --release -p fluideda-core --example place_bench -- <design>
//!
//! Before 7.8 a placement was ~104 bytes in memory (child + a full `f64`
//! transform + the array fields; the same on wasm32 and 64-bit hosts) and
//! 53 bytes encoded (+40 for an array). Now: `size_of::<Instance>()` (20 B on
//! wasm32) plus any boxed transform or lattice, and the varint encoding.
//! Every placement's transform must come back bit for bit.

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: place_bench <design>");
    let bytes = std::fs::read(&path).expect("read design");
    let t = Instant::now();
    let db = fluideda_core::load_layout_db(&bytes).expect("parse");
    let parse_ms = t.elapsed().as_secs_f64() * 1e3;
    drop(bytes);

    let insts: Vec<&fluideda_core::Instance> = db.cells.iter().flat_map(|c| &c.instances).collect();
    let n = insts.len();
    let compact = insts.iter().filter(|i| i.manhattan_place().is_some()).count();
    let arrays = insts.iter().filter(|i| matches!(i.kind(), fluideda_core::InstanceKind::Array { .. })).count();
    // Boxed parts: a general transform (48 B) or a lattice (40 B) each.
    let boxed = (n - compact) * 48 + arrays * 40;
    let host = n * std::mem::size_of::<fluideda_core::Instance>() + boxed;
    let wasm = n * 20 + boxed;
    let old_mem = n * 104;
    let old_enc = n * 53 + arrays * 40;

    let t = Instant::now();
    let mut enc = 0usize;
    let mut same = true;
    for c in &db.cells {
        if c.instances.is_empty() {
            continue;
        }
        let rec = fluideda_core::serialize::encode_instances(&c.instances);
        enc += rec.len() - 4;
        let back = fluideda_core::serialize::decode_instances(&rec, db.cells.len()).expect("decode");
        same &= back == c.instances;
    }
    let codec_ms = t.elapsed().as_secs_f64() * 1e3;

    println!(
        "{n} placements ({compact} compact, {arrays} arrays), parse {parse_ms:.0} ms\n\
         memory: {:.1} MB before -> {:.1} MB wasm32 ({:.1} MB on this host)\n\
         encoded: {:.1} MB before -> {:.1} MB ({:.1} B/placement); encode+decode {codec_ms:.0} ms; round trip exact: {same}",
        old_mem as f64 / 1e6,
        wasm as f64 / 1e6,
        host as f64 / 1e6,
        old_enc as f64 / 1e6,
        enc as f64 / 1e6,
        enc as f64 / n.max(1) as f64,
    );
}
