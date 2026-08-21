//! Design-rule checks — the Phase 5 SLICE 3 skeleton, FluidEDA's first
//! *verification* feature.
//!
//! ## Scope (honest)
//!
//! Checks run over ONE cell's shapes in **cell-local** space, composited with the
//! edit overlay (base minus tombstones, plus moved/vertex-edited copies and added
//! shapes). That makes them **intra-cell** checks: two shapes in the *same* cell
//! def closer than `min_spacing` are caught; a shape in cell A closer than
//! `min_spacing` to a shape in an *instance of cell B placed next to it* is NOT —
//! cross-instance / hierarchical spacing needs flattened-context analysis (or
//! interaction regions along instance boundaries), which is the hard real-DRC
//! problem and is deliberately deferred. Same-layer only; touching or overlapping
//! shapes count as merged (legal), as in real DRC.
//!
//! ## What is exact and what is approximate
//!
//! * **Spacing** is exact for any polygon pair: candidate pairs come from the
//!   cell's existing R-tree (bbox inflated by the rule), then the true minimum
//!   boundary distance is computed edge-pair-wise (segment–segment). Overlap /
//!   containment / touching ⇒ merged ⇒ no violation.
//! * **Width** is exact for axis-aligned rectangles (min(w, h)); other polygons
//!   use the bbox min dimension, an over-approximation of true internal width
//!   (misses a thin arm on an L-shape). Deferred and documented in STATUS.
//! * **Width is per-shape, PRE-merge** — the measured limitation that matters on
//!   production GDS: place-and-route output arrives *fractured* (big polygons
//!   split into abutting rectangles, slivers included), and a fracture sliver is
//!   not a real width violation — the merged region is wide. Measured on
//!   caravel_core: 593 k of 611 k flags are per-shape width on met1 fracture
//!   slivers. Real min-width needs merge-first (boolean union of same-layer
//!   shapes) — exactly the deferred booleans feature. Per-shape width IS correct
//!   for isolated shapes, which covers the interactive use this slice ships:
//!   a freshly *drawn* too-narrow shape flags immediately.
//!
//! All decision math is integer (`i64` squared distances; segment projections in
//! `f64` only as a final measure, exact for i32-nm inputs well past this range).
//! No wgpu, no network — core stays the Web Worker seam, so a future
//! DRC-in-worker offload links this file unchanged.

use crate::db::{Cell, CellId};
use crate::edit::EditOverlay;
use crate::transform::Bboxf;
use crate::types::{Bbox, Layer, Point};
use std::collections::HashMap;

/// Rules for one layer, in nm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerRule {
    pub min_width: i32,
    pub min_spacing: i32,
}

/// The rule table: per-layer entries plus an optional blanket default.
#[derive(Debug, Clone, Default)]
pub struct DrcRules {
    /// Applied to any layer without a `per_layer` entry (None ⇒ unchecked).
    pub default_rule: Option<LayerRule>,
    pub per_layer: HashMap<Layer, LayerRule>,
}

impl DrcRules {
    /// The small default table: a blanket 100 nm width / 100 nm spacing on every
    /// layer. A placeholder, not a PDK. It makes anything drawn carelessly (the
    /// 5 nm snap allows arbitrarily thin shapes) flag at once — but on fractured
    /// production GDS the per-shape width check over-flags massively (see the
    /// module note; measured on caravel), so real signoff needs BOTH a per-layer
    /// PDK table (skipping marker/pin datatypes) and merge-first width. Deferred.
    pub fn default_table() -> Self {
        DrcRules {
            default_rule: Some(LayerRule { min_width: 100, min_spacing: 100 }),
            per_layer: HashMap::new(),
        }
    }

