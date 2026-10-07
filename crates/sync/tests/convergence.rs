//! Phase 7.1b: randomized convergence. Three peers make random edits over a
//! small id space, so they collide often: moves, deletes and vertex edits of
//! base shapes, adds and their edits and deletes, hidden and shown
//! placements, batches of those, undo and redo. Every update reaches every
//! other peer — in random order, some twice, some only at the very end.
//!
//! Properties:
//! * after everything is delivered, every peer holds the same edits, adds
//!   and hidden placements;
//! * an undo or redo changes only entries its own peer has written — it
//!   never reverts another peer's edit.
//!
//! `proptest` shrinks a failure to a minimal sequence of operations.

use fluideda_core::{AddId, Layer, PlacementKey, Point};
use fluideda_sync::{BatchOp, EditKind, Presence, Session};
use glam::Vec2;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// One peer's operation. Indices into the peer's live adds wrap around.
#[derive(Debug, Clone)]
enum Op {
    Move(usize, u32, i32, i32),
    Delete(usize, u32),
    SetPoints(usize, u32, i32),
    Add(usize, i32),
    DeleteAdded(usize),
    EditAdded(usize, i32),
    Hide(PlacementKey),
    Show(PlacementKey),
    Batch(Vec<Op>),
    Undo,
    Redo,
}

fn ring(k: i32) -> Vec<Point> {
    let (x, y) = (k * 37 % 500, k * 91 % 500);
    vec![Point::new(x, y), Point::new(x + 40 + k % 7, y), Point::new(x + 20, y + 30 + k % 5), Point::new(x, y)]
}

fn key() -> impl Strategy<Value = PlacementKey> {
    (0usize..2, 0u32..3, 0i32..2, 0i32..2)
}

fn simple_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..2, 0u32..4, -50i32..50, -50i32..50).prop_map(|(c, i, dx, dy)| Op::Move(c, i, dx, dy)),
        (0usize..2, 0u32..4).prop_map(|(c, i)| Op::Delete(c, i)),
        (0usize..2, 0u32..4, 0i32..1000).prop_map(|(c, i, k)| Op::SetPoints(c, i, k)),
        (0usize..2, 0i32..1000).prop_map(|(c, k)| Op::Add(c, k)),
        (0usize..8).prop_map(Op::DeleteAdded),
        (0usize..8, 0i32..1000).prop_map(|(i, k)| Op::EditAdded(i, k)),
        key().prop_map(Op::Hide),
        key().prop_map(Op::Show),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => simple_op(),
        1 => prop::collection::vec(simple_op(), 1..4).prop_map(Op::Batch),
        2 => Just(Op::Undo),
        1 => Just(Op::Redo),
    ]
}

/// Everything a peer shows, by entry: what convergence compares and what an
/// undo may touch.
fn view(s: &Session) -> BTreeMap<String, String> {
    let mut v = BTreeMap::new();
    for (id, k) in s.edits() {
        v.insert(format!("e{id:?}"), format!("{k:?}"));
    }
    for (id, g) in s.adds() {
        v.insert(format!("a{id:?}"), format!("{g:?}"));
    }
    for k in s.hidden() {
        v.insert(format!("h{k:?}"), String::from("hidden"));
    }
    v
}

fn live_adds(s: &Session) -> Vec<AddId> {
    let mut ids: Vec<AddId> = s.adds().into_iter().filter(|(_, g)| g.is_some()).map(|(id, _)| id).collect();
    ids.sort();
    ids
}

/// Apply `op` to peer `s`, recording the entries it writes into `wrote`.
fn apply(s: &mut Session, op: &Op, wrote: &mut BTreeSet<String>) {
    match op {
        Op::Move(c, i, dx, dy) => {
            s.set_edit((*c, *i), EditKind::Move { dx: *dx, dy: *dy });
            wrote.insert(format!("e{:?}", (*c, *i)));
        }
        Op::Delete(c, i) => {
            s.set_edit((*c, *i), EditKind::Delete);
            wrote.insert(format!("e{:?}", (*c, *i)));
        }
        Op::SetPoints(c, i, k) => {
            s.set_edit((*c, *i), EditKind::SetPoints(ring(*k)));
            wrote.insert(format!("e{:?}", (*c, *i)));
        }
        Op::Add(c, k) => {
            let id = s.add_shape(*c, Layer::new(1, 0), &ring(*k));
            wrote.insert(format!("a{id:?}"));
        }
        Op::DeleteAdded(i) => {
            let live = live_adds(s);
            if !live.is_empty() {
                let id = live[i % live.len()];
                s.delete_added(id);
                wrote.insert(format!("a{id:?}"));
            }
        }
        Op::EditAdded(i, k) => {
            let live = live_adds(s);
            if !live.is_empty() {
                let id = live[i % live.len()];
                s.set_added_geometry(id, 0, Layer::new(2, 0), &ring(*k));
                wrote.insert(format!("a{id:?}"));
            }
        }
        Op::Hide(k) => {
            s.apply_batch(vec![BatchOp::Hide(*k)]);
            wrote.insert(format!("h{k:?}"));
        }
        Op::Show(k) => {
            s.apply_batch(vec![BatchOp::Show(*k)]);
            wrote.insert(format!("h{k:?}"));
        }
        Op::Batch(ops) => {
            let live = live_adds(s);
            let mut batch = Vec::new();
            for o in ops {
                match o {
                    Op::Move(c, i, dx, dy) => batch.push(BatchOp::Edit((*c, *i), EditKind::Move { dx: *dx, dy: *dy })),
                    Op::Delete(c, i) => batch.push(BatchOp::Edit((*c, *i), EditKind::Delete)),
                    Op::SetPoints(c, i, k) => batch.push(BatchOp::Edit((*c, *i), EditKind::SetPoints(ring(*k)))),
                    Op::Add(c, k) => batch.push(BatchOp::Add { cell: *c, layer: Layer::new(1, 0), points: ring(*k) }),
                    Op::DeleteAdded(i) if !live.is_empty() => batch.push(BatchOp::DeleteAdded(live[i % live.len()])),
                    Op::Hide(k) => batch.push(BatchOp::Hide(*k)),
                    Op::Show(k) => batch.push(BatchOp::Show(*k)),
                    _ => {}
                }
            }
            for b in &batch {
                match b {
                    BatchOp::Edit(id, _) => wrote.insert(format!("e{id:?}")),
                    BatchOp::DeleteAdded(id) => wrote.insert(format!("a{id:?}")),
                    BatchOp::Hide(k) | BatchOp::Show(k) => wrote.insert(format!("h{k:?}")),
                    BatchOp::Add { .. } => false,
                };
            }
            for id in s.apply_batch(batch) {
                wrote.insert(format!("a{id:?}"));
            }
        }
        Op::Undo | Op::Redo => {
            let before = view(s);
            if matches!(op, Op::Undo) {
                s.undo();
            } else {
                s.redo();
            }
            let after = view(s);
            let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            for k in keys {
                if before.get(k) != after.get(k) {
                    assert!(wrote.contains(k), "{op:?} changed {k}, which this peer never wrote: {:?} → {:?}", before.get(k), after.get(k));
                }
            }
        }
    }
}

