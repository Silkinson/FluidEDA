//! Design packs over HTTP (Phase 6 SLICE 3): build on first request, cache,
//! and stream a chunk the way a browser does — one byte range of the
//! geometry blob.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use fluideda_server::packs::Manifest;
use fluideda_server::room::{Hub, SnapshotPolicy};
use fluideda_server::store::Store;
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

const FIXTURE: &[u8] = include_bytes!("../../assets/edit_fixture.gds");

fn hub() -> Arc<Hub> {
    let dir = std::env::temp_dir().join(format!("fluideda-packs-{}-{}", std::process::id(), fluideda_server::rand_id()));
    Arc::new(Hub::new(Store::open(dir).unwrap(), SnapshotPolicy::default()))
}

async fn get(hub: &Arc<Hub>, uri: &str, range: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    let resp = fluideda_server::app(hub.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    (status, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn a_stored_design_becomes_a_streamable_pack() {
    let hub = hub();
    let design = hub.store.put_blob(FIXTURE).unwrap();

    let (s, body) = get(&hub, &format!("/packs/{design}"), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let m: Manifest = serde_json::from_slice(&body).unwrap();
    let db = fluideda_core::load_layout_db(FIXTURE).unwrap();
    assert_eq!((m.design.as_str(), m.cells, m.ids), (design.as_str(), db.cells.len(), fluideda_core::SHAPE_ID_SCHEME));
    assert_eq!(m.chunks as u32, *fluideda_core::pack::chunk_base(&db).last().unwrap());

    // The skeleton is the design's cell table: it installs as-is.
    let (_, skeleton) = get(&hub, &format!("/blobs/{}", m.skeleton.hash), None).await;
    assert_eq!(skeleton, fluideda_core::serialize::cell_table_meta(&db, m.inline_placements));
    let (_, p_index) = get(&hub, &format!("/blobs/{}", m.placement_index.hash), None).await;
    let p_offsets = fluideda_core::pack::parse_index(&p_index).unwrap();
    assert_eq!(p_offsets.len() as u32, *fluideda_core::pack::inst_chunk_base(&db).last().unwrap() + 1);
    assert_eq!(*p_offsets.last().unwrap(), m.placements.bytes);
    let (_, index) = get(&hub, &format!("/blobs/{}", m.index.hash), None).await;
    let offsets = fluideda_core::pack::parse_index(&index).unwrap();
    assert_eq!(*offsets.last().unwrap(), m.geometry.bytes);

    // Every chunk, fetched as a byte range, holds exactly that chunk's shapes.
    let base = fluideda_core::pack::chunk_base(&db);
    for (c, cell) in db.cells.iter().enumerate() {
        for (i, ch) in cell.chunks.iter().enumerate() {
            let k = (base[c] + i as u32) as usize;
            let range = format!("bytes={}-{}", offsets[k], offsets[k + 1] - 1);
            let (s, rec) = get(&hub, &format!("/blobs/{}", m.geometry.hash), Some(&range)).await;
            assert_eq!(s, StatusCode::PARTIAL_CONTENT);
            let (coords, starts, layers) = fluideda_core::pack::decode_record(&rec).unwrap();
            let (c0, s0, l0) = cell.shapes.chunk_columns(ch.start, ch.end);
            assert_eq!((coords.as_slice(), &starts, layers.as_slice()), (c0, &s0, l0));
        }
    }

    // Built once: a second request serves the cached manifest.
    let (_, again) = get(&hub, &format!("/packs/{design}"), None).await;
    assert_eq!(serde_json::from_slice::<Manifest>(&again).unwrap(), m);
}

#[tokio::test]
async fn unknown_and_malformed_designs() {
    let hub = hub();
    let (s, _) = get(&hub, &format!("/packs/{}", "0".repeat(64)), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = get(&hub, "/packs/nope", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A blob that is not a layout fails to build (and says why).
    let junk = hub.store.put_blob(b"not a layout").unwrap();
    let (s, _) = get(&hub, &format!("/packs/{junk}"), None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
}
