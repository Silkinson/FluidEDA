//! The DRC tile grid — how a cell's check is cut into independent, budgetable,
//! incrementally re-checkable units (Phase 5b).
//!
//! A cell's own-shape extent is covered by a uniform grid of square tiles. Each
//! tile is checked on its own: the shapes within `reach` of its **core** are
//! merged inside the core's `reach`-inflated window and measured (see
//! [`super::check_tile`]); violation markers are clipped to the half-open core,
//! so tiles partition the plane and every violation is reported exactly once
//! (a marker straddling a tile border is reported as two pieces).
//!
//! ## Why a grid index and not the R-tree
//!
//! Every base shape is bucketed into each tile its bbox ⊕ `reach` touches, so a
//! tile's candidate list is one contiguous slice — no R-tree query. Building it
//! is two linear passes (count, fill) that can be **budgeted** across frames with
//! a cursor ([`DrcGridBuilder::step`]); the per-cell R-tree it replaces was an
//! atomic ~0.3–0.9 s lazy build on caravel_core — the one DRC block that could
//! not be split across frames. The grid depends only on the base (which never
//! mutates) and the rule deck's reach, so edits never invalidate it.
//!
//! ## Keys, range, and overlay shapes
//!
//! Tiles are addressed by integer `(tx, ty)` keys relative to the grid origin.
//! Stored buckets cover the base extent **plus `reach` on every side**, so every
//! base shape's inflated bbox lands in range, and a key outside the stored range
//! has no base shape within `reach` of it. That is exactly what an overlay shape
//! drawn beyond the cell's original extent needs: it is checked on its own
//! (virtual) tile, whose empty bucket is correct. Overlay shapes are never
//! bucketed — they are few and change constantly; the gather tests them
//! directly.

use crate::db::Cell;
use crate::types::{Bbox, Point};

/// A tile address: grid tile `(tx, ty)` relative to the grid origin, and the
/// sub-tile `(sx, sy)` within it. A grid tile whose bucket holds more than
/// the target shape count is split `k × k` (see [`DrcGrid::split`]) so that
/// shape **density**, not just area, bounds the unit of work: caravel_core's
/// uniform 110 µm tiles range from empty to 35 k shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub tx: i32,
    pub ty: i32,
    pub sx: u16,
    pub sy: u16,
}

impl TileKey {
    pub fn new(tx: i32, ty: i32, sx: u16, sy: u16) -> Self {
        TileKey { tx, ty, sx, sy }
    }
}

/// Cap on the per-axis split of one grid tile.
const MAX_SPLIT: u16 = 64;

/// Target base shapes per tile. Balances per-tile overhead (window, union setup)
/// against the latency of the unit of work — a tile is the atom both the inline
/// budget and the worker pool schedule, and the unit an edit re-checks.
pub const TILE_TARGET_SHAPES: usize = 2_000;

/// Floor on tile side, as a multiple of the deck's reach: keeps the halo (which
/// every tile re-reads from its neighbours) a small fraction of the tile.
const MIN_SIDE_REACHES: i64 = 16;

/// Absolute floor on tile side (nm).
const MIN_SIDE_NM: i64 = 2_000;

/// The finished grid for one cell (see the module note).
#[derive(Debug, Clone)]
pub struct DrcGrid {
    /// Grid origin (nm, cell-local): the min corner of tile `(0, 0)`.
    pub origin: Point,
    /// Tile side (nm).
    pub side: i32,
    /// Stored bucket range: tiles `0..nx` × `0..ny`.
    pub nx: i32,
    pub ny: i32,
    /// Shapes are bucketed by bbox ⊕ `reach` (the deck's max per-layer reach).
    pub reach: i32,
    /// Target shapes per checked (sub-)tile.
    pub target: usize,
    offsets: Vec<u32>,
    entries: Vec<u32>,
}

impl DrcGrid {
    /// A grid with no tiles (placeholder while a build finishes).
    pub fn empty() -> DrcGrid {
        DrcGrid {
            origin: Point::new(0, 0),
            side: 1,
            nx: 0,
            ny: 0,
            reach: 0,
            target: 0,
            offsets: vec![0],
            entries: Vec::new(),
        }
    }

    /// Build the whole grid at once (tests, native bench). The app uses the
    /// budgeted [`DrcGridBuilder`].
    pub fn build(cell: &Cell, reach: i32, checked: &dyn Fn(crate::types::Layer) -> bool) -> DrcGrid {
        let mut b = DrcGridBuilder::new(cell, reach, TILE_TARGET_SHAPES);
        while !b.step(cell, usize::MAX, checked) {}
        b.finish()
    }

    /// How many ways (per axis) grid tile `(tx, ty)` is split: `⌈√(n/target)⌉`
    /// for a bucket of `n` shapes, 1 for a sparse tile or a key outside the
    /// stored range.
    pub fn split(&self, tx: i32, ty: i32) -> u16 {
        let n = self.bucket_len(tx, ty);
        if n <= self.target || self.target == 0 {
            return 1;
        }
        ((n as f64 / self.target as f64).sqrt().ceil() as u16).clamp(1, MAX_SPLIT)
    }

