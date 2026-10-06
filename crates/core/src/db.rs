//! Hierarchical cell database — the Phase 1 replacement for `FlatLayout`.
//!
//! A `CellDb` holds cell **definitions** keyed by name. Each cell owns its local
//! shapes and a list of `Instance`s (SREF = one placement, AREF = a regular
//! `cols × rows` lattice) referring to other cells by id. Nothing is flattened:
//! a leaf cell placed a million times is stored once. The top cell is the one no
//! other cell references.
//!
//! Two derived structures live here:
//!   * `local_bbox[cell]` — union of the cell's own shapes and its children's
//!     transformed footprints, precomputed in topological order at build time
//!     (cheap: one per *definition*, not per placement).
//!   * a lazily-built [`ShapeIndex`] per cell (see `index()`), materialised on
//!     first query.

use crate::chunk::{self, Chunk};
use crate::index::ShapeIndex;
use crate::store::ShapeStore;
use crate::transform::{Bboxf, Transform};
use crate::types::{Bbox, FlatLayout, Layer, Point, Polygon};
use std::cell::OnceCell;
use std::collections::HashMap;

pub type CellId = usize;

/// How a child cell is placed: a single SREF instance or an AREF lattice.
#[derive(Debug, Clone, Copy)]
pub enum InstanceKind {
    /// One placement (SREF).
    Single,
    /// A `cols × rows` array (AREF). `col_step`/`row_step` are the per-column and
    /// per-row displacement **in the parent's local frame**, derived from the
    /// AREF's three-point `xy` (see `gds.rs`). They may be non-integer.
    Array {
        cols: i32,
        rows: i32,
        col_step: [f64; 2],
        row_step: [f64; 2],
    },
}

/// A placement of `child` into a parent cell.
///
/// **Compact (Phase 7.8).** Every one of caravel's 462,523 placements is a
/// plain 90° rotation at scale 1, yet each carried a full `f64` transform and
/// the array fields: about 104 bytes. A placement whose transform is exactly
/// what [`Transform::from_strans`] builds for an integer origin, a multiple
/// of 90° and unit magnification is now stored as `x, y` and one of 8
/// orientations; anything else keeps its transform, boxed, as does an
/// array's lattice. [`transform`](Self::transform) re-expands to the very
/// bits the reader produced, so nothing downstream can tell. ~20 bytes on
/// wasm32.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub child: CellId,
    place: Place,
    /// An AREF's lattice; `None` for a single placement.
    array: Option<Box<ArrayParams>>,
}

// Phase 7.8: the compact placement is 20 bytes on wasm32 (4 + 12 + 4).
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<Instance>() == 20);

/// An AREF's lattice (see [`InstanceKind::Array`]).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ArrayParams {
    cols: i32,
    rows: i32,
    col_step: [f64; 2],
    row_step: [f64; 2],
}

impl ArrayParams {
    fn of(kind: InstanceKind) -> Option<Box<ArrayParams>> {
        match kind {
            InstanceKind::Single => None,
            InstanceKind::Array { cols, rows, col_step, row_step } => {
                Some(Box::new(ArrayParams { cols, rows, col_step, row_step }))
            }
        }
    }
}

/// How a placement's transform is stored (Phase 7.8).
#[derive(Debug, Clone, PartialEq)]
enum Place {
    /// Exactly `Transform::from_strans(x, y, 90° × (o & 3), o & 4 != 0, 1)`.
    Manhattan { x: i32, y: i32, o: u8 },
    /// Anything else.
    General(Box<Transform>),
}

/// The linear part `[a, b, c, d]` [`Transform::from_strans`] builds for
/// orientation `o` (rotation `90° × (o & 3)`, mirrored if `o & 4`) at unit
/// magnification — computed once with that very function, so a compact
/// placement re-expands bit for bit.
fn manhattan_linear(o: u8) -> [f64; 4] {
    static TABLE: std::sync::OnceLock<[[f64; 4]; 8]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|o| {
            let t = Transform::from_strans(0.0, 0.0, 90.0 * (o & 3) as f64, o & 4 != 0, 1.0);
            [t.a, t.b, t.c, t.d]
        })
    })[o as usize & 7]
}