    pub fn rule_for(&self, layer: Layer) -> Option<LayerRule> {
        self.per_layer.get(&layer).copied().or(self.default_rule)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrcKind {
    Width,
    Spacing,
}

/// One violation, in the checked cell's local nm. `bbox` is the marker box the
/// renderer draws (the shape for width; the gap region for spacing).
#[derive(Debug, Clone)]
pub struct Violation {
    pub kind: DrcKind,
    pub layer: Layer,
    pub bbox: Bbox,
}

/// Check shapes `start..end` of `cell` (base indices), composited with `overlay`,
/// appending violations to `out`. Range-based so a caller can budget a huge cell
/// across frames: the union of disjoint ranges covering `0..shapes.len()` checks
/// every base pair exactly once (pairs are counted at their lower index via the
/// R-tree candidates). Overlay shapes (moved copies + adds) are processed by the
/// range containing the END of the base list, so they are also checked once.
pub fn check_cell_range(
    cell: &Cell,
    cell_id: CellId,
    overlay: &EditOverlay,
    rules: &DrcRules,
    start: usize,
    end: usize,
    out: &mut Vec<Violation>,
) {
    let n = cell.shapes.len();
    let (start, end) = (start.min(n), end.min(n));
    let empty = std::collections::HashSet::new();
    let suppressed = overlay.suppressed_in(cell_id).unwrap_or(&empty);

    // --- base shapes in range -------------------------------------------------
    for i in start..end {
        if suppressed.contains(&(i as u32)) {
            continue; // deleted or moved away — its copy is checked as overlay
        }
        let layer = cell.shapes.layer(i);
        let Some(rule) = rules.rule_for(layer) else { continue };
        let pts = cell.shapes.points(i);

        check_width(pts, layer, rule.min_width, out);

        // Spacing candidates from the R-tree, bbox inflated by the rule. Only
        // pairs (i, j>i) so each base pair is counted once across ranges.
        let bb = ring_bbox(pts);
        let s = rule.min_spacing as f64;
        let query = Bboxf::new(
            (bb.min.x as f64) - s,
            (bb.min.y as f64) - s,
            (bb.max.x as f64) + s,
            (bb.max.y as f64) + s,
        );
        for j in cell.index().query(&query) {
            let j = j as usize;
            if j <= i || suppressed.contains(&(j as u32)) {
                continue;
            }
            if cell.shapes.layer(j) != layer {
                continue; // spacing is same-layer only
            }
            check_spacing(pts, cell.shapes.points(j), layer, rule.min_spacing, out);
        }
    }

    // --- overlay shapes (once, with the final base range) ---------------------
    if end == n {
        // The live overlay geometry of this cell: moved/vertex-edited copies of
        // tombstoned bases, plus added shapes. Few per cell, so linear is fine.
        let mut ov: Vec<(Layer, &[Point])> = Vec::new();
        for (&(c, i), s) in &overlay.edited {
            if c == cell_id && overlay.deleted.contains(&(c, i)) {
                ov.push((s.layer, &s.points));
            }
        }
        for a in &overlay.added {
            if a.cell == cell_id {
                ov.push((a.shape.layer, &a.shape.points));
            }
        }

        for (k, &(layer, pts)) in ov.iter().enumerate() {
            let Some(rule) = rules.rule_for(layer) else { continue };
            check_width(pts, layer, rule.min_width, out);

            let bb = ring_bbox(pts);
            let s = rule.min_spacing as f64;
            let query = Bboxf::new(
                (bb.min.x as f64) - s,
                (bb.min.y as f64) - s,
                (bb.max.x as f64) + s,
                (bb.max.y as f64) + s,
            );
            // vs live base shapes (the R-tree indexes every base shape; skip
            // suppressed ones — their live geometry is in `ov`).
            for j in cell.index().query(&query) {
                if suppressed.contains(&j) {
                    continue;
                }
                let j = j as usize;
                if cell.shapes.layer(j) != layer {
                    continue;
                }
                check_spacing(pts, cell.shapes.points(j), layer, rule.min_spacing, out);
            }
            // vs the other overlay shapes (each pair once).
            for &(l2, pts2) in ov.iter().skip(k + 1) {
                if l2 == layer {
                    check_spacing(pts, pts2, layer, rule.min_spacing, out);
                }
            }
        }
    }
}

/// Convenience: check the whole cell in one call.
pub fn check_cell(
    cell: &Cell,
    cell_id: CellId,
    overlay: &EditOverlay,
    rules: &DrcRules,
) -> Vec<Violation> {
    let mut out = Vec::new();
    check_cell_range(cell, cell_id, overlay, rules, 0, cell.shapes.len(), &mut out);
    out
}

// --- width -------------------------------------------------------------------

fn check_width(pts: &[Point], layer: Layer, min_width: i32, out: &mut Vec<Violation>) {
    let Some(w) = shape_width(pts) else { return };
    if w < min_width {
        out.push(Violation { kind: DrcKind::Width, layer, bbox: ring_bbox(pts) });
    }
}

/// The shape's checked width: the bbox's min dimension. For an axis-aligned
/// rectangle (which equals its bbox) this IS the exact min width; for any other
/// polygon it is the documented over-approximation (see the module note).
/// `None` for degenerate rings.
fn shape_width(pts: &[Point]) -> Option<i32> {
    let ring = open_ring(pts);
    if ring.len() < 3 {
        return None;
    }
    let bb = ring_bbox(pts);
    let (w, h) = (bb.max.x - bb.min.x, bb.max.y - bb.min.y);
    Some(w.min(h))
}

/// The ring without its closing duplicate.
fn open_ring(pts: &[Point]) -> &[Point] {
    match (pts.first(), pts.last()) {
        (Some(f), Some(l)) if pts.len() > 3 && f == l => &pts[..pts.len() - 1],
        _ => pts,
    }
}

// --- spacing -----------------------------------------------------------------

fn check_spacing(a: &[Point], b: &[Point], layer: Layer, min_spacing: i32, out: &mut Vec<Violation>) {
    let (ra, rb) = (open_ring(a), open_ring(b));
    if ra.len() < 3 || rb.len() < 3 {
        return;
    }
    let s2 = (min_spacing as i64) * (min_spacing as i64);

    // Cheap reject: bbox gap ≥ rule ⇒ true distance ≥ rule (bbox distance is a
    // lower bound on shape distance).
    let (ba, bb_) = (ring_bbox(a), ring_bbox(b));
    if bbox_gap2(&ba, &bb_) >= s2 {
        return;
    }

    // Exact boundary distance via edge pairs; also find the closest point pair
    // for the marker box.
    let mut best: Option<(i64, Point, Point)> = None;
    for i in 0..ra.len() {
        let (a0, a1) = (ra[i], ra[(i + 1) % ra.len()]);
        for j in 0..rb.len() {
            let (b0, b1) = (rb[j], rb[(j + 1) % rb.len()]);
            let (d2, pa, pb) = seg_seg_dist2(a0, a1, b0, b1);
            if best.as_ref().map_or(true, |(bd, _, _)| d2 < *bd) {
                best = Some((d2, pa, pb));
            }
        }
    }
    let Some((d2, pa, pb)) = best else { return };

    if d2 == 0 {
        return; // touching / crossing ⇒ merged geometry, legal
    }
    // One containing the other (no boundary contact) is overlap ⇒ merged, legal.
    if crate::pick::point_in_polygon(a, rb[0]) || crate::pick::point_in_polygon(b, ra[0]) {
        return;
    }
    if d2 < s2 {
        // Marker: the gap segment, inflated a little so it is visible.
        let m = 4;
        let bbox = Bbox {
            min: Point::new(pa.x.min(pb.x) - m, pa.y.min(pb.y) - m),
            max: Point::new(pa.x.max(pb.x) + m, pa.y.max(pb.y) + m),
        };
        out.push(Violation { kind: DrcKind::Spacing, layer, bbox });
    }
}

fn ring_bbox(pts: &[Point]) -> Bbox {
    let mut b = Bbox::empty();
    for p in pts {
        b.expand(*p);
    }
    b
}

/// Squared axis-aligned gap between two bboxes (0 if they touch/overlap).
fn bbox_gap2(a: &Bbox, b: &Bbox) -> i64 {
    let dx = (b.min.x - a.max.x).max(a.min.x - b.max.x).max(0) as i64;
    let dy = (b.min.y - a.max.y).max(a.min.y - b.max.y).max(0) as i64;
    dx * dx + dy * dy
}

/// Squared distance between segments a0a1 and b0b1, plus (approximately) the
/// closest point pair for the marker. Exact 0 when they properly intersect. For
/// non-intersecting segments the minimum is attained at an endpoint of one
/// against the other, so four point–segment tests cover it.
fn seg_seg_dist2(a0: Point, a1: Point, b0: Point, b1: Point) -> (i64, Point, Point) {
    if segments_intersect(a0, a1, b0, b1) {
        return (0, a0, b0);
    }
    let mut best: Option<(i64, Point, Point)> = None;
    let mut consider = |d2: i64, pa: Point, pb: Point| {
        if best.as_ref().map_or(true, |(bd, _, _)| d2 < *bd) {
            best = Some((d2, pa, pb));
        }
    };
    let (d, q) = point_seg_dist2(a0, b0, b1);
    consider(d, a0, q);
    let (d, q) = point_seg_dist2(a1, b0, b1);
    consider(d, a1, q);
    let (d, q) = point_seg_dist2(b0, a0, a1);
    consider(d, q, b0);
    let (d, q) = point_seg_dist2(b1, a0, a1);
    consider(d, q, b1);
    best.unwrap()
}

/// Squared distance from `p` to segment `s0s1`, plus the closest point on the
/// segment. Projection in f64 (exact for i32 inputs), result re-squared in i64
/// via rounding of the foot point — accurate to the nm grid, which is the unit
/// the rules are written in.
fn point_seg_dist2(p: Point, s0: Point, s1: Point) -> (i64, Point) {
    let (vx, vy) = ((s1.x - s0.x) as f64, (s1.y - s0.y) as f64);
    let (wx, wy) = ((p.x - s0.x) as f64, (p.y - s0.y) as f64);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= 0.0 { 0.0 } else { ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0) };
    let fx = s0.x as f64 + t * vx;
    let fy = s0.y as f64 + t * vy;
    let dx = p.x as f64 - fx;
    let dy = p.y as f64 - fy;
    let d2 = dx * dx + dy * dy;
    ((d2.round()) as i64, Point::new(fx.round() as i32, fy.round() as i32))
}

/// Do the two closed segments intersect (including touching)? Standard integer
/// orientation tests — no floats, no overflow (i32 coords, i64 products... cross
/// of two deltas needs i64; products of two i64 sums stay in i128-safe range —
/// use i64 with i32 deltas: |delta| < 2^31, product < 2^62, sum < 2^63. OK).
fn segments_intersect(a0: Point, a1: Point, b0: Point, b1: Point) -> bool {
    fn orient(a: Point, b: Point, c: Point) -> i64 {
        let v = (b.x - a.x) as i64 * (c.y - a.y) as i64 - (b.y - a.y) as i64 * (c.x - a.x) as i64;
        v.signum()
    }
    fn on_seg(a: Point, b: Point, p: Point) -> bool {
        p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
    }
    let (o1, o2) = (orient(a0, a1, b0), orient(a0, a1, b1));
    let (o3, o4) = (orient(b0, b1, a0), orient(b0, b1, a1));
    if o1 != o2 && o3 != o4 {
        return true;
    }
    (o1 == 0 && on_seg(a0, a1, b0))
        || (o2 == 0 && on_seg(a0, a1, b1))
        || (o3 == 0 && on_seg(b0, b1, a0))
        || (o4 == 0 && on_seg(b0, b1, a1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Cell;
    use crate::store::ShapeStore;

    /// Closed axis-aligned rect ring.
    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![
            Point::new(x, y),
            Point::new(x + w, y),
            Point::new(x + w, y + h),
            Point::new(x, y + h),
            Point::new(x, y),
        ]
    }

    fn cell_of(shapes: Vec<(Layer, Vec<Point>)>) -> Cell {
        let mut store = ShapeStore::new();
        for (l, pts) in shapes {
            store.push(l, pts);
        }
        let chunks = crate::chunk::chunk_store(&mut store);
        Cell::from_parts("T".into(), store, Vec::new(), chunks)
    }

    fn rules(w: i32, s: i32) -> DrcRules {
        DrcRules { default_rule: Some(LayerRule { min_width: w, min_spacing: s }), per_layer: HashMap::new() }
    }

    const L1: Layer = Layer { layer: 1, datatype: 0 };
    const L2: Layer = Layer { layer: 2, datatype: 0 };

    #[test]
    fn narrow_rect_flags_width_wide_passes() {
        let cell = cell_of(vec![
            (L1, rect(0, 0, 50, 1000)),   // 50 nm wide < 100 ⇒ violation
            (L1, rect(5000, 0, 500, 500)), // fine
        ]);
        let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DrcKind::Width);
        assert_eq!(v[0].layer, L1);
    }

    #[test]
    fn spacing_gap_flags_touching_and_far_do_not() {
        let cell = cell_of(vec![
            (L1, rect(0, 0, 1000, 1000)),
            (L1, rect(1050, 0, 1000, 1000)),  // 50 nm gap < 100 ⇒ violation
            (L1, rect(3000, 0, 1000, 1000)),  // 950 gap to neighbor ⇒ fine
            (L1, rect(4000, 0, 1000, 1000)),  // touches previous (gap 0) ⇒ merged, fine
            (L2, rect(1010, 2000, 1000, 1000)), // different layer ⇒ never pairs
        ]);
        let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        assert_eq!(v.len(), 1, "exactly the 50 nm gap flags: {v:?}");
        assert_eq!(v[0].kind, DrcKind::Spacing);
        // The marker sits in the gap between x=1000 and x=1050.
        assert!(v[0].bbox.min.x >= 990 && v[0].bbox.max.x <= 1060, "marker in the gap: {:?}", v[0].bbox);
    }

    #[test]
    fn overlapping_same_layer_is_merged_not_a_violation() {
        let cell = cell_of(vec![
            (L1, rect(0, 0, 1000, 1000)),
            (L1, rect(500, 500, 1000, 1000)), // overlaps ⇒ merged
            (L1, rect(200, 200, 400, 400)),   // fully contained ⇒ merged
        ]);
        let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        assert!(v.is_empty(), "overlap/containment must not flag: {v:?}");
    }

    #[test]
    fn diagonal_spacing_is_exact_not_bbox() {
        // Two unit squares diagonally offset: bbox gap is (70,70) ⇒ corner-to-
        // corner distance ≈ 99 < 100 ⇒ must flag (bbox-only would compute the
        // same here; the point is the exact diagonal measure).
        let cell = cell_of(vec![
            (L1, rect(0, 0, 1000, 1000)),
            (L1, rect(1070, 1070, 1000, 1000)),
        ]);
        let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        assert_eq!(v.len(), 1, "corner distance ~99 nm flags");
        // And at 71,71 offset (distance ~100.4) it must NOT flag.
        let cell2 = cell_of(vec![
            (L1, rect(0, 0, 1000, 1000)),
            (L1, rect(1071, 1071, 1000, 1000)),
        ]);
        let v2 = check_cell(&cell2, 0, &EditOverlay::new(), &rules(100, 100));
        assert!(v2.is_empty(), "corner distance ~100.4 nm passes: {v2:?}");
    }

    #[test]
    fn overlay_composites_into_the_check() {
        // Base: two rects with a legal 200 nm gap. The overlay moves one closer
        // (violation appears) — and deleting it instead clears everything.
        let cell = cell_of(vec![
            (L1, rect(0, 0, 1000, 1000)),
            (L1, rect(1200, 0, 1000, 1000)),
        ]);
        let clean = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        assert!(clean.is_empty());

        // Move shape 1 left by 150 ⇒ gap 50 ⇒ violation (checked as overlay copy).
        let mut moved = EditOverlay::new();
        moved.move_shape((0, 1), crate::edit::translate(L1, &rect(1200, 0, 1000, 1000), -150, 0));
        let v = check_cell(&cell, 0, &moved, &rules(100, 100));
        assert_eq!(v.len(), 1, "moved-closer pair flags: {v:?}");
        assert_eq!(v[0].kind, DrcKind::Spacing);

        // Delete shape 1 ⇒ no pair at all.
        let mut deleted = EditOverlay::new();
        deleted.delete((0, 1));
        assert!(check_cell(&cell, 0, &deleted, &rules(100, 100)).is_empty());

        // Draw a 40 nm-wide sliver ⇒ width violation from the added path.
        let mut added = EditOverlay::new();
        added.add((1, 0), 0, crate::edit::Shape { layer: L1, points: rect(5000, 5000, 40, 2000) });
        let v = check_cell(&cell, 0, &added, &rules(100, 100));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DrcKind::Width);
    }

    #[test]
    fn range_split_equals_whole_cell() {
        // The budgeted scan must find the same violations as one pass, each once.
        let cell = cell_of(vec![
            (L1, rect(0, 0, 50, 1000)),        // width
            (L1, rect(2000, 0, 1000, 1000)),
            (L1, rect(3050, 0, 1000, 1000)),   // 50 gap ⇒ spacing
            (L1, rect(9000, 0, 1000, 1000)),
        ]);
        let whole = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
        let mut split = Vec::new();
        check_cell_range(&cell, 0, &EditOverlay::new(), &rules(100, 100), 0, 2, &mut split);
        check_cell_range(&cell, 0, &EditOverlay::new(), &rules(100, 100), 2, cell.shapes.len(), &mut split);
        assert_eq!(whole.len(), split.len(), "split ranges find the same set once");
        assert_eq!(whole.len(), 2);
    }

    #[test]
    fn no_rules_means_no_violations() {
        let cell = cell_of(vec![(L1, rect(0, 0, 10, 10))]);
        let none = DrcRules::default(); // no default rule, no per-layer entries
        assert!(check_cell(&cell, 0, &EditOverlay::new(), &none).is_empty());
    }
}
