//! Rooms (Phase 6): one authoritative CRDT document per collaborative session.
//!
//! Phase 0's relay only forwarded bytes, so a peer saw exactly the updates
//! sent while it was connected: a late joiner missed everything before it,
//! and a restart forgot the room. Now the server keeps each room's
//! `yrs::Doc`. Every document update is applied to it and appended to the
//! room's WAL before it is relayed, and a joining peer first receives the
//! **whole room state**. When the room goes quiet the state is written as a
//! content-addressed snapshot and the WAL truncated ([`crate::store`]).
//!
//! **Design binding.** Edits are keyed by base-shape ids, which only mean
//! something for one design file parsed one way. A room is therefore bound to
//! the design — SHA-256 of the file bytes plus the shape-id scheme — of the
//! first peer that joins with one. The binding lives in the document's `meta`
//! map, where every peer can read it. A peer with a different design is
//! refused before any state is exchanged, so it cannot corrupt the room.

use crate::store::{wal_append, Store, Version};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapRef, Out, ReadTxn, StateVector, Transact, Update};

/// Byte 0 of a message: the channel (unchanged since Phase 0).
pub const TAG_DOC: u8 = 0x01;
pub const TAG_AWARENESS: u8 = 0x02;

/// Peer id the server's own messages carry (no peer skips them).
pub const SERVER_PEER: u64 = 0;

/// The design a peer has loaded (join parameters `design`, `ids`, `name`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DesignKey {
    /// SHA-256 of the design file's bytes.
    pub hash: String,
    /// The parser's shape-id scheme (`fluideda_core::SHAPE_ID_SCHEME`).
    pub ids: u32,
    /// The file name, for display only (not part of the identity).
    pub name: String,
}

impl DesignKey {
    fn same_design(&self, other: &DesignKey) -> bool {
        self.hash == other.hash && self.ids == other.ids
    }
}

/// A message on a room's broadcast: (sender peer id, framed bytes).
pub type Frame = (u64, Vec<u8>);

/// When a room with unsnapshotted updates is written out.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotPolicy {
    /// No update for this long: the editing burst is over.
    pub idle: Duration,
    /// Dirty for this long: snapshot anyway, bounding WAL replay.
    pub max_age: Duration,
    /// This many WAL bytes: snapshot anyway.
    pub max_wal: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self { idle: Duration::from_secs(2), max_age: Duration::from_secs(30), max_wal: 1 << 20 }
    }
}

pub struct Room {
    pub name: String,
    store: Arc<Store>,
    tx: broadcast::Sender<Frame>,
    state: Mutex<RoomState>,
    /// One snapshot of this room at a time (updates never wait on it).
    snapshotting: Mutex<()>,
}

struct RoomState {
    doc: Doc,
    meta: MapRef,
    /// Opened on the first update (a room nobody wrote to leaves no files).
    wal: Option<File>,
    wal_bytes: u64,
    /// Updates applied so far (a snapshot knows which it covers).
    updates: u64,
    /// When the oldest unsnapshotted update arrived (`None` = clean).
    dirty_since: Option<Instant>,
    last_update: Instant,
    head: Option<String>,
    peers: usize,
}

/// What `GET /rooms/<name>` reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomInfo {
    pub room: String,
    pub design: Option<DesignKey>,
    pub head: Option<String>,
    pub versions: Vec<Version>,
    pub peers: usize,
    /// Encoded size of the current document state.
    pub state_bytes: usize,
}

