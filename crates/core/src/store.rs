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
//!      `Vec` headers — so a parse worker can serialize the store to transferable
//!      `ArrayBuffer`s (`bytemuck::cast_slice`, a memcpy) and hand it to the main
//!      thread with no traversal (Phase 2b STEP 2; see `crate::serialize`).
//!
//! **Compact rectangles (Phase 7.7).** 99.8% of caravel's 3.9 M polygons are
//! axis-aligned rectangles, each a closed 5-point ring (40 B of coordinates).
//! A rectangle is now stored as its two extreme corners (16 B) plus a kind
//! byte that records exactly how the file drew it — which corner it started
//! at, which way it turned, and whether it repeated the first point — so
//! [`ShapeStore::points`] gives back the identical ring and nothing
//! downstream (exports, vertex numbering, picks) can tell the difference.
//! Any ring that does not re-expand bit for bit stays a ring.
//!
//! Coordinates stay `i32` nm throughout; nothing here casts to float except the
//! bbox helpers, which use `f64` (exact for `i32` inputs).

use crate::types::{Bbox, Layer, Point};
use bytemuck::cast_slice;
use std::sync::atomic::{AtomicBool, Ordering};

/// Store rectangles compactly (Phase 7.7). On by default; `?norects` turns it
/// off for a same-build A/B (every shape then stays a ring, as before).
static COMPACT_RECTS: AtomicBool = AtomicBool::new(true);

/// Turn compact rectangle storage on or off for stores built from now on.
pub fn set_compact_rects(on: bool) {
    COMPACT_RECTS.store(on, Ordering::Relaxed);
}

/// Kind byte of a shape stored as its ring, verbatim.
pub const KIND_RING: u8 = 0;

/// A shape's outline: borrowed from the store, or a rectangle re-expanded
/// from its corners (Phase 7.7). Derefs to the points — the closing point
/// included exactly when the file had it.
#[derive(Clone, Copy, Debug)]
pub enum Ring<'a> {
    Stored(&'a [Point]),
    Rect([Point; 5], u8),
}

impl std::ops::Deref for Ring<'_> {
    type Target = [Point];
    fn deref(&self) -> &[Point] {
        match self {
            Ring::Stored(s) => s,
            Ring::Rect(a, n) => &a[..*n as usize],
        }
    }
}

impl PartialEq<[Point]> for Ring<'_> {
    fn eq(&self, other: &[Point]) -> bool {
        **self == *other
    }
}

/// The rectangle's corners counter-clockwise from its min corner.
fn corners(x0: i32, y0: i32, x1: i32, y1: i32) -> [Point; 4] {
    [Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)]
}

/// The ring a rectangle kind stands for: kind `1 + (closed << 3 | cw << 2 |
/// start)`, with `start` the corner (counter-clockwise from the min corner)
/// the ring began at.
fn expand(kind: u8, x0: i32, y0: i32, x1: i32, y1: i32) -> ([Point; 5], u8) {
    let code = kind - 1;
    let (start, cw, closed) = ((code & 3) as usize, code & 4 != 0, code & 8 != 0);
    let c = corners(x0, y0, x1, y1);
    let mut out = [Point::new(0, 0); 5];
    for (k, slot) in out.iter_mut().take(4).enumerate() {
        let idx = if cw { (start + 4 - k) % 4 } else { (start + k) % 4 };
        *slot = c[idx];
    }
    if closed {
        out[4] = out[0];
    }
    (out, if closed { 5 } else { 4 })
}

