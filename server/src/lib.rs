//! FluidEDA collaboration server (Phase 6).
//!
//! * `GET /ws?room=<name>[&design=<sha256>&ids=<n>&name=<file>]` — the room's
//!   WebSocket. The wire format is Phase 0's: byte 0 tags the channel (0x01 a
//!   yrs update, 0x02 presence). What changed: the server keeps each room's
//!   document ([`room`]), a joining peer first receives the whole room state,
//!   and a peer whose design differs from the room's is refused with a text
//!   frame `{"error":"design_mismatch",...}` and a close (code 4409).
//! * `GET|HEAD /blobs/<sha256>` — immutable content (designs, snapshots):
//!   `Cache-Control: immutable`, `ETag` = the hash, single byte `Range`s.
//! * `PUT /blobs/<sha256>` — upload; refused unless the body hashes to the name.
//! * `GET /rooms/<name>` — the room's design binding and version history.
//! * `GET /packs/<sha256>` — a stored design's streaming pack manifest, built
//!   on first request ([`packs`]); its blobs are served from `/blobs/`.
//! * `POST /export/<sha256>?format=gds|oas&ids=<n>` — the design with the
//!   edits of the document state in the body, written into the blob store
//!   ([`export`], Phase 7.4); `POST /rooms/<name>/export?format=` does the
//!   same with the room's design and state.
//! * `GET /health`.

pub mod export;
pub mod packs;
pub mod room;
pub mod store;

use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use room::{DesignKey, Hub, Room, TAG_AWARENESS, TAG_DOC};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;

/// Largest blob `PUT /blobs` accepts.
pub const MAX_BLOB: u64 = 16 << 30;

/// Testing (Phase 7.1c): every `GET /blobs` waits this many ms before
/// answering, standing in for a network round trip. 0 = off.
static LATENCY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Delay every blob read by `ms` (`FLUIDEDA_LATENCY_MS`; testing only).
pub fn set_latency_ms(ms: u64) {
    LATENCY_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}
/// Read size when streaming a blob out.
const CHUNK: u64 = 256 << 10;

pub fn app(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/blobs/:hash", get(get_blob).put(put_blob))
        .route("/rooms/:room", get(room_info))
        .route("/rooms/:room/export", post(export_room))
        .route("/packs/:design", get(pack_manifest))
        .route("/export/:design", post(export_design))
        .route("/health", get(|| async { "ok" }))
        .with_state(hub)
}

/// Serve the API on `listener` until the future is dropped.
pub async fn serve(listener: tokio::net::TcpListener, hub: Arc<Hub>) -> io::Result<()> {
    axum::serve(listener, app(hub)).await
}

/// Snapshot due rooms every half second, off the async workers.
pub fn spawn_snapshotter(hub: Arc<Hub>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            let h = hub.clone();
            let _ = tokio::task::spawn_blocking(move || h.snapshot_due(Instant::now())).await;
        }
    })
}

#[derive(Deserialize)]
struct WsQuery {
    #[serde(default = "default_room")]
    room: String,
    design: Option<String>,
    ids: Option<u32>,
    name: Option<String>,
}

fn default_room() -> String {
    "demo".to_string()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<WsQuery>, State(hub): State<Arc<Hub>>) -> Response {
    if !store::is_room_name(&q.room) {
        return bad_request("room names are 1-64 characters of [A-Za-z0-9_-]");
    }
    let design = match q.design {
        Some(hash) if store::is_hash(&hash) => Some(DesignKey {
            hash,
            ids: q.ids.unwrap_or(0),
            name: q.name.unwrap_or_default().chars().take(256).collect(),
        }),
        Some(_) => return bad_request("design must be a lower-case sha256 hex digest"),
        None => None,
    };
    let room = match hub.room(&q.room) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("room {}: {e}", q.room)).into_response(),
    };
    ws.on_upgrade(move |socket| handle_socket(socket, room, design))
}

