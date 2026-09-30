//! How big a room's document gets, and what persisting it costs (Phase 6
//! findings). Builds rooms of N edits in the sync crate's formats — a mix of
//! moves, deletes and drawn rectangles, each its own transaction as clients
//! send them — then measures:
//!
//! * the snapshot: encoding the whole state, and writing it (blob + fsync +
//!   log + head), which the room lock is held across;
//! * restore from the snapshot (a graceful restart);
//! * restore from the WAL alone (a crash before any snapshot: one record per
//!   edit — the worst case the snapshot policy bounds).
//!
//! Run: cargo run --release -p fluideda-server --example room_scale

use fluideda_server::room::Room;
use fluideda_server::store::{wal_append, Store};
use std::sync::Arc;
use std::time::Instant;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact};

fn main() {
    for n in [1_000usize, 10_000, 100_000] {
        let dir = std::env::temp_dir().join(format!("fluideda-scale-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let doc = Doc::new();
        let edits = doc.get_or_insert_map("edits");
        let mut wal = store.open_wal("wal-only").unwrap();
        let mut wal_bytes = 0u64;
        let t = Instant::now();
        for i in 0..n {
            let mut txn = doc.transact_mut();
            let (cell, idx) = (i % 997, i / 997);
            match i % 10 {
                0 => {
                    let (x, y) = ((i * 37 % 100_000) as i32, (i * 91 % 100_000) as i32);
                    let v = format!("A 12 68 20 {x} {y} {} {y} {} {} {x} {} {x} {y}", x + 400, x + 400, y + 170, y + 170);
                    edits.insert(&mut txn, format!("a4242424242:{i}"), v);
                }
                1 | 2 => {
                    edits.insert(&mut txn, format!("{cell}:{idx}"), "D".to_string());
                }
                _ => {
                    edits.insert(&mut txn, format!("{cell}:{idx}"), format!("M {} {}", (i % 500) * 5, -((i % 300) as i32) * 5));
                }
            }
            wal_bytes += wal_append(&mut wal, &txn.encode_update_v1()).unwrap();
        }
        let build = t.elapsed();

        let t = Instant::now();
        let state = doc.transact().encode_state_as_update_v1(&StateVector::default());
        let encode = t.elapsed();
        let t = Instant::now();
        let hash = store.put_blob(&state).unwrap();
        store.record_snapshot("snap", &hash, state.len() as u64, 0).unwrap();
        let write = t.elapsed();

        let t = Instant::now();
        let room = Room::restore(store.clone(), "snap").unwrap();
        let restore_snap = t.elapsed();
        assert_eq!(room.state_frame().len(), state.len() + 1);

        let t = Instant::now();
        let room = Room::restore(store.clone(), "wal-only").unwrap();
        let restore_wal = t.elapsed();
        assert_eq!(room.state_frame().len(), state.len() + 1, "WAL replay rebuilds the same state");

        println!(
            "{n:>7} edits: state {:>8} B ({:.1} B/edit) | encode {:>6.1} ms, write {:>5.1} ms (snapshot, lock held) | \
             restore: snapshot {:>6.1} ms, WAL-only {:>7.1} ms ({} B WAL) | build {:.0} ms",
            state.len(),
            state.len() as f64 / n as f64,
            encode.as_secs_f64() * 1e3,
            write.as_secs_f64() * 1e3,
            restore_snap.as_secs_f64() * 1e3,
            restore_wal.as_secs_f64() * 1e3,
            wal_bytes,
            build.as_secs_f64() * 1e3,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
