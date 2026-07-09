//! fluideda-core — data model + geometry, with **no** rendering or networking
//! dependencies. This isolation is the seam that lets the geometry pipeline move
//! into a Web Worker in Phase 2 without touching the renderer.

pub mod gds;
pub mod tess;
pub mod types;

pub use tess::{tessellate, LayerMesh};
pub use types::{Bbox, FlatLayout, Layer, Point, Polygon, Vertex};

/// Convenience: bytes -> tessellated per-layer meshes + bbox.
pub fn load_and_tessellate(bytes: &[u8]) -> anyhow::Result<(Vec<LayerMesh>, Bbox)> {
    let layout = gds::load_gds_bytes(bytes)?;
    let bbox = layout.bbox();
    let meshes = tessellate(&layout);
    Ok((meshes, bbox))
}