async fn handle_socket(socket: WebSocket, room: Arc<Room>, design: Option<DesignKey>) {
    let peer_id = rand_id();
    let (mut sink, mut stream) = socket.split();
    let (state, mut rx) = match room.join(design.as_ref()) {
        Ok(joined) => joined,
        Err(bound) => {
            log::info!("peer {peer_id} refused from room {}: its design differs", room.name);
            let msg = serde_json::json!({ "error": "design_mismatch", "room": room.name, "design": bound });
            let _ = sink.send(Message::Text(msg.to_string())).await;
            let close = CloseFrame { code: 4409, reason: "design mismatch".into() };
            let _ = sink.send(Message::Close(Some(close))).await;
            return;
        }
    };
    log::info!("peer {peer_id} joined room {}", room.name);

    // The late-joiner fix: the whole room state goes first.
    if sink.send(Message::Binary(state)).await.is_err() {
        room.leave();
        return;
    }

    let room_in = room.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(data) => match data.first() {
                    Some(&TAG_DOC) => {
                        if !room_in.apply(peer_id, data) {
                            log::warn!("peer {peer_id}: undecodable update dropped");
                        }
                    }
                    Some(&TAG_AWARENESS) => room_in.relay(peer_id, data),
                    _ => {}
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let room_out = room.clone();
    let mut send_task = tokio::spawn(async move {
        loop {
            let data = match rx.recv().await {
                Ok((from, _)) if from == peer_id => continue,
                Ok((_, data)) => data,
                // Fell more than the channel's capacity behind: resync with the
                // whole state (updates are idempotent, so overlap is harmless).
                Err(RecvError::Lagged(n)) => {
                    log::warn!("peer {peer_id} lagged {n} messages; resyncing");
                    room_out.state_frame()
                }
                Err(RecvError::Closed) => break,
            };
            if sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut recv_task => send_task.abort(),
        _ = &mut send_task => recv_task.abort(),
    }
    room.leave();
    log::info!("peer {peer_id} left room {}", room.name);
}

// --- blobs ------------------------------------------------------------------

/// A `Range` header, as far as a single-range server cares.
#[derive(Debug, PartialEq, Eq)]
enum RangeReq {
    /// Absent, malformed or multi-range: serve the whole blob (RFC 9110 lets a
    /// server ignore Range).
    Full,
    /// Inclusive byte range.
    Part(u64, u64),
    Unsatisfiable,
}

fn parse_range(h: &str, len: u64) -> RangeReq {
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return RangeReq::Full };
    let Some((a, b)) = spec.split_once('-') else { return RangeReq::Full };
    if spec.contains(',') {
        return RangeReq::Full;
    }
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix range: the last `n` bytes.
        return match b.parse::<u64>() {
            Ok(0) => RangeReq::Unsatisfiable,
            Ok(_) if len == 0 => RangeReq::Unsatisfiable,
            Ok(n) => RangeReq::Part(len - n.min(len), len - 1),
            Err(_) => RangeReq::Full,
        };
    }
    let Ok(first) = a.parse::<u64>() else { return RangeReq::Full };
    let last = if b.is_empty() {
        u64::MAX
    } else {
        match b.parse::<u64>() {
            Ok(v) if v >= first => v,
            _ => return RangeReq::Full,
        }
    };
    if first >= len {
        return RangeReq::Unsatisfiable;
    }
    RangeReq::Part(first, last.min(len - 1))
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().trim_start_matches("W/"))
        .any(|t| t == etag || t == "*")
}

