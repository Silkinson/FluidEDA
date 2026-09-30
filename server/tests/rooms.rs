//! End to end (Phase 6): a real server on an ephemeral port, real
//! `fluideda_sync::Session`s over WebSockets, a real store directory.
//!
//! Each server runs on its own Tokio runtime; dropping the runtime kills it
//! without a flush — the crash case the WAL exists for.

use fluideda_server::room::{Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use fluideda_sync::{EditKind, Presence, RoomDesign, Session};
use glam::Vec2;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use yrs::updates::decoder::Decode;
use yrs::{Doc, Map, Transact, Update};

const D1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const D2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Never snapshot on its own: tests decide when (via `Hub::flush`).
const MANUAL: SnapshotPolicy =
    SnapshotPolicy { idle: Duration::from_secs(3600), max_age: Duration::from_secs(3600), max_wal: u64::MAX };

struct TestServer {
    rt: tokio::runtime::Runtime,
    hub: Arc<Hub>,
    addr: SocketAddr,
}

impl TestServer {
    fn start(dir: &Path) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let hub = Arc::new(Hub::new(Store::open(dir).unwrap(), MANUAL));
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(fluideda_server::serve(listener, hub.clone()));
        Self { rt, hub, addr }
    }

    fn url(&self, room: &str, design: Option<&str>) -> String {
        let mut u = format!("ws://{}/ws?room={room}", self.addr);
        if let Some(d) = design {
            u += &format!("&design={d}&ids=2&name=chip.gds");
        }
        u
    }

    /// How many entries the server's copy of `room` holds in its edits map.
    fn edit_count(&self, room: &str) -> u32 {
        let frame = self.hub.room(room).unwrap().state_frame();
        let doc = Doc::new();
        let edits = doc.get_or_insert_map("edits");
        doc.transact_mut().apply_update(Update::decode_v1(&frame[1..]).unwrap());
        let txn = doc.transact();
        edits.len(&txn)
    }

    /// Kill the server without flushing anything.
    fn crash(self) {
        self.rt.shutdown_background();
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fluideda-it-{tag}-{}-{}", std::process::id(), fluideda_server::rand_id()))
}

fn session() -> Session {
    Session::new(Presence { cursor: Vec2::ZERO, name: "t".into(), color: [1.0; 4] })
}

/// Poll `sessions` until `done` holds (5 s limit).
fn pump(sessions: &mut [&mut Session], what: &str, mut done: impl FnMut(&[&mut Session]) -> bool) {
    let t0 = Instant::now();
    loop {
        for s in sessions.iter_mut() {
            s.poll();
        }
        if done(sessions) {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_server(what: &str, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn mv(dx: i32) -> EditKind {
    EditKind::Move { dx, dy: 0 }
}

#[test]
fn a_late_joiner_receives_what_was_done_before_it_connected() {
    let srv = TestServer::start(&temp_dir("late"));
    let mut a = session();
    a.join(&srv.url("late", Some(D1))).unwrap();
    pump(&mut [&mut a], "A synced", |s| s[0].is_synced());
    a.set_edit((0, 1), mv(5));
    a.set_edit((0, 2), EditKind::Delete);
    wait_server("the server to hold A's edits", || srv.edit_count("late") == 2);
    // A leaves before B arrives: nobody is left to relay anything to B.
    a.disconnect();

    let mut b = session();
    b.join(&srv.url("late", Some(D1))).unwrap();
    pump(&mut [&mut b], "B converges", |s| s[0].edits().len() == 2);
    let mut got = b.edits();
    got.sort_by_key(|e| e.0);
    assert_eq!(got, vec![((0, 1), mv(5)), ((0, 2), EditKind::Delete)]);
    assert_eq!(b.room_design().map(|d| d.hash), Some(D1.to_string()), "the binding arrives with the state");
}

#[test]
fn edits_made_before_joining_reach_the_room_when_the_socket_opens() {
    let srv = TestServer::start(&temp_dir("offline"));
    let mut a = session();
    a.set_edit((3, 7), mv(-2)); // offline
    a.join(&srv.url("offline", Some(D1))).unwrap();
    pump(&mut [&mut a], "A synced", |s| s[0].is_synced());
    wait_server("the server to merge A's offline edit", || srv.edit_count("offline") == 1);
}

#[test]
fn a_room_survives_a_crash_and_a_restart() {
    let dir = temp_dir("restart");
    let srv = TestServer::start(&dir);
    let mut a = session();
    a.join(&srv.url("r", Some(D1))).unwrap();
    pump(&mut [&mut a], "A synced", |s| s[0].is_synced());
    a.set_edit((0, 1), mv(1));
    a.set_edit((0, 2), mv(2));
    wait_server("the server to hold A's edits", || srv.edit_count("r") == 2);
    assert!(srv.hub.store.head("r").is_none(), "no snapshot yet: the edits live in the WAL only");
    srv.crash();
    a.poll();

    // Restart on the same store: the WAL is replayed.
    let srv = TestServer::start(&dir);
    let mut b = session();
    b.join(&srv.url("r", Some(D1))).unwrap();
    pump(&mut [&mut b], "B sees the WAL-recovered edits", |s| s[0].edits().len() == 2);
    b.set_edit((0, 3), mv(3));
    wait_server("the server to hold B's edit", || srv.edit_count("r") == 3);

    // Snapshot: a version is recorded and the WAL truncated.
    srv.hub.flush();
    let versions = srv.hub.store.versions("r");
    assert_eq!(versions.len(), 1);
    assert_eq!(srv.hub.store.head("r"), Some(versions[0].hash.clone()));
    assert_eq!(std::fs::metadata(dir.join("rooms/r/wal")).unwrap().len(), 0, "WAL truncated");
    srv.hub.flush();
    assert_eq!(srv.hub.store.versions("r").len(), 1, "nothing changed: no new version");
    srv.crash();

    // Restart again: now from the snapshot alone.
    let srv = TestServer::start(&dir);
    let mut c = session();
    c.join(&srv.url("r", Some(D1))).unwrap();
    pump(&mut [&mut c], "C sees the snapshot", |s| s[0].edits().len() == 3);
    assert_eq!(c.room_design().map(|d| d.name), Some("chip.gds".to_string()));
}

#[test]
fn a_peer_with_another_design_is_refused_and_cannot_touch_the_room() {
    let srv = TestServer::start(&temp_dir("bind"));
    let mut a = session();
    a.join(&srv.url("bind", Some(D1))).unwrap();
    pump(&mut [&mut a], "A synced", |s| s[0].is_synced());
    a.set_edit((0, 1), mv(1));
    wait_server("A's edit", || srv.edit_count("bind") == 1);

    let mut b = session();
    b.set_edit((9, 9), EditKind::Delete); // B's own edits, for another chip
    b.join(&srv.url("bind", Some(D2))).unwrap();
    pump(&mut [&mut b], "B refused", |s| s[0].refused().is_some());
    assert_eq!(b.refused(), Some(&RoomDesign { hash: D1.into(), ids: 2, name: "chip.gds".into() }));
    assert!(!b.is_connected() && b.error().is_some_and(|e| e.contains("chip.gds")));
    assert_eq!(b.edits().len(), 1, "B received nothing from the room");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(srv.edit_count("bind"), 1, "the room received nothing from B");

    // A peer that names no design is not checked (and binds nothing).
    let mut c = session();
    c.join(&srv.url("bind", None)).unwrap();
    pump(&mut [&mut c], "C converges", |s| s[0].edits().len() == 1);
}

// --- HTTP ---------------------------------------------------------------------

mod http {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn hub(tag: &str) -> Arc<Hub> {
        Arc::new(Hub::new(Store::open(temp_dir(tag)).unwrap(), MANUAL))
    }

    async fn call(hub: &Arc<Hub>, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let resp = fluideda_server::app(hub.clone()).oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        (parts.status, parts.headers, body.collect().await.unwrap().to_bytes().to_vec())
    }

    fn req(method: &str, uri: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(uri)
    }

    #[tokio::test]
    async fn blobs_are_immutable_content_addressed_and_rangeable() {
        let hub = hub("blobs");
        let body: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let hash = fluideda_server::store::sha256_hex(&body);
        let uri = format!("/blobs/{hash}");

        let (s, _, _) = call(&hub, req("GET", &uri).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let wrong = format!("/blobs/{D1}");
        let (s, _, msg) = call(&hub, req("PUT", &wrong).body(Body::from(body.clone())).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "a body must hash to its name");
        assert!(String::from_utf8_lossy(&msg).contains(&hash));
        let (s, _, _) = call(&hub, req("PUT", &uri).body(Body::from(body.clone())).unwrap()).await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, _, _) = call(&hub, req("PUT", &uri).body(Body::from(body.clone())).unwrap()).await;
        assert_eq!(s, StatusCode::OK, "already there");

        let (s, h, got) = call(&hub, req("GET", &uri).body(Body::empty()).unwrap()).await;
        assert_eq!((s, got.len()), (StatusCode::OK, body.len()));
        assert_eq!(got, body);
        assert_eq!(h[header::ETAG], format!("\"{hash}\"").as_str());
        assert!(h[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));

        let (s, h, got) = call(&hub, req("HEAD", &uri).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert!(got.is_empty());
        assert_eq!(h[header::CONTENT_LENGTH], "10000");

        let etag = format!("\"{hash}\"");
        let (s, _, got) =
            call(&hub, req("GET", &uri).header(header::IF_NONE_MATCH, &etag).body(Body::empty()).unwrap()).await;
        assert_eq!((s, got.len()), (StatusCode::NOT_MODIFIED, 0));

        let (s, h, got) =
            call(&hub, req("GET", &uri).header(header::RANGE, "bytes=300-599").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::PARTIAL_CONTENT);
        assert_eq!(got, &body[300..600]);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 300-599/10000");
        let (s, _, got) =
            call(&hub, req("GET", &uri).header(header::RANGE, "bytes=-16").body(Body::empty()).unwrap()).await;
        assert_eq!((s, got.as_slice()), (StatusCode::PARTIAL_CONTENT, &body[9984..]));
        let (s, _, _) =
            call(&hub, req("GET", &uri).header(header::RANGE, "bytes=10000-").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::RANGE_NOT_SATISFIABLE);

        let (s, _, _) = call(&hub, req("GET", "/blobs/..%2F..%2Fetc").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn room_info_reports_binding_and_history_without_creating_rooms() {
        let hub = hub("info");
        let (s, _, body) = call(&hub, req("GET", "/rooms/nobody").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["design"].is_null() && v["versions"].as_array().unwrap().is_empty());
        assert!(!hub.store.room_exists("nobody"), "a lookup creates nothing on disk");
        let (s, _, _) = call(&hub, req("GET", "/rooms/a.b").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Bind a room the way a joining peer does, snapshot it, and look again.
        let room = hub.room("chip").unwrap();
        let key = fluideda_server::room::DesignKey { hash: D1.into(), ids: 2, name: "chip.gds".into() };
        let (_, _rx) = room.join(Some(&key)).unwrap();
        hub.flush();
        let (_, _, body) = call(&hub, req("GET", "/rooms/chip").body(Body::empty()).unwrap()).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["design"]["hash"], D1);
        assert_eq!(v["design"]["name"], "chip.gds");
        assert_eq!(v["peers"], 1);
        assert_eq!(v["versions"].as_array().unwrap().len(), 1);
        assert_eq!(v["head"], v["versions"][0]["hash"]);
        // The snapshot is itself a blob.
        let snap = format!("/blobs/{}", v["head"].as_str().unwrap());
        let (s, _, _) = call(&hub, req("GET", &snap).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
    }
}
