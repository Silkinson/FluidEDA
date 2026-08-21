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
use fluideda_core::{AddId, BaseShapeId, CellId, Layer, Point};
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

/// A base-shape edit as carried by the CRDT. Geometry is NOT shipped for
/// move/delete — a move stores only its translation, and the receiver
/// reconstructs the moved shape from its own copy of the base (both peers loaded
/// the same GDS, so base ids + geometry match). Keeps edits tiny and independent
/// of tessellation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditKind {
    /// Translate the base shape by `(dx, dy)` nm.
    Move { dx: i32, dy: i32 },
    /// Tombstone the base shape.
    Delete,
}

/// The CRDT-side record of an **added** shape (Phase 5 SLICE 1).
///
/// AMENDMENT to the "ship the op, not geometry" rule: an add MUST ship its full
/// geometry — there is no base shape on the peer to rebuild it from. The record
/// is the shape's complete current state (cell + layer + points), last-writer-wins
/// per [`AddId`]; a later move/vertex-edit of the added shape rewrites this record
/// wholesale. `None` geometry = the add was deleted (tombstone value), kept as an
/// entry so LWW between a concurrent move and delete stays well-defined.
#[derive(Debug, Clone, PartialEq)]
pub struct AddedGeom {
    pub cell: CellId,
    pub layer: Layer,
    pub points: Vec<Point>,
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
    /// Per-client counter minting [`AddId`]s for shapes drawn in this session.
    /// `(client_id, seq)` is unique across peers (distinct client ids) and across
    /// this session's own adds (monotonic seq) — the id survives concurrent adds.
    next_add_seq: u32,
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
            next_add_seq: 0,
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

    // --- Phase 5 SLICE 1: added shapes ---------------------------------------
    //
    // Adds live in the SAME "edits" map as base edits, under a distinct key form
    // (`a<client>:<seq>` — base keys are digit-first `<cell>:<idx>`, so the two
    // can never collide). One map ⇒ one UndoManager scope: undo/redo covers adds
    // with zero extra machinery, and per-key LWW gives the concurrency story:
    // concurrent adds have distinct keys (both survive); concurrent ops on the
    // SAME added shape (move vs delete vs vertex-edit) converge last-writer-wins.

    /// Draw a brand-new shape into `cell`. Ships the FULL geometry (see
    /// [`AddedGeom`] for the documented amendment) and returns the freshly minted
    /// stable id peers will know it by.
    pub fn add_shape(&mut self, cell: CellId, layer: Layer, points: &[Point]) -> AddId {
        let id: AddId = (self.client_id, self.next_add_seq);
        self.next_add_seq += 1;
        self.write_add(id, cell, layer, points);
        id
    }

    /// Rewrite an added shape's geometry wholesale (its move or vertex-edit —
    /// geometry IS the state for adds, last-writer-wins per id).
    pub fn set_added_geometry(&mut self, id: AddId, cell: CellId, layer: Layer, points: &[Point]) {
        self.write_add(id, cell, layer, points);
    }

    /// Delete an added shape: overwrite its record with the tombstone value.
    /// (An overwrite, not a key removal, so a concurrent geometry rewrite and a
    /// delete converge by plain LWW like every other pair of ops.)
    pub fn delete_added(&mut self, id: AddId) {
        {
            let mut txn = self.doc.transact_mut_with(self.origin.clone());
            self.edits.insert(&mut txn, add_key_of(id), "X".to_string());
        }
        self.broadcast_local();
    }

    fn write_add(&mut self, id: AddId, cell: CellId, layer: Layer, points: &[Point]) {
        let mut value = format!("A {} {} {}", cell, layer.layer, layer.datatype);
        for p in points {
            value.push_str(&format!(" {} {}", p.x, p.y));
        }
        {
            let mut txn = self.doc.transact_mut_with(self.origin.clone());
            self.edits.insert(&mut txn, add_key_of(id), value);
        }
        self.broadcast_local();
    }

