//! Columnar shape store — one flat coordinate buffer per cell definition.
//!
//! Replaces the per-shape `Polygon { layer, points: Vec<Point> }`. Three reasons,
//! all load-bearing for Phase 2b:
//!
//!   1. **A chunk is a range.** Shapes are reordered so every spatial chunk is a
//!      contiguous `start..end` (see `chunk.rs`), which makes "tessellate only
//!      what's on screen" a slice, not a gather.
//!   2. **Per-shape `Vec` overhead disappears.** A `Vec<Point>` costs 24 B of
//!      header plus a separate allocation (rounded up to a malloc bucket) *per
//!      shape*; caravel has 3.9 M shapes, so that overhead alone ran to ~120 MB.
//!      Here a shape costs one `u32` offset + one 4-byte `Layer`.
//!   3. **It is plain bytes.** Every column is `Pod` — no pointers, no enums, no
//!      `Vec` headers — so the store can back onto a `SharedArrayBuffer` and be
//!      read by a worker and the main thread with zero copies.
//!
//! Coordinates stay `i32` nm throughout; nothing here casts to float except the
//! bbox helpers, which use `f64` (exact for `i32` inputs).

use crate::types::{Bbox, Layer, Point};
use bytemuck::cast_slice;

/// Shapes for one cell definition, stored as parallel columns.
///
/// Shape `i` owns the points `coords[2*starts[i] .. 2*starts[i+1]]` and the
/// layer `layers[i]`. `starts` always has `len() + 1` entries.
pub struct ShapeStore {
    /// Interleaved x,y in nm.
    coords: Vec<i32>,
    /// Per-shape *point* offset into `coords` (so byte offset = `2 * 4 * start`).
    starts: Vec<u32>,
    /// Per-shape layer.
    layers: Vec<Layer>,
}

impl Default for ShapeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShapeStore {
    pub fn new() -> Self {
        Self {
            coords: Vec::new(),
            starts: vec![0],
            layers: Vec::new(),
        }
    }

    /// Number of shapes.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Append one shape. Points are copied verbatim — including GDSII's repeated
    /// closing point — so tessellation sees exactly the ring the file specified.
    pub fn push(&mut self, layer: Layer, points: impl IntoIterator<Item = Point>) {
        for p in points {
            self.coords.push(p.x);
            self.coords.push(p.y);
        }
        self.starts.push((self.coords.len() / 2) as u32);
        self.layers.push(layer);
    }

    /// Shape `i`'s ring, borrowed straight out of the coordinate column.
    pub fn points(&self, i: usize) -> &[Point] {
        let s = self.starts[i] as usize * 2;
        let e = self.starts[i + 1] as usize * 2;
        cast_slice(&self.coords[s..e])
    }

    pub fn layer(&self, i: usize) -> Layer {
        self.layers[i]
    }

    /// The layer column — used to derive a cell's distinct layer set.
    pub fn layer_column(&self) -> &[Layer] {
        &self.layers
    }

    /// The three raw columns, for zero-work serialization (Phase 2b worker
    /// transfer). Everything here is `Pod`, so a serializer casts these straight
    /// to bytes with no per-shape traversal.
    pub fn columns(&self) -> (&[i32], &[u32], &[Layer]) {
        (&self.coords, &self.starts, &self.layers)
    }

    /// Rebuild a store from its raw columns (the inverse of [`columns`]). Used to
    /// reconstruct a cell on the main thread from the worker's transferred
    /// buffers. Trusts the columns as an already-valid store (as the worker
    /// produced them); `starts` must have `layers.len() + 1` entries and end at
    /// `coords.len() / 2`.
    pub fn from_columns(coords: Vec<i32>, starts: Vec<u32>, layers: Vec<Layer>) -> Self {
        debug_assert_eq!(starts.len(), layers.len() + 1, "starts must be layers+1");
        debug_assert_eq!(
            starts.last().copied().unwrap_or(0) as usize * 2,
            coords.len(),
            "starts must end at coords.len()/2"
        );
        Self { coords, starts, layers }
    }

    /// Bbox of shape `i` (cell-local nm).
    pub fn bbox_of(&self, i: usize) -> Bbox {
        let mut b = Bbox::empty();
        for &p in self.points(i) {
            b.expand(p);
        }
        b
    }

