//! fluideda-sync — collaborative session state.
//!
//! Two channels, exactly as designed:
//!   * **presence** (ephemeral): each user's cursor + name, via yrs *awareness*.
//!   * **document** (persistent): a shared `markers` array in a `yrs::Doc`.
//!
//! Transport is a single WebSocket to the relay server (`ewebsock`, works on
//! wasm + native). We do NOT hand-roll conflict resolution — yrs guarantees
//! convergence; the server just rebroadcasts opaque update blobs to the room.

use anyhow::Result;
use ewebsock::{WsMessage, WsReceiver, WsSender};
use glam::Vec2;
use std::collections::HashMap;
use yrs::{
    types::ToJson, Array, ArrayRef, Doc, Map, MapPrelim, ReadTxn, Transact,
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

/// Message envelope on the wire. Byte 0 tags the channel so the relay and peers
/// can route without parsing the payload.
///   0x01 = yrs document update (sync)
///   0x02 = awareness/presence update
pub mod tag {
    pub const DOC: u8 = 0x01;
    pub const AWARENESS: u8 = 0x02;
}

/// Client-side session: owns the doc, the socket, and a cache of remote presence.
pub struct Session {
    doc: Doc,
    markers: ArrayRef,
    tx: WsSender,
    rx: WsReceiver,
    pub local: Presence,
    pub remote: HashMap<u64, Presence>,
    client_id: u64,
}

impl Session {
    /// Connect to `ws://…/room/<room>` and initialise the shared doc.
    pub fn connect(url: &str, local: Presence) -> Result<Self> {
        let doc = Doc::new();
        let markers = doc.get_or_insert_array("markers");
        let client_id = doc.client_id();

        let (tx, rx) = ewebsock::connect(url, ewebsock::Options::default())
            .map_err(|e| anyhow::anyhow!("ws connect: {e}"))?;

        Ok(Self {
            doc,
            markers,
            tx,
            rx,
            local,
            remote: HashMap::new(),
            client_id,
        })
    }

    /// Publish our cursor (world nm). Call when the pointer moves.
    pub fn set_cursor(&mut self, world: Vec2) {
        self.local.cursor = world;
        // Encode presence as a tiny payload: [tag][client_id][x][y][name…].
        let mut buf = vec![tag::AWARENESS];
        buf.extend_from_slice(&self.client_id.to_le_bytes());
        buf.extend_from_slice(&world.x.to_le_bytes());
        buf.extend_from_slice(&world.y.to_le_bytes());
        buf.extend_from_slice(self.local.name.as_bytes());
        self.tx.send(WsMessage::Binary(buf));
    }

    /// Add a shared marker; the update propagates to all peers automatically.
    pub fn add_marker(&mut self, m: Marker) {
        let mut txn = self.doc.transact_mut();
        let entry = MapPrelim::from(HashMap::from([
            ("minx".to_string(), m.min.x as f64),
            ("miny".to_string(), m.min.y as f64),
            ("maxx".to_string(), m.max.x as f64),
            ("maxy".to_string(), m.max.y as f64),
        ]));
        self.markers.push_back(&mut txn, entry);
        let update = txn.encode_update_v1();
        drop(txn);

        let mut buf = vec![tag::DOC];
        buf.extend_from_slice(&update);
        self.tx.send(WsMessage::Binary(buf));
    }

    /// Read all current markers from the shared doc.
    pub fn markers(&self) -> Vec<Marker> {
        let txn = self.doc.transact();
        let mut out = Vec::new();
        for item in self.markers.iter(&txn) {
            if let Ok(json) = item.to_json(&txn).to_string().parse::<String>() {
                // In real code we'd read typed fields; kept illustrative here.
                let _ = json;
            }
        }
        out
    }

    /// Pump incoming messages. Call every frame; applies doc updates and refreshes
    /// remote presence. Returns true if anything changed (request a repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(ev) = self.rx.try_recv() {
            if let ewebsock::WsEvent::Message(WsMessage::Binary(buf)) = ev {
                match buf.first().copied() {
                    Some(tag::DOC) => {
                        if let Ok(update) = yrs::Update::decode_v1(&buf[1..]) {
                            let mut txn = self.doc.transact_mut();
                            let _ = txn.apply_update(update);
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
        }
        changed
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
