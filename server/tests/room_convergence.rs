//! Phase 7.1b, the server in the loop: three peers edit through a `Room`,
//! which applies, logs and relays each update. At random points the room
//! writes a snapshot, or crashes: its WAL is cut at a random byte (losing
//! any suffix, which is more than a process crash can) and the room is
//! restored from disk. Peers then reconnect: each takes the room's state and
//! pushes its whole own, as a session does when its socket opens. Once
//! everything is delivered, the room's document and every peer's must agree.
//!
//! `proptest` shrinks a failure to a minimal sequence of events.

use fluideda_core::{Layer, PlacementKey, Point};
use fluideda_server::room::{Room, SnapshotPolicy};
use fluideda_server::store::Store;
use fluideda_sync::{BatchOp, EditKind, Presence, Session};
use glam::Vec2;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

const TAG_DOC: u8 = fluideda_sync::tag::DOC;

#[derive(Debug, Clone)]
enum Event {
    /// Peer `p` edits base shape `(cell, idx)`: 0 move, 1 delete, 2 vertices.
    Edit(usize, usize, u32, u8, i32),
    /// Peer `p` adds a shape, or deletes one of its live adds.
    Add(usize, i32),
    DeleteAdded(usize, usize),
    /// Peer `p` hides or shows a placement.
    Hide(usize, PlacementKey, bool),
    Undo(usize),
    /// Deliver up to this many relayed updates to peer `p`.
    Deliver(usize, usize),
    Snapshot,
    /// Crash, keeping this share (0–255 ⇒ 0–100%) of the WAL.
    Crash(u8),
}

fn ring(k: i32) -> Vec<Point> {
    let (x, y) = (k * 37 % 500, k * 91 % 500);
    vec![Point::new(x, y), Point::new(x + 40 + k % 7, y), Point::new(x + 20, y + 30 + k % 5), Point::new(x, y)]
}

fn event() -> impl Strategy<Value = Event> {
    let p = 0usize..3;
    prop_oneof![
        6 => (p.clone(), 0usize..2, 0u32..4, 0u8..3, 0i32..1000).prop_map(|(p, c, i, k, v)| Event::Edit(p, c, i, k, v)),
        2 => (p.clone(), 0i32..1000).prop_map(|(p, k)| Event::Add(p, k)),
        1 => (p.clone(), 0usize..8).prop_map(|(p, i)| Event::DeleteAdded(p, i)),
        2 => (p.clone(), (0usize..2, 0u32..3, 0i32..2, 0i32..2), any::<bool>()).prop_map(|(p, k, h)| Event::Hide(p, k, h)),
        1 => p.clone().prop_map(Event::Undo),
        6 => (p, 1usize..8).prop_map(|(p, n)| Event::Deliver(p, n)),
        1 => Just(Event::Snapshot),
        1 => any::<u8>().prop_map(Event::Crash),
    ]
}

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

fn framed(update: &[u8]) -> Vec<u8> {
    let mut m = vec![TAG_DOC];
    m.extend_from_slice(update);
    m
}

struct Peer {
    s: Session,
    rx: broadcast::Receiver<(u64, Vec<u8>)>,
}

/// Join `room`: take its whole state, then push this peer's whole own.
fn connect(room: &Room, id: u64, s: &mut Session) -> broadcast::Receiver<(u64, Vec<u8>)> {
    let (state, rx) = room.join(None).expect("an unbound room admits everyone");
    s.apply_remote(&state[1..]).unwrap();
    assert!(room.apply(id, framed(&s.update_since(&[0]).unwrap())));
    rx
}

fn receive(p: &mut Peer, id: u64, n: usize) {
    for _ in 0..n {
        match p.rx.try_recv() {
            Ok((from, msg)) if from != id && msg.first() == Some(&TAG_DOC) => p.s.apply_remote(&msg[1..]).unwrap(),
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                // The server resyncs a lagging peer with the whole state.
                unreachable!("1,024 frames of slack are never exceeded here")
            }
            Err(_) => break,
        }
    }
}

fn run(events: &[Event]) -> Result<(), TestCaseError> {
    let dir = std::env::temp_dir().join(format!("fluideda-conv-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(&dir).unwrap());
    let mut room = Room::restore(store.clone(), "conv").unwrap();
    let mut peers: Vec<Peer> = (0..3)
        .map(|i| {
            let mut s = Session::new(Presence { cursor: Vec2::ZERO, name: format!("p{i}"), color: [0.0; 4] });
            let rx = connect(&room, i as u64, &mut s);
            Peer { s, rx }
        })
        .collect();
    for e in events {
        let author = match e {
            Event::Edit(p, ..) | Event::Add(p, _) | Event::DeleteAdded(p, _) | Event::Hide(p, ..) | Event::Undo(p) => Some(*p),
            _ => None,
        };
        let sv = author.map(|p| peers[p].s.state_vector());
        match e {
            Event::Edit(p, c, i, k, v) => {
                let kind = match k {
                    0 => EditKind::Move { dx: v % 97 - 48, dy: v % 89 - 44 },
                    1 => EditKind::Delete,
                    _ => EditKind::SetPoints(ring(*v)),
                };
                peers[*p].s.set_edit((*c, *i), kind);
            }
            Event::Add(p, k) => {
                peers[*p].s.add_shape(0, Layer::new(1, 0), &ring(*k));
            }
            Event::DeleteAdded(p, i) => {
                let live: Vec<_> = peers[*p].s.adds().into_iter().filter(|(_, g)| g.is_some()).map(|(id, _)| id).collect();
                if !live.is_empty() {
                    peers[*p].s.delete_added(live[i % live.len()]);
                }
            }
            Event::Hide(p, key, hide) => {
                let op = if *hide { BatchOp::Hide(*key) } else { BatchOp::Show(*key) };
                peers[*p].s.apply_batch(vec![op]);
            }
            Event::Undo(p) => {
                peers[*p].s.undo();
            }
            Event::Deliver(p, n) => receive(&mut peers[*p], *p as u64, *n),
            Event::Snapshot => {
                room.snapshot(&SnapshotPolicy::default(), Instant::now(), true).unwrap();
            }
            Event::Crash(keep) => {
                drop(room);
                let wal = dir.join("rooms").join("conv").join("wal");
                if let Ok(len) = std::fs::metadata(&wal).map(|m| m.len()) {
                    let f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
                    f.set_len(len * *keep as u64 / 255).unwrap();
                }
                room = Room::restore(store.clone(), "conv").unwrap();
                for (i, p) in peers.iter_mut().enumerate() {
                    p.rx = connect(&room, i as u64, &mut p.s);
                }
            }
        }
        if let (Some(p), Some(sv)) = (author, sv) {
            let update = peers[p].s.update_since(&sv).unwrap();
            assert!(room.apply(p as u64, framed(&update)));
        }
    }
    // Everyone reconnects one last time, then takes everything relayed.
    for (i, p) in peers.iter_mut().enumerate() {
        p.rx = connect(&room, i as u64, &mut p.s);
    }
    for (i, p) in peers.iter_mut().enumerate() {
        receive(p, i as u64, usize::MAX);
    }
    let server = Session::from_state(&room.design_and_state().1).unwrap();
    let want = view(&server);
    for p in &peers {
        prop_assert_eq!(&view(&p.s), &want);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn peers_and_a_crashing_room_converge(events in prop::collection::vec(event(), 1..300)) {
        run(&events)?;
    }
}