impl Place {
    fn of(t: &Transform) -> Place {
        let (x, y) = (t.tx as i32, t.ty as i32);
        if (x as f64).to_bits() == t.tx.to_bits() && (y as f64).to_bits() == t.ty.to_bits() {
            let lin = [t.a.to_bits(), t.b.to_bits(), t.c.to_bits(), t.d.to_bits()];
            for o in 0..8u8 {
                if manhattan_linear(o).map(f64::to_bits) == lin {
                    return Place::Manhattan { x, y, o };
                }
            }
        }
        Place::General(Box::new(*t))
    }

    fn transform(&self) -> Transform {
        match self {
            Place::Manhattan { x, y, o } => {
                let [a, b, c, d] = manhattan_linear(*o);
                Transform { a, b, c, d, tx: *x as f64, ty: *y as f64 }
            }
            Place::General(t) => **t,
        }
    }
}

impl Instance {
    /// A placement of `child` by `transform` (stored compactly when it is a
    /// plain one).
    pub fn new(child: CellId, transform: Transform, kind: InstanceKind) -> Self {
        Instance { child, place: Place::of(&transform), array: ArrayParams::of(kind) }
    }

    /// Child-local → parent-local transform for the *base* placement (the
    /// STRANS applied at the reference origin; for an AREF this is instance
    /// `[0,0]`).
    pub fn transform(&self) -> Transform {
        self.place.transform()
    }

    /// A single placement or an array.
    pub fn kind(&self) -> InstanceKind {
        match &self.array {
            None => InstanceKind::Single,
            Some(a) => InstanceKind::Array { cols: a.cols, rows: a.rows, col_step: a.col_step, row_step: a.row_step },
        }
    }

    /// The compact form of the transform, if it has one: `(x, y,
    /// orientation)` (see [`Instance`]).
    pub fn manhattan_place(&self) -> Option<(i32, i32, u8)> {
        match &self.place {
            Place::Manhattan { x, y, o } => Some((*x, *y, *o)),
            Place::General(_) => None,
        }
    }

    /// A placement whose transform is the compact `(x, y, orientation)`
    /// form — what [`manhattan_place`](Self::manhattan_place) gives back.
    pub fn from_manhattan(child: CellId, x: i32, y: i32, o: u8, kind: InstanceKind) -> Self {
        Instance { child, place: Place::Manhattan { x, y, o: o & 7 }, array: ArrayParams::of(kind) }
    }

    /// Number of concrete placements this instance expands to.
    pub fn count(&self) -> u64 {
        match &self.array {
            None => 1,
            Some(a) => (a.cols.max(0) as u64) * (a.rows.max(0) as u64),
        }
    }

    /// Transform for array element `(col, row)` (col,row = 0 for `Single`).
    /// The lattice offset is a pure translation in the parent frame, so it simply
    /// shifts the base transform's translation — the linear (rotation/mag) part
    /// is shared by every element.
    pub fn placement(&self, col: i32, row: i32) -> Transform {
        let mut t = self.transform();
        if let Some(a) = &self.array {
            t.tx += col as f64 * a.col_step[0] + row as f64 * a.row_step[0];
            t.ty += col as f64 * a.col_step[1] + row as f64 * a.row_step[1];
        }
        t
    }
}

