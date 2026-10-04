//! Server-side export (Phase 7.4 SLICE C): a stored design written with a
//! document's edits — a client's own state, or a room's — is byte for byte the
//! file an in-core client saves, and re-reads as exactly what was drawn.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fluideda_core::{CellDb, EditOverlay, Layer, Point};
use fluideda_server::export::Exported;
use fluideda_server::room::{framed, DesignKey, Hub, SnapshotPolicy, TAG_DOC};
use fluideda_server::store::Store;
use fluideda_sync::{BatchOp, EditKind, Presence, Session};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

const FIXTURE: &[u8] = include_bytes!("../../assets/edit_fixture.gds");
const SCHEME: u32 = fluideda_core::SHAPE_ID_SCHEME;

fn hub() -> Arc<Hub> {
    let dir = std::env::temp_dir().join(format!("fluideda-export-{}-{}", std::process::id(), fluideda_server::rand_id()));
    Arc::new(Hub::new(Store::open(dir).unwrap(), SnapshotPolicy::default()))
}

async fn post(hub: &Arc<Hub>, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap();
    let resp = fluideda_server::app(hub.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
    vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h), Point::new(x, y)]
}

/// Edits of every kind, made through a client session: moves (TOP and the
/// LEAF master), a delete, a vertex edit, shapes drawn into TOP and LEAF, and
/// LEAF's placement hidden ("edit this placement only" hides one).
fn edited(db: &CellDb) -> Session {
    let mut s = Session::new(Presence { cursor: glam::Vec2::ZERO, name: "test".into(), color: [1.0; 4] });
    let (top, leaf) = (db.top, db.id_of("LEAF").unwrap());
    s.set_edit((top, 0), EditKind::Move { dx: 500, dy: -250 });
    s.set_edit((top, 2), EditKind::Delete);
    s.set_edit((top, 4), EditKind::SetPoints(vec![
        Point::new(5200, 3400),
        Point::new(6100, 3400),
        Point::new(5800, 4000),
        Point::new(5200, 4000),
        Point::new(5200, 3400),
    ]));
    s.set_edit((leaf, 1), EditKind::Move { dx: 10, dy: 10 });
    s.add_shape(top, Layer::new(5, 0), &rect(9000, 9000, 3000, 2000));
    s.add_shape(leaf, Layer::new(6, 0), &rect(20, 20, 160, 40));
    s.apply_batch(vec![BatchOp::Hide((top, 0, 0, 0))]);
    s
}

/// Everything drawn, flattened and sorted (a layout's composite as a multiset).
fn drawn(db: &CellDb, overlay: &EditOverlay) -> Vec<(Layer, Vec<(i32, i32)>)> {
    let mut v: Vec<_> = fluideda_core::flatten::composite_flat(db, overlay)
        .into_iter()
        .map(|s| (s.layer, s.points.iter().map(|p| (p.x, p.y)).collect()))
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_server_export_is_the_file_an_in_core_client_saves() {
    let hub = hub();
    let design = hub.store.put_blob(FIXTURE).unwrap();
    let db = fluideda_core::load_layout_db(FIXTURE).unwrap();
    let session = edited(&db);
    let (overlay, waiting) = session.overlay(&db);
    assert!(waiting.is_empty(), "in-core: nothing waits for a chunk");
    assert_eq!((overlay.deleted.len(), overlay.edited.len(), overlay.added.len(), overlay.hidden.len()), (4, 3, 2, 1));

    let gds = fluideda_core::write_gds(&db, &overlay);
    let oas = fluideda_core::write_oasis(&db, &overlay);
    for (format, local) in [("gds", &gds), ("oas", &oas)] {
        let (s, body) = post(&hub, &format!("/export/{design}?format={format}&ids={SCHEME}"), session.state()).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let e: Exported = serde_json::from_slice(&body).unwrap();
        assert_eq!((e.format.as_str(), e.edited, e.added, e.hidden), (format, 4, 2, 1));
        let out = hub.store.read_blob(&e.hash).unwrap();
        assert_eq!(e.bytes, out.len() as u64);
        assert!(out == *local, "{format}: the server's file differs from the in-core save");
    }

    // And it re-reads as exactly what is drawn: the composite, as a multiset.
    // (With TOP's only LEAF placement hidden, LEAF is a second root of the
    // file: compose from TOP.)
    let mut back = fluideda_core::load_layout_db(&gds).unwrap();
    back.top = back.id_of("TOP").unwrap();
    assert_eq!(drawn(&back, &EditOverlay::new()), drawn(&db, &overlay));
    // An empty document exports the design unchanged.
    let (s, body) = post(&hub, &format!("/export/{design}"), Session::new(Presence {
        cursor: glam::Vec2::ZERO,
        name: String::new(),
        color: [0.0; 4],
    })
    .state())
    .await;
    assert_eq!(s, StatusCode::OK);
    let e: Exported = serde_json::from_slice(&body).unwrap();
    let plain = fluideda_core::load_layout_db(&hub.store.read_blob(&e.hash).unwrap()).unwrap();
    assert_eq!(drawn(&plain, &EditOverlay::new()), drawn(&db, &EditOverlay::new()));
}

#[tokio::test]
async fn a_room_exports_its_own_edits() {
    let hub = hub();
    let design = hub.store.put_blob(FIXTURE).unwrap();
    let db = fluideda_core::load_layout_db(FIXTURE).unwrap();
    let session = edited(&db);

    // A peer binds the room to the design and sends its edits.
    let room = hub.room("exp").unwrap();
    let key = DesignKey { hash: design.clone(), ids: SCHEME, name: "edit_fixture.gds".into() };
    room.join(Some(&key)).unwrap();
    assert!(room.apply(7, framed(TAG_DOC, &session.state())));

    let (s, body) = post(&hub, "/rooms/exp/export?format=gds", Vec::new()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let e: Exported = serde_json::from_slice(&body).unwrap();
    let local = fluideda_core::write_gds(&db, &session.overlay(&db).0);
    assert!(hub.store.read_blob(&e.hash).unwrap() == local, "the room's export differs from the in-core save");
}

#[tokio::test]
async fn export_refusals() {
    let hub = hub();
    let design = hub.store.put_blob(FIXTURE).unwrap();
    let empty = Session::new(Presence { cursor: glam::Vec2::ZERO, name: String::new(), color: [0.0; 4] }).state();
    let unknown = "0".repeat(64);
    for (uri, body, want) in [
        (format!("/export/{unknown}"), empty.clone(), StatusCode::NOT_FOUND),
        (format!("/export/{design}?format=dxf"), empty.clone(), StatusCode::BAD_REQUEST),
        (format!("/export/{design}?ids={}", SCHEME + 1), empty.clone(), StatusCode::CONFLICT),
        (format!("/export/{design}"), b"not a document".to_vec(), StatusCode::BAD_REQUEST),
        ("/export/xyz".to_string(), empty.clone(), StatusCode::BAD_REQUEST),
        ("/rooms/nobody/export".to_string(), Vec::new(), StatusCode::NOT_FOUND),
    ] {
        let (s, msg) = post(&hub, &uri, body).await;
        assert_eq!(s, want, "{uri}: {}", String::from_utf8_lossy(&msg));
    }
    // A room nobody bound has no design to export.
    hub.room("unbound").unwrap().join(None).unwrap();
    let (s, _) = post(&hub, "/rooms/unbound/export", Vec::new()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
