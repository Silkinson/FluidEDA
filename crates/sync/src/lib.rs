//! fluideda-sync — collaborative session state.
//!
//! Three channels over one WebSocket to the relay (`ewebsock`, wasm + native):
//!   * **presence** (ephemeral): each user's cursor + name (a tiny hand-rolled
//!     payload, tag [`tag::AWARENESS`]).
//!   * **markers** (persistent): the Phase 0 shared marker array.
//!   * **edits** (persistent, Phase 3): a CRDT map of shape edits — move/delete
//!     keyed by base shape id — plus a per-user [`UndoManager`].
//!
//! Markers and edits live in **one** `yrs::Doc`; we never hand-roll conflict
//! resolution — yrs guarantees convergence and the relay just rebroadcasts opaque
//! update blobs to the room.
//!
//! A `Session` exists **before** any connection (offline editing + undo work
//! against the local doc); [`Session::join`] later opens the socket. Local doc
//! mutations broadcast the delta since the last broadcast; remote updates are
//! applied with no origin, so the local [`UndoManager`] — which tracks only this
//! client's origin — never captures a peer's edits. That is what makes undo
//! **per-user**: undo in A reverts only A's edits.

use anyhow::Result;
use ewebsock::{WsMessage, WsReceiver, WsSender};
use fluideda_core::BaseShapeId;
use glam::Vec2;
use std::collections::HashMap;
use std::sync::Arc;
use yrs::types::ToJson;
use yrs::undo::{Options as UndoOptions, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayRef, Doc, Map, MapPrelim, MapRef, Origin, Out, ReadTxn, StateVector, Transact,
};

/// A shared rectangle marker as stored in the CRDT.
#[derive(Debug, Clone, Copy)]
pub struct Marker {
    pub min: Vec2,
    pub max: Vec2,
    pub color: [f32; 4],
}

/// One collaborator's ephemeral presence.
#[derive(Debug, Clone)]
pub struct Presence {
    pub cursor: Vec2,
    pub name: String,
    pub color: [f32; 4],
}

/// A shape edit as carried by the CRDT. Geometry is NOT shipped — a move stores
/// only its translation, and the receiver reconstructs the moved shape from its
/// own copy of the base (both peers loaded the same GDS, so base ids + geometry
/// match). Keeps edits tiny and independent of tessellation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditKind {
    /// Translate the base shape by `(dx, dy)` nm.
    Move { dx: i32, dy: i32 },
    /// Tombstone the base shape.
    Delete,
}

/// Message envelope on the wire. Byte 0 tags the channel.
///   0x01 = yrs document update (markers + edits share the doc)
///   0x02 = awareness/presence update
pub mod tag {
    pub const DOC: u8 = 0x01;
    pub const AWARENESS: u8 = 0x02;
}

struct Socket {
    tx: WsSender,
    rx: WsReceiver,
}

/// Client-side session: the shared doc + undo, an optional socket, and a cache of
/// remote presence.
pub struct Session {
    doc: Doc,
    markers: ArrayRef,
    edits: MapRef,
    undo: UndoManager<()>,
    /// State vector already broadcast; the next broadcast sends only the delta
    /// past it. Advanced after each local mutation AND after applying a remote
    /// update, so a remote change is never echoed back as if it were local.
    last_sv: StateVector,
    /// This client's transaction origin. Local edits use it; the UndoManager
    /// tracks it, so only local edits are undoable.
    origin: Origin,
    socket: Option<Socket>,
    pub local: Presence,
    pub remote: HashMap<u64, Presence>,
    client_id: u64,
}

impl Session {
    /// Create an **offline** session: the shared doc, the edits map + undo, and
    /// no socket. Editing and undo work immediately; [`join`](Self::join)
    /// connects later.
    pub fn new(local: Presence) -> Self {
        let doc = Doc::new();
        let markers = doc.get_or_insert_array("markers");
        let edits = doc.get_or_insert_map("edits");
        let client_id = doc.client_id();
        let origin = Origin::from(client_id);

        // Undo config, constructed by hand because `UndoOptions::default()` is
        // non-wasm only (it wants a system clock). capture_timeout_millis = 0 makes
        // every tracked transaction its own undo step (so one nudge/delete = one
        // undo); the clock is therefore irrelevant and returns a constant.
        let opts = UndoOptions {
            capture_timeout_millis: 0,
            tracked_origins: Default::default(),
            capture_transaction: None,
            timestamp: Arc::new(|| 0u64),
        };
        let mut undo = UndoManager::with_scope_and_options(&doc, &edits, opts);
        // Track THIS client's origin, so the undo stack holds only local edits.
        undo.include_origin(origin.clone());

        let last_sv = doc.transact().state_vector();
        Self {
            doc,
            markers,
            edits,
            undo,
            last_sv,
            origin,
            socket: None,
            local,
            remote: HashMap::new(),
            client_id,
        }
    }

    /// Create a session and immediately connect to the relay.
    pub fn connect(url: &str, local: Presence) -> Result<Self> {
        let mut s = Self::new(local);
        s.join(url)?;
        Ok(s)
    }

