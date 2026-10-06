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
//!
//! **Connection lifecycle (Phase 6).** The server keeps the room's document:
//! its first message to a joining peer is the whole room state, so a late
//! joiner converges. The session pushes its own whole state when the socket
//! *opens* (a browser WebSocket drops anything sent while still connecting —
//! before Phase 6 the join push was lost that way on the web), which also
//! merges edits made offline or while a dropped connection was down. A peer
//! whose design differs from the room's is refused ([`Session::refused`]).

use anyhow::Result;
use ewebsock::{WsMessage, WsReceiver, WsSender};
use fluideda_core::{AddId, BaseShapeId, CellDb, CellId, EditOverlay, Layer, PlacementKey, Point};
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
///
/// `SetPoints` is the second documented AMENDMENT to that rule (Phase 5 SLICE 2,
/// after adds): a vertex edit must ship the shape's full new point list — the
/// receiver cannot reconstruct an arbitrary per-vertex change from a compact op.
/// Concurrent vertex-edits of the SAME shape converge last-writer-wins via the
/// yrs map (one entry per shape id): both peers end on the same single record,
/// whichever wrote last. That is accepted, and documented in STATUS.
#[derive(Debug, Clone, PartialEq)]
pub enum EditKind {
    /// Translate the base shape by `(dx, dy)` nm.
    Move { dx: i32, dy: i32 },
    /// Tombstone the base shape.
    Delete,
    /// Replace the base shape's ring with these points (cell-local nm, closing
    /// duplicate preserved verbatim). The layer never changes in a vertex edit,
    /// so it is not carried — the receiver reads it from its base.
    SetPoints(Vec<Point>),
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

/// The design a room is bound to (Phase 6): the server writes it into the
/// document's `meta` map when the first peer with a design joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomDesign {
    /// SHA-256 of the design file's bytes (lower-case hex).
    pub hash: String,
    /// The shape-id scheme it was parsed with.
    pub ids: u32,
    /// Its file name (display only).
    pub name: String,
}

