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

/// Identifies one **added** shape: `(creator's CRDT client id, per-client seq)`.
///
/// An added shape has no base index to agree on, so its id must be minted by its
/// creator and survive concurrent adds: two peers drawing simultaneously mint
/// distinct ids (distinct client ids) and both shapes exist after convergence.
/// Plain data — core stays network-free; the sync crate maps this to a CRDT key.
pub type AddId = (u64, u32);

/// An overlay polygon (a moved copy or an added shape), in its cell's local nm.
/// Aliased to [`Polygon`] so the existing tessellator consumes it directly.
pub type Shape = Polygon;

/// A brand-new shape drawn in this session (Phase 5 SLICE 1). Carries its stable
/// cross-peer [`AddId`] (so later moves/deletes/vertex-edits of it reference the
/// id, not a `BaseShapeId`) and its owning cell (the coordinate frame — TOP for
/// everything the draw tools produce today).
#[derive(Debug, Clone, PartialEq)]
pub struct AddedShape {
    pub id: AddId,
    pub cell: CellId,
    pub shape: Shape,
}

/// The copy-on-write editing overlay. Empty overlay ⇒ the scene renders exactly
/// as the base (the golden invariant).
#[derive(Default, Clone)]
pub struct EditOverlay {
    /// Brand-new shapes (Phase 5 SLICE 1 — the draw tools). Each carries its
    /// stable cross-peer id and its owning cell (the coordinate frame and its
    /// home for GDS export). Kept sorted by id when rebuilt from the CRDT so
    /// every peer draws them in the same deterministic order.
    pub added: Vec<AddedShape>,
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

    // --- added shapes (Phase 5 SLICE 1) --------------------------------------

    /// Record a brand-new shape. Replaces any existing entry with the same id
    /// (an add's geometry is last-writer-wins — a move/vertex-edit of an added
    /// shape rewrites it wholesale, since there is no base to diff against).
    pub fn add(&mut self, id: AddId, cell: CellId, shape: Shape) {
        match self.added.iter_mut().find(|a| a.id == id) {
            Some(a) => {
                let old_cell = a.cell;
                a.cell = cell;
                a.shape = shape;
                if old_cell != cell {
                    self.recompute_cell(old_cell);
                }
            }
            None => self.added.push(AddedShape { id, cell, shape }),
        }
        self.edited_cells.insert(cell);
    }

    /// Drop an added shape (its delete, or the undo of its add). No tombstone —
    /// there is no base underneath to suppress.
    pub fn remove_added(&mut self, id: AddId) {
        if let Some(pos) = self.added.iter().position(|a| a.id == id) {
            let cell = self.added.remove(pos).cell;
            self.recompute_cell(cell);
        }
    }

