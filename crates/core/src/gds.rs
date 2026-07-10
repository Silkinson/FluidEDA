//! GDSII loader (Phase 0): flatten the top cell's boundary elements into a
//! single polygon list. Cell references (SREF/AREF) and paths are intentionally
//! ignored here — hierarchy arrives in Phase 1. OASIS is Phase 4.

use crate::types::{FlatLayout, Layer, Point, Polygon};
use anyhow::{anyhow, Result};
use gds21::{GdsElement, GdsLibrary};

/// Parse GDSII bytes into a flat layout.
///
/// We walk every structure (cell) and collect `Boundary` elements. In a real
/// file the "top" cell references the others; flattening every boundary is a
/// deliberate Phase 0 shortcut that renders correctly for small/flat samples.
pub fn load_gds_bytes(bytes: &[u8]) -> Result<FlatLayout> {
    // gds21 0.2: `from_bytes` takes an owned `Vec<u8>` and returns a GdsResult.
    let lib = GdsLibrary::from_bytes(bytes.to_vec())
        .map_err(|e| anyhow!("GDS parse failed: {e:?}"))?;
    load_library(&lib)
}

fn load_library(lib: &GdsLibrary) -> Result<FlatLayout> {
    let mut out = FlatLayout::default();

    for cell in &lib.structs {
        for elem in &cell.elems {
            if let GdsElement::GdsBoundary(b) = elem {
                let layer = Layer::new(b.layer as u16, b.datatype as u16);
                // gds21 0.2: `xy` is a Vec<GdsPoint> { x: i32, y: i32 }, with the
                // closing point duplicated per spec (harmless for tessellation).
                let pts: Vec<Point> =
                    b.xy.iter().map(|p| Point::new(p.x, p.y)).collect();
                if pts.len() >= 3 {
                    out.polygons.push(Polygon { layer, points: pts });
                }
            }
        }
    }

    if out.polygons.is_empty() {
        return Err(anyhow!("no boundary polygons found in GDS"));
    }
    log::info!(
        "loaded {} polygons across {} layers",
        out.polygons.len(),
        out.layers().len()
    );
    Ok(out)
}