/// A cell definition: local shapes + child instances, with lazily derived index.
pub struct Cell {
    pub name: String,
    /// This cell's own shapes, in chunk order (see `chunks`).
    pub shapes: ShapeStore,
    pub instances: Vec<Instance>,
    /// Spatial partition of `shapes`. Empty iff `shapes` is empty; a small cell
    /// gets exactly one chunk covering everything, so the draw path never needs
    /// to distinguish "chunked" from "not chunked".
    pub chunks: Vec<Chunk>,
    /// Spatial partition of `instances` (Phase 7.3): contiguous runs, each
    /// with the footprint (cell frame) of the placements it holds. Empty iff
    /// there are no instances; a cell with few placements has one chunk.
    pub inst_chunks: Vec<Chunk>,
    /// A streamed design's cell whose placements are fetched by chunk
    /// (Phase 7.3 SLICE B): `instances` stays empty and the chunks that have
    /// arrived live here. `None` for every in-core cell.
    pub remote: Option<RemotePlacements>,
    /// What each shape chunk looks like from far away (Phase 7.3 SLICE C):
    /// its layers' area coverage (/255), densest first. A streamed chunk too
    /// dense to fetch is drawn as the colour they make. Empty unless a design
    /// pack computed it (`pack::annotate_chunk_looks`).
    pub chunk_looks: Vec<Vec<(Layer, u8)>>,
    /// That colour per shape chunk, packed straight RGBA8 (0 = draw the
    /// neutral proxy). Set by the renderer's owner from `chunk_looks`, the
    /// palette and the visible layers; the cull hands it to the renderer.
    pub chunk_rgba: Vec<u32>,
    /// A streamed design's cell whose shapes are fetched by chunk (Phase
    /// 7.4): `shapes` stays empty and the chunks that are in memory live
    /// here. Read shapes through [`Cell::shape_points`] /
    /// [`Cell::chunk_store`], which serve both kinds of cell. `None` in-core.
    pub remote_shapes: Option<RemoteShapes>,
    index: OnceCell<ShapeIndex>,
}

/// The resident shape chunks of a streamed cell (Phase 7.4): chunk `i`'s
/// shapes as their own store — shape `chunks[i].start + j` of the cell is
/// shape `j` there.
#[derive(Default)]
pub struct RemoteShapes {
    pub chunks: Vec<Option<ShapeStore>>,
}

/// The resident placement chunks of a remote cell. Chunk `i` holds the
/// instances numbered `inst_chunks[i].start..end`, in order.
#[derive(Debug, Default)]
pub struct RemotePlacements {
    pub chunks: Vec<Option<Vec<Instance>>>,
}

impl Cell {
    fn new(name: String) -> Self {
        Self {
            name,
            shapes: ShapeStore::new(),
            instances: Vec::new(),
            chunks: Vec::new(),
            inst_chunks: Vec::new(),
            remote: None,
            chunk_looks: Vec::new(),
            chunk_rgba: Vec::new(),
            remote_shapes: None,
            index: OnceCell::new(),
        }
    }

    /// Mark this cell's placements as fetched by chunk, none resident yet
    /// (a streamed design's cell table, Phase 7.3 SLICE B).
    pub fn make_remote(&mut self) {
        self.remote = Some(RemotePlacements { chunks: (0..self.inst_chunks.len()).map(|_| None).collect() });
    }

    /// Mark this cell's shapes as fetched by chunk, none resident yet (a
    /// streamed design, Phase 7.4).
    pub fn make_remote_shapes(&mut self) {
        self.remote_shapes = Some(RemoteShapes { chunks: (0..self.chunks.len()).map(|_| None).collect() });
    }

    /// How many shapes the cell has, in memory or not.
    pub fn shape_count(&self) -> usize {
        match &self.remote_shapes {
            None => self.shapes.len(),
            Some(_) => self.chunks.last().map_or(0, |c| c.end as usize),
        }
    }

    /// The store holding shape chunk `ci`, if it is in memory, and the cell
    /// index of that store's shape 0 (in-core: the cell's own store, 0).
    pub fn chunk_store(&self, ci: usize) -> Option<(&ShapeStore, u32)> {
        match &self.remote_shapes {
            None => (ci < self.chunks.len()).then_some((&self.shapes, 0)),
            Some(r) => r.chunks.get(ci)?.as_ref().map(|s| (s, self.chunks[ci].start)),
        }
    }