    /// Side of a sub-tile of a tile split `k` ways (the last one may be short).
    fn sub_side(&self, k: u16) -> i64 {
        (self.side as i64 + k as i64 - 1) / k as i64
    }

    /// The half-open core rectangle `[min, max)` of tile `t` (cell-local nm).
    pub fn tile_core(&self, t: TileKey) -> Bbox {
        let x0 = self.origin.x as i64 + t.tx as i64 * self.side as i64;
        let y0 = self.origin.y as i64 + t.ty as i64 * self.side as i64;
        let k = self.split(t.tx, t.ty);
        let ss = self.sub_side(k);
        let (sx0, sy0) = (x0 + t.sx as i64 * ss, y0 + t.sy as i64 * ss);
        let side = self.side as i64;
        Bbox {
            min: Point::new(clamp_i32(sx0), clamp_i32(sy0)),
            max: Point::new(clamp_i32((sx0 + ss).min(x0 + side)), clamp_i32((sy0 + ss).min(y0 + side))),
        }
    }

    /// Stored-range tile index, or `None` for a key outside the stored range.
    fn index(&self, tx: i32, ty: i32) -> Option<usize> {
        (tx >= 0 && ty >= 0 && tx < self.nx && ty < self.ny).then(|| (ty * self.nx + tx) as usize)
    }

    fn bucket_len(&self, tx: i32, ty: i32) -> usize {
        match self.index(tx, ty) {
            Some(i) => (self.offsets[i + 1] - self.offsets[i]) as usize,
            None => 0,
        }
    }

    /// Base shape indices bucketed into `t`'s grid tile (a superset of what
    /// any of its sub-tiles needs; empty outside the stored range).
    pub fn bucket(&self, t: TileKey) -> &[u32] {
        match self.index(t.tx, t.ty) {
            Some(i) => &self.entries[self.offsets[i] as usize..self.offsets[i + 1] as usize],
            None => &[],
        }
    }

    /// The non-empty sub-tiles of grid tile `(tx, ty)`.
    fn sub_tiles(&self, tx: i32, ty: i32, out: &mut Vec<TileKey>) {
        let k = self.split(tx, ty);
        let ss = self.sub_side(k);
        for sy in 0..k {
            for sx in 0..k {
                // Guard against a degenerate trailing sub-tile (k·ss > side).
                if (sx as i64) * ss < self.side as i64 && (sy as i64) * ss < self.side as i64 {
                    out.push(TileKey::new(tx, ty, sx, sy));
                }
            }
        }
    }

    /// Tiles with any base shape within `reach` of their grid tile — the tiles
    /// a full check of the unedited cell visits.
    pub fn nonempty_tiles(&self) -> Vec<TileKey> {
        let mut out = Vec::new();
        for ty in 0..self.ny {
            for tx in 0..self.nx {
                if self.bucket_len(tx, ty) > 0 {
                    self.sub_tiles(tx, ty, &mut out);
                }
            }
        }
        out
    }

    /// Every (sub-)tile whose core lies within `reach` of `b` — the tiles whose
    /// result can change when geometry inside `b` changes (an edit's dirty set),
    /// or the tiles an overlay shape with bbox `b` must be checked on.
    pub fn tiles_near(&self, b: &Bbox) -> Vec<TileKey> {
        let r = self.reach;
        let q = Bbox {
            min: Point::new(b.min.x.saturating_sub(r), b.min.y.saturating_sub(r)),
            max: Point::new(b.max.x.saturating_add(r), b.max.y.saturating_add(r)),
        };
        let (x0, x1) = self.span(q.min.x as i64, q.max.x as i64, self.origin.x);
        let (y0, y1) = self.span(q.min.y as i64, q.max.y as i64, self.origin.y);
        let mut out = Vec::new();
        let mut subs = Vec::new();
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                subs.clear();
                self.sub_tiles(tx, ty, &mut subs);
                for &t in &subs {
                    let c = self.tile_core(t);
                    // Half-open core vs closed query box.
                    if c.min.x <= q.max.x && q.min.x < c.max.x && c.min.y <= q.max.y && q.min.y < c.max.y {
                        out.push(t);
                    }
                }
            }
        }
        out
    }

    /// Tile-coordinate span `[floor((lo−o)/side), floor((hi−o)/side)]`.
    fn span(&self, lo: i64, hi: i64, o: i32) -> (i32, i32) {
        let s = self.side as i64;
        let f = |v: i64| (v - o as i64).div_euclid(s) as i32;
        (f(lo), f(hi))
    }

    /// Bucket entries held (memory readout).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Budgeted two-pass grid build: `step` advances a shape cursor through the
/// count pass, then the fill pass; `finish` returns the grid. Each pass reads
/// every shape's points once to bound it (no per-shape scratch kept).
pub struct DrcGridBuilder {
    grid: DrcGrid,
    pass: u8,
    cursor: usize,
    counts: Vec<u32>,
}

