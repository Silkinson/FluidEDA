//! GDSII loader (Phase 1): build a **hierarchical** `CellDb`.
//!
//! Unlike Phase 0 (which flattened every boundary and ignored references), we now
//! preserve the hierarchy: each `GdsStruct` becomes a `Cell`, boundaries become
//! its local shapes, and `SREF`/`AREF` become `Instance`s pointing at other
//! cells. This is what lets a 10M-shape design stay a few-KB file and render by
//! culling. Paths become outline polygons (Phase 6); boxes/text are skipped;
//! OASIS lives in [`crate::oasis`].
//!
//! **Parsing is streaming** (Phase 2b step 3, [`crate::gds_stream`]): a
//! hand-rolled record reader emits directly into the columnar store, so the
//! ~1.4 GB gds21 intermediate is gone. gds21 is retained only as a *test oracle*
//! (see [`load_gds_db_gds21`] + the `oracle` tests): the two paths must build
//! identical `CellDb`s.

use crate::db::{CellDb, CellDbBuilder, Instance, InstanceKind};
use crate::transform::Transform;
use crate::types::{FlatLayout, Layer, Point};
use anyhow::{anyhow, Result};
use gds21::{GdsElement, GdsLibrary, GdsStrans};

/// Parse GDSII bytes into a hierarchical cell database (streaming).
pub fn load_gds_db(bytes: &[u8]) -> Result<CellDb> {
    let db = crate::gds_stream::stream_gds_db(bytes)?;
    log::info!(
        "loaded {} cell defs, top='{}', ~{} flattened shapes",
        db.cells.len(),
        db.cell(db.top).name,
        db.flat_shape_count()
    );
    Ok(db)
}

/// Parse via gds21 (the whole-library path). **Kept only as a correctness
/// oracle** for the streaming reader — it materialises the entire `GdsLibrary`
/// before converting, which is exactly the ~1.4 GB high-water Phase 2b step 3
/// removed. Not on any app path.
pub fn load_gds_db_gds21(bytes: &[u8]) -> Result<CellDb> {
    let lib = GdsLibrary::from_bytes(bytes.to_vec())
        .map_err(|e| anyhow!("GDS parse failed: {e:?}"))?;
    let builder = collect(&lib)?;
    drop(lib);
    builder.build()
}

/// Phase 0 compatibility: parse then flatten to a `FlatLayout`. Correct for
/// small/flat files; on a large hierarchical file this materialises everything,
/// so prefer `load_gds_db` + culling there.
pub fn load_gds_bytes(bytes: &[u8]) -> Result<FlatLayout> {
    let db = load_gds_db(bytes)?;
    let flat = db.flatten();
    if flat.polygons.is_empty() {
        return Err(anyhow!("no boundary polygons found in GDS"));
    }
    log::info!(
        "flattened {} polygons across {} layers",
        flat.polygons.len(),
        flat.layers().len()
    );
    Ok(flat)
}

/// Collect the parsed library into a `CellDbBuilder` (shapes + instances only).
/// Finalisation is the caller's job, so it can drop `lib` first.
fn collect(lib: &GdsLibrary) -> Result<CellDbBuilder> {
    let mut b = CellDbBuilder::new();

    // First pass: intern every struct name so references resolve regardless of
    // declaration order (a cell may be referenced before it is defined).
    for s in &lib.structs {
        b.intern(&s.name);
    }

    // Second pass: fill in shapes + instances.
    for s in &lib.structs {
        let id = b.intern(&s.name);
        for elem in &s.elems {
            match elem {
                GdsElement::GdsBoundary(bd) => {
                    let layer = Layer::new(bd.layer as u16, bd.datatype as u16);
                    if bd.xy.len() >= 3 {
                        // Straight into the columnar store — no per-shape Vec is
                        // ever built, so the 3.9 M `Vec<Point>` headers caravel
                        // used to allocate simply never exist.
                        b.cell_mut(id)
                            .shapes
                            .push(layer, bd.xy.iter().map(|p| Point::new(p.x, p.y)));
                    }
                }
                GdsElement::GdsStructRef(sref) => {
                    let child = b.intern(&sref.name);
                    let (angle, refl, mag) = strans_params(&sref.strans);
                    let transform = Transform::from_strans(
                        sref.xy.x as f64,
                        sref.xy.y as f64,
                        angle,
                        refl,
                        mag,
                    );
                    b.cell_mut(id).instances.push(Instance {
                        child,
                        transform,
                        kind: InstanceKind::Single,
                    });
                }
                GdsElement::GdsArrayRef(aref) => {
                    let child = b.intern(&aref.name);
                    let (angle, refl, mag) = strans_params(&aref.strans);
                    // GDSII AREF three-point xy: p0 = reference (element [0,0]);
                    // p1 encodes the *total* column span, p2 the *total* row span.
                    // Per-step vectors: (p1-p0)/cols and (p2-p0)/rows. Getting this
                    // right (dividing by cols/rows, not using p1/p2 as steps) is the
                    // classic AREF bug.
                    let p0 = &aref.xy[0];
                    let p1 = &aref.xy[1];
                    let p2 = &aref.xy[2];
                    let cols = aref.cols as i32;
                    let rows = aref.rows as i32;
                    let col_step = [
                        (p1.x - p0.x) as f64 / cols.max(1) as f64,
                        (p1.y - p0.y) as f64 / cols.max(1) as f64,
                    ];
                    let row_step = [
                        (p2.x - p0.x) as f64 / rows.max(1) as f64,
                        (p2.y - p0.y) as f64 / rows.max(1) as f64,
                    ];
                    let transform =
                        Transform::from_strans(p0.x as f64, p0.y as f64, angle, refl, mag);
                    b.cell_mut(id).instances.push(Instance {
                        child,
                        transform,
                        kind: InstanceKind::Array {
                            cols,
                            rows,
                            col_step,
                            row_step,
                        },
                    });
                }
                // Phase 6: paths become their outline polygons (same helper as
                // the streaming reader, so the two stay cell-for-cell equal).
                GdsElement::GdsPath(p) => {
                    let center: Vec<Point> = p.xy.iter().map(|q| Point::new(q.x, q.y)).collect();
                    if let Some(ring) = crate::path::gds_path_polygon(
                        &center,
                        p.width.unwrap_or(0),
                        p.path_type.unwrap_or(0),
                        p.begin_extn.unwrap_or(0),
                        p.end_extn.unwrap_or(0),
                    ) {
                        b.cell_mut(id).shapes.push(Layer::new(p.layer as u16, p.datatype as u16), ring);
                    }
                }
                // Boxes, nodes, text: not drawn.
                _ => {}
            }
        }
    }

    Ok(b)
}