    /// Shape `i`'s ring, if it is in memory (always, in-core).
    pub fn shape_points(&self, i: usize) -> Option<crate::store::Ring<'_>> {
        match &self.remote_shapes {
            None => (i < self.shapes.len()).then(|| self.shapes.points(i)),
            Some(_) => {
                let ci = self.chunk_of(i as u32)? as usize;
                let (store, base) = self.chunk_store(ci)?;
                Some(store.points(i - base as usize))
            }
        }
    }

    /// Shape `i`'s layer, if it is in memory.
    pub fn shape_layer(&self, i: usize) -> Option<Layer> {
        match &self.remote_shapes {
            None => (i < self.shapes.len()).then(|| self.shapes.layer(i)),
            Some(_) => {
                let ci = self.chunk_of(i as u32)? as usize;
                let (store, base) = self.chunk_store(ci)?;
                Some(store.layer(i - base as usize))
            }
        }
    }

    /// The placement chunk holding placement `ii` (chunks are contiguous runs).
    pub fn inst_chunk_of(&self, ii: u32) -> Option<u32> {
        let ci = self.inst_chunks.partition_point(|c| c.end <= ii);
        (ci < self.inst_chunks.len() && self.inst_chunks[ci].start <= ii).then_some(ci as u32)
    }

    /// How many placements the cell has, in memory or not.
    pub fn placement_count(&self) -> usize {
        match &self.remote {
            None => self.instances.len(),
            Some(_) => self.inst_chunks.last().map_or(0, |c| c.end as usize),
        }
    }

    /// Is shape chunk `ci` in memory?
    pub fn chunk_resident(&self, ci: usize) -> bool {
        self.chunk_store(ci).is_some()
    }

    /// The placements the cell has in memory, with their numbers: all of
    /// them in-core, the resident placement chunks' of a remote cell.
    pub fn resident_instances(&self) -> Box<dyn Iterator<Item = (u32, Instance)> + '_> {
        match &self.remote {
            None => Box::new(self.instances.iter().cloned().enumerate().map(|(i, x)| (i as u32, x))),
            Some(r) => Box::new(
                self.inst_chunks
                    .iter()
                    .zip(&r.chunks)
                    .filter_map(|(ch, c)| c.as_ref().map(move |v| (ch.start, v)))
                    .flat_map(|(start, v)| v.iter().cloned().enumerate().map(move |(k, x)| (start + k as u32, x))),
            ),
        }
    }

    /// Replace the cell's shape store (Phase 5c progressive load: a skeleton
    /// cell's geometry arrives after the cell is installed). Drops any cached
    /// R-tree, which was built from the old store.
    pub fn set_shapes(&mut self, shapes: ShapeStore) {
        self.shapes = shapes;
        self.index = OnceCell::new();
    }

    /// The cell's spatial index, built on first access and cached thereafter.
    pub fn index(&self) -> &ShapeIndex {
        self.index.get_or_init(|| ShapeIndex::build(&self.shapes))
    }

    /// Distinct layers among this cell's own shapes.
    pub fn layers(&self) -> Vec<Layer> {
        self.shapes.distinct_layers()
    }

    /// Split chunk `ci` into contiguous sub-chunks of at most `fine_target` shapes
    /// each, so a later single-shape edit re-tessellates only ~`fine_target` shapes
    /// instead of the whole (coarse) chunk. Phase 3 SLICE 2 incremental fix for big
    /// flat cells (caravel_core's ~35–75 k-shape chunks are too coarse for a
    /// sub-16 ms edit).
    ///
    /// Contiguous ranges ⇒ shape indices are **unchanged** (the store is not
    /// reordered), so this is safe for the CRDT (`BaseShapeId` stability) and can
    /// be applied independently on each peer. No-op if the chunk is already small
    /// enough. Reads only chunk `ci`'s shapes to bound the sub-bboxes (~1 ms for a
    /// 35 k-shape chunk), not the whole cell.
    pub fn subdivide_chunk(&mut self, ci: usize, fine_target: usize) {
        let chunk = self.chunks[ci];
        let n = (chunk.end - chunk.start) as usize;
        if n <= fine_target || fine_target == 0 {
            return;
        }
        let parts = n.div_ceil(fine_target);
        let per = (n.div_ceil(parts)).max(1) as u32;
        let mut subs = Vec::with_capacity(parts);
        let mut s = chunk.start;
        while s < chunk.end {
            let e = (s + per).min(chunk.end);
            let mut bbox = Bbox::empty();
            for i in s..e {
                let b = self.shapes.bbox_of(i as usize);
                if b.min.x > b.max.x {
                    continue; // degenerate shape
                }
                bbox.expand(b.min);
                bbox.expand(b.max);
            }
            subs.push(Chunk { start: s, end: e, bbox });
            s = e;
        }
        self.chunks.splice(ci..ci + 1, subs);
    }

    /// Which chunk owns shape `idx`. Chunks are contiguous, sorted, and cover
    /// every shape (Phase 2b invariant), so this is a binary search. Used by the
    /// editor to invalidate exactly the chunk an edit touches (Phase 3), not the
    /// whole cell — the incremental-retessellation property.
    pub fn chunk_of(&self, idx: u32) -> Option<crate::chunk::ChunkId> {
        self.chunks
            .partition_point(|c| c.end <= idx)
            .try_into()
            .ok()
            .filter(|&ci: &usize| ci < self.chunks.len() && self.chunks[ci].start <= idx)
            .map(|ci| ci as crate::chunk::ChunkId)
    }

    /// Bbox of just this cell's own shapes (cell-local nm).
    ///
    /// Folded from the chunk table rather than rescanned from the coordinate
    /// column: chunking already computed every shape's bbox, and the chunks'
    /// union is exactly this cell's own bbox. Cells have a handful of chunks, so
    /// this turns a walk over millions of points into a walk over tens of boxes.
    /// Requires `chunks` to be built first — `CellDbBuilder::build` guarantees it.
    fn own_bbox(&self) -> Bbox {
        let mut b = Bbox::empty();
        for c in &self.chunks {
            if c.bbox.min.x > c.bbox.max.x {
                continue; // chunk of only degenerate shapes
            }
            b.expand(c.bbox.min);
            b.expand(c.bbox.max);
        }
        b
    }
}

