//! Tessellation: polygons -> GPU-ready triangle buffers, grouped by layer.
//!
//! Done lazily, once per **(cell, chunk)** the viewport actually touches — never
//! per frame. Pan/zoom only changes the camera uniform; these buffers are static
//! until the geometry itself changes.

use crate::chunk::Chunk;
use crate::store::ShapeStore;
use crate::types::{FlatLayout, Layer, Point, Polygon, Vertex};
use lyon::math::point;
use lyon::path::Path;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertexConstructor, VertexBuffers,
};
use std::collections::BTreeMap;

/// CPU-side mesh for a single layer.
#[derive(Debug, Clone)]
pub struct LayerMesh {
    pub layer: Layer,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

struct VertexCtor;
impl FillVertexConstructor<Vertex> for VertexCtor {
    fn new_vertex(&mut self, v: lyon::tessellation::FillVertex) -> Vertex {
        let p = v.position();
        Vertex { pos: [p.x, p.y] }
    }
}

/// Tessellate the whole (flat) layout, one mesh per layer (sorted = draw order).
pub fn tessellate(layout: &FlatLayout) -> Vec<LayerMesh> {
    tessellate_shapes(&layout.polygons)
}

/// Tessellate a slice of polygons (the Phase 0 flat path).
pub fn tessellate_shapes(polygons: &[Polygon]) -> Vec<LayerMesh> {
    tessellate_iter(polygons.iter().map(|p| (p.layer, p.points.as_slice())))
}

/// Tessellate one chunk of a cell definition — the Phase 2b unit of work.
///
/// Shapes stay in **cell-local** nm, so the resulting buffer is reused across
/// every instance of the cell and only the per-instance transform differs.
/// Because chunking reorders the store so a chunk is a contiguous range, this is
/// a slice of the cell's shapes, not a gather.
pub fn tessellate_chunk(store: &ShapeStore, chunk: &Chunk) -> Vec<LayerMesh> {
    tessellate_iter(
        (chunk.start as usize..chunk.end as usize).map(|i| (store.layer(i), store.points(i))),
    )
}

/// Like [`tessellate_chunk`] but **skips** the shape indices in `suppressed`
/// (Phase 3). This is how the overlay's `deleted` set removes a base shape
/// without mutating the base column: the chunk containing it is simply
/// re-tessellated without it. Only chunks of edited cells ever take this path;
/// everything else uses the plain [`tessellate_chunk`].
pub fn tessellate_chunk_filtered(
    store: &ShapeStore,
    chunk: &Chunk,
    suppressed: &std::collections::HashSet<u32>,
) -> Vec<LayerMesh> {
    tessellate_iter(
        (chunk.start as usize..chunk.end as usize)
            .filter(|i| !suppressed.contains(&(*i as u32)))
            .map(|i| (store.layer(i), store.points(i))),
    )
}

/// One `LayerMesh` per distinct layer, sorted (= draw order).
fn tessellate_iter<'a>(shapes: impl Iterator<Item = (Layer, &'a [Point])>) -> Vec<LayerMesh> {
    let mut per_layer: BTreeMap<Layer, VertexBuffers<Vertex, u32>> = BTreeMap::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::non_zero();

    for (layer, points) in shapes {
        if points.len() < 3 {
            continue;
        }
        let mut builder = Path::builder();
        let first = points[0];
        builder.begin(point(first.x as f32, first.y as f32));
        for p in &points[1..] {
            builder.line_to(point(p.x as f32, p.y as f32));
        }
        builder.close();
        let path = builder.build();

        let buffers = per_layer.entry(layer).or_default();
        let mut bb = BuffersBuilder::new(buffers, VertexCtor);
        if let Err(e) = tess.tessellate_path(&path, &opts, &mut bb) {
            log::warn!("tessellation error on layer {layer:?}: {e:?}");
        }
    }

    per_layer
        .into_iter()
        .map(|(layer, buf)| LayerMesh {
            layer,
            vertices: buf.vertices,
            indices: buf.indices,
        })
        .collect()
}