    /// Every added-shape record currently in the CRDT: `(id, Some(geometry))` for
    /// live shapes, `(id, None)` for tombstoned ones. The app rebuilds its
    /// overlay's `added` from the live entries (sorted by id for a deterministic
    /// cross-peer draw order).
    pub fn adds(&self) -> Vec<(AddId, Option<AddedGeom>)> {
        let txn = self.doc.transact();
        let mut out = Vec::new();
        for (k, v) in self.edits.iter(&txn) {
            let (Some(id), Out::Any(Any::String(s))) = (parse_add_key(k), v) else { continue };
            out.push((id, parse_add_value(&s)));
        }
        out.sort_by_key(|(id, _)| *id);
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

/// Added-shape key: `a<client>:<seq>`. Digit-first base keys can never collide
/// with the `a` prefix, so both kinds share the one map (and one undo scope).
fn add_key_of(id: AddId) -> String {
    format!("a{}:{}", id.0, id.1)
}

fn parse_add_key(k: &str) -> Option<AddId> {
    let rest = k.strip_prefix('a')?;
    let (c, s) = rest.split_once(':')?;
    Some((c.parse().ok()?, s.parse().ok()?))
}

/// Added-shape value: `A <cell> <layer> <datatype> <x0> <y0> ...` (live) or `X`
/// (tombstoned). Returns `None` for the tombstone or anything malformed.
fn parse_add_value(s: &str) -> Option<AddedGeom> {
    let mut it = s.split_whitespace();
    if it.next()? != "A" {
        return None; // "X" tombstone, or garbage
    }
    let cell: CellId = it.next()?.parse().ok()?;
    let layer: u16 = it.next()?.parse().ok()?;
    let datatype: u16 = it.next()?.parse().ok()?;
    let coords: Vec<i32> = it.map(|t| t.parse().ok()).collect::<Option<Vec<_>>>()?;
    if coords.len() < 6 || coords.len() % 2 != 0 {
        return None; // need at least 3 points, in pairs
    }
    let points = coords.chunks_exact(2).map(|c| Point::new(c[0], c[1])).collect();
    Some(AddedGeom { cell, layer: Layer::new(layer, datatype), points })
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

    fn exchange(a: &mut Session, b: &mut Session) {
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
    }

    fn tri(x: i32, y: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + 100, y), Point::new(x + 100, y + 80), Point::new(x, y)]
    }

    #[test]
    fn add_key_and_value_round_trip() {
        assert_eq!(parse_add_key("a184467:12"), Some((184467u64, 12u32)));
        assert_eq!(parse_add_key("5:3"), None, "base keys must not parse as adds");
        let g = parse_add_value("A 7 66 20 0 0 100 0 100 80").unwrap();
        assert_eq!(g.cell, 7);
        assert_eq!(g.layer, Layer::new(66, 20));
        assert_eq!(g.points.len(), 3);
        assert_eq!(parse_add_value("X"), None, "tombstone parses as no geometry");
        assert_eq!(parse_add_value("A 7 66 20 0 0 100 0"), None, "< 3 points rejected");
    }

    /// THE SLICE 1 convergence requirement: two peers drawing simultaneously
    /// (before any exchange) must both survive — distinct creators mint distinct
    /// ids, so the adds land under different map keys.
    #[test]
    fn concurrent_adds_converge_to_both() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let mut b = Session::new(Presence { cursor: Vec2::ZERO, name: "b".into(), color: [0.0; 4] });

        let ida = a.add_shape(0, Layer::new(1, 0), &tri(0, 0));
        let idb = b.add_shape(0, Layer::new(2, 0), &tri(500, 500));
        assert_ne!(ida, idb, "distinct clients must mint distinct ids");

        exchange(&mut a, &mut b);

        let (adds_a, adds_b) = (a.adds(), b.adds());
        assert_eq!(adds_a, adds_b, "both peers see the identical add set");
        assert_eq!(adds_a.len(), 2, "both concurrent adds survive");
        assert!(adds_a.iter().all(|(_, g)| g.is_some()));
    }

    /// Moving/deleting an ADDED shape references its AddId (not a BaseShapeId)
    /// and converges: here A adds, B (after sync) deletes A's shape by id.
    #[test]
    fn peer_can_delete_an_added_shape_by_id() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let mut b = Session::new(Presence { cursor: Vec2::ZERO, name: "b".into(), color: [0.0; 4] });

        let id = a.add_shape(0, Layer::new(1, 0), &tri(0, 0));
        exchange(&mut a, &mut b);
        assert_eq!(b.adds().len(), 1, "B received the add");

        b.delete_added(id);
        exchange(&mut a, &mut b);
        assert_eq!(a.adds(), b.adds());
        assert!(a.adds()[0].1.is_none(), "the add is tombstoned on both peers");
    }

    /// Undo covers adds with no extra machinery (same map, same tracked origin):
    /// undoing an add removes it; redo restores it — and only for the author.
    #[test]
    fn undo_covers_adds_per_user() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let id = a.add_shape(0, Layer::new(1, 0), &tri(0, 0));
        assert_eq!(a.adds().len(), 1);

        assert!(a.undo(), "undo of the add");
        assert!(a.adds().is_empty() || a.adds()[0].1.is_none(), "add gone after undo");

        assert!(a.redo(), "redo of the add");
        let redone = a.adds();
        assert_eq!(redone.len(), 1);
        assert_eq!(redone[0].0, id, "redo restores the same stable id");
        assert!(redone[0].1.is_some());
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
