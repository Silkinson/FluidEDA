//! Tessellation: polygons -> GPU-ready triangle buffers, grouped by layer.
//!
//! Done ONCE at load time (not per frame). Pan/zoom only changes the camera
//! uniform; these buffers are static until the geometry itself changes.

use crate::types::{FlatLayout, Layer, Vertex};
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

/// Tessellate the whole layout, one mesh per layer (sorted, = draw order).
pub fn tessellate(layout: &FlatLayout) -> Vec<LayerMesh> {
    let mut per_layer: BTreeMap<Layer, VertexBuffers<Vertex, u32>> = BTreeMap::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::non_zero();

    for poly in &layout.polygons {
        if poly.points.len() < 3 {
            continue;
        }
        let mut builder = Path::builder();
        let first = poly.points[0];
        builder.begin(point(first.x as f32, first.y as f32));
        for p in &poly.points[1..] {
            builder.line_to(point(p.x as f32, p.y as f32));
        }
        builder.close();
        let path = builder.build();

        let buffers = per_layer.entry(poly.layer).or_default();
        let mut bb = BuffersBuilder::new(buffers, VertexCtor);
        if let Err(e) = tess.tessellate_path(&path, &opts, &mut bb) {
            log::warn!("tessellation error on layer {:?}: {e:?}", poly.layer);
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