/// Message envelope on the wire. Byte 0 tags the channel.
///   0x01 = yrs document update (markers + edits share the doc)
///   0x02 = awareness/presence update
///
/// The server's only other message is a JSON text frame refusing the join
/// (`{"error":"design_mismatch",...}`), followed by a close.
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
    /// Room metadata, written only by the server (the design binding).
    meta: MapRef,
    undo: UndoManager<()>,
    /// State vector already broadcast; the next broadcast sends only the delta
    /// past it. Advanced after each local mutation AND after applying a remote
    /// update, so a remote change is never echoed back as if it were local.
    last_sv: StateVector,
    /// This client's transaction origin. Local edits use it; the UndoManager
    /// tracks it, so only local edits are undoable.
    origin: Origin,
    socket: Option<Socket>,
    /// The socket has opened (sends before that are dropped by browsers).
    opened: bool,
    /// The room's state has arrived since the socket opened.
    synced: bool,
    /// Why the last connection ended, if it failed.
    error: Option<String>,
    /// The room's design, when the server refused us for having another.
    refused: Option<RoomDesign>,
    pub local: Presence,
    pub remote: HashMap<u64, Presence>,
    client_id: u64,
    /// Called when the socket has news (Phase 7.1a: an idle UI draws only
    /// when woken).
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
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
        let meta = doc.get_or_insert_map("meta");
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
            meta,
            undo,
            last_sv,
            origin,
            socket: None,
            opened: false,
            synced: false,
            error: None,
            refused: None,
            local,
            remote: HashMap::new(),
            client_id,
            wakeup: None,
            next_add_seq: 0,
        }
    }

    /// Call `f` whenever the socket has news (a message, open, close), so a
    /// UI that idles can wake to [`poll`](Self::poll). Applies from the next
    /// [`join`](Self::join).
    pub fn set_wakeup(&mut self, f: Arc<dyn Fn() + Send + Sync>) {
        self.wakeup = Some(f);
    }

    /// An offline session holding a document state — a v1 update, as a room
    /// or [`state`](Self::state) encodes it. The server reads a room's or a
    /// client's edits through one to export them (Phase 7.4).
    pub fn from_state(update: &[u8]) -> Result<Self> {
        let mut s = Self::new(Presence { cursor: Vec2::ZERO, name: String::new(), color: [0.0; 4] });
        let update = yrs::Update::decode_v1(update).map_err(|e| anyhow::anyhow!("document state: {e}"))?;
        s.doc.transact_mut().apply_update(update);
        s.last_sv = s.doc.transact().state_vector();
        Ok(s)
    }

    /// The whole document state, as one v1 update.
    pub fn state(&self) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(&StateVector::default())
    }

    /// The edit overlay these records make over `db` — the one builder the
    /// app and the server's export share (Phase 7.4). Moves are translated
    /// against `db`'s own base geometry; ids out of range (a peer on another
    /// file) are ignored. A move or vertex edit needs its base shape in
    /// memory: on a streamed design those that are not are left out, and the
    /// shape chunks they wait for are returned (in-core: never any).
    pub fn overlay(&self, db: &CellDb) -> (EditOverlay, Vec<(CellId, u32)>) {
        let mut o = EditOverlay::new();
        let mut waiting = Vec::new();
        for (id, kind) in self.edits() {
            let (cell, idx) = id;
            if cell >= db.cells.len() || idx as usize >= db.cell(cell).shape_count() {
                continue;
            }
            let c = db.cell(cell);
            let base = c.shape_layer(idx as usize).zip(c.shape_points(idx as usize));
            match (kind, base) {
                (EditKind::Delete, _) => o.delete(id),
                (EditKind::Move { dx, dy }, Some((layer, pts))) => {
                    o.move_shape(id, fluideda_core::edit::translate(layer, &pts, dx, dy));
                }
                // A vertex edit ships the full ring (SLICE 2 amendment); the
                // layer comes from the receiver's own base — it never changes.
                (EditKind::SetPoints(points), Some((layer, _))) => {
                    o.move_shape(id, fluideda_core::Shape { layer, points });
                }
                (_, None) => waiting.extend(c.chunk_of(idx).map(|ci| (cell, ci))),
            }
        }
        // Hidden placements (Phase 5d "edit this placement only").
        for key in self.hidden() {
            let (parent, ii, _, _) = key;
            if parent < db.cells.len() && (ii as usize) < db.cell(parent).placement_count() {
                o.hide_placement(key);
            }
        }
        // Added shapes: geometry ships in the record; tombstoned adds are
        // skipped. `adds()` is sorted by id, so every peer rebuilds the same
        // draw order.
        for (id, geom) in self.adds() {
            let Some(g) = geom else { continue };
            if g.cell >= db.cells.len() {
                continue;
            }
            o.add(id, g.cell, fluideda_core::Shape { layer: g.layer, points: g.points });
        }
        waiting.sort_unstable();
        waiting.dedup();
        (o, waiting)
    }

    /// Create a session and immediately connect to the relay.
    pub fn connect(url: &str, local: Presence) -> Result<Self> {
        let mut s = Self::new(local);
        s.join(url)?;
        Ok(s)
    }

    /// Open the socket for an offline session (the "Join room" path), or
    /// reconnect after a drop. When it opens, [`poll`](Self::poll) pushes our
    /// whole doc state, so the room merges edits made before joining; the
    /// server's first message is the whole room state.
    pub fn join(&mut self, url: &str) -> Result<()> {
        let opts = ewebsock::Options::default();
        let (tx, rx) = match self.wakeup.clone() {
            Some(w) => ewebsock::connect_with_wakeup(url, opts, move || w()),
            None => ewebsock::connect(url, opts),
        }
        .map_err(|e| anyhow::anyhow!("ws connect: {e}"))?;
        self.socket = Some(Socket { tx, rx });
        self.opened = false;
        self.synced = false;
        // `error` stays until the socket opens: while reconnecting, it still
        // says why the last connection ended.
        self.refused = None;
        Ok(())
    }

    /// Close the socket (edits keep working offline).
    pub fn disconnect(&mut self) {
        self.socket = None;
        self.opened = false;
        self.synced = false;
    }

    /// The socket is open.
    pub fn is_connected(&self) -> bool {
        self.socket.is_some() && self.opened
    }

    /// A socket exists but has not opened yet.
    pub fn is_connecting(&self) -> bool {
        self.socket.is_some() && !self.opened
    }

    /// The room's state has arrived on the current connection: the server
    /// accepted us and we hold everything the room held when we joined.
    pub fn is_synced(&self) -> bool {
        self.is_connected() && self.synced
    }

    /// Why the last connection ended, if it failed (kept while reconnecting,
    /// cleared when a socket opens).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Set when the server refused the join because the room holds another
    /// design: that design.
    pub fn refused(&self) -> Option<&RoomDesign> {
        self.refused.as_ref()
    }

    /// The design the room is bound to, once its state has arrived.
    pub fn room_design(&self) -> Option<RoomDesign> {
        let txn = self.doc.transact();
        let get = |k: &str| match self.meta.get(&txn, k) {
            Some(Out::Any(Any::String(s))) => Some(s.to_string()),
            _ => None,
        };
        Some(RoomDesign {
            hash: get("design")?,
            ids: get("ids").and_then(|v| v.parse().ok()).unwrap_or(0),
            name: get("design_name").unwrap_or_default(),
        })
    }

    /// Publish our cursor (world nm). No-op when offline.
    pub fn set_cursor(&mut self, world: Vec2) {
        self.local.cursor = world;
        if !self.opened {
            return;
        }
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
        let value = edit_value(&kind);
        {
            let mut txn = self.doc.transact_mut_with(self.origin.clone());
            self.edits.insert(&mut txn, key, value);
        }
        self.broadcast_local();
    }

    /// Apply several edits as ONE transaction — one broadcast, and one step on
    /// this user's undo stack (Phase 5d: a boolean replaces N shapes with M,
    /// and undo must restore all of them at once). Returns the `AddId`s minted
    /// for the batch's `Add` ops, in order.
    pub fn apply_batch(&mut self, ops: Vec<BatchOp>) -> Vec<AddId> {
        let mut minted = Vec::new();
        {
            let mut txn = self.doc.transact_mut_with(self.origin.clone());
            for op in ops {
                match op {
                    BatchOp::Edit(id, kind) => {
                        self.edits.insert(&mut txn, key_of(id), edit_value(&kind));
                    }
                    BatchOp::DeleteAdded(id) => {
                        self.edits.insert(&mut txn, add_key_of(id), "X".to_string());
                    }
                    BatchOp::Hide(key) => {
                        self.edits.insert(&mut txn, hide_key_of(key), "H".to_string());
                    }
                    BatchOp::Show(key) => {
                        self.edits.insert(&mut txn, hide_key_of(key), "S".to_string());
                    }
                    BatchOp::Add { cell, layer, points } => {
                        let id: AddId = (self.client_id, self.next_add_seq);
                        self.next_add_seq += 1;
                        self.edits.insert(&mut txn, add_key_of(id), add_value(cell, layer, &points));
                        minted.push(id);
                    }
                }
            }
        }
        self.broadcast_local();
        minted
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

    /// Every placement currently hidden (Phase 5d), sorted.
    pub fn hidden(&self) -> Vec<PlacementKey> {
        let txn = self.doc.transact();
        let mut out: Vec<PlacementKey> = self
            .edits
            .iter(&txn)
            .filter_map(|(k, v)| match (parse_hide_key(k), v) {
                (Some(key), Out::Any(Any::String(s))) if &*s == "H" => Some(key),
                _ => None,
            })
            .collect();
        out.sort();
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
        let value = add_value(cell, layer, points);
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

    /// Send an update — only on an open socket: anything a connecting socket
    /// would drop is covered by the whole-state push when it opens.
    fn send_doc(&mut self, update: Vec<u8>) {
        if !self.opened {
            return;
        }
        if let Some(sock) = &mut self.socket {
            let mut buf = vec![tag::DOC];
            buf.extend_from_slice(&update);
            sock.tx.send(WsMessage::Binary(buf));
        }
    }

    /// Pump incoming messages. Returns true if the shared doc, presence or
    /// the connection state changed (so the app can reconcile + repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut incoming: Vec<Vec<u8>> = Vec::new();
        let mut opened = false;
        let mut ended: Option<Option<String>> = None; // Some(reason) once the socket is gone
        if let Some(sock) = &mut self.socket {
            while let Some(ev) = sock.rx.try_recv() {
                match ev {
                    ewebsock::WsEvent::Opened => opened = true,
                    ewebsock::WsEvent::Message(WsMessage::Binary(buf)) => incoming.push(buf),
                    ewebsock::WsEvent::Message(WsMessage::Text(t)) => {
                        // The server's only text frame: a refusal.
                        self.refused = parse_refusal(&t);
                        ended = Some(Some(match &self.refused {
                            Some(d) => format!("room holds another design: {} ({})", d.name, &d.hash[..d.hash.len().min(12)]),
                            None => format!("server: {t}"),
                        }));
                    }
                    ewebsock::WsEvent::Message(_) => {}
                    ewebsock::WsEvent::Error(e) => {
                        ended.get_or_insert(Some(e));
                    }
                    ewebsock::WsEvent::Closed => {
                        ended.get_or_insert(None);
                    }
                }
            }
        }
        if opened {
            // Push our whole state now that the socket can carry it: the room
            // merges whatever we did before joining (or while disconnected).
            self.opened = true;
            self.error = None;
            let full = self.doc.transact().encode_state_as_update_v1(&StateVector::default());
            self.send_doc(full);
            changed = true;
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
                        self.synced = true;
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
        if let Some(reason) = ended {
            self.error = reason.or_else(|| Some("connection closed".to_string()));
            self.disconnect();
            changed = true;
        }
        changed
    }
}

/// The room's design from a `{"error":"design_mismatch","design":{...}}`
/// refusal. (Hand-parsed: the sync crate has no JSON dependency, and the
/// server writes exactly this shape.)
fn parse_refusal(t: &str) -> Option<RoomDesign> {
    if !t.contains("\"design_mismatch\"") {
        return None;
    }
    let field = |k: &str| -> Option<String> {
        let at = t.find(&format!("\"{k}\":"))? + k.len() + 3;
        let rest = t[at..].trim_start();
        if let Some(r) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = r.chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => return Some(out),
                    '\\' => out.push(chars.next()?),
                    c => out.push(c),
                }
            }
            None
        } else {
            Some(rest.chars().take_while(|c| c.is_ascii_digit()).collect())
        }
    };
    Some(RoomDesign { hash: field("hash")?, ids: field("ids")?.parse().ok()?, name: field("name").unwrap_or_default() })
}