    /// Open the socket for an offline session (the "Join room" path). On connect
    /// we push our whole doc state so a peer already in the room converges with
    /// our local edits.
    pub fn join(&mut self, url: &str) -> Result<()> {
        let (tx, rx) = ewebsock::connect(url, ewebsock::Options::default())
            .map_err(|e| anyhow::anyhow!("ws connect: {e}"))?;
        self.socket = Some(Socket { tx, rx });
        // Broadcast full state from the empty base so peers merge our edits.
        let full = self.doc.transact().encode_state_as_update_v1(&StateVector::default());
        self.send_doc(full);
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    /// Publish our cursor (world nm). No-op when offline.
    pub fn set_cursor(&mut self, world: Vec2) {
        self.local.cursor = world;
        let Some(sock) = &mut self.socket else { return };
        let mut buf = vec![tag::AWARENESS];
        buf.extend_from_slice(&self.client_id.to_le_bytes());
        buf.extend_from_slice(&world.x.to_le_bytes());
        buf.extend_from_slice(&world.y.to_le_bytes());
        buf.extend_from_slice(self.local.name.as_bytes());
        sock.tx.send(WsMessage::Binary(buf));
    }

    /// Add a shared marker; broadcasts to peers.
    pub fn add_marker(&mut self, m: Marker) {
        {
            let mut txn = self.doc.transact_mut();
            let entry = MapPrelim::from([
                ("minx".to_string(), m.min.x as f64),
                ("miny".to_string(), m.min.y as f64),
                ("maxx".to_string(), m.max.x as f64),
                ("maxy".to_string(), m.max.y as f64),
            ]);
            self.markers.push_back(&mut txn, entry);
        }
        self.broadcast_local();
    }

    /// Read all current markers (illustrative; kept from Phase 0).
    pub fn markers(&self) -> Vec<Marker> {
        let txn = self.doc.transact();
        let out = Vec::new();
        for item in self.markers.iter(&txn) {
            let _ = item.to_json(&txn);
        }
        out
    }

    // --- Phase 3 edits -------------------------------------------------------

    /// Record an edit of a base shape into the CRDT (local origin) and broadcast.
    /// Last-writer-wins per shape id: two peers editing the *same* shape converge
    /// on one entry; editing *different* shapes keeps both.
    pub fn set_edit(&mut self, id: BaseShapeId, kind: EditKind) {
        let key = key_of(id);
        let value = match kind {
            EditKind::Move { dx, dy } => format!("M {dx} {dy}"),
            EditKind::Delete => "D".to_string(),
        };
        {
            let mut txn = self.doc.transact_mut_with(self.origin.clone());
            self.edits.insert(&mut txn, key, value);
        }
        self.broadcast_local();
    }

    /// Every edit currently in the CRDT, parsed. The app rebuilds its overlay
    /// from this (translating moves against its base geometry).
    pub fn edits(&self) -> Vec<(BaseShapeId, EditKind)> {
        let txn = self.doc.transact();
        let mut out = Vec::new();
        for (k, v) in self.edits.iter(&txn) {
            let (Some(id), Out::Any(Any::String(s))) = (parse_key(k), v) else { continue };
            if let Some(kind) = parse_kind(&s) {
                out.push((id, kind));
            }
        }
        out
    }

    /// Undo the last **local** edit (per-user: the UndoManager tracks only this
    /// client's origin). Broadcasts the revert. Returns true if anything changed.
    pub fn undo(&mut self) -> bool {
        let changed = self.undo.undo().unwrap_or(false);
        if changed {
            self.broadcast_local();
        }
        changed
    }

    /// Redo the last undone local edit. Broadcasts. Returns true if changed.
    pub fn redo(&mut self) -> bool {
        let changed = self.undo.redo().unwrap_or(false);
        if changed {
            self.broadcast_local();
        }
        changed
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.undo_stack().is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undo.redo_stack().is_empty()
    }

    // --- transport -----------------------------------------------------------

    /// Send the delta since `last_sv` (the changes a local op just made), then
    /// advance `last_sv`. When offline, only advances — so a later `join` starts
    /// from a clean baseline.
    fn broadcast_local(&mut self) {
        let (update, sv) = {
            let txn = self.doc.transact();
            (txn.encode_state_as_update_v1(&self.last_sv), txn.state_vector())
        };
        self.last_sv = sv;
        if !update.is_empty() {
            self.send_doc(update);
        }
    }

    fn send_doc(&mut self, update: Vec<u8>) {
        if let Some(sock) = &mut self.socket {
            let mut buf = vec![tag::DOC];
            buf.extend_from_slice(&update);
            sock.tx.send(WsMessage::Binary(buf));
        }
    }

    /// Pump incoming messages. Returns true if the shared doc or presence changed
    /// (so the app can reconcile + repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut incoming: Vec<Vec<u8>> = Vec::new();
        if let Some(sock) = &mut self.socket {
            while let Some(ev) = sock.rx.try_recv() {
                if let ewebsock::WsEvent::Message(WsMessage::Binary(buf)) = ev {
                    incoming.push(buf);
                }
            }
        }
        for buf in incoming {
            match buf.first().copied() {
                Some(tag::DOC) => {
                    if let Ok(update) = yrs::Update::decode_v1(&buf[1..]) {
                        // No origin ⇒ the local UndoManager ignores it (per-user
                        // undo), and it is not re-broadcast (see last_sv advance).
                        {
                            let mut txn = self.doc.transact_mut();
                            let _ = txn.apply_update(update);
                        }
                        self.last_sv = self.doc.transact().state_vector();
                        changed = true;
                    }
                }
                Some(tag::AWARENESS) => {
                    if let Some(p) = decode_presence(&buf[1..]) {
                        self.remote.insert(p.0, p.1);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        changed
    }
}

fn key_of(id: BaseShapeId) -> String {
    format!("{}:{}", id.0, id.1)
}

fn parse_key(k: &str) -> Option<BaseShapeId> {
    let (c, i) = k.split_once(':')?;
    Some((c.parse().ok()?, i.parse().ok()?))
}

fn parse_kind(s: &str) -> Option<EditKind> {
    let mut it = s.split_whitespace();
    match it.next()? {
        "D" => Some(EditKind::Delete),
        "M" => {
            let dx = it.next()?.parse().ok()?;
            let dy = it.next()?.parse().ok()?;
            Some(EditKind::Move { dx, dy })
        }
        _ => None,
    }
}

fn decode_presence(b: &[u8]) -> Option<(u64, Presence)> {
    if b.len() < 16 {
        return None;
    }
    let id = u64::from_le_bytes(b[0..8].try_into().ok()?);
    let x = f32::from_le_bytes(b[8..12].try_into().ok()?);
    let y = f32::from_le_bytes(b[12..16].try_into().ok()?);
    let name = String::from_utf8_lossy(&b[16..]).to_string();
    Some((
        id,
        Presence {
            cursor: Vec2::new(x, y),
            name,
            color: color_from_id(id),
        },
    ))
}

/// Stable per-user color from client id.
pub fn color_from_id(id: u64) -> [f32; 4] {
    let h = (id % 360) as f32 / 360.0;
    let (r, g, b) = hsv(h, 0.7, 0.95);
    [r, g, b, 1.0]
}

fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32) % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trip() {
        assert_eq!(parse_key("291:1252136"), Some((291usize, 1252136u32)));
        assert_eq!(parse_key("garbage"), None);
    }

    #[test]
    fn kind_round_trip() {
        assert_eq!(parse_kind("D"), Some(EditKind::Delete));
        assert_eq!(parse_kind("M -100 200"), Some(EditKind::Move { dx: -100, dy: 200 }));
        assert_eq!(parse_kind("M 1"), None);
    }

    /// Two independent sessions editing *different* shapes converge to both edits
    /// after exchanging updates — the CRDT correctness the two-browser demo relies
    /// on, checked here without a socket.
    #[test]
    fn concurrent_different_shapes_converge() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let mut b = Session::new(Presence { cursor: Vec2::ZERO, name: "b".into(), color: [0.0; 4] });

        a.set_edit((0, 1), EditKind::Move { dx: 10, dy: 0 });
        b.set_edit((0, 2), EditKind::Delete);

        // Exchange full states (stand-in for the relay).
        let ua = a.doc.transact().encode_state_as_update_v1(&StateVector::default());
        let ub = b.doc.transact().encode_state_as_update_v1(&StateVector::default());
        {
            let mut txn = a.doc.transact_mut();
            let _ = txn.apply_update(yrs::Update::decode_v1(&ub).unwrap());
        }
        {
            let mut txn = b.doc.transact_mut();
            let _ = txn.apply_update(yrs::Update::decode_v1(&ua).unwrap());
        }

        let mut ea = a.edits();
        let mut eb = b.edits();
        ea.sort_by_key(|(id, _)| *id);
        eb.sort_by_key(|(id, _)| *id);
        assert_eq!(ea, eb, "both sessions must converge to the same edit set");
        assert_eq!(ea.len(), 2);
    }

    /// Undo reverts only the local client's edits, not a peer's.
    #[test]
    fn undo_is_per_user() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let mut b = Session::new(Presence { cursor: Vec2::ZERO, name: "b".into(), color: [0.0; 4] });

        a.set_edit((0, 1), EditKind::Delete); // A's edit
        // B makes an edit and A receives it (no origin ⇒ not on A's undo stack).
        b.set_edit((0, 2), EditKind::Move { dx: 5, dy: 5 });
        let ub = b.doc.transact().encode_state_as_update_v1(&StateVector::default());
        {
            let mut txn = a.doc.transact_mut();
            let _ = txn.apply_update(yrs::Update::decode_v1(&ub).unwrap());
        }
        assert_eq!(a.edits().len(), 2, "A now sees both edits");

        // A undoes: only A's own (0,1) delete is reverted; B's (0,2) survives.
        assert!(a.undo());
        let after: Vec<_> = a.edits();
        assert_eq!(after.len(), 1, "A's undo removed exactly one edit");
        assert_eq!(after[0].0, (0, 2), "the surviving edit is B's");
    }
}