    /// Bbox of every shape in the store.
    pub fn total_bbox(&self) -> Bbox {
        let mut b = Bbox::empty();
        for pair in self.coords.chunks_exact(2) {
            b.expand(Point::new(pair[0], pair[1]));
        }
        b
    }

    /// Distinct layers present, sorted.
    pub fn distinct_layers(&self) -> Vec<Layer> {
        let mut ls = self.layers.clone();
        ls.sort_unstable();
        ls.dedup();
        ls
    }

    /// Heap bytes held by the three columns — the number the memory findings
    /// compare against the old per-`Polygon` layout.
    pub fn heap_bytes(&self) -> usize {
        self.coords.capacity() * 4 + self.starts.capacity() * 4 + self.layers.capacity() * 4
    }

    /// Rebuild the store with shapes in `order` (a permutation of `0..len()`).
    ///
    /// Used once at build time to group each chunk's shapes into a contiguous
    /// range. Transiently doubles *this cell's* coordinate column; the caller
    /// assigns over the original, so the old one is freed immediately.
    pub fn reordered(&self, order: &[u32]) -> ShapeStore {
        let mut out = ShapeStore {
            coords: Vec::with_capacity(self.coords.len()),
            starts: Vec::with_capacity(self.starts.len()),
            layers: Vec::with_capacity(self.layers.len()),
        };
        out.starts.push(0);
        for &i in order {
            let i = i as usize;
            let s = self.starts[i] as usize * 2;
            let e = self.starts[i + 1] as usize * 2;
            out.coords.extend_from_slice(&self.coords[s..e]);
            out.starts.push((out.coords.len() / 2) as u32);
            out.layers.push(self.layers[i]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(v: &[(i32, i32)]) -> Vec<Point> {
        v.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    #[test]
    fn round_trips_shapes_and_layers() {
        let mut s = ShapeStore::new();
        s.push(Layer::new(1, 0), pts(&[(0, 0), (10, 0), (10, 5)]));
        s.push(Layer::new(2, 7), pts(&[(-4, -4), (4, -4), (4, 4), (-4, 4)]));

        assert_eq!(s.len(), 2);
        assert_eq!(s.points(0), pts(&[(0, 0), (10, 0), (10, 5)]).as_slice());
        assert_eq!(s.points(1).len(), 4);
        assert_eq!(s.layer(1), Layer::new(2, 7));
        assert_eq!(s.distinct_layers(), vec![Layer::new(1, 0), Layer::new(2, 7)]);
    }

    #[test]
    fn bboxes_cover_shapes() {
        let mut s = ShapeStore::new();
        s.push(Layer::new(1, 0), pts(&[(0, 0), (10, 0), (10, 5)]));
        s.push(Layer::new(1, 0), pts(&[(-4, -8), (4, -8), (4, 4)]));

        let b0 = s.bbox_of(0);
        assert_eq!((b0.min.x, b0.min.y, b0.max.x, b0.max.y), (0, 0, 10, 5));
        let t = s.total_bbox();
        assert_eq!((t.min.x, t.min.y, t.max.x, t.max.y), (-4, -8, 10, 5));
    }

    #[test]
    fn reorder_permutes_shapes_intact() {
        let mut s = ShapeStore::new();
        s.push(Layer::new(1, 0), pts(&[(0, 0), (1, 0), (1, 1)]));
        s.push(Layer::new(2, 0), pts(&[(5, 5), (6, 5), (6, 6), (5, 6)]));
        s.push(Layer::new(3, 0), pts(&[(9, 9), (8, 9), (8, 8)]));

        // Shapes must survive a permutation byte-for-byte, including ragged
        // point counts — that is what makes a chunk a safe contiguous range.
        let r = s.reordered(&[2, 0, 1]);
        assert_eq!(r.len(), 3);
        assert_eq!(r.layer(0), Layer::new(3, 0));
        assert_eq!(r.points(0), pts(&[(9, 9), (8, 9), (8, 8)]).as_slice());
        assert_eq!(r.layer(1), Layer::new(1, 0));
        assert_eq!(r.points(1), pts(&[(0, 0), (1, 0), (1, 1)]).as_slice());
        assert_eq!(r.layer(2), Layer::new(2, 0));
        assert_eq!(r.points(2), pts(&[(5, 5), (6, 5), (6, 6), (5, 6)]).as_slice());
    }
}