impl DrcGridBuilder {
    /// Plan the grid for `cell`: tile side from its shape density, range = its
    /// own-shape bbox (read from the chunk table — no shape scan) ⊕ `reach`.
    pub fn new(cell: &Cell, reach: i32, target: usize) -> Self {
        let reach = reach.max(0);
        // Own-shape extent from the chunk table (union of chunk bboxes).
        let mut ext = Bbox::empty();
        for c in &cell.chunks {
            if c.bbox.min.x <= c.bbox.max.x {
                ext.expand(c.bbox.min);
                ext.expand(c.bbox.max);
            }
        }
        if ext.min.x > ext.max.x {
            ext = Bbox { min: Point::new(0, 0), max: Point::new(0, 0) };
        }
        let r = reach as i64;
        let (w, h) = (ext.max.x as i64 - ext.min.x as i64 + 2 * r, ext.max.y as i64 - ext.min.y as i64 + 2 * r);
        let n = cell.shapes.len().max(1);
        let tiles_wanted = n.div_ceil(target.max(1)).max(1) as f64;
        let floor = (MIN_SIDE_REACHES * r).max(MIN_SIDE_NM);
        let ideal = ((w as f64 * h as f64) / tiles_wanted).sqrt().ceil() as i64;
        // A cell small enough for one tile gets exactly one: side > its extent.
        let side = if tiles_wanted <= 1.0 { w.max(h) + 1 } else { ideal.max(floor) }.clamp(1, i32::MAX as i64 / 4);
        let nx = (w / side + 1).clamp(1, 1 << 15) as i32;
        let ny = (h / side + 1).clamp(1, 1 << 15) as i32;
        let origin = Point::new(clamp_i32(ext.min.x as i64 - r), clamp_i32(ext.min.y as i64 - r));
        let tiles = (nx as usize) * (ny as usize);
        DrcGridBuilder {
            grid: DrcGrid {
                origin,
                side: side as i32,
                nx,
                ny,
                reach,
                target,
                offsets: Vec::new(),
                entries: Vec::new(),
            },
            pass: 0,
            cursor: 0,
            counts: vec![0; tiles],
        }
    }

    /// Advance by up to `max_shapes` shapes. Returns `true` when the grid is
    /// complete. `checked(layer)` filters which shapes are bucketed at all (the
    /// deck's ruled layers).
    pub fn step(&mut self, cell: &Cell, max_shapes: usize, checked: &dyn Fn(crate::types::Layer) -> bool) -> bool {
        let n = cell.shapes.len();
        let end = self.cursor.saturating_add(max_shapes).min(n);
        let (nx, ny, r) = (self.grid.nx, self.grid.ny, self.grid.reach as i64);
        let (ox, oy) = (self.grid.origin.x, self.grid.origin.y);
        for i in self.cursor..end {
            if !checked(cell.shapes.layer(i)) {
                continue;
            }
            let pts = cell.shapes.points(i);
            if pts.len() < 3 {
                continue;
            }
            let mut b = Bbox::empty();
            for p in pts {
                b.expand(*p);
            }
            let (x0, x1) = self.grid.span(b.min.x as i64 - r, b.max.x as i64 + r, ox);
            let (y0, y1) = self.grid.span(b.min.y as i64 - r, b.max.y as i64 + r, oy);
            let (x0, x1) = (x0.clamp(0, nx - 1), x1.clamp(0, nx - 1));
            let (y0, y1) = (y0.clamp(0, ny - 1), y1.clamp(0, ny - 1));
            for ty in y0..=y1 {
                for tx in x0..=x1 {
                    let t = (ty * nx + tx) as usize;
                    if self.pass == 0 {
                        self.counts[t] += 1;
                    } else {
                        let slot = self.counts[t] as usize;
                        self.grid.entries[slot] = i as u32;
                        self.counts[t] += 1;
                    }
                }
            }
        }
        self.cursor = end;
        if self.cursor < n {
            return false;
        }
        if self.pass == 0 {
            // Count pass done: prefix-sum into offsets; `counts` becomes the
            // per-tile fill cursor for pass 1.
            let tiles = self.counts.len();
            let mut offsets = Vec::with_capacity(tiles + 1);
            let mut acc = 0u32;
            offsets.push(0);
            for c in &mut self.counts {
                let here = acc;
                acc += *c;
                *c = here;
                offsets.push(acc);
            }
            self.grid.offsets = offsets;
            self.grid.entries = vec![0; acc as usize];
            self.pass = 1;
            self.cursor = 0;
            return n == 0;
        }
        true
    }

    /// The finished grid (call once `step` returned `true`).
    pub fn finish(self) -> DrcGrid {
        let mut g = self.grid;
        if g.offsets.is_empty() {
            g.offsets = vec![0; (g.nx as usize) * (g.ny as usize) + 1];
        }
        g
    }

    /// Progress in `[0, 1]` (panel readout).
    pub fn progress(&self, cell: &Cell) -> f64 {
        let n = cell.shapes.len().max(1) as f64;
        (self.pass as f64 + self.cursor as f64 / n) / 2.0
    }
}