/// Extract (angle_deg, reflected, mag) from an optional STRANS, applying spec
/// defaults (angle 0, not reflected, mag 1).
fn strans_params(strans: &Option<GdsStrans>) -> (f64, bool, f64) {
    match strans {
        Some(s) => (
            s.angle.unwrap_or(0.0),
            s.reflected,
            s.mag.unwrap_or(1.0),
        ),
        None => (0.0, false, 1.0),
    }
}

/// Oracle tests: the streaming reader ([`crate::gds_stream`]) must build a
/// `CellDb` byte-for-byte identical to the gds21 path. This is the cheapest
/// possible correctness proof for the rewrite — a dropped or mis-parsed record
/// shows up as a diverging cell, shape, or instance here rather than as silently
/// missing geometry in the app.
#[cfg(test)]
mod oracle {
    use crate::db::{CellDb, InstanceKind};

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
    const SKY130: &str = "../../testdata/sky130_fd_sc_hd_merged.gds";

    /// Assert two `CellDb`s are identical in everything the renderer sees:
    /// cell count, top, and per cell the name, every shape (layer + points), and
    /// every instance (child + transform + array params). Both DBs are already
    /// chunked; chunking is deterministic in the shape order, so a divergence
    /// here is a real parse difference, not chunk noise.
    fn assert_same(stream: &CellDb, oracle: &CellDb, label: &str) {
        assert_eq!(stream.cells.len(), oracle.cells.len(), "{label}: cell count");
        assert_eq!(stream.top, oracle.top, "{label}: top cell id");
        for id in 0..stream.cells.len() {
            let (cs, co) = (&stream.cells[id], &oracle.cells[id]);
            assert_eq!(cs.name, co.name, "{label}: cell {id} name");
            assert_eq!(cs.shapes.len(), co.shapes.len(), "{label}: cell '{}' shape count", cs.name);
            for i in 0..cs.shapes.len() {
                assert_eq!(cs.shapes.layer(i), co.shapes.layer(i), "{label}: '{}' shape {i} layer", cs.name);
                assert_eq!(&*cs.shapes.points(i), &*co.shapes.points(i), "{label}: '{}' shape {i} points", cs.name);
            }
            assert_eq!(cs.instances.len(), co.instances.len(), "{label}: cell '{}' instance count", cs.name);
            for (j, (a, b)) in cs.instances.iter().zip(&co.instances).enumerate() {
                assert_eq!(a.child, b.child, "{label}: '{}' inst {j} child", cs.name);
                assert_eq!(a.transform, b.transform, "{label}: '{}' inst {j} transform", cs.name);
                match (a.kind, b.kind) {
                    (InstanceKind::Single, InstanceKind::Single) => {}
                    (
                        InstanceKind::Array { cols: c1, rows: r1, col_step: cs1, row_step: rs1 },
                        InstanceKind::Array { cols: c2, rows: r2, col_step: cs2, row_step: rs2 },
                    ) => {
                        assert_eq!((c1, r1), (c2, r2), "{label}: '{}' inst {j} array dims", cs.name);
                        assert_eq!((cs1, rs1), (cs2, rs2), "{label}: '{}' inst {j} array steps", cs.name);
                    }
                    _ => panic!("{label}: '{}' inst {j} kind mismatch (SREF vs AREF)", cs.name),
                }
            }
        }
    }

    #[test]
    fn streaming_matches_gds21_on_hier() {
        let stream = super::load_gds_db(HIER).unwrap();
        let oracle = super::load_gds_db_gds21(HIER).unwrap();
        assert_same(&stream, &oracle, "hier");
    }

    #[test]
    fn streaming_matches_gds21_on_sky130() {
        let Ok(bytes) = std::fs::read(SKY130) else {
            eprintln!("SKIP: {SKY130} not present (fetch testdata to enable)");
            return;
        };
        let stream = super::load_gds_db(&bytes).unwrap();
        let oracle = super::load_gds_db_gds21(&bytes).unwrap();
        assert_same(&stream, &oracle, "sky130");
    }
}
