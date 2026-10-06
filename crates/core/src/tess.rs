//! Tessellation: polygons -> GPU-ready triangle buffers, grouped by layer.
//!
//! Done lazily, once per **(cell, chunk)** the viewport actually touches — never
//! per frame. Pan/zoom only changes the camera uniform; these buffers are static
//! until the geometry itself changes.

use crate::chunk::Chunk;
use crate::store::ShapeStore;
use crate::types::{FlatLayout, Layer, Point, Polygon, Vertex};
use bytemuck::cast_slice;
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
    tessellate_iter(polygons.iter().map(|p| (p.layer, Outline::Ring(p.points.as_slice()))))
}

/// What a shape gives the tessellator: a ring for lyon, or a stored
/// rectangle's corners `[x0, y0, x1, y1]`, which become two triangles with no
/// lyon work at all (Phase 7.7). The two triangles cover exactly the pixels
/// lyon's would: the same four corners, and whichever diagonal splits them,
/// each pixel inside is drawn once.
#[derive(Clone, Copy)]
enum Outline<'a> {
    Ring(&'a [Point]),
    Rect([i32; 4]),
}

/// Shape `i` of `store` as the tessellator takes it.
fn outline(store: &ShapeStore, i: usize) -> Outline<'_> {
    match store.rect(i) {
        Some(r) => Outline::Rect(r),
        None => match store.points(i) {
            crate::store::Ring::Stored(s) => Outline::Ring(s),
            crate::store::Ring::Rect(..) => unreachable!("rect() covers every rectangle"),
        },
    }
}

/// Tessellate one chunk of a cell definition — the Phase 2b unit of work.
///
/// Shapes stay in **cell-local** nm, so the resulting buffer is reused across
/// every instance of the cell and only the per-instance transform differs.
/// Because chunking reorders the store so a chunk is a contiguous range, this is
/// a slice of the cell's shapes, not a gather.
pub fn tessellate_chunk(store: &ShapeStore, chunk: &Chunk) -> Vec<LayerMesh> {
    tessellate_iter((chunk.start as usize..chunk.end as usize).map(|i| (store.layer(i), outline(store, i))))
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
            .map(|i| (store.layer(i), outline(store, i))),
    )
}

/// Tessellate a chunk straight from its raw **columns** — the entry the Phase 4
/// tessellation worker calls off the main thread.
///
/// `coords` is interleaved x,y nm; `starts` is 0-based per-shape point offsets
/// (`len == n_shapes + 1`, `starts[0] == 0`); `layers` is one packed layer per
/// shape (`layer | datatype << 16` — the little-endian byte layout of [`Layer`],
/// see `crate::serialize`); `kinds` is the store's kind column (a rectangle's
/// points are its two corners). Produces the exact same per-layer meshes
/// [`tessellate_chunk`] would for those shapes, so the worker path and the inline
/// path are byte-for-byte interchangeable. Pure geometry — core stays
/// wgpu/network-free, so the worker links it directly.
pub fn tessellate_columns(coords: &[i32], starts: &[u32], layers: &[u32], kinds: &[u8]) -> Vec<LayerMesh> {
    // Two Pod casts, not per-shape unpacks: [i32;2] == Point, and a packed layer
    // u32 has Layer's exact LE bytes (see `serialize::pack_layer`).
    let pts: &[Point] = cast_slice(coords);
    let layers: &[Layer] = cast_slice(layers);
    tessellate_iter(layers.iter().enumerate().map(|(i, &layer)| {
        let (s, e) = (starts[i] as usize, starts[i + 1] as usize);
        let shape = match kinds[i] {
            crate::store::KIND_RING => Outline::Ring(&pts[s..e]),
            _ => Outline::Rect([pts[s].x, pts[s].y, pts[s + 1].x, pts[s + 1].y]),
        };
        (layer, shape)
    }))
}

/// One `LayerMesh` per distinct layer, sorted (= draw order).
fn tessellate_iter<'a>(shapes: impl Iterator<Item = (Layer, Outline<'a>)>) -> Vec<LayerMesh> {
    let mut per_layer: BTreeMap<Layer, VertexBuffers<Vertex, u32>> = BTreeMap::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::non_zero();

    for (layer, shape) in shapes {
        let points = match shape {
            Outline::Rect([x0, y0, x1, y1]) => {
                let buffers = per_layer.entry(layer).or_default();
                let base = buffers.vertices.len() as u32;
                for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
                    buffers.vertices.push(Vertex { pos: [x as f32, y as f32] });
                }
                buffers.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                continue;
            }
            Outline::Ring(points) => points,
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ShapeStore;

    /// The tessellation worker (`tessellate_columns`) and the inline path
    /// (`tessellate_chunk`) must produce identical meshes for the same shapes —
    /// they are used interchangeably per chunk (Phase 4 STEP 2), so any divergence
    /// would make an offloaded chunk render differently from an inline one.
    #[test]
    fn columns_match_chunk() {
        let mut store = ShapeStore::new();
        store.push(Layer::new(1, 0), [Point::new(0, 0), Point::new(100, 0), Point::new(100, 80), Point::new(0, 80)]);
        store.push(Layer::new(5, 2), [Point::new(200, 10), Point::new(260, 10), Point::new(230, 90)]);
        store.push(Layer::new(1, 0), [Point::new(-50, -50), Point::new(-10, -50), Point::new(-10, -20), Point::new(-50, -20)]);
        let chunk = Chunk { start: 0, end: store.len() as u32, bbox: store.total_bbox() };

        let inline = tessellate_chunk(&store, &chunk);
        let (coords, starts, layers, kinds) = store.chunk_columns(chunk.start, chunk.end);
        let via_columns = tessellate_columns(coords, &starts, layers, kinds);

        assert_eq!(inline.len(), via_columns.len(), "layer-mesh count");
        for (a, b) in inline.iter().zip(&via_columns) {
            assert_eq!(a.layer, b.layer, "layer");
            let (av, bv): (&[u8], &[u8]) = (cast_slice(&a.vertices), cast_slice(&b.vertices));
            assert_eq!(av, bv, "vertices for {:?}", a.layer);
            assert_eq!(a.indices, b.indices, "indices for {:?}", a.layer);
        }
    }

    /// A sub-range (one chunk of several) tessellates exactly the shapes in the
    /// range — `chunk_columns`' rebased `starts` must line up with the coord slice.
    #[test]
    fn columns_respect_a_sub_range() {
        let mut store = ShapeStore::new();
        for i in 0..6 {
            let x = i * 100;
            store.push(Layer::new(1, 0), [Point::new(x, 0), Point::new(x + 40, 0), Point::new(x + 40, 40)]);
        }
        // Shapes [2, 5) only.
        let chunk = Chunk { start: 2, end: 5, bbox: crate::types::Bbox::empty() };
        let inline = tessellate_chunk(&store, &chunk);
        let (coords, starts, layers, kinds) = store.chunk_columns(2, 5);
        let via_columns = tessellate_columns(coords, &starts, layers, kinds);
        assert_eq!(inline.len(), via_columns.len());
        for (a, b) in inline.iter().zip(&via_columns) {
            let (av, bv): (&[u8], &[u8]) = (cast_slice(&a.vertices), cast_slice(&b.vertices));
            assert_eq!(av, bv);
            assert_eq!(a.indices, b.indices);
        }
    }
}
