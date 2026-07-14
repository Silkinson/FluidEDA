//! fluideda-core — data model + geometry, with **no** rendering or networking
//! dependencies. This isolation is the seam that lets the geometry pipeline move
//! into a Web Worker in Phase 2 without touching the renderer.
//!
//! Phase 1 makes the model **hierarchical**: `gds::load_gds_db` builds a
//! [`CellDb`] (cells + SREF/AREF instances, nothing flattened), each cell gets a
//! lazily-built R-tree ([`index::ShapeIndex`]), and [`cull::cull`] walks the
//! instance tree against a viewport to emit only what's visible. The Phase 0
//! flat path (`FlatLayout`, `tessellate`) still exists so nothing regresses.

pub mod cull;
pub mod db;
pub mod gds;
pub mod index;
pub mod tess;
pub mod transform;
pub mod types;

pub use cull::{cull, cull_counted, CellDraw, CullStats, VisibleSet};
pub use db::{Cell, CellDb, CellId, Instance, InstanceKind};
pub use index::ShapeIndex;
pub use tess::{tessellate, tessellate_shapes, LayerMesh};
pub use transform::{Bboxf, Transform};
pub use types::{Bbox, FlatLayout, Layer, Point, Polygon, Vertex};

/// Convenience (Phase 0 path): bytes -> flattened per-layer meshes + bbox.
/// Kept for the flat sample and golden-image comparisons.
pub fn load_and_tessellate(bytes: &[u8]) -> anyhow::Result<(Vec<LayerMesh>, Bbox)> {
    let layout = gds::load_gds_bytes(bytes)?;
    let bbox = layout.bbox();
    let meshes = tessellate(&layout);
    Ok((meshes, bbox))
}
