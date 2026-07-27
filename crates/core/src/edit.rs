//! Editing overlay — the Phase 3 copy-on-write layer over the immutable base.
//!
//! ## Why an overlay, not in-place mutation
//!
//! The Phase 2b base store (`ShapeStore`) is immutable `Pod` columns with a
//! bulk-loaded R-tree. Two things depend on that immutability: the worker transfer
//! (Phase 2b STEP 2 ships the columns as plain bytes) and the golden-image match
//! (the base renders byte-identical across changes). So editing never touches the
//! base columns. Instead every edit is expressed here, and the cull path
//! *composites* base ∪ overlay at draw time.
//!
//! ## The three kinds of edit
//!
//!   * **delete** — tombstone a base shape ([`deleted`](EditOverlay::deleted)).
//!   * **move** — copy-on-write: tombstone the base shape *and* write a translated
//!     copy into [`edited`](EditOverlay::edited). A move is deliberately NOT an
//!     in-place coordinate rewrite: doing that would "work" for a translate while
//!     building none of the tombstone+add machinery that delete and future
//!     add/vertex-edit need. Proving that path is the whole point of the skeleton.
//!   * **add** — a brand-new shape ([`added`](EditOverlay::added)); modeled here
//!     but not produced by the move/delete skeleton.
//!
//! ## Compositing
//!
//! A shape is *suppressed* if its id is in `deleted` (which includes the base half
//! of every move). The composite the renderer draws is:
//!
//! > (every base shape whose id is NOT suppressed) ∪ (every `edited` value) ∪ `added`
//!
//! Cull applies this — see [`crate::cull`]. Crucially, only cells that actually
//! carry an overlay entry pay any compositing cost; [`edits_cell`](EditOverlay::edits_cell)
//! is the flag that keeps untouched cells on the exact Phase 2b path.
//!
//! ## Scope cuts (Phase 3 skeleton — restore later)
//!
//!   * Only the **TOP cell** is edited, so overlay geometry is emitted at identity
//!     (TOP has no transform stack above it). Editing a cell placed under a
//!     transform, or one AREF instance vs. the cell master, is deferred.
//!   * Move (translate) + delete only. No vertex edit, booleans, or new-shape draw.
//!
//! ## Non-goal, but do not block it
//!
//! Exporting edits back to GDS means flattening base+overlay. Not built here — but
//! the overlay carries the owning `CellId` on every entry (rather than assuming
//! TOP) precisely so a flattening exporter has a coordinate frame to work in.

use crate::db::CellId;
use crate::types::{Layer, Point, Polygon};
use std::collections::{HashMap, HashSet};

/// Identifies one base shape: `(cell, index into that cell's ShapeStore)`.
///
/// The index is stable for a given parse of a given file — chunk ordering is
/// deterministic — so two collaborators who load the same GDS agree on ids
/// without exchanging them. That is what lets a move/delete be a tiny CRDT op
/// keyed by `(cell, index)` rather than by shipping geometry.
pub type BaseShapeId = (CellId, u32);

/// An overlay polygon (a moved copy or an added shape), in its cell's local nm.
/// Aliased to [`Polygon`] so the existing tessellator consumes it directly.
pub type Shape = Polygon;

/// The copy-on-write editing overlay. Empty overlay ⇒ the scene renders exactly
/// as the base (the golden invariant).
#[derive(Default, Clone)]
pub struct EditOverlay {
    /// Brand-new shapes and their owning cell. (The prompt models this as
    /// `Vec<Shape>`; we carry the `CellId` too so added geometry has a coordinate
    /// frame and a home for GDS export — see the module non-goal note.)
    pub added: Vec<(CellId, Shape)>,
    /// Copy-on-write replacements: the moved copy of a base shape.
    pub edited: HashMap<BaseShapeId, Shape>,
    /// Tombstoned base shapes — a delete, and the base half of every move.
    pub deleted: HashSet<BaseShapeId>,

    /// Cells with any overlay entry. Derived, kept in sync by the mutators; it is
    /// the fast "does this cell need compositing?" flag that keeps every untouched
    /// cell on the pristine Phase 2b cull path.
    edited_cells: HashSet<CellId>,
    /// Per-cell suppressed base indices (= the `deleted` ids grouped by cell), so
    /// a chunk can be re-tessellated skipping exactly those shapes.
    suppressed: HashMap<CellId, HashSet<u32>>,
}