/// One op of [`Session::apply_batch`].
#[derive(Debug, Clone)]
pub enum BatchOp {
    /// A base-shape edit (move / delete / set points).
    Edit(BaseShapeId, EditKind),
    /// Tombstone an added shape.
    DeleteAdded(AddId),
    /// A new shape (its id is minted by the batch).
    Add { cell: CellId, layer: Layer, points: Vec<Point> },
    /// Hide one placement ("edit this placement only", Phase 5d). Its record
    /// is LWW like every other: `H` hidden, `S` shown again.
    Hide(PlacementKey),
    /// Show a hidden placement again.
    Show(PlacementKey),
}

/// `h<parent>:<inst>:<col>:<row>` — cannot collide with base keys (digit-first)
/// or add keys (`a…`).
fn hide_key_of(k: PlacementKey) -> String {
    format!("h{}:{}:{}:{}", k.0, k.1, k.2, k.3)
}

fn parse_hide_key(k: &str) -> Option<PlacementKey> {
    let mut it = k.strip_prefix('h')?.split(':');
    let key = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some(key)
}

/// The CRDT value of a base-shape edit.
fn edit_value(kind: &EditKind) -> String {
    match kind {
        EditKind::Move { dx, dy } => format!("M {dx} {dy}"),
        EditKind::Delete => "D".to_string(),
        EditKind::SetPoints(pts) => {
            let mut v = String::from("P");
            for p in pts {
                v.push_str(&format!(" {} {}", p.x, p.y));
            }
            v
        }
    }
}

