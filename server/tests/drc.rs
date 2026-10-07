//! Phase 7.5: server-side in-context DRC. The endpoint's answer equals the
//! in-context check run in-process on the same design and edits; an area
//! re-check returns what lies in that area; links and bad input are refused.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fluideda_core::{Layer, Point};
use fluideda_server::drc::Checked;
use fluideda_server::room::{Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use fluideda_sync::{Presence, Session};
use glam::Vec2;
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

const FIXTURE: &[u8] = include_bytes!("../../assets/edit_fixture.gds");

async fn post(hub: &Arc<Hub>, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap();
    let resp = fluideda_server::app(hub.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn in_context_drc_on_the_server() {
    let dir = std::env::temp_dir().join(format!("fluideda-drc-{}-{}", std::process::id(), fluideda_server::rand_id()));
    let hub = Arc::new(Hub::new(Store::open(&dir).unwrap(), SnapshotPolicy::default()));
    let design = hub.store.put_blob(FIXTURE).unwrap();
    let db = fluideda_core::load_layout_db(FIXTURE).unwrap();

    // Edits: a 50 nm sliver drawn in TOP — a blanket-deck width violation.
    let mut s = Session::new(Presence { cursor: Vec2::ZERO, name: "t".into(), color: [0.0; 4] });
    let sliver = [Point::new(-20_000, -20_000), Point::new(-19_950, -20_000), Point::new(-19_950, -18_000), Point::new(-20_000, -18_000)];
    s.add_shape(db.top, Layer::new(1, 0), &sliver);
    let state = s.state();

    let (status, body) = post(&hub, &format!("/drc/{design}?deck=blanket"), state.clone()).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let got: Checked = serde_json::from_slice(&body).unwrap();
    let (overlay, _) = s.overlay(&db);
    let want = fluideda_core::check_world(&db, &overlay, &fluideda_core::Deck::Blanket.rules(), 2);
    let key = |m: &fluideda_server::drc::Marker| (m.kind.clone(), m.layer, m.datatype, m.bbox);
    let mut want_m: Vec<_> = want
        .violations
        .iter()
        .map(|v| {
            let kind = if v.kind == fluideda_core::DrcKind::Width { "width" } else { "space" };
            (kind.to_string(), v.layer.layer, v.layer.datatype, [v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y])
        })
        .collect();
    let mut got_m: Vec<_> = got.violations.iter().map(key).collect();
    want_m.sort();
    got_m.sort();
    assert_eq!(got_m, want_m);
    assert_eq!(got.added, 1);
    let at_sliver = |m: &&(String, u16, u16, [i32; 4])| m.3[0] <= -19_950 && m.3[2] >= -20_000 && m.3[1] <= -18_000 && m.3[3] >= -20_000;
    assert!(got_m.iter().any(|m| at_sliver(&m) && m.0 == "width"), "the sliver is flagged");

    // An area re-check: only what lies there, the sliver among it.
    let (status, body) = post(&hub, &format!("/drc/{design}?deck=blanket&area=-21000,-21000,-17000,-17000"), state).await;
    assert_eq!(status, StatusCode::OK);
    let area: Checked = serde_json::from_slice(&body).unwrap();
    assert!(area.area.is_some() && area.tiles <= got.tiles);
    assert!(area.violations.iter().any(|m| m.kind == "width" && m.bbox[0] <= -19_950 && m.bbox[2] >= -20_000));

    // Bad input.
    assert_eq!(post(&hub, &format!("/drc/{design}?deck=nope"), vec![]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post(&hub, &format!("/drc/{design}?area=1,2,3"), vec![]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post(&hub, &format!("/drc/{}", "0".repeat(64)), vec![]).await.0, StatusCode::BAD_REQUEST, "an empty state does not decode");
    let _ = std::fs::remove_dir_all(&dir);
}