/// The hierarchical database of cell definitions.
pub struct CellDb {
    pub cells: Vec<Cell>,
    pub top: CellId,
    by_name: HashMap<String, CellId>,
    /// Cell-local bbox including children, indexed by `CellId`. Precomputed.
    local_bboxes: Vec<Bbox>,
}

impl Cell {
    /// Reconstruct a cell from already-computed parts (Phase 2b worker transfer):
    /// name, chunked shape store, instances, and both chunk tables. The R-tree
    /// index is **not** transferred — it stays a lazy `OnceCell`, rebuilt on the
    /// main thread on first query exactly as for a locally-parsed cell.
    pub fn from_parts(
        name: String,
        shapes: ShapeStore,
        instances: Vec<Instance>,
        chunks: Vec<Chunk>,
        inst_chunks: Vec<Chunk>,
    ) -> Self {
        Self { name, shapes, instances, chunks, inst_chunks, remote: None, chunk_looks: Vec::new(), chunk_rgba: Vec::new(), remote_shapes: None, index: OnceCell::new() }
    }
}

impl CellDb {
    /// Reconstruct a `CellDb` from parts the worker already computed — cells (each
    /// with its chunked store), the top id, and the precomputed local bboxes.
    /// `by_name` is cheap to rebuild from the cell names, so it is not transferred.
    /// Does **no** geometry work (no chunking, no bbox recompute): that all
    /// happened in the worker.
    pub fn from_parts(cells: Vec<Cell>, top: CellId, local_bboxes: Vec<Bbox>) -> Self {
        let by_name = cells
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        CellDb { cells, top, by_name, local_bboxes }
    }

    /// The precomputed local bbox column, for serialization (Phase 2b worker).
    pub fn local_bbox_column(&self) -> &[Bbox] {
        &self.local_bboxes
    }

    pub fn cell(&self, id: CellId) -> &Cell {
        &self.cells[id]
    }

    pub fn id_of(&self, name: &str) -> Option<CellId> {
        self.by_name.get(name).copied()
    }

    /// The precomputed cell-local bbox (own shapes ∪ child footprints).
    pub fn local_bbox(&self, id: CellId) -> Bbox {
        self.local_bboxes[id]
    }