/// The CRDT value of an added shape's geometry.
fn add_value(cell: CellId, layer: Layer, points: &[Point]) -> String {
    let mut value = format!("A {} {} {}", cell, layer.layer, layer.datatype);
    for p in points {
        value.push_str(&format!(" {} {}", p.x, p.y));
    }
    value
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
        "P" => {
            let coords: Vec<i32> = it.map(|t| t.parse().ok()).collect::<Option<Vec<_>>>()?;
            if coords.len() < 6 || coords.len() % 2 != 0 {
                return None; // need at least 3 points, in pairs
            }
            let points = coords.chunks_exact(2).map(|c| Point::new(c[0], c[1])).collect();
            Some(EditKind::SetPoints(points))
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
    fn refusal_names_the_rooms_design() {
        // Exactly what the server sends (serde_json, room.rs `DesignKey`).
        let t = r#"{"error":"design_mismatch","room":"r1","design":{"hash":"ab12","ids":2,"name":"my \"chip\".gds"}}"#;
        assert_eq!(
            parse_refusal(t),
            Some(RoomDesign { hash: "ab12".into(), ids: 2, name: "my \"chip\".gds".into() })
        );
        assert_eq!(parse_refusal(r#"{"error":"something_else"}"#), None);
    }

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
        assert_eq!(
            parse_kind("P 0 0 10 0 10 10 0 0"),
            Some(EditKind::SetPoints(vec![
                Point::new(0, 0),
                Point::new(10, 0),
                Point::new(10, 10),
                Point::new(0, 0),
            ])),
            "SetPoints round-trips with the closing duplicate preserved"
        );
        assert_eq!(parse_kind("P 0 0 10 0"), None, "< 3 points rejected");
    }

    /// Concurrent vertex-edits of the SAME shape converge last-writer-wins: after
    /// exchange both peers hold the identical single record — one of the two
    /// writes, not a merge and not a divergence. (Documented acceptance.)
    #[test]
    fn concurrent_setpoints_same_shape_lww_converges() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let mut b = Session::new(Presence { cursor: Vec2::ZERO, name: "b".into(), color: [0.0; 4] });

        let pa = vec![Point::new(0, 0), Point::new(10, 0), Point::new(10, 99), Point::new(0, 0)];
        let pb = vec![Point::new(0, 0), Point::new(77, 0), Point::new(10, 10), Point::new(0, 0)];
        a.set_edit((0, 5), EditKind::SetPoints(pa.clone()));
        b.set_edit((0, 5), EditKind::SetPoints(pb.clone()));

        exchange(&mut a, &mut b);

        let (ea, eb) = (a.edits(), b.edits());
        assert_eq!(ea, eb, "both peers hold the same record after exchange");
        assert_eq!(ea.len(), 1, "one entry per shape id (LWW, not a merge)");
        match &ea[0].1 {
            EditKind::SetPoints(p) => {
                assert!(*p == pa || *p == pb, "the survivor is one of the two writes")
            }
            k => panic!("expected SetPoints, got {k:?}"),
        }
    }

    /// A vertex edit is one undo step for its author, like every other edit.
    #[test]
    fn undo_covers_setpoints() {
        let mut a = Session::new(Presence { cursor: Vec2::ZERO, name: "a".into(), color: [0.0; 4] });
        let pts = vec![Point::new(0, 0), Point::new(10, 0), Point::new(10, 10), Point::new(0, 0)];
        a.set_edit((0, 1), EditKind::SetPoints(pts));
        assert_eq!(a.edits().len(), 1);
        assert!(a.undo());
        assert!(a.edits().is_empty(), "vertex edit reverted");
        assert!(a.redo());
        assert_eq!(a.edits().len(), 1, "and restored");
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
    fn batch_is_one_undo_step() {
        let mut s = Session::new(Presence { cursor: Vec2::ZERO, name: "t".into(), color: [1.0; 4] });
        let a = s.add_shape(0, Layer::new(1, 0), &[Point::new(0, 0), Point::new(10, 0), Point::new(10, 10)]);
        let tri = vec![Point::new(0, 0), Point::new(5, 0), Point::new(5, 5)];
        let minted = s.apply_batch(vec![
            BatchOp::Edit((0, 3), EditKind::Delete),
            BatchOp::DeleteAdded(a),
            BatchOp::Add { cell: 0, layer: Layer::new(1, 0), points: tri.clone() },
            BatchOp::Add { cell: 0, layer: Layer::new(1, 0), points: tri },
        ]);
        assert_eq!(minted.len(), 2);
        assert_ne!(minted[0], minted[1]);
        assert_eq!(s.edits().len(), 1, "the base delete");
        assert_eq!(s.adds().iter().filter(|(_, g)| g.is_some()).count(), 2, "two new adds, the old one tombstoned");
        // ONE undo reverts the whole batch.
        assert!(s.undo());
        assert!(s.edits().is_empty());
        let live: Vec<AddId> = s.adds().into_iter().filter(|(_, g)| g.is_some()).map(|(id, _)| id).collect();
        assert_eq!(live, vec![a], "the original add is back, the batch's adds are gone");
    }

    #[test]
    fn hidden_placements_round_trip_and_undo() {
        let mut s = Session::new(Presence { cursor: Vec2::ZERO, name: "t".into(), color: [1.0; 4] });
        assert_eq!(parse_hide_key(&hide_key_of((3, 7, -1, 2))), Some((3, 7, -1, 2)));
        assert_eq!(parse_hide_key("3:7"), None, "a base key is not a hide key");
        s.apply_batch(vec![
            BatchOp::Hide((0, 1, 2, 0)),
            BatchOp::Add { cell: 0, layer: Layer::new(1, 0), points: vec![Point::new(0, 0), Point::new(1, 0), Point::new(1, 1)] },
        ]);
        assert_eq!(s.hidden(), vec![(0, 1, 2, 0)]);
        assert!(s.edits().is_empty(), "hide keys are not base edits");
        s.apply_batch(vec![BatchOp::Show((0, 1, 2, 0))]);
        assert!(s.hidden().is_empty());
        assert!(s.undo(), "undo the show");
        assert_eq!(s.hidden(), vec![(0, 1, 2, 0)]);
        assert!(s.undo(), "undo the flatten");
        assert!(s.hidden().is_empty());
        assert!(s.adds().iter().all(|(_, g)| g.is_none()));
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
