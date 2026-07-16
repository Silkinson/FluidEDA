//! GDSII loader (Phase 1): build a **hierarchical** `CellDb`.
//!
//! Unlike Phase 0 (which flattened every boundary and ignored references), we now
//! preserve the hierarchy: each `GdsStruct` becomes a `Cell`, boundaries become
//! its local shapes, and `SREF`/`AREF` become `Instance`s pointing at other
//! cells. This is what lets a 10M-shape design stay a few-KB file and render by
//! culling. Paths/boxes/text are still skipped (Phase 3+); OASIS is Phase 4.

use crate::db::{CellDb, CellDbBuilder, Instance, InstanceKind};
use crate::transform::Transform;
use crate::types::{FlatLayout, Layer, Point};
use anyhow::{anyhow, Result};
use gds21::{GdsElement, GdsLibrary, GdsStrans};

/// Parse GDSII bytes into a hierarchical cell database.
///
/// Deliberately sequenced so the two big allocations never coexist at their
/// peaks: `gds21` materialises the whole library (caravel's high-water, ~1.5 GB)
/// and we then collect it into our compact columnar cells. Finalisation —
/// chunking, which transiently copies a cell's coordinate column to reorder it —
/// runs only **after** the library is dropped. Doing it while `lib` was still
/// alive stacked the reorder copy on top of the parse high-water and pushed
/// caravel's peak RSS from 1.56 GB to 1.8 GB.
pub fn load_gds_db(bytes: &[u8]) -> Result<CellDb> {
    let lib = GdsLibrary::from_bytes(bytes.to_vec())
        .map_err(|e| anyhow!("GDS parse failed: {e:?}"))?;
    let builder = collect(&lib)?;
    drop(lib); // free the raw library before finalising (see above)
    let db = builder.build()?;
    log::info!(
        "loaded {} cell defs, top='{}', ~{} flattened shapes",
        db.cells.len(),
        db.cell(db.top).name,
        db.flat_shape_count()
    );
    Ok(db)
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
                // Paths, boxes, nodes, text: not yet drawn.
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