impl Room {
    /// Load room `name`: its head snapshot, then every WAL record after it
    /// (updates are idempotent, so records the snapshot already holds are
    /// harmless). A new room starts empty.
    pub fn restore(store: Arc<Store>, name: &str) -> io::Result<Room> {
        let doc = Doc::new();
        let meta = doc.get_or_insert_map("meta");
        let head = store.head(name);
        if let Some(h) = &head {
            let bytes = store.read_blob(h)?;
            let u = Update::decode_v1(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("snapshot {h}: {e}")))?;
            doc.transact_mut().apply_update(u);
        }
        let wal = store.read_wal(name)?;
        let mut wal_bytes = 0;
        for rec in &wal {
            wal_bytes += 4 + rec.len() as u64;
            match Update::decode_v1(rec) {
                Ok(u) => doc.transact_mut().apply_update(u),
                Err(e) => log::warn!("room {name}: skipping undecodable WAL record: {e}"),
            }
        }
        let now = Instant::now();
        if head.is_some() || !wal.is_empty() {
            log::info!("room {name}: restored (head {:?}, {} WAL records)", head.as_deref().map(short), wal.len());
        }
        Ok(Room {
            name: name.to_string(),
            store,
            tx: broadcast::channel(1024).0,
            state: Mutex::new(RoomState {
                doc,
                meta,
                wal: None,
                wal_bytes,
                updates: 0,
                // Replayed records get compacted into the next snapshot.
                dirty_since: (!wal.is_empty()).then_some(now),
                last_update: now,
                head,
                peers: 0,
            }),
            snapshotting: Mutex::new(()),
        })
    }

    /// Admit a peer. A peer with a design binds an unbound room to it; one
    /// whose design differs from the room's is refused with the room's
    /// design. Returns the framed whole-room state (always sent, even when
    /// empty: it is also the peer's "you are in" signal) and a subscription
    /// taken under the same lock, so no update falls between the two.
    pub fn join(&self, design: Option<&DesignKey>) -> Result<(Vec<u8>, broadcast::Receiver<Frame>), DesignKey> {
        let mut st = self.state.lock().unwrap();
        match (st.design(), design) {
            (Some(bound), Some(d)) if !bound.same_design(d) => return Err(bound),
            (None, Some(d)) => st.bind(d, &self.name, &self.store, &self.tx),
            _ => {}
        }
        let state = st.doc.transact().encode_state_as_update_v1(&StateVector::default());
        let rx = self.tx.subscribe();
        st.peers += 1;
        Ok((framed(TAG_DOC, &state), rx))
    }

    pub fn leave(&self) {
        let mut st = self.state.lock().unwrap();
        st.peers = st.peers.saturating_sub(1);
    }

    /// Apply a peer's document update (`msg` = tag + v1 update), log it, and
    /// relay it. False if it does not decode (then it is dropped, not relayed).
    pub fn apply(&self, peer: u64, msg: Vec<u8>) -> bool {
        let Ok(update) = Update::decode_v1(&msg[1..]) else { return false };
        let mut st = self.state.lock().unwrap();
        let bound = st.design();
        st.doc.transact_mut().apply_update(update);
        st.log_update(&msg[1..], &self.name, &self.store);
        let _ = self.tx.send((peer, msg));
        // Clients never write `meta`; if an update did change the binding, the
        // server writes it back (its write is causally later, so it wins).
        if let Some(b) = bound {
            if st.design().is_none_or(|d| d != b) {
                log::warn!("room {}: peer {peer} rewrote the design binding; restoring it", self.name);
                st.bind(&b, &self.name, &self.store, &self.tx);
            }
        }
        true
    }

    /// Relay an ephemeral message (presence) without storing it.
    pub fn relay(&self, peer: u64, msg: Vec<u8>) {
        let _ = self.tx.send((peer, msg));
    }

    /// The room's design binding and its whole document state (Phase 7.4:
    /// what a server-side export writes).
    pub fn design_and_state(&self) -> (Option<DesignKey>, Vec<u8>) {
        let st = self.state.lock().unwrap();
        let state = st.doc.transact().encode_state_as_update_v1(&StateVector::default());
        (st.design(), state)
    }

    /// The framed whole-room state (a lagging peer's resync).
    pub fn state_frame(&self) -> Vec<u8> {
        let st = self.state.lock().unwrap();
        let state = st.doc.transact().encode_state_as_update_v1(&StateVector::default());
        framed(TAG_DOC, &state)
    }

    /// The room's binding and history. Waits out a snapshot in progress, so
    /// a version it lists is complete (head set, WAL trimmed). Blocking.
    pub fn info(&self) -> RoomInfo {
        let _complete = self.snapshotting.lock().unwrap();
        let st = self.state.lock().unwrap();
        let state_bytes = st.doc.transact().encode_state_as_update_v1(&StateVector::default()).len();
        RoomInfo {
            room: self.name.clone(),
            design: st.design(),
            head: st.head.clone(),
            versions: self.store.versions(&self.name),
            peers: st.peers,
            state_bytes,
        }
    }

    /// Write the room's state as a snapshot if it has unsnapshotted updates
    /// and `policy` says it is time (or `force`), then drop the WAL records
    /// it covers. Returns the version recorded, if any (an unchanged state
    /// records none).
    ///
    /// The room lock is held only to encode the state and, afterwards, to
    /// trim the WAL — not across the blob write and its fsyncs (16–28 ms on
    /// macOS), so updates keep flowing while a snapshot is written. Updates
    /// that land meanwhile stay in the WAL and keep the room dirty.
    pub fn snapshot(&self, policy: &SnapshotPolicy, now: Instant, force: bool) -> io::Result<Option<Version>> {
        let _one = self.snapshotting.lock().unwrap();
        let (state, covered_bytes, covered_updates, head) = {
            let st = self.state.lock().unwrap();
            let Some(since) = st.dirty_since else { return Ok(None) };
            let due = now.duration_since(st.last_update) >= policy.idle
                || now.duration_since(since) >= policy.max_age
                || st.wal_bytes >= policy.max_wal;
            if !(due || force) {
                return Ok(None);
            }
            let state = st.doc.transact().encode_state_as_update_v1(&StateVector::default());
            (state, st.wal_bytes, st.updates, st.head.clone())
        };

        let hash = self.store.put_blob(&state)?;
        let version = if head.as_deref() == Some(hash.as_str()) {
            None
        } else {
            let v = self.store.record_snapshot(&self.name, &hash, state.len() as u64, unix_ms())?;
            log::info!("room {}: snapshot v{} {} ({} bytes)", self.name, v.seq, short(&hash), v.bytes);
            Some(v)
        };

        let mut st = self.state.lock().unwrap();
        if version.is_some() {
            st.head = Some(hash);
        }
        if covered_bytes > 0 {
            st.wal = Some(self.store.trim_wal(&self.name, covered_bytes)?);
            st.wal_bytes -= covered_bytes;
        }
        if st.updates == covered_updates {
            st.dirty_since = None;
        }
        Ok(version)
    }
}