async fn get_blob(State(hub): State<Arc<Hub>>, Path(hash): Path<String>, headers: HeaderMap) -> Response {
    if !store::is_hash(&hash) {
        return bad_request("not a sha256 hex digest");
    }
    let delay = LATENCY_MS.load(std::sync::atomic::Ordering::Relaxed);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let Ok(mut file) = tokio::fs::File::open(hub.store.blob_path(&hash)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let etag = format!("\"{hash}\"");
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    // Named by content: it can never change, so cache it forever.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if if_none_match(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map_or(RangeReq::Full, |r| parse_range(r, len));
    let (status, start, n) = match range {
        RangeReq::Full => (StatusCode::OK, 0, len),
        RangeReq::Part(a, b) => {
            h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")).unwrap());
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        RangeReq::Unsatisfiable => {
            h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{len}")).unwrap());
            return (StatusCode::RANGE_NOT_SATISFIABLE, h).into_response();
        }
    };
    if start > 0 {
        if let Err(e) = file.seek(SeekFrom::Start(start)).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    (status, h, Body::from_stream(file_chunks(file, n))).into_response()
}

/// `n` bytes of `file` from its current position, as a body stream.
fn file_chunks(file: tokio::fs::File, n: u64) -> impl futures::Stream<Item = io::Result<Bytes>> {
    futures::stream::try_unfold((file, n), |(mut f, left)| async move {
        if left == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; left.min(CHUNK) as usize];
        f.read_exact(&mut buf).await?;
        let rest = left - buf.len() as u64;
        Ok(Some((Bytes::from(buf), (f, rest))))
    })
}

enum PutError {
    Mismatch(String),
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for PutError {
    fn from(e: io::Error) -> Self {
        PutError::Io(e)
    }
}

async fn put_blob(State(hub): State<Arc<Hub>>, Path(hash): Path<String>, body: Body) -> Response {
    if !store::is_hash(&hash) {
        return bad_request("not a sha256 hex digest");
    }
    if hub.store.has_blob(&hash) {
        return (StatusCode::OK, "exists").into_response();
    }
    match receive_blob(&hub.store, &hash, body).await {
        Ok(_) => {
            log::info!("blob {} stored", &hash[..12]);
            (StatusCode::CREATED, "stored").into_response()
        }
        Err(PutError::Mismatch(got)) => bad_request(format!("body hashes to {got}, not {hash}")),
        Err(PutError::TooLarge) => (StatusCode::PAYLOAD_TOO_LARGE, format!("blobs are at most {MAX_BLOB} bytes")).into_response(),
        Err(PutError::Io(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Stream `body` to a temp file while hashing it, then move it into place as
/// blob `hash` — only if the bytes really hash to that name.
async fn receive_blob(store: &store::Store, hash: &str, body: Body) -> Result<bool, PutError> {
    let tmp = store.tmp_path();
    let written = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut n = 0u64;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(io::Error::other)?;
            n += chunk.len() as u64;
            if n > MAX_BLOB {
                return Err(PutError::TooLarge);
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        let got = store::hex(&hasher.finalize());
        if got != hash {
            return Err(PutError::Mismatch(got));
        }
        Ok(())
    }
    .await;
    match written {
        Ok(()) => Ok(store.commit_blob(&tmp, hash)?),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

// --- rooms ------------------------------------------------------------------

async fn room_info(State(hub): State<Arc<Hub>>, Path(name): Path<String>) -> Response {
    if !store::is_room_name(&name) {
        return bad_request("room names are 1-64 characters of [A-Za-z0-9_-]");
    }
    // Disk reads, and possibly a wait for a snapshot to finish: off the async workers.
    let info = tokio::task::spawn_blocking(move || match hub.existing_room(&name)? {
        Some(r) => Ok(r.info()),
        None => Ok(room::RoomInfo { room: name, design: None, head: None, versions: Vec::new(), peers: 0, state_bytes: 0 }),
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e)));
    match info {
        Ok(info) => ([(header::CACHE_CONTROL, "no-cache")], Json(info)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// --- packs ------------------------------------------------------------------

/// One pack build at a time (a build parses a whole design).
static PACK_BUILDS: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn pack_manifest(State(hub): State<Arc<Hub>>, Path(design): Path<String>) -> Response {
    if !store::is_hash(&design) {
        return bad_request("not a sha256 hex digest");
    }
    let store = hub.store.clone();
    let built = tokio::task::spawn_blocking(move || {
        let _one = PACK_BUILDS.lock().unwrap_or_else(|e| e.into_inner());
        packs::ensure(&store, &design)
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e)));
    match built {
        Ok(Some(m)) => ([(header::CACHE_CONTROL, "no-cache")], Json(m)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such design blob").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// --- export (Phase 7.4 SLICE C) ---------------------------------------------

/// Largest document state `POST /export` accepts.
const MAX_STATE: usize = 256 << 20;

/// One export at a time (an export parses a whole design).
static EXPORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Deserialize)]
struct ExportQuery {
    /// `gds` (the default) or `oas`.
    format: Option<String>,
    /// The client's shape-id scheme: its edits number shapes by it.
    ids: Option<u32>,
}

impl ExportQuery {
    fn oasis(&self) -> Result<bool, String> {
        match self.format.as_deref() {
            None | Some("gds") => Ok(false),
            Some("oas") => Ok(true),
            Some(f) => Err(format!("format {f:?}: gds or oas")),
        }
    }
}

/// `POST /export/<design>`: the design with the edits of the document state
/// in the body (a client's own — it need not be in a room).
async fn export_design(
    State(hub): State<Arc<Hub>>,
    Path(design): Path<String>,
    Query(q): Query<ExportQuery>,
    body: Body,
) -> Response {
    if !store::is_hash(&design) {
        return bad_request("not a sha256 hex digest");
    }
    let oasis = match q.oasis() {
        Ok(o) => o,
        Err(e) => return bad_request(e),
    };
    if let Some(ids) = q.ids.filter(|&i| i != fluideda_core::SHAPE_ID_SCHEME) {
        return id_scheme_conflict(ids);
    }
    let Ok(state) = axum::body::to_bytes(body, MAX_STATE).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, format!("document states are at most {MAX_STATE} bytes")).into_response();
    };
    run_export(&hub, design, state.to_vec(), oasis).await
}

/// `POST /rooms/<room>/export`: the room's design with the room's edits.
async fn export_room(State(hub): State<Arc<Hub>>, Path(name): Path<String>, Query(q): Query<ExportQuery>) -> Response {
    if !store::is_room_name(&name) {
        return bad_request("room names are 1-64 characters of [A-Za-z0-9_-]");
    }
    let oasis = match q.oasis() {
        Ok(o) => o,
        Err(e) => return bad_request(e),
    };
    let h = hub.clone();
    let found = tokio::task::spawn_blocking(move || h.existing_room(&name).map(|r| r.map(|r| r.design_and_state())))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
    match found {
        Ok(Some((Some(design), state))) => {
            if design.ids != fluideda_core::SHAPE_ID_SCHEME {
                return id_scheme_conflict(design.ids);
            }
            run_export(&hub, design.hash, state, oasis).await
        }
        Ok(Some((None, _))) => (StatusCode::NOT_FOUND, "the room is not bound to a design").into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such room").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Edits numbered under another shape-id scheme would land on other shapes.
fn id_scheme_conflict(ids: u32) -> Response {
    (
        StatusCode::CONFLICT,
        format!("edits numbered under shape-id scheme {ids}; this server parses with {}", fluideda_core::SHAPE_ID_SCHEME),
    )
        .into_response()
}

async fn run_export(hub: &Arc<Hub>, design: String, state: Vec<u8>, oasis: bool) -> Response {
    let store = hub.store.clone();
    let done = tokio::task::spawn_blocking(move || {
        let _one = EXPORTS.lock().unwrap_or_else(|e| e.into_inner());
        export::export(&store, &design, &state, oasis)
    })
    .await
    .unwrap_or_else(|e| Err(export::ExportError::Io(io::Error::other(e))));
    match done {
        Ok(e) => ([(header::CACHE_CONTROL, "no-cache")], Json(e)).into_response(),
        Err(export::ExportError::NoDesign) => (StatusCode::NOT_FOUND, "no such design blob").into_response(),
        Err(export::ExportError::BadState(m)) => bad_request(m),
        Err(export::ExportError::BadDesign(m)) => (StatusCode::UNPROCESSABLE_ENTITY, m).into_response(),
        Err(export::ExportError::Io(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// A peer id: unique enough per connection, never [`room::SERVER_PEER`].
pub fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let id = (n ^ SEQ.fetch_add(1, Ordering::Relaxed).rotate_left(32)).wrapping_mul(0x9E3779B97F4A7C15);
    id.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        use RangeReq::*;
        assert_eq!(parse_range("bytes=0-9", 100), Part(0, 9));
        assert_eq!(parse_range("bytes=90-", 100), Part(90, 99));
        assert_eq!(parse_range("bytes=90-200", 100), Part(90, 99), "clamped to the end");
        assert_eq!(parse_range("bytes=-10", 100), Part(90, 99), "suffix");
        assert_eq!(parse_range("bytes=-500", 100), Part(0, 99));
        assert_eq!(parse_range("bytes=100-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-1", 100), Full, "inverted: ignored");
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Full, "multi-range: ignored");
        assert_eq!(parse_range("items=0-1", 100), Full);
    }
}