    /// Total flattened shape count (own shapes × placement multiplicities down
    /// the tree). Reported by the bench / culling proof as the denominator — it
    /// is the number of polygons a flattening loader would emit. Uses `u64`
    /// because this can dwarf `usize` on a 32-bit wasm target.
    pub fn flat_shape_count(&self) -> u64 {
        let mut memo = vec![None; self.cells.len()];
        self.count_rec(self.top, &mut memo)
    }

    fn count_rec(&self, id: CellId, memo: &mut [Option<u64>]) -> u64 {
        if let Some(v) = memo[id] {
            return v;
        }
        let cell = &self.cells[id];
        let mut total = cell.shapes.len() as u64;
        for inst in &cell.instances {
            total = total.saturating_add(inst.count().saturating_mul(self.count_rec(inst.child, memo)));
        }
        memo[id] = Some(total);
        total
    }

    /// Flatten the whole hierarchy into a `FlatLayout` (Phase 0 compatibility and
    /// golden-image / small-file paths). Applies every transform down the tree,
    /// rounding transformed coordinates back to integer nm. **Do not** call this
    /// on a huge hierarchical file — it materialises the full flattened set; it
    /// exists so nothing that relied on `FlatLayout` regresses.
    pub fn flatten(&self) -> FlatLayout {
        let mut out = FlatLayout::default();
        self.flatten_rec(self.top, Transform::IDENTITY, &mut out);
        out
    }

    fn flatten_rec(&self, id: CellId, t: Transform, out: &mut FlatLayout) {
        let cell = &self.cells[id];
        for i in 0..cell.shapes.len() {
            let points = cell
                .shapes
                .points(i)
                .iter()
                .map(|&p| {
                    let (x, y) = t.apply_point(p);
                    Point::new(x.round() as i32, y.round() as i32)
                })
                .collect();
            out.polygons.push(Polygon {
                layer: cell.shapes.layer(i),
                points,
            });
        }
        for inst in &cell.instances {
            match inst.kind() {
                InstanceKind::Single => {
                    self.flatten_rec(inst.child, t.then(&inst.transform()), out);
                }
                InstanceKind::Array { cols, rows, .. } => {
                    for r in 0..rows {
                        for c in 0..cols {
                            self.flatten_rec(inst.child, t.then(&inst.placement(c, r)), out);
                        }
                    }
                }
            }
        }
    }

    /// World-space bbox of the whole design (top cell footprint).
    pub fn world_bbox(&self) -> Bboxf {
        Transform::IDENTITY.apply_bbox(&self.local_bbox(self.top))
    }

    /// Union of all layers appearing in any cell definition, sorted — drives the
    /// (global) layer panel and per-layer colors in the hierarchical renderer.
    pub fn all_layers(&self) -> Vec<Layer> {
        let mut ls: Vec<Layer> = self
            .cells
            .iter()
            .flat_map(|c| c.shapes.layer_column().iter().copied())
            .collect();
        ls.sort();
        ls.dedup();
        ls
    }

    /// Heap bytes held by every cell's shape columns — the parsed-geometry
    /// footprint, reported by the benches.
    pub fn store_bytes(&self) -> usize {
        self.cells.iter().map(|c| c.shapes.heap_bytes()).sum()
    }
}

/// Builder used by `gds.rs` to assemble a `CellDb`, resolving names to ids and
/// computing the top cell + local bboxes once all cells are present.
pub struct CellDbBuilder {
    cells: Vec<Cell>,
    by_name: HashMap<String, CellId>,
}

