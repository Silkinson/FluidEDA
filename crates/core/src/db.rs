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

use crate::index::ShapeIndex;
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
#[derive(Debug, Clone, Copy)]
pub struct Instance {
    pub child: CellId,
    /// Child-local → parent-local transform for the *base* placement (the STRANS
    /// applied at the reference origin; for an AREF this is instance `[0,0]`).
    pub transform: Transform,
    pub kind: InstanceKind,
}

impl Instance {
    /// Number of concrete placements this instance expands to.
    pub fn count(&self) -> u64 {
        match self.kind {
            InstanceKind::Single => 1,
            InstanceKind::Array { cols, rows, .. } => (cols.max(0) as u64) * (rows.max(0) as u64),
        }
    }

    /// Transform for array element `(col, row)` (col,row = 0 for `Single`).
    /// The lattice offset is a pure translation in the parent frame, so it simply
    /// shifts the base transform's translation — the linear (rotation/mag) part
    /// is shared by every element.
    pub fn placement(&self, col: i32, row: i32) -> Transform {
        match self.kind {
            InstanceKind::Single => self.transform,
            InstanceKind::Array {
                col_step, row_step, ..
            } => {
                let mut t = self.transform;
                t.tx += col as f64 * col_step[0] + row as f64 * row_step[0];
                t.ty += col as f64 * col_step[1] + row as f64 * row_step[1];
                t
            }
        }
    }
}

/// A cell definition: local shapes + child instances, with lazily derived index.
pub struct Cell {
    pub name: String,
    pub shapes: Vec<Polygon>,
    pub instances: Vec<Instance>,
    index: OnceCell<ShapeIndex>,
}

impl Cell {
    fn new(name: String) -> Self {
        Self {
            name,
            shapes: Vec::new(),
            instances: Vec::new(),
            index: OnceCell::new(),
        }
    }

    /// The cell's spatial index, built on first access and cached thereafter.
    pub fn index(&self) -> &ShapeIndex {
        self.index.get_or_init(|| ShapeIndex::build(&self.shapes))
    }

    /// Distinct layers among this cell's own shapes.
    pub fn layers(&self) -> Vec<Layer> {
        let mut ls: Vec<Layer> = self.shapes.iter().map(|p| p.layer).collect();
        ls.sort();
        ls.dedup();
        ls
    }

    /// Bbox of just this cell's own shapes (cell-local nm).
    fn own_bbox(&self) -> Bbox {
        let mut b = Bbox::empty();
        for poly in &self.shapes {
            for &p in &poly.points {
                b.expand(p);
            }
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

impl CellDb {
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
        for poly in &cell.shapes {
            let points = poly
                .points
                .iter()
                .map(|&p| {
                    let (x, y) = t.apply_point(p);
                    Point::new(x.round() as i32, y.round() as i32)
                })
                .collect();
            out.polygons.push(Polygon {
                layer: poly.layer,
                points,
            });
        }
        for inst in &cell.instances {
            match inst.kind {
                InstanceKind::Single => {
                    self.flatten_rec(inst.child, t.then(&inst.transform), out);
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
            .flat_map(|c| c.shapes.iter().map(|p| p.layer))
            .collect();
        ls.sort();
        ls.dedup();
        ls
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

    /// Finalise: pick the top cell (referenced by nobody), then precompute each
    /// cell's local bbox in topological (children-first) order.
    pub fn build(self) -> anyhow::Result<CellDb> {
        let CellDbBuilder { cells, by_name } = self;
        if cells.is_empty() {
            anyhow::bail!("GDS contains no cells");
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
                    let child_b = bboxes[inst.child];
                    if child_b.min.x > child_b.max.x {
                        continue; // empty child
                    }
                    match inst.kind {
                        InstanceKind::Single => expand_by_transform(&mut b, &child_b, inst.transform),
                        InstanceKind::Array { cols, rows, .. } => {
                            // Corners of the array = base and the far element only
                            // matter for bounds; fold the four extreme placements.
                            for &(c, r) in &[(0, 0), (cols - 1, 0), (0, rows - 1), (cols - 1, rows - 1)] {
                                let c = c.max(0);
                                let r = r.max(0);
                                expand_by_transform(&mut b, &child_b, inst.placement(c, r));
                            }
                        }
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

/// Expand integer bbox `b` by the transformed corners of child bbox `child_b`.
fn expand_by_transform(b: &mut Bbox, child_b: &Bbox, t: Transform) {
    let f = t.apply_bbox(child_b);
    b.expand(Point::new(f.min_x.floor() as i32, f.min_y.floor() as i32));
    b.expand(Point::new(f.max_x.ceil() as i32, f.max_y.ceil() as i32));
}