/// A small deterministic generator for delivery choices.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Peers get fixed client ids, so a failing case replays (and shrinks)
/// exactly.
fn peer(id: u64) -> Session {
    Session::with_client_id(Presence { cursor: Vec2::ZERO, name: format!("p{id}"), color: [0.0; 4] }, id)
}

/// Run `ops` (peer, op) on three peers; returns the peers. Every update goes
/// into one log in the order it was made, which is the order the room relays
/// it, and each peer reads that log at its own pace. Peers interleave at
/// random, some updates arrive twice, and a peer sometimes falls behind
/// until the end.
fn run(ops: &[(usize, Op)], seed: u64) -> [Session; 3] {
    let mut peers = [peer(1), peer(2), peer(3)];
    let mut wrote = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
    let mut log: Vec<(usize, Vec<u8>)> = Vec::new();
    let (mut read, mut behind) = ([0usize; 3], [false; 3]);
    let mut rng = Rng(seed | 1);
    let deliver = |peers: &mut [Session; 3], q: usize, (from, u): &(usize, Vec<u8>)| {
        if *from != q {
            peers[q].apply_remote(u).unwrap();
        }
    };
    for (p, op) in ops {
        let sv = peers[*p].state_vector();
        apply(&mut peers[*p], op, &mut wrote[*p]);
        log.push((*p, peers[*p].update_since(&sv).unwrap()));
        for _ in 0..rng.below(4) {
            let q = rng.below(3);
            match rng.below(10) {
                0 => behind[q] = !behind[q],
                1 if read[q] > 0 => {
                    let i = rng.below(read[q]);
                    deliver(&mut peers, q, &log[i]); // again
                }
                _ if !behind[q] && read[q] < log.len() => {
                    deliver(&mut peers, q, &log[read[q]]);
                    read[q] += 1;
                }
                _ => {}
            }
        }
    }
    for q in 0..3 {
        for entry in &log[read[q]..] {
            deliver(&mut peers, q, entry);
        }
    }
    peers
}

/// `CONV_REORDER=1`: deliver every update in an arbitrary order instead, as
/// no FluidEDA transport does (the room relays in order; reconnects send
/// whole states). yrs does not converge under this: 0.19 drops a deletion
/// that arrives before anything from its item's client, and 0.28 still
/// diverges on a map key overwritten across undo and redo (Phase 7.1b
/// findings). Kept to check a future yrs.
fn run_reordered(ops: &[(usize, Op)], seed: u64) -> [Session; 3] {
    let mut peers = [peer(1), peer(2), peer(3)];
    let mut wrote = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
    let mut inbox: Vec<Vec<Vec<u8>>> = vec![Vec::new(); 3];
    let mut rng = Rng(seed | 1);
    for (p, op) in ops {
        let sv = peers[*p].state_vector();
        apply(&mut peers[*p], op, &mut wrote[*p]);
        let update = peers[*p].update_since(&sv).unwrap();
        for (q, inbox) in inbox.iter_mut().enumerate() {
            if q != *p {
                inbox.push(update.clone());
            }
        }
        for _ in 0..rng.below(4) {
            let q = rng.below(3);
            if !inbox[q].is_empty() {
                let i = rng.below(inbox[q].len());
                let u = inbox[q].swap_remove(i);
                peers[q].apply_remote(&u).unwrap();
            }
        }
    }
    for (q, mut rest) in inbox.into_iter().enumerate() {
        while !rest.is_empty() {
            let i = rng.below(rest.len());
            peers[q].apply_remote(&rest.swap_remove(i)).unwrap();
        }
    }
    peers
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn three_peers_converge(
        ops in prop::collection::vec((0usize..3, op()), 1..600),
        seed in any::<u64>(),
    ) {
        let peers = if std::env::var("CONV_REORDER").is_ok() { run_reordered(&ops, seed) } else { run(&ops, seed) };
        let (a, b, c) = (view(&peers[0]), view(&peers[1]), view(&peers[2]));
        for (x, y) in [(&a, &b), (&a, &c)] {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys.into_iter().filter(|k| x.get(*k) != y.get(*k)) {
                eprintln!("differs: {k}: {:?} vs {:?}", x.get(k), y.get(k));
            }
        }
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&a, &c);
    }
}
