//! fluideda-core — data model + geometry, with **no** rendering or networking
//! dependencies. This isolation is the seam that lets the geometry pipeline move
//! into a Web Worker in Phase 2 without touching the renderer.
//!
//! Phase 1 makes the model **hierarchical**: `gds::load_gds_db` builds a
//! [`CellDb`] (cells + SREF/AREF instances, nothing flattened), each cell gets a
//! lazily-built R-tree ([`index::ShapeIndex`]), and [`cull::cull`] walks the
//! instance tree against a viewport to emit only what's visible. The Phase 0
//! flat path (`FlatLayout`, `tessellate`) still exists so nothing regresses.
//!
//! Phase 2b makes the unit of geometry work **spatial** rather than per-cell: a
//! cell's shapes live in a columnar [`store::ShapeStore`] (plain `Pod` columns,
//! no per-shape `Vec`), partitioned by [`chunk`] into contiguous ranges, so
//! culling emits `(cell, chunk)` and only on-screen chunks are ever tessellated.
//!
//! Phase 3 adds **editing** as a copy-on-write [`edit::EditOverlay`] over the
//! immutable base: cull composites base ∪ overlay, untouched cells stay on the
//! Phase 2b path. Still no rendering or networking deps here — the overlay is
//! plain data the app mirrors into the CRDT.
//!
//! Phase 5b adds a **boolean kernel** ([`boolean`], integer `i_overlay`) and
//! rebuilds DRC on it: merge-first, edge-pair width/space, checked per spatial
//! tile so the same pure function runs inline or in a worker pool.

pub mod boolean;
pub mod chunk;
pub mod cull;
pub mod db;
pub mod drc;
pub mod edit;
pub mod flatten;
pub mod gds;
pub mod gds_stream;
pub mod gds_write;
pub mod oasis;
pub mod oasis_write;
pub mod index;
pub mod pack;
pub mod path;
pub mod pick;
pub mod serialize;
pub mod store;
pub mod tess;
pub mod transform;
pub mod types;

pub use chunk::{Chunk, ChunkId};
pub use cull::{cull, cull_counted, cull_with_overlay, CellDraw, CullStats, VisibleSet};
pub use db::{Cell, CellDb, CellId, Instance, InstanceKind};
pub use drc::{
    build_grid, cell_tiles, check_cell, check_tile, check_tile_columns, gather_tile, Deck, DrcDriver, DrcGrid,
    DrcGridBuilder, DrcKind, DrcRules, DrcStep, LayerRule, PassStats, TileInput, TileJob, TileKey, Violation,
};
pub use edit::{AddId, AddedShape, BaseShapeId, EditOverlay, PlacementKey, Shape};
pub use flatten::{flatten_hop, flatten_needs, flatten_placement, placement_counts, subtree_shapes, FLATTEN_LIMIT};
pub use gds_stream::ByteSource;
pub use gds_write::{write_gds, write_gds_to};
pub use oasis_write::{write_oasis, write_oasis_to};
pub use index::ShapeIndex;
pub use pick::{
    pick_composite, pick_composite_needs, pick_hierarchy, pick_in_cell, point_in_polygon, ChunkNeed, CompositeHit, PickHit,
    PickTarget,
};
pub use serialize::{
    deserialize, deserialize_from, serialize, skeleton_meta, ColumnSource, DbColumns, ProgressiveLoad, SkeletonParse,
};
pub use store::{set_compact_rects, Ring, ShapeStore};

/// Version of the rule that numbers each cell's shapes and placements (the
/// index in a [`BaseShapeId`] or a [`PlacementKey`]). Collaborative edits are
/// keyed by these ids, so a room records the scheme its design was parsed with
/// and refuses peers on another (Phase 6). 1: Phases 3–5 (GDS PATHs skipped).
/// 2: Phase 6 (PATHs become polygons; OASIS read). 3: Phase 7.3 (the
/// placements of cells with many are sorted spatially into placement chunks).
pub const SHAPE_ID_SCHEME: u32 = 3;
pub use tess::{
    tessellate, tessellate_chunk, tessellate_chunk_filtered, tessellate_columns, tessellate_shapes,
    LayerMesh,
};
pub use transform::{Bboxf, Transform};
pub use types::{Bbox, FlatLayout, Layer, Point, Polygon, Vertex};

/// Parse a layout file of either format into a `CellDb`: OASIS (by its magic
/// string) or GDSII (Phase 6).
pub fn load_layout_db(bytes: &[u8]) -> anyhow::Result<CellDb> {
    if oasis::is_oasis(bytes) {
        oasis::load_oasis_db(bytes)
    } else {
        gds::load_gds_db(bytes)
    }
}

/// [`load_layout_db`] from a [`ByteSource`] (Phase 7.9). GDSII is read
/// through a window, so the file never sits whole in this module's memory;
/// OASIS (compressed blocks, references by offset) is read in full first.
pub fn load_layout_db_from(src: &mut dyn ByteSource) -> anyhow::Result<CellDb> {
    let mut head = vec![0u8; (src.len() as usize).min(16)];
    src.read_at(0, &mut head)?;
    if oasis::is_oasis(&head) {
        let mut all = vec![0u8; src.len() as usize];
        src.read_at(0, &mut all)?;
        oasis::load_oasis_db(&all)
    } else {
        gds_stream::stream_gds_db_from(src)
    }
}

/// Convenience (Phase 0 path): bytes -> flattened per-layer meshes + bbox.
/// Kept for the flat sample and golden-image comparisons.
pub fn load_and_tessellate(bytes: &[u8]) -> anyhow::Result<(Vec<LayerMesh>, Bbox)> {
    let layout = gds::load_gds_bytes(bytes)?;
    let bbox = layout.bbox();
    let meshes = tessellate(&layout);
    Ok((meshes, bbox))
}
