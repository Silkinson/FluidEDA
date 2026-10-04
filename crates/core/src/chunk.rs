//! Spatial chunking of large cell definitions.
//!
//! The cell definition used to be the atomic unit of tessellation, which meant
//! zooming into a 5 µm window still tessellated every shape of whatever cell
//! spanned the die. On caravel that is `caravel_core`: **2.5 M shapes — 64% of
//! the whole file — in one definition covering 3194 × 4819 µm**. Chunking makes
//! the unit of work spatial instead.
//!
//! ## What gets chunked
//!
//! Only definitions with more than [`CHUNK_MIN_SHAPES`] shapes — on caravel, 4
//! of 1014 cells, holding 84% of all geometry. sky130's 435 standard cells
//! (~110 shapes each) stay whole: one chunk, the same single draw they were
//! before, so nothing regresses for the small-cell case. Both thresholds were
//! chosen by measurement; see the constants for the sweeps and the trade-offs.
//!
//! ## Assignment policy: single-owner + loose bbox
//!
//! Each shape belongs to **exactly one** chunk — the one containing its bbox
//! centre — and each chunk records the **union bbox of the shapes it actually
//! owns**, which may extend past its nominal grid rect. Selection tests that
//! union, not the grid rect (a "loose grid").
//!
//! The alternative — assigning a border-spanning shape to every chunk it
//! overlaps — was rejected because it *duplicates* geometry: the pipeline
//! alpha-blends, so a shape tessellated into two visible chunks would blend with
//! itself and render a different colour than the same file renders today. That
//! breaks the golden-image invariant. Single ownership also keeps tessellation
//! total work equal to the unchunked path (every shape tessellated exactly once)
//! and keeps a chunk a clean contiguous range.
//!
//! The cost of single ownership is that one long shape inflates its owner
//! chunk's bbox — a die-spanning power strap makes its chunk visible from
//! anywhere, so that chunk's shapes always tessellate. That is bounded (one
//! chunk's worth of work, not the cell's) and is the standard loose-grid
//! trade-off. `chunk_stats` reports the resulting inflation so the findings can
//! show what it actually costs on a real chip rather than assuming.

use crate::store::ShapeStore;
use crate::types::Bbox;

/// Index of a chunk within its cell's chunk table.
pub type ChunkId = u32;

/// Cells with more shapes than this get a chunk grid; smaller cells stay whole.
///
/// Also measured, not guessed. Chunking is only ever a *cost* at fit view (every
/// chunk of a visible cell is on screen, so each is an extra draw call buying no
/// culling), and only a *win* when a viewport sees a fraction of a cell. Native
/// caravel, 3 runs each:
///
/// | threshold | cells chunked | fit frame | leaf tess | leaf GPU |
/// |----------:|--------------:|----------:|----------:|---------:|
/// |   (none)  |             0 |   18.8 ms |   1469 ms |   141 MB |
/// |     50 k  |             9 |   22.3 ms |    11.9 ms|   1.1 MB |
/// |    100 k  |             4 |   20.0 ms |    12.0 ms|   1.1 MB |
/// |    200 k  |             3 |   21.9 ms |    72.9 ms|   7.1 MB |
/// |    500 k  |             1 |   19.1 ms |    73.5 ms|   7.1 MB |
///
/// 100 k is the pick, and the reason is structural rather than numerological.
/// Dropping from 50 k to 100 k excludes five `sky130_fd_io__*` pad cells that are
/// shape-dense (55–77 k) but spatially *tiny* (75 × 200 µm). Hierarchy culling
/// already handles a small cell — it is one bbox test — so chunking them bought
/// nothing and cost ~50 draw calls at fit. What actually defeats hierarchy
/// culling is a cell that spans the die: `caravel_core` (2.5 M shapes) and
/// `manual_power_connections` (112 k), both 3194 × 4819 µm. 100 k keeps both.
/// Going further (200 k+) drops `manual_power_connections` and leaf tessellation
/// jumps 6×.
///
/// Accepted cost: a leaf view centred *inside* one of the 55–77 k pad cells
/// tessellates it whole (~45 ms). That is a one-off, well under a frame budget's
/// worth of hitch, and worth the fit-view draw calls it saves.
pub const CHUNK_MIN_SHAPES: usize = 100_000;

