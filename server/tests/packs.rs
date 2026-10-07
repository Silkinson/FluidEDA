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
    let mut db = fluideda_core::load_layout_db(FIXTURE).unwrap();
    fluideda_core::pack::annotate_chunk_looks(&mut db);
    assert_eq!((m.design.as_str(), m.cells, m.ids), (design.as_str(), db.cells.len(), fluideda_core::SHAPE_ID_SCHEME));
    assert_eq!(m.chunks as u32, *fluideda_core::pack::chunk_base(&db).last().unwrap());
    // What "edit this placement only" decides with on a streamed client (v5).
    assert_eq!(m.placement_counts, fluideda_core::placement_counts(&db));
    assert_eq!(m.subtree_shapes, fluideda_core::subtree_shapes(&db));
    assert_eq!(m.placement_counts[db.top], 1);

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
            let (coords, starts, layers, kinds) = fluideda_core::pack::decode_record(&rec).unwrap();
            let (c0, s0, l0, k0) = cell.shapes.chunk_columns(ch.start, ch.end);
            assert_eq!((coords.as_slice(), &starts, layers.as_slice(), kinds.as_slice()), (c0, &s0, l0, k0));
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

/// Finer streamed picks: `GET /packs/<d>/pick/<k>?x=&y=` answers with exactly
/// the shapes of chunk `k` that hold the point, as an in-core pick sees them.
#[tokio::test]
async fn a_pick_answer_holds_exactly_the_shapes_under_the_point() {
    let hub = hub();
    let bytes = include_bytes!("../../assets/hier.gds");
    let design = hub.store.put_blob(bytes).unwrap();
    let (status, _) = get(&hub, &format!("/packs/{design}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let db = fluideda_core::load_layout_db(bytes).unwrap();
    let base = fluideda_core::pack::chunk_base(&db);
    let mut checked = 0;
    for (c, cell) in db.cells.iter().enumerate() {
        for (ci, ch) in cell.chunks.iter().enumerate().take(3) {
            // A point inside the chunk's first shape (its bbox centre, then
            // the first vertex nudged in if that misses).
            let b = cell.shapes.bbox_of(ch.start as usize);
            let p = fluideda_core::Point::new((b.min.x + b.max.x) / 2, (b.min.y + b.max.y) / 2);
            let want: Vec<u32> = (ch.start..ch.end)
                .filter(|&i| fluideda_core::point_in_polygon(&cell.shapes.points(i as usize), p))
                .collect();
            let k = base[c] + ci as u32;
            let (status, body) = post(&hub, &format!("/packs/{design}/pick"), fluideda_core::pack::encode_pick_questions(&[(k, p.x, p.y)])).await;
            assert_eq!(status, StatusCode::OK);
            let answers = fluideda_core::pack::split_pick_answers(&body).unwrap();
            assert_eq!(answers.len(), 1);
            let answer = fluideda_core::pack::decode_pick_answer(answers[0], ch.start).unwrap();
            assert_eq!(answer.ids, want, "cell {c} chunk {ci} at {p:?}");
            for (j, &i) in answer.ids.iter().enumerate() {
                assert_eq!(answer.store.points(j).to_vec(), cell.shapes.points(i as usize).to_vec());
            }
            checked += !want.is_empty() as usize;
        }
    }
    assert!(checked > 0, "no point hit a shape");
    // A batch answers in order, and decodes each chunk once.
    let qs = [(0, 0, 0), (0, 10, 10), (0, 5, 5)];
    let (status, body) = post(&hub, &format!("/packs/{design}/pick"), fluideda_core::pack::encode_pick_questions(&qs)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fluideda_core::pack::split_pick_answers(&body).unwrap().len(), 3);
    let (status, _) = post(&hub, &format!("/packs/{design}/pick"), fluideda_core::pack::encode_pick_questions(&[(999_999, 0, 0)])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post(&hub, &format!("/packs/{design}/pick"), vec![1, 2, 3]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn post(hub: &Arc<Hub>, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap();
    let resp = fluideda_server::app(hub.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}