impl CellDbBuilder {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Intern a cell by name, returning its id (creating an empty def if new).
    pub fn intern(&mut self, name: &str) -> CellId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = self.cells.len();
        self.cells.push(Cell::new(name.to_string()));
        self.by_name.insert(name.to_string(), id);
        id
    }

    pub fn cell_mut(&mut self, id: CellId) -> &mut Cell {
        &mut self.cells[id]
    }

    /// Finalise: chunk the large cells, pick the top cell (referenced by
    /// nobody), then precompute each cell's local bbox in topological
    /// (children-first) order.
    pub fn build(self) -> anyhow::Result<CellDb> {
        let CellDbBuilder { mut cells, by_name } = self;
        if cells.is_empty() {
            anyhow::bail!("GDS contains no cells");
        }

        // Partition each cell's shapes spatially. This reorders the shape store,
        // so it must happen before anything derives an index or a bbox from it.
        for cell in &mut cells {
            cell.chunks = chunk::chunk_store(&mut cell.shapes);
        }

        // Referenced-by-someone set → top candidates are the complement.
        let mut referenced = vec![false; cells.len()];
        for cell in &cells {
            for inst in &cell.instances {
                referenced[inst.child] = true;
            }
        }
        let roots: Vec<CellId> = (0..cells.len()).filter(|&i| !referenced[i]).collect();
        // Prefer a root that actually carries geometry (own shapes or instances)
        // over a stray empty struct; fall back to the first root, then cell 0.
        let top = roots
            .iter()
            .copied()
            .find(|&i| !cells[i].shapes.is_empty() || !cells[i].instances.is_empty())
            .or_else(|| roots.first().copied())
            .unwrap_or(0);
        if roots.len() > 1 {
            log::warn!(
                "GDS has {} top-level (unreferenced) cells; using '{}'",
                roots.len(),
                cells[top].name
            );
        }

        let local_bboxes = compute_local_bboxes(&cells);

        // Partition each cell's placements spatially (Phase 7.3). This reorders
        // the instances of cells with many placements — placement indices
        // (`PlacementKey`) are numbered after it, hence `SHAPE_ID_SCHEME` 3.
        // Bboxes do not depend on instance order, so they are computed first.
        for cell in &mut cells {
            let boxes: Vec<Bbox> =
                cell.instances.iter().map(|inst| placement_bbox(inst, &local_bboxes[inst.child])).collect();
            let (order, inst_chunks) = chunk::chunk_instances(&boxes);
            drop(boxes);
            if let Some(order) = order {
                permute_in_place(&mut cell.instances, &order);
            }
            cell.inst_chunks = inst_chunks;
        }

        Ok(CellDb {
            cells,
            top,
            by_name,
            local_bboxes,
        })
    }
}