    /// The overlay geometry that is LIVE in `cell`: every moved/vertex-edited
    /// copy of a tombstoned base shape, then every added shape — the part of the
    /// composite the base store doesn't hold. (Consumers pair it with the base
    /// minus [`suppressed_in`](Self::suppressed_in) to get the whole cell.)
    pub fn live_shapes_in(&self, cell: CellId) -> impl Iterator<Item = &Shape> + '_ {
        let edited = self
            .edits_cell(cell)
            .then(|| {
                self.edited
                    .iter()
                    .filter(move |(&(c, i), _)| c == cell && self.deleted.contains(&(c, i)))
                    .map(|(_, s)| s)
            })
            .into_iter()
            .flatten();
        let added = self.added.iter().filter(move |a| a.cell == cell).map(|a| &a.shape);
        edited.chain(added)
    }

    /// The added shape with this id, if it exists.
    pub fn find_added(&self, id: AddId) -> Option<&AddedShape> {
        self.added.iter().find(|a| a.id == id)
    }

    /// Mutable access to an added shape (the local drag preview mutates geometry
    /// in place; the authoritative CRDT write happens on release).
    pub fn find_added_mut(&mut self, id: AddId) -> Option<&mut AddedShape> {
        self.added.iter_mut().find(|a| a.id == id)
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
            || self.added.iter().any(|a| a.cell == cell);
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

/// Vertices of a ring that are handles (the closing duplicate, GDS convention
/// `last == first`, is not its own vertex).
pub fn vertex_count(ring: &[Point]) -> usize {
    match (ring.first(), ring.last()) {
        (Some(f), Some(l)) if ring.len() > 3 && f == l => ring.len() - 1,
        _ => ring.len(),
    }
}

/// Insert vertex `p` on edge `edge` (from vertex `edge` to the next one) of a
/// ring — Phase 5d. Keeps a closed ring closed. The caller commits the result
/// like any vertex edit (`SetPoints` / added-shape geometry: both ship the whole
/// ring, so a count change needs no new CRDT kind).
pub fn insert_vertex(ring: &[Point], edge: usize, p: Point) -> Vec<Point> {
    let n = vertex_count(ring);
    let closed = n < ring.len();
    let mut out: Vec<Point> = ring[..n].to_vec();
    let at = (edge % n.max(1)) + 1;
    out.insert(at.min(out.len()), p);
    if closed {
        out.push(out[0]);
    }
    out
}

/// Delete vertex `i` of a ring — Phase 5d. `None` if fewer than three distinct
/// vertices would remain or the ring would enclose no area (the edit is then
/// refused rather than committing a degenerate shape). Keeps a closed ring
/// closed (deleting vertex 0 moves the closing duplicate with it).
pub fn delete_vertex(ring: &[Point], i: usize) -> Option<Vec<Point>> {
    let n = vertex_count(ring);
    let closed = n < ring.len();
    if i >= n {
        return None;
    }
    let mut out: Vec<Point> = ring[..n].to_vec();
    out.remove(i);
    out.dedup();
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    if out.len() < 3 || crate::boolean::signed_area2(&out) == 0 {
        return None;
    }
    if closed {
        out.push(out[0]);
    }
    Some(out)
}

/// The edge of `ring` nearest to `p` and the nearest point on it, as
/// `(edge index, point, squared distance)` — where an insert would go.
pub fn nearest_edge(ring: &[Point], p: Point) -> Option<(usize, Point, f64)> {
    let n = vertex_count(ring);
    if n < 2 {
        return None;
    }
    let mut best: Option<(usize, Point, f64)> = None;
    for e in 0..n {
        let (a, b) = (ring[e], ring[(e + 1) % n]);
        let (vx, vy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
        let l2 = vx * vx + vy * vy;
        let t = if l2 > 0.0 { (((p.x - a.x) as f64 * vx + (p.y - a.y) as f64 * vy) / l2).clamp(0.0, 1.0) } else { 0.0 };
        let q = (a.x as f64 + t * vx, a.y as f64 + t * vy);
        let d2 = (q.0 - p.x as f64).powi(2) + (q.1 - p.y as f64).powi(2);
        if best.map_or(true, |(_, _, bd)| d2 < bd) {
            best = Some((e, Point::new(q.0.round() as i32, q.1.round() as i32), d2));
        }
    }
    best
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
    fn add_flags_cell_and_remove_unflags() {
        let mut o = EditOverlay::new();
        let id: AddId = (42, 0);
        o.add(id, 3, poly());
        assert!(!o.is_empty());
        assert!(o.edits_cell(3), "added shape must flag its cell for compositing");
        assert!(o.suppressed_in(3).is_none(), "an add suppresses no base shape");
        assert_eq!(o.find_added(id).map(|a| &a.shape), Some(&poly()));

        o.remove_added(id);
        assert!(o.is_empty());
        assert!(!o.edits_cell(3), "cell back on the pristine path");
    }

    #[test]
    fn add_same_id_is_last_writer_wins() {
        // A move/vertex-edit of an added shape rewrites its geometry wholesale.
        let mut o = EditOverlay::new();
        let id: AddId = (7, 1);
        o.add(id, 0, poly());
        let moved = translate(poly().layer, &poly().points, 50, 50);
        o.add(id, 0, moved.clone());
        assert_eq!(o.added.len(), 1, "same id must replace, not duplicate");
        assert_eq!(o.find_added(id).map(|a| &a.shape), Some(&moved));
    }

    #[test]
    fn concurrent_ids_from_different_clients_coexist() {
        // The convergence property the id scheme exists for: distinct creators ⇒
        // distinct ids ⇒ both shapes survive.
        let mut o = EditOverlay::new();
        o.add((1, 0), 0, poly());
        o.add((2, 0), 0, poly());
        assert_eq!(o.added.len(), 2);
    }

    fn sq() -> Vec<Point> {
        vec![Point::new(0, 0), Point::new(100, 0), Point::new(100, 100), Point::new(0, 100), Point::new(0, 0)]
    }

    #[test]
    fn insert_vertex_keeps_a_closed_ring_closed() {
        let r = insert_vertex(&sq(), 1, Point::new(100, 50)); // on edge (100,0)-(100,100)
        assert_eq!(
            r,
            vec![Point::new(0, 0), Point::new(100, 0), Point::new(100, 50), Point::new(100, 100), Point::new(0, 100), Point::new(0, 0)]
        );
        // On the closing edge (vertex 3 → vertex 0): appended before the duplicate.
        let r = insert_vertex(&sq(), 3, Point::new(0, 50));
        assert_eq!(r[4], Point::new(0, 50));
        assert_eq!(r.first(), r.last());
        assert_eq!(vertex_count(&r), 5);
    }

    #[test]
    fn delete_vertex_refuses_degenerate_rings_and_moves_the_duplicate() {
        let pent = insert_vertex(&sq(), 1, Point::new(150, 50));
        let back = delete_vertex(&pent, 2).unwrap();
        assert_eq!(back, sq(), "deleting the inserted vertex restores the square");
        // Deleting vertex 0 keeps the ring closed on the new first vertex.
        let r = delete_vertex(&pent, 0).unwrap();
        assert_eq!(r.first(), r.last());
        assert_eq!(r[0], Point::new(100, 0));
        // A triangle cannot lose a vertex.
        let tri = vec![Point::new(0, 0), Point::new(10, 0), Point::new(0, 10), Point::new(0, 0)];
        assert!(delete_vertex(&tri, 1).is_none());
        // Nor may a delete leave zero area (collinear remains).
        let flat = vec![Point::new(0, 0), Point::new(10, 0), Point::new(20, 0), Point::new(10, 5), Point::new(0, 0)];
        assert!(delete_vertex(&flat, 3).is_none());
    }

    #[test]
    fn nearest_edge_finds_the_insert_point() {
        let (e, q, d2) = nearest_edge(&sq(), Point::new(104, 30)).unwrap();
        assert_eq!((e, q), (1, Point::new(100, 30)));
        assert!((d2 - 16.0).abs() < 1e-9);
    }

    #[test]
    fn translate_shifts_every_point() {
        let s = translate(Layer::new(2, 0), &[Point::new(1, 2), Point::new(3, 4)], 100, -50);
        assert_eq!(s.points, vec![Point::new(101, -48), Point::new(103, -46)]);
        assert_eq!(s.layer, Layer::new(2, 0));
    }
}