impl RoomState {
    fn design(&self) -> Option<DesignKey> {
        let txn = self.doc.transact();
        let get = |k: &str| match self.meta.get(&txn, k) {
            Some(Out::Any(Any::String(s))) => Some(s.to_string()),
            _ => None,
        };
        Some(DesignKey {
            hash: get("design")?,
            ids: get("ids").and_then(|v| v.parse().ok()).unwrap_or(0),
            name: get("design_name").unwrap_or_default(),
        })
    }

    /// Write the design binding into `meta` as a server transaction: logged,
    /// and relayed to every peer already in the room.
    fn bind(&mut self, d: &DesignKey, room: &str, store: &Store, tx: &broadcast::Sender<Frame>) {
        let update = {
            let mut txn = self.doc.transact_mut();
            self.meta.insert(&mut txn, "design", d.hash.clone());
            self.meta.insert(&mut txn, "ids", d.ids.to_string());
            self.meta.insert(&mut txn, "design_name", d.name.clone());
            txn.encode_update_v1()
        };
        log::info!("room {room}: bound to design {} ({}, ids v{})", short(&d.hash), d.name, d.ids);
        self.log_update(&update, room, store);
        let _ = tx.send((SERVER_PEER, framed(TAG_DOC, &update)));
    }

    fn log_update(&mut self, update: &[u8], room: &str, store: &Store) {
        let now = Instant::now();
        self.last_update = now;
        self.updates += 1;
        self.dirty_since.get_or_insert(now);
        if self.wal.is_none() {
            match store.open_wal(room) {
                Ok(f) => self.wal = Some(f),
                Err(e) => log::error!("room {room}: cannot open WAL: {e}"),
            }
        }
        if let Some(wal) = &mut self.wal {
            match wal_append(wal, update) {
                Ok(n) => self.wal_bytes += n,
                // Still applied in memory; the next snapshot persists it.
                Err(e) => log::error!("room {room}: WAL append failed: {e}"),
            }
        }
    }
}