impl Default for CellDbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute each cell's local bbox (own shapes ∪ transformed child footprints)
/// in a children-before-parents order derived by DFS post-order over the DAG.
fn compute_local_bboxes(cells: &[Cell]) -> Vec<Bbox> {
    let n = cells.len();
    let mut bboxes = vec![Bbox::empty(); n];
    let mut done = vec![false; n];
    // Iterative post-order DFS to avoid recursion limits and to be robust to any
    // accidental cycle (a cell already on the stack is treated as bbox-empty).
    let mut on_stack = vec![false; n];
    for start in 0..n {
        if done[start] {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((id, children_done)) = stack.pop() {
            if children_done {
                // All children resolved: fold them in.
                let mut b = cells[id].own_bbox();
                for inst in &cells[id].instances {
                    let p = placement_bbox(inst, &bboxes[inst.child]);
                    if p.min.x <= p.max.x {
                        b.expand(p.min);
                        b.expand(p.max);
                    }
                }
                bboxes[id] = b;
                done[id] = true;
                on_stack[id] = false;
            } else {
                if done[id] {
                    continue;
                }
                on_stack[id] = true;
                stack.push((id, true));
                for inst in &cells[id].instances {
                    if !done[inst.child] && !on_stack[inst.child] {
                        stack.push((inst.child, false));
                    }
                }
            }
        }
    }
    bboxes
}

/// Reorder `v` so that `v[new] = old_v[order[new]]`, in place: one pass per
/// cycle of the permutation, with a bitset instead of a second copy of `v`
/// (a 50 M-placement cell is 5 GB of instances — copying it would double the
/// parse peak).
fn permute_in_place<T>(v: &mut [T], order: &[u32]) {
    debug_assert_eq!(v.len(), order.len());
    let mut done = vec![0u64; v.len().div_ceil(64)];
    for start in 0..v.len() {
        if done[start / 64] >> (start % 64) & 1 == 1 {
            continue;
        }
        // Rotate the cycle start → order[start] → order[order[start]] → … by
        // swaps: after swapping i with order[i], position i holds its final
        // element and the cycle's first element moves on to order[i].
        let mut i = start;
        loop {
            done[i / 64] |= 1 << (i % 64);
            let src = order[i] as usize;
            if src == start {
                break;
            }
            v.swap(i, src);
            i = src;
        }
    }
}

/// Instance `inst`'s footprint in its parent's frame, given its child's local
/// bbox: one placement, or an AREF's whole array (its four extreme elements
/// bound it). Empty for an empty child.
pub fn placement_bbox(inst: &Instance, child_b: &Bbox) -> Bbox {
    let mut b = Bbox::empty();
    if child_b.min.x > child_b.max.x {
        return b;
    }
    match inst.kind() {
        InstanceKind::Single => expand_by_transform(&mut b, child_b, inst.transform()),
        InstanceKind::Array { cols, rows, .. } => {
            for &(c, r) in &[(0, 0), (cols - 1, 0), (0, rows - 1), (cols - 1, rows - 1)] {
                expand_by_transform(&mut b, child_b, inst.placement(c.max(0), r.max(0)));
            }
        }
    }
    b
}

/// Expand integer bbox `b` by the transformed corners of child bbox `child_b`.
fn expand_by_transform(b: &mut Bbox, child_b: &Bbox, t: Transform) {
    let f = t.apply_bbox(child_b);
    b.expand(Point::new(f.min_x.floor() as i32, f.min_y.floor() as i32));
    b.expand(Point::new(f.max_x.ceil() as i32, f.max_y.ceil() as i32));
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 7.8: every plain orientation is stored compactly and
    /// re-expands bit for bit; anything else keeps its transform.
    #[test]
    fn plain_placements_are_compact_and_exact() {
        let bits = |t: Transform| [t.a, t.b, t.c, t.d, t.tx, t.ty].map(f64::to_bits);
        for o in 0..8u8 {
            let t = Transform::from_strans(1234.0, -5678.0, 90.0 * (o & 3) as f64, o & 4 != 0, 1.0);
            let inst = Instance::new(3, t, InstanceKind::Single);
            assert_eq!(inst.manhattan_place(), Some((1234, -5678, o)), "orientation {o}");
            assert_eq!(bits(inst.transform()), bits(t), "orientation {o}");
        }
        for t in [
            Transform::from_strans(0.0, 0.0, 45.0, false, 1.0),
            Transform::from_strans(0.0, 0.0, 90.0, false, 2.0),
            Transform::from_strans(0.5, 0.0, 0.0, false, 1.0),
            Transform::from_strans(3e9, 0.0, 0.0, false, 1.0),
            Transform::translate(5.0, 6.0), // +0.0 where from_strans has -0.0
        ] {
            let inst = Instance::new(0, t, InstanceKind::Single);
            assert_eq!(inst.manhattan_place(), None, "{t:?}");
            assert_eq!(bits(inst.transform()), bits(t));
        }
        let t = Transform::from_strans(10.0, 20.0, 180.0, true, 1.0);
        let kind = InstanceKind::Array { cols: 3, rows: 2, col_step: [7.5, 0.0], row_step: [0.0, 9.0] };
        let arr = Instance::new(1, t, kind);
        assert_eq!(arr.manhattan_place(), Some((10, 20, 6)));
        assert_eq!(arr.count(), 6);
        assert_eq!(arr.placement(2, 1).tx, 25.0);
        assert!(matches!(arr.kind(), InstanceKind::Array { cols: 3, rows: 2, .. }));
        assert!(std::mem::size_of::<Instance>() <= 32, "{} B", std::mem::size_of::<Instance>());
    }

    #[test]
    fn permuting_in_place_matches_a_gather() {
        let order: Vec<u32> = vec![3, 0, 4, 1, 2, 5, 7, 6];
        let src: Vec<u32> = (10..18).collect();
        let mut v = src.clone();
        permute_in_place(&mut v, &order);
        let want: Vec<u32> = order.iter().map(|&i| src[i as usize]).collect();
        assert_eq!(v, want);
    }
}