/// Grid resolution is chosen to land near this many shapes per chunk.
///
/// Picked by measuring caravel across 4 k … 64 k rather than by intuition, and
/// the curve has a genuine sweet spot. Chunks buy leaf-zoom time but cost
/// fit-view time: at fit every chunk of a die-spanning cell is on screen, so
/// each one is an extra draw call for zero culling benefit. Native caravel,
/// 1000×1000, tessellation of the leaf view vs frame time at fit:
///
/// | target |  chunks @fit | leaf tess | leaf GPU | fit frame |
/// |-------:|-------------:|----------:|---------:|----------:|
/// |   4 k  |          677 |   12.0 ms |   1.1 MB |   26.2 ms |
/// |   8 k  |          420 |   11.9 ms |   1.1 MB |   24.4 ms |
/// |  16 k  |          297 |   11.8 ms |   1.1 MB |   20.4 ms |
/// |  32 k  |          239 |   29.3 ms |   2.7 MB |   21.6 ms |
/// |  64 k  |          203 |   29.1 ms |   2.7 MB |   21.1 ms |
///
/// (unchunked baseline: leaf tess 1469 ms / 141 MB, fit frame 19.4 ms)
///
/// 16 k is the knee: it keeps the finest setting's leaf cost while giving back
/// nearly all the fit-view regression. Going coarser doubles leaf work and buys
/// no further fit time.
pub const CHUNK_TARGET_SHAPES: usize = 16_000;

/// Cap on grid dimension (so at most `CHUNK_MAX_DIM²` chunks per cell). Bounds
/// the draw-call count at fit view, where every chunk of a big cell is visible.
pub const CHUNK_MAX_DIM: u32 = 32;

/// A contiguous run of shapes in a cell's store (or, for a placement chunk,
/// of its instances — Phase 7.3), plus the bbox they cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// First shape index (inclusive).
    pub start: u32,
    /// Last shape index (exclusive).
    pub end: u32,
    /// Union bbox of the shapes in `start..end`, in cell-local nm. Loose: it can
    /// exceed the grid rect this chunk came from.
    pub bbox: Bbox,
}