/// Every live room, restored from the store on first use.
pub struct Hub {
    pub store: Arc<Store>,
    pub policy: SnapshotPolicy,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    /// Phase 7.2: capability links, and the bytes each token has uploaded.
    pub auth: crate::auth::Auth,
    uploads: Mutex<HashMap<String, u64>>,
}

impl Hub {
    /// A hub with links not required (loopback, tests).
    pub fn new(store: Store, policy: SnapshotPolicy) -> Self {
        Self::with_auth(store, policy, false).expect("the store's secret")
    }

    /// A hub that requires a link token for rooms, packs, uploads and exports
    /// when `required`.
    pub fn with_auth(store: Store, policy: SnapshotPolicy, required: bool) -> io::Result<Self> {
        let auth = crate::auth::Auth::open(store.root(), required)?;
        Ok(Self { store: Arc::new(store), policy, rooms: Mutex::new(HashMap::new()), auth, uploads: Mutex::new(HashMap::new()) })
    }

    /// The grant `token` carries, checked now; `None` when links are not
    /// required (everything is allowed).
    pub fn grant(&self, token: Option<&str>) -> Result<Option<crate::auth::Grant>, crate::auth::Denied> {
        if !self.auth.required {
            return Ok(None);
        }
        let t = token.ok_or(crate::auth::Denied::Missing)?;
        self.auth.check(t, crate::auth::now(), |room| self.store.room_epoch(room)).map(Some)
    }

    /// `token` allows `need` in `room`.
    pub fn allow(&self, token: Option<&str>, room: &str, need: crate::auth::Role) -> Result<(), crate::auth::Denied> {
        match self.grant(token)? {
            None => Ok(()),
            Some(g) if g.room == room && g.role >= need => Ok(()),
            Some(_) => Err(crate::auth::Denied::NotAllowed),
        }
    }

    /// Count `bytes` against `token`'s upload quota; false once over it.
    pub fn charge_upload(&self, token: &str, bytes: u64, quota: u64) -> bool {
        let mut used = self.uploads.lock().unwrap();
        let u = used.entry(token.to_string()).or_insert(0);
        if *u + bytes > quota {
            return false;
        }
        *u += bytes;
        true
    }

    /// Room `name` (must satisfy [`crate::store::is_room_name`]), restored
    /// from the store if it is not live yet.
    pub fn room(&self, name: &str) -> io::Result<Arc<Room>> {
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get(name) {
            return Ok(r.clone());
        }
        let r = Arc::new(Room::restore(self.store.clone(), name)?);
        rooms.insert(name.to_string(), r.clone());
        Ok(r)
    }

    /// Room `name` if it is live or has anything on disk — a lookup that
    /// never creates one.
    pub fn existing_room(&self, name: &str) -> io::Result<Option<Arc<Room>>> {
        let live = self.rooms.lock().unwrap().get(name).cloned();
        match live {
            Some(r) => Ok(Some(r)),
            None if self.store.room_exists(name) => self.room(name).map(Some),
            None => Ok(None),
        }
    }

    fn live(&self) -> Vec<Arc<Room>> {
        self.rooms.lock().unwrap().values().cloned().collect()
    }

    /// Snapshot every room whose policy says it is due.
    pub fn snapshot_due(&self, now: Instant) {
        for r in self.live() {
            if let Err(e) = r.snapshot(&self.policy, now, false) {
                log::error!("room {}: snapshot failed: {e}", r.name);
            }
        }
    }

    /// Snapshot every room with unsnapshotted updates (shutdown).
    pub fn flush(&self) {
        for r in self.live() {
            if let Err(e) = r.snapshot(&self.policy, Instant::now(), true) {
                log::error!("room {}: snapshot failed: {e}", r.name);
            }
        }
    }
}

pub fn framed(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + payload.len());
    v.push(tag);
    v.extend_from_slice(payload);
    v
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}