impl EditOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.edited.is_empty() && self.deleted.is_empty()
    }

    /// Does this cell carry any overlay entry? A single hash lookup — the guard
    /// that keeps compositing off untouched cells.
    pub fn edits_cell(&self, cell: CellId) -> bool {
        self.edited_cells.contains(&cell)
    }

    /// The base shape indices suppressed in `cell` (deleted + moved-away), or
    /// `None` if the cell is untouched. Drives suppression-aware tessellation.
    pub fn suppressed_in(&self, cell: CellId) -> Option<&HashSet<u32>> {
        self.suppressed.get(&cell)
    }

    /// Tombstone a base shape (delete). Idempotent. A prior move of the same shape
    /// is superseded — the moved copy is dropped.
    pub fn delete(&mut self, id: BaseShapeId) {
        self.edited.remove(&id);
        self.deleted.insert(id);
        self.suppress(id);
    }

    /// Move a base shape: tombstone the base and record the translated copy.
    /// `moved` is the base geometry translated by the edit — the caller builds it
    /// from the base shape (the overlay does not hold the `CellDb`). This is the
    /// copy-on-write path; it never rewrites a base column.
    pub fn move_shape(&mut self, id: BaseShapeId, moved: Shape) {
        self.deleted.insert(id);
        self.edited.insert(id, moved);
        self.suppress(id);
    }

    /// Undo any edit of a base shape, restoring the original (used when a remote
    /// op or an undo clears an entry). Leaves the cell flagged only if other
    /// entries remain.
    pub fn restore(&mut self, id: BaseShapeId) {
        self.deleted.remove(&id);
        self.edited.remove(&id);
        self.recompute_cell(id.0);
    }

    fn suppress(&mut self, id: BaseShapeId) {
        self.edited_cells.insert(id.0);
        self.suppressed.entry(id.0).or_default().insert(id.1);
    }

    /// Rebuild the derived flags for one cell from `deleted`/`edited`/`added`.
    /// Called after a removal, where an incremental update can't know whether the
    /// cell still has other entries.
    fn recompute_cell(&mut self, cell: CellId) {
        let mut idxs = HashSet::new();
        for &(c, i) in &self.deleted {
            if c == cell {
                idxs.insert(i);
            }
        }
        let touched = !idxs.is_empty()
            || self.edited.keys().any(|&(c, _)| c == cell)
            || self.added.iter().any(|(c, _)| *c == cell);
        if idxs.is_empty() {
            self.suppressed.remove(&cell);
        } else {
            self.suppressed.insert(cell, idxs);
        }
        if touched {
            self.edited_cells.insert(cell);
        } else {
            self.edited_cells.remove(&cell);
        }
    }
}

/// Translate a base polygon by `(dx, dy)` nm to produce its moved copy. The
/// shared helper both the local editor and the CRDT-apply path use, so a move
/// reconstructed from `(dx, dy)` on a peer is identical to the one made locally.
pub fn translate(layer: Layer, points: &[Point], dx: i32, dy: i32) -> Shape {
    Shape {
        layer,
        points: points.iter().map(|p| Point::new(p.x + dx, p.y + dy)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poly() -> Shape {
        Shape { layer: Layer::new(1, 0), points: vec![Point::new(0, 0), Point::new(10, 0), Point::new(10, 10)] }
    }

    #[test]
    fn empty_overlay_touches_no_cell() {
        let o = EditOverlay::new();
        assert!(o.is_empty());
        assert!(!o.edits_cell(0));
        assert!(o.suppressed_in(0).is_none());
    }

    #[test]
    fn delete_suppresses_the_base() {
        let mut o = EditOverlay::new();
        o.delete((5, 3));
        assert!(o.edits_cell(5));
        assert!(o.suppressed_in(5).unwrap().contains(&3));
        assert!(o.edited.is_empty(), "a delete adds no geometry");
    }

    #[test]
    fn move_is_copy_on_write_not_in_place() {
        // The load-bearing property: a move BOTH tombstones the base AND records a
        // moved copy — it must never look like a bare coordinate rewrite.
        let mut o = EditOverlay::new();
        o.move_shape((0, 7), poly());
        assert!(o.deleted.contains(&(0, 7)), "base must be tombstoned");
        assert_eq!(o.edited.get(&(0, 7)), Some(&poly()), "moved copy must be recorded");
        assert!(o.suppressed_in(0).unwrap().contains(&7));
    }

    #[test]
    fn delete_supersedes_a_prior_move() {
        let mut o = EditOverlay::new();
        o.move_shape((0, 7), poly());
        o.delete((0, 7));
        assert!(o.edited.is_empty(), "deleting a moved shape drops the moved copy");
        assert!(o.deleted.contains(&(0, 7)));
    }

    #[test]
    fn restore_clears_the_cell_flag_when_last_entry_goes() {
        let mut o = EditOverlay::new();
        o.delete((2, 1));
        o.move_shape((2, 4), poly());
        o.restore((2, 1));
        assert!(o.edits_cell(2), "still one edit left");
        o.restore((2, 4));
        assert!(!o.edits_cell(2), "no edits left → cell back on pristine path");
        assert!(o.suppressed_in(2).is_none());
    }

    #[test]
    fn translate_shifts_every_point() {
        let s = translate(Layer::new(2, 0), &[Point::new(1, 2), Point::new(3, 4)], 100, -50);
        assert_eq!(s.points, vec![Point::new(101, -48), Point::new(103, -46)]);
        assert_eq!(s.layer, Layer::new(2, 0));
    }
}