/// If `points` is an axis-aligned rectangle with positive area, its corners
/// `[x0, y0, x1, y1]` and the kind byte that re-expands to exactly `points`.
fn rect_kind(points: &[Point]) -> Option<([i32; 4], u8)> {
    let closed = match points.len() {
        5 if points[4] == points[0] => true,
        4 => false,
        _ => return None,
    };
    let (mut x0, mut y0, mut x1, mut y1) = (points[0].x, points[0].y, points[0].x, points[0].y);
    for p in &points[..4] {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    if x0 == x1 || y0 == y1 {
        return None;
    }
    let c = corners(x0, y0, x1, y1);
    let start = c.iter().position(|&q| q == points[0])?;
    for cw in [false, true] {
        let kind = 1 + ((closed as u8) << 3 | (cw as u8) << 2 | start as u8);
        let (ring, n) = expand(kind, x0, y0, x1, y1);
        if ring[..n as usize] == *points {
            return Some(([x0, y0, x1, y1], kind));
        }
    }
    None
}

/// Shapes for one cell definition, stored as parallel columns.
///
/// Shape `i` owns the points `coords[2*starts[i] .. 2*starts[i+1]]`, the layer
/// `layers[i]` and the kind `kinds[i]`: [`KIND_RING`] for a ring stored
/// verbatim, else a rectangle stored as its min and max corners (two points)
/// that [`points`](Self::points) re-expands. `starts` always has `len() + 1`
/// entries.
pub struct ShapeStore {
    /// Interleaved x,y in nm.
    coords: Vec<i32>,
    /// Per-shape *point* offset into `coords` (so byte offset = `2 * 4 * start`).
    starts: Vec<u32>,
    /// Per-shape layer.
    layers: Vec<Layer>,
    /// Per-shape kind (Phase 7.7).
    kinds: Vec<u8>,
}

impl Default for ShapeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShapeStore {
    pub fn new() -> Self {
        Self { coords: Vec::new(), starts: vec![0], layers: Vec::new(), kinds: Vec::new() }
    }

    /// Number of shapes.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Append one shape. A rectangle is stored as its corners (Phase 7.7);
    /// any other ring is copied verbatim — including GDSII's repeated closing
    /// point — so tessellation sees exactly the ring the file specified.
    pub fn push(&mut self, layer: Layer, points: impl IntoIterator<Item = Point>) {
        let at = self.coords.len();
        for p in points {
            self.coords.push(p.x);
            self.coords.push(p.y);
        }
        let mut kind = KIND_RING;
        if COMPACT_RECTS.load(Ordering::Relaxed) {
            if let Some(([x0, y0, x1, y1], k)) = rect_kind(cast_slice(&self.coords[at..])) {
                self.coords.truncate(at);
                self.coords.extend_from_slice(&[x0, y0, x1, y1]);
                kind = k;
            }
        }
        self.kinds.push(kind);
        self.starts.push((self.coords.len() / 2) as u32);
        self.layers.push(layer);
    }

    /// Shape `i`'s ring: borrowed from the coordinate column, or a rectangle
    /// re-expanded — either way the exact points the file had.
    pub fn points(&self, i: usize) -> Ring<'_> {
        let s = self.starts[i] as usize * 2;
        let e = self.starts[i + 1] as usize * 2;
        match self.kinds[i] {
            KIND_RING => Ring::Stored(cast_slice(&self.coords[s..e])),
            kind => {
                let c = &self.coords[s..s + 4];
                let (ring, n) = expand(kind, c[0], c[1], c[2], c[3]);
                Ring::Rect(ring, n)
            }
        }
    }

    /// Shape `i` as `[x0, y0, x1, y1]` if it is a stored rectangle.
    pub fn rect(&self, i: usize) -> Option<[i32; 4]> {
        (self.kinds[i] != KIND_RING).then(|| {
            let s = self.starts[i] as usize * 2;
            [self.coords[s], self.coords[s + 1], self.coords[s + 2], self.coords[s + 3]]
        })
    }

    pub fn layer(&self, i: usize) -> Layer {
        self.layers[i]
    }

    /// The layer column — used to derive a cell's distinct layer set.
    pub fn layer_column(&self) -> &[Layer] {
        &self.layers
    }

    /// The raw columns, for zero-work serialization (Phase 2b worker
    /// transfer). Everything here is `Pod`, so a serializer casts these straight
    /// to bytes with no per-shape traversal.
    pub fn columns(&self) -> (&[i32], &[u32], &[Layer], &[u8]) {
        (&self.coords, &self.starts, &self.layers, &self.kinds)
    }

    /// The column slices a tessellation job (or a pack record) needs for chunk
    /// `start..end`.
    ///
    /// `coords`, packed `layers` and `kinds` are **borrowed** straight out of
    /// this store; only `starts` is rebased to 0 (owned, `len == n + 1`) so the
    /// consumer indexes the coord slice directly. Feeds
    /// [`crate::tessellate_columns`], which reads them exactly as
    /// [`crate::tessellate_chunk`] reads this store — so an offloaded chunk and an
    /// inline chunk tessellate identically.
    pub fn chunk_columns(&self, start: u32, end: u32) -> (&[i32], Vec<u32>, &[u32], &[u8]) {
        let (s, e) = (start as usize, end as usize);
        let base = self.starts[s];
        let coords = &self.coords[self.starts[s] as usize * 2..self.starts[e] as usize * 2];
        let starts: Vec<u32> = self.starts[s..=e].iter().map(|&x| x - base).collect();
        let layers: &[u32] = cast_slice(&self.layers[s..e]);
        (coords, starts, layers, &self.kinds[s..e])
    }

    /// Rebuild a store from its raw columns (the inverse of [`columns`]). Used to
    /// reconstruct a cell on the main thread from the worker's transferred
    /// buffers. Trusts the columns as an already-valid store (as the worker
    /// produced them); `starts` must have `layers.len() + 1` entries and end at
    /// `coords.len() / 2`, and `kinds` one entry per shape.
    pub fn from_columns(coords: Vec<i32>, starts: Vec<u32>, layers: Vec<Layer>, kinds: Vec<u8>) -> Self {
        debug_assert_eq!(starts.len(), layers.len() + 1, "starts must be layers+1");
        debug_assert_eq!(kinds.len(), layers.len(), "one kind per shape");
        debug_assert_eq!(
            starts.last().copied().unwrap_or(0) as usize * 2,
            coords.len(),
            "starts must end at coords.len()/2"
        );
        Self { coords, starts, layers, kinds }
    }

    /// Bbox of shape `i` (cell-local nm).
    pub fn bbox_of(&self, i: usize) -> Bbox {
        let mut b = Bbox::empty();
        let s = self.starts[i] as usize * 2;
        let e = self.starts[i + 1] as usize * 2;
        // A rectangle's two stored corners are its extremes.
        for pair in self.coords[s..e].chunks_exact(2) {
            b.expand(Point::new(pair[0], pair[1]));
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

    /// Heap bytes held by the columns — the number the memory findings
    /// compare against the old per-`Polygon` layout.
    pub fn heap_bytes(&self) -> usize {
        self.coords.capacity() * 4 + self.starts.capacity() * 4 + self.layers.capacity() * 4 + self.kinds.capacity()
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
            kinds: Vec::with_capacity(self.kinds.len()),
        };
        out.starts.push(0);
        for &i in order {
            let i = i as usize;
            let s = self.starts[i] as usize * 2;
            let e = self.starts[i + 1] as usize * 2;
            out.coords.extend_from_slice(&self.coords[s..e]);
            out.starts.push((out.coords.len() / 2) as u32);
            out.layers.push(self.layers[i]);
            out.kinds.push(self.kinds[i]);
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
        assert_eq!(&*s.points(0), pts(&[(0, 0), (10, 0), (10, 5)]).as_slice());
        assert_eq!(s.points(1).len(), 4);
        assert_eq!(s.layer(1), Layer::new(2, 7));
        assert_eq!(s.distinct_layers(), vec![Layer::new(1, 0), Layer::new(2, 7)]);
    }

    /// Phase 7.7: every way a file can draw a rectangle — any start corner,
    /// either winding, closed or not — comes back exactly, from 2 stored
    /// points; anything else stays a ring.
    #[test]
    fn rectangles_are_stored_as_corners_and_come_back_exactly() {
        let c = [(0, 0), (30, 0), (30, 20), (0, 20)];
        let mut s = ShapeStore::new();
        let mut want = Vec::new();
        for start in 0..4 {
            for cw in [false, true] {
                for closed in [false, true] {
                    let mut ring: Vec<Point> = (0..4)
                        .map(|k| {
                            let i = if cw { (start + 4 - k) % 4 } else { (start + k) % 4 };
                            Point::new(c[i].0 + 100, c[i].1 - 7)
                        })
                        .collect();
                    if closed {
                        ring.push(ring[0]);
                    }
                    s.push(Layer::new(1, 0), ring.clone());
                    want.push(ring);
                }
            }
        }
        // Not rectangles: a triangle, a degenerate box, a diagonal quad, a
        // ring whose closing point is not its first, an L.
        for ring in [
            pts(&[(0, 0), (10, 0), (10, 5)]),
            pts(&[(0, 0), (10, 0), (10, 0), (0, 0), (0, 0)]),
            pts(&[(0, 0), (10, 2), (12, 12), (2, 10), (0, 0)]),
            pts(&[(0, 0), (10, 0), (10, 5), (0, 5), (0, 1)]),
            pts(&[(0, 0), (10, 0), (10, 5), (5, 5), (5, 10), (0, 10), (0, 0)]),
        ] {
            s.push(Layer::new(2, 0), ring.clone());
            want.push(ring);
        }
        for (i, ring) in want.iter().enumerate() {
            assert_eq!(&*s.points(i), ring.as_slice(), "shape {i}");
            assert_eq!(s.rect(i).is_some(), i < 16, "shape {i}");
        }
        // 16 rectangles × 2 points + the 5 rings' 3 + 5 + 5 + 5 + 7 points.
        assert_eq!(s.columns().0.len() / 2, 16 * 2 + 25);
        // Bboxes and re-ordering see the same shapes.
        let b = s.bbox_of(3);
        assert_eq!((b.min.x, b.min.y, b.max.x, b.max.y), (100, -7, 130, 13));
        let order: Vec<u32> = (0..s.len() as u32).rev().collect();
        let r = s.reordered(&order);
        for (k, &i) in order.iter().enumerate() {
            assert_eq!(&*r.points(k), &*s.points(i as usize));
        }
        // Rebuilt from its columns, the store is the same.
        let (co, st, la, ki) = s.columns();
        let back = ShapeStore::from_columns(co.to_vec(), st.to_vec(), la.to_vec(), ki.to_vec());
        for i in 0..s.len() {
            assert_eq!(&*back.points(i), &*s.points(i));
        }
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
        assert_eq!(&*r.points(0), pts(&[(9, 9), (8, 9), (8, 8)]).as_slice());
        assert_eq!(r.layer(1), Layer::new(1, 0));
        assert_eq!(&*r.points(1), pts(&[(0, 0), (1, 0), (1, 1)]).as_slice());
        assert_eq!(r.layer(2), Layer::new(2, 0));
        assert_eq!(&*r.points(2), pts(&[(5, 5), (6, 5), (6, 6), (5, 6)]).as_slice());
    }
}
