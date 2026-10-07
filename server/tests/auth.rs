//! Phase 7.2: capability links, end to end. With links required, every
//! endpoint is called with no token, a forged one, an expired one, and a
//! viewer's, an editor's and an owner's for the room, plus an editor's for
//! another room; then a rotation revokes them all. Over real sockets: a
//! viewer follows an editor's edits live, its own edits never reach the
//! room's document or its WAL, and a peer without a link is refused.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use fluideda_server::auth::Role;
use fluideda_server::room::{DesignKey, Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use fluideda_sync::{EditKind, Presence, Session};
use glam::Vec2;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const HIER: &[u8] = include_bytes!("../../assets/hier.gds");

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fluideda-auth-{tag}-{}-{}", std::process::id(), fluideda_server::rand_id()))
}

fn hex_sha256(b: &[u8]) -> String {
    use sha2::Digest;
    fluideda_server::store::hex(&sha2::Sha256::digest(b))
}

async fn call(hub: &Arc<Hub>, method: Method, uri: &str, token: Option<&str>, body: Vec<u8>) -> StatusCode {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = fluideda_server::app(hub.clone()).oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    resp.status()
}

/// What an endpoint is called with, and the status expected for each token
/// of the matrix, in order.
type Case = (&'static str, Method, String, Vec<u8>, [StatusCode; 7]);

/// The tokens every endpoint is tried with, and what each is.
struct Tokens {
    forged: String,
    expired: String,
    viewer: String,
    editor: String,
    owner: String,
    elsewhere: String,
}

fn tokens(hub: &Hub) -> Tokens {
    let owner = hub.auth.mint("r1", Role::Owner, 0, 0);
    Tokens {
        forged: owner.replace("owner", "editor"),
        expired: hub.auth.mint("r1", Role::Owner, 1, 0),
        viewer: hub.auth.mint("r1", Role::Viewer, 0, 0),
        editor: hub.auth.mint("r1", Role::Editor, 0, 0),
        owner,
        elsewhere: hub.auth.mint("r2", Role::Editor, 0, 0),
    }
}

#[tokio::test]
async fn every_endpoint_checks_its_link() {
    let dir = temp_dir("matrix");
    let hub = Arc::new(Hub::with_auth(Store::open(&dir).unwrap(), SnapshotPolicy::default(), true).unwrap());
    let design = hex_sha256(HIER);
    hub.store.put_blob(HIER).unwrap();
    // Room r1 is bound to the design (as the first peer with it binds it).
    let key = DesignKey { hash: design.clone(), ids: fluideda_core::SHAPE_ID_SCHEME, name: "hier.gds".into() };
    hub.room("r1").unwrap().join(Some(&key)).unwrap();
    let t = tokens(&hub);
    let all = [
        ("none", None),
        ("forged", Some(t.forged.as_str())),
        ("expired", Some(t.expired.as_str())),
        ("viewer", Some(t.viewer.as_str())),
        ("editor", Some(t.editor.as_str())),
        ("owner", Some(t.owner.as_str())),
        ("other room", Some(t.elsewhere.as_str())),
    ];
    const U: StatusCode = StatusCode::UNAUTHORIZED;
    const F: StatusCode = StatusCode::FORBIDDEN;
    const OK: StatusCode = StatusCode::OK;
    let new_blob = b"a blob nobody stored yet".to_vec();
    let state = Session::new(Presence { cursor: Vec2::ZERO, name: String::new(), color: [0.0; 4] }).state();
    // (what, method, uri, body, expected per token in `all`'s order)
    let cases: Vec<Case> = vec![
        ("room info", Method::GET, "/rooms/r1".into(), vec![], [U, U, U, OK, OK, OK, F]),
        ("pack", Method::GET, format!("/packs/{design}"), vec![], [U, U, U, OK, OK, OK, F]),
        ("pick", Method::POST, format!("/packs/{design}/pick"), fluideda_core::pack::encode_pick_questions(&[(0, 0, 0)]), [U, U, U, OK, OK, OK, F]),
        ("room export", Method::POST, "/rooms/r1/export".into(), vec![], [U, U, U, OK, OK, OK, F]),
        ("export", Method::POST, format!("/export/{design}"), state.clone(), [U, U, U, F, OK, OK, F]),
        ("upload (stored)", Method::PUT, format!("/blobs/{design}"), HIER.to_vec(), [U, U, U, F, OK, OK, OK]),
        ("share", Method::POST, "/rooms/r1/share?role=viewer".into(), vec![], [U, U, U, F, F, OK, F]),
    ];
    for (what, method, uri, body, want) in &cases {
        for ((who, tok), want) in all.iter().zip(want) {
            let got = call(&hub, method.clone(), uri, *tok, body.clone()).await;
            assert_eq!(got, *want, "{what} with {who}'s link");
        }
    }
    // A new upload, by an editor of any room: stored once.
    let h = hex_sha256(&new_blob);
    assert_eq!(call(&hub, Method::PUT, &format!("/blobs/{h}"), Some(&t.viewer), new_blob.clone()).await, F);
    assert_eq!(call(&hub, Method::PUT, &format!("/blobs/{h}"), Some(&t.elsewhere), new_blob.clone()).await, StatusCode::CREATED);
    // Blobs need no link: a SHA-256 cannot be guessed.
    assert_eq!(call(&hub, Method::GET, &format!("/blobs/{design}"), None, vec![]).await, OK);
    // `?t=` works where a header cannot be set.
    assert_eq!(call(&hub, Method::GET, &format!("/rooms/r1?t={}", t.viewer), None, vec![]).await, OK);

    // Rotation revokes every link issued so far; the owner gets a fresh one.
    assert_eq!(call(&hub, Method::POST, "/rooms/r1/rotate", Some(&t.viewer), vec![]).await, F);
    let req = Request::builder().method(Method::POST).uri("/rooms/r1/rotate").header("authorization", format!("Bearer {}", t.owner));
    let resp = fluideda_server::app(hub.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
    let fresh = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
    for (who, tok) in [("viewer", &t.viewer), ("editor", &t.editor), ("owner", &t.owner)] {
        assert_eq!(call(&hub, Method::GET, "/rooms/r1", Some(tok), vec![]).await, U, "{who}'s link after rotation");
    }
    assert_eq!(call(&hub, Method::GET, "/rooms/r1", Some(&fresh), vec![]).await, OK);
    // Room r2 has its own key: its links still work.
    assert_eq!(call(&hub, Method::GET, "/rooms/r2", Some(&t.elsewhere), vec![]).await, OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn links_are_not_needed_on_loopback() {
    let dir = temp_dir("off");
    let hub = Arc::new(Hub::new(Store::open(&dir).unwrap(), SnapshotPolicy::default()));
    assert_eq!(call(&hub, Method::GET, "/rooms/r1", None, vec![]).await, StatusCode::OK);
    assert_eq!(call(&hub, Method::POST, "/rooms/r1/share?role=editor", None, vec![]).await, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

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
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_viewer_follows_but_cannot_edit() {
    let dir = temp_dir("viewer");
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let manual = SnapshotPolicy { idle: Duration::from_secs(3600), max_age: Duration::from_secs(3600), max_wal: u64::MAX };
    let hub = Arc::new(Hub::with_auth(Store::open(&dir).unwrap(), manual, true).unwrap());
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(fluideda_server::serve(listener, hub.clone()));
    let url = |t: &str| format!("ws://{addr}/ws?room=r1&t={t}");
    let me = |n: &str| Presence { cursor: Vec2::ZERO, name: n.into(), color: [1.0; 4] };

    let mut editor = Session::connect(&url(&hub.auth.mint("r1", Role::Editor, 0, 0)), me("editor")).unwrap();
    let mut viewer = Session::connect(&url(&hub.auth.mint("r1", Role::Viewer, 0, 0)), me("viewer")).unwrap();
    pump(&mut [&mut editor, &mut viewer], "both joined", |s| s.iter().all(|s| s.is_synced()));

    // The editor's edit reaches the viewer, live.
    editor.set_edit((0, 1), EditKind::Delete);
    pump(&mut [&mut editor, &mut viewer], "the viewer to see the edit", |s| s[1].edits().len() == 1);

    // The viewer's own edit stays on its screen: the room drops it.
    viewer.set_edit((0, 2), EditKind::Move { dx: 5, dy: 5 });
    editor.set_edit((0, 3), EditKind::Delete); // a marker that the room has caught up
    pump(&mut [&mut editor, &mut viewer], "the second edit", |s| s[1].edits().len() == 3);
    let room = hub.room("r1").unwrap();
    let server = Session::from_state(&room.design_and_state().1).unwrap();
    let keys: Vec<_> = server.edits().into_iter().map(|(id, _)| id).collect();
    assert_eq!(keys.len(), 2, "the room holds the editor's two edits only: {keys:?}");
    assert!(!keys.contains(&(0, 2)), "the viewer's edit reached the room");
    assert!(editor.edits().iter().all(|(id, _)| *id != (0, 2)), "the viewer's edit reached the editor");
    // Nor its WAL: replaying the room from disk gives the same two.
    drop(room);
    let replayed = fluideda_server::room::Room::restore(hub.store.clone(), "r1").unwrap();
    assert_eq!(Session::from_state(&replayed.design_and_state().1).unwrap().edits().len(), 2);

    // No link, or a forged one: refused before any state moves.
    for t in ["", "r1.owner.0.0.00"] {
        let mut stranger = Session::connect(&url(t), me("stranger")).unwrap();
        pump(&mut [&mut stranger], "the refusal", |s| s[0].error().is_some());
        assert!(stranger.error().unwrap().contains("unauthorized"), "{:?}", stranger.error());
        assert!(stranger.edits().is_empty());
    }
    rt.shutdown_background();
    let _ = std::fs::remove_dir_all(&dir);
}