impl Chunk {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Partition `store` into spatial chunks, **reordering it in place** so every
/// chunk is a contiguous shape range. Returns the chunk table (never contains
/// empty chunks; empty grid squares are simply dropped).
///
/// An empty store yields an empty table. A store at or below
/// [`CHUNK_MIN_SHAPES`] yields exactly one chunk spanning it, so callers can
/// treat "unchunked" as "one chunk" with no special case.
pub fn chunk_store(store: &mut ShapeStore) -> Vec<Chunk> {
    let n = store.len();
    if n == 0 {
        return Vec::new();
    }
    if n <= CHUNK_MIN_SHAPES {
        return vec![Chunk { start: 0, end: n as u32, bbox: store.total_bbox() }];
    }

    // Every shape's bbox, computed **once**. Both the grid key and the chunks'
    // union bboxes need it, and re-deriving it per use meant walking all of a
    // 2.5 M-shape cell's coordinates three times. That is cheap enough to miss
    // natively (+0.2 s) but cost +1.3 s of wasm parse — enough to cancel out the
    // entire tessellation win this module exists to deliver. 16 B/shape of scratch
    // buys it back, and it is freed before the caller returns.
    let boxes: Vec<Bbox> = (0..n).map(|i| store.bbox_of(i)).collect();
    let mut total = Bbox::empty();
    for b in &boxes {
        if b.min.x <= b.max.x {
            total.expand(b.min);
            total.expand(b.max);
        }
    }

    // Aim for CHUNK_TARGET_SHAPES per chunk, as a square grid, capped.
    let want = n.div_ceil(CHUNK_TARGET_SHAPES);
    let dim = ((want as f64).sqrt().ceil() as u32).clamp(1, CHUNK_MAX_DIM);
    if dim <= 1 {
        return vec![Chunk { start: 0, end: n as u32, bbox: total }];
    }

    let (order, chunks) = grid_runs(&boxes, &total, dim);
    drop(boxes);
    *store = store.reordered(&order);
    chunks
}

/// The single-owner grid sort shared by shape chunks and placement chunks:
/// key each box by the grid square (`dim` × `dim` over `total`) holding its
/// centre, stable-sort by key, and return the permutation (`order[new] = old`)
/// plus one [`Chunk`] per non-empty square, each with the loose union bbox of
/// what it owns. Empty boxes (`min > max`) are sorted but add nothing to bboxes.
fn grid_runs(boxes: &[Bbox], total: &Bbox, dim: u32) -> (Vec<u32>, Vec<Chunk>) {
    let n = boxes.len();
    // Grid over the total bbox. `size()` floors at 1, so a degenerate
    // (zero-width) extent still yields a positive step rather than dividing by 0.
    let (w, h) = total.size();
    let step_x = (w / dim as f64).max(1.0);
    let step_y = (h / dim as f64).max(1.0);
    let ox = total.min.x as f64;
    let oy = total.min.y as f64;

    // Key each box by the grid square holding its centre (single owner).
    let ncells = (dim * dim) as usize;
    let mut keys: Vec<u32> = Vec::with_capacity(n);
    let mut counts: Vec<u32> = vec![0; ncells];
    for b in boxes {
        let (cx, cy) = if b.min.x <= b.max.x { b.center() } else { (ox, oy) };
        let col = (((cx - ox) / step_x).floor() as i64).clamp(0, dim as i64 - 1) as u32;
        let row = (((cy - oy) / step_y).floor() as i64).clamp(0, dim as i64 - 1) as u32;
        let k = row * dim + col;
        keys.push(k);
        counts[k as usize] += 1;
    }

    // Stable counting sort into grid-square order.
    let mut offsets: Vec<u32> = Vec::with_capacity(ncells + 1);
    let mut acc = 0u32;
    offsets.push(0);
    for &c in &counts {
        acc += c;
        offsets.push(acc);
    }
    let mut cursor = offsets.clone();
    let mut order: Vec<u32> = vec![0; n];
    for (i, &k) in keys.iter().enumerate() {
        let slot = &mut cursor[k as usize];
        order[*slot as usize] = i as u32;
        *slot += 1;
    }
    drop(keys);

    // Chunk table over the runs the sort just created, each with its loose union
    // bbox. Built from `order` (new index -> old index) so it reads the cached
    // boxes rather than re-scanning the reordered data.
    let mut chunks = Vec::new();
    for k in 0..ncells {
        let (start, end) = (offsets[k], offsets[k + 1]);
        if start == end {
            continue; // empty grid square — no chunk id spent on it
        }
        let mut bbox = Bbox::empty();
        for j in start..end {
            let b = boxes[order[j as usize] as usize];
            if b.min.x > b.max.x {
                continue; // degenerate/pointless box
            }
            bbox.expand(b.min);
            bbox.expand(b.max);
        }
        chunks.push(Chunk { start, end, bbox });
    }
    (order, chunks)
}

// --- placement chunks (Phase 7.3) --------------------------------------------
//
// The same treatment for a cell's *placements*. caravel_core places 460 k
// standard cells directly; the cull visited every one of them each frame
// (most only to find them off-screen), and an out-of-core client had to hold
// all of them before its first frame. A placement chunk is a contiguous run
// of a cell's instances plus the parent-frame bbox they cover, so the cull
// skips a whole off-screen run with one test — and a streamed design can
// fetch the runs on screen only.

/// Cells with more placements than this get a placement grid; others keep one
/// chunk spanning every instance (as shape chunks do below CHUNK_MIN_SHAPES).
pub const INST_CHUNK_MIN: usize = 4096;
/// Grid resolution aims for about this many placements per chunk — also the
/// unit a streamed design fetches (≈ 2 k × 60 B ≈ 120 KB).
pub const INST_CHUNK_TARGET: usize = 2048;
/// Upper bound on the placement grid's resolution per axis (16 k chunks, so a
/// cell with 50 M placements still gets ~3 k-placement chunks).
pub const INST_CHUNK_MAX_DIM: u32 = 128;

/// Partition a cell's placements spatially. `boxes[i]` is instance `i`'s
/// footprint in the cell's frame (an AREF's whole array; empty for an empty
/// child). Returns the permutation to apply (`order[new] = old`; identity —
/// and `None` — when the cell stays one chunk) and the chunk table (empty iff
/// there are no instances).
pub fn chunk_instances(boxes: &[Bbox]) -> (Option<Vec<u32>>, Vec<Chunk>) {
    let n = boxes.len();
    if n == 0 {
        return (None, Vec::new());
    }
    let mut total = Bbox::empty();
    for b in boxes {
        if b.min.x <= b.max.x {
            total.expand(b.min);
            total.expand(b.max);
        }
    }
    let want = n.div_ceil(INST_CHUNK_TARGET);
    let dim = ((want as f64).sqrt().ceil() as u32).clamp(1, INST_CHUNK_MAX_DIM);
    if n <= INST_CHUNK_MIN || dim <= 1 {
        return (None, vec![Chunk { start: 0, end: n as u32, bbox: total }]);
    }
    let (order, chunks) = grid_runs(boxes, &total, dim);
    (Some(order), chunks)
}

/// How much the loose bboxes overlap their ideal grid squares, for the findings.
///
/// Returns `(chunks, mean_shapes, worst_bbox_area_ratio)` where the ratio is a
/// chunk's loose bbox area divided by the nominal grid-square area — 1.0 means
/// the chunk stayed inside its square, large means a long shape stretched it.
pub fn chunk_stats(chunks: &[Chunk], total: &Bbox) -> (usize, f64, f64) {
    if chunks.is_empty() {
        return (0, 0.0, 0.0);
    }
    let shapes: usize = chunks.iter().map(|c| c.len()).sum();
    let (w, h) = total.size();
    let dim = (chunks.len() as f64).sqrt().ceil().max(1.0);
    let square = (w / dim) * (h / dim);
    let worst = chunks
        .iter()
        .map(|c| {
            let (cw, ch) = c.bbox.size();
            if square > 0.0 { cw * ch / square } else { 0.0 }
        })
        .fold(0.0f64, f64::max);
    (chunks.len(), shapes as f64 / chunks.len() as f64, worst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Layer, Point};

    /// A square of side `s` at (x,y).
    fn push_sq(store: &mut ShapeStore, x: i32, y: i32, s: i32) {
        store.push(
            Layer::new(1, 0),
            [
                Point::new(x, y),
                Point::new(x + s, y),
                Point::new(x + s, y + s),
                Point::new(x, y + s),
            ],
        );
    }

    #[test]
    fn small_cell_stays_one_chunk() {
        let mut s = ShapeStore::new();
        for i in 0..100 {
            push_sq(&mut s, i * 10, 0, 5);
        }
        let chunks = chunk_store(&mut s);
        assert_eq!(chunks.len(), 1, "a small cell must not be split");
        assert_eq!((chunks[0].start, chunks[0].end), (0, 100));
    }

    #[test]
    fn empty_cell_has_no_chunks() {
        let mut s = ShapeStore::new();
        assert!(chunk_store(&mut s).is_empty());
    }

    /// Enough shapes on a lattice to clear CHUNK_MIN_SHAPES and actually chunk.
    /// Derived from the constant so tuning the threshold can't silently turn
    /// these tests into no-ops against an unchunked cell.
    fn big_lattice() -> ShapeStore {
        let mut s = ShapeStore::new();
        let n = (CHUNK_MIN_SHAPES + 10_000) as i32;
        let side = 512;
        for i in 0..n {
            push_sq(&mut s, (i % side) * 1_000, (i / side) * 1_000, 400);
        }
        s
    }

    /// The invariant the renderer depends on: chunks tile the shape list exactly
    /// once — contiguous, non-overlapping, covering every shape. If this broke,
    /// geometry would be dropped or double-drawn.
    #[test]
    fn chunks_partition_every_shape_exactly_once() {
        let mut s = big_lattice();
        let n = s.len();
        let chunks = chunk_store(&mut s);
        assert!(chunks.len() > 1, "a cell over CHUNK_MIN_SHAPES should split");

        let mut cursor = 0u32;
        for c in &chunks {
            assert_eq!(c.start, cursor, "chunks must be contiguous with no gap");
            assert!(c.end > c.start, "no empty chunks in the table");
            cursor = c.end;
        }
        assert_eq!(cursor as usize, n, "chunks must cover every shape");
    }

    fn sq(x: i32, y: i32, s: i32) -> Bbox {
        Bbox { min: Point::new(x, y), max: Point::new(x + s, y + s) }
    }

    #[test]
    fn placement_chunks_tile_every_instance_and_contain_it() {
        // A lattice of placements big enough to chunk, plus an empty child and
        // a die-spanning macro.
        let n = INST_CHUNK_MIN + 3_000;
        let mut boxes: Vec<Bbox> = (0..n as i32).map(|i| sq((i % 128) * 5_000, (i / 128) * 3_000, 2_000)).collect();
        boxes.push(Bbox::empty());
        boxes.push(sq(0, 0, 640_000));
        let (order, chunks) = chunk_instances(&boxes);
        let order = order.expect("a cell over INST_CHUNK_MIN must reorder");
        assert!(chunks.len() > 1);
        let mut seen = vec![false; boxes.len()];
        let mut cursor = 0;
        for c in &chunks {
            assert_eq!(c.start, cursor, "contiguous");
            cursor = c.end;
            for j in c.start..c.end {
                let i = order[j as usize] as usize;
                assert!(!seen[i], "placement {i} in two chunks");
                seen[i] = true;
                let b = boxes[i];
                if b.min.x <= b.max.x {
                    assert!(b.min.x >= c.bbox.min.x && b.max.x <= c.bbox.max.x && b.min.y >= c.bbox.min.y && b.max.y <= c.bbox.max.y);
                }
            }
        }
        assert_eq!(cursor as usize, boxes.len());
        assert!(seen.iter().all(|&s| s), "every placement owned");
        // Sorting an already-sorted list changes nothing (a written-then-reread
        // file keeps its order).
        let sorted: Vec<Bbox> = order.iter().map(|&i| boxes[i as usize]).collect();
        let (again, chunks2) = chunk_instances(&sorted);
        assert!(again.unwrap().iter().enumerate().all(|(k, &v)| k as u32 == v), "idempotent");
        assert_eq!(chunks, chunks2);
    }

    #[test]
    fn few_placements_stay_one_chunk_unsorted() {
        let boxes: Vec<Bbox> = (0..100).map(|i| sq(i * 10, 0, 5)).collect();
        let (order, chunks) = chunk_instances(&boxes);
        assert!(order.is_none());
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start, chunks[0].end), (0, 100));
        assert!(chunk_instances(&[]).1.is_empty());
    }

    /// Single-owner assignment + loose bbox must never lose a shape: every shape
    /// is inside its own chunk's bbox, so any viewport showing the shape also
    /// selects the chunk holding it.
    #[test]
    fn every_shape_is_contained_by_its_chunk_bbox() {
        let mut s = big_lattice();
        // A die-spanning strap: the pathological case for a loose grid.
        s.push(
            Layer::new(2, 0),
            [
                Point::new(0, 0),
                Point::new(512_000, 0),
                Point::new(512_000, 100),
                Point::new(0, 100),
            ],
        );
        let chunks = chunk_store(&mut s);
        assert!(chunks.len() > 1, "expected this cell to chunk");
        for c in &chunks {
            for i in c.start..c.end {
                let b = s.bbox_of(i as usize);
                assert!(
                    b.min.x >= c.bbox.min.x
                        && b.min.y >= c.bbox.min.y
                        && b.max.x <= c.bbox.max.x
                        && b.max.y <= c.bbox.max.y,
                    "shape {i} escapes its chunk bbox — it would vanish when culled"
                );
            }
        }
    }
}
