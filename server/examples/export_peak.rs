//! One server-side export, for its peak memory (Phase 7.9): opens the
//! store, exports design `<sha256>` with no edits as GDSII (or OASIS with
//! `oas`) into it, and prints what it wrote. Run it under `/usr/bin/time -l`
//! (macOS) or `-v` (Linux) for the process's peak resident set.
//!
//! Run: cargo run --release -p fluideda-server --example export_peak -- <store> <sha256> [gds|oas]

use fluideda_server::store::Store;

fn main() {
    let usage = "usage: export_peak <store> <sha256> [gds|oas]";
    let mut args = std::env::args().skip(1);
    let root = args.next().expect(usage);
    let design = args.next().expect(usage);
    let oasis = args.next().as_deref() == Some("oas");
    let store = Store::open(root).expect("open store");
    let none = fluideda_sync::Presence { cursor: glam::Vec2::ZERO, name: String::new(), color: [0.0; 4] };
    let state = fluideda_sync::Session::new(none).state();
    match fluideda_server::export::export(&store, &design, &state, oasis) {
        Ok(e) => println!("{} as {}: {} B in {:.0} ms", e.hash, e.format, e.bytes, e.ms),
        Err(e) => println!("export failed: {e:?}"),
    }
}
