//! Polygon booleans over i32-nm rings — the Phase 5b geometry kernel.
//!
//! A thin wrapper on `i_overlay`'s **integer** engine: robust union / intersect /
//! difference / xor with no float snapping, so a merged region's vertices are
//! still exact nm. Two consumers:
//!
//!   * **merge-first DRC** ([`crate::drc`]): the union of a layer's shapes
//!     (fractured P&R output becomes the real wide region) clipped to a check
//!     window, whose boundary edges the width/space checks then measure;
//!   * editing booleans (later): the same ops over selected shapes.
//!
//! ## Conventions
//!
//! * Input rings may be open or closed (GDS repeats the first point — tolerated)
//!   and of **either orientation**: every ring is normalised to counter-clockwise
//!   before it is added, so overlapping shapes drawn with opposite windings never
//!   cancel into a false hole under the non-zero fill rule.
//! * Output is a [`Region`]: polygons, each an outer ring (CCW) followed by its
//!   holes (CW). Rings are **open** (no closing duplicate) and collinear points
//!   are removed. With that orientation the region's interior lies to the **left**
//!   of every directed edge — the invariant the DRC edge-pair checks rely on.
//! * The i32 engine requires coordinates within ±2^30 nm (±1.07 m); callers that
//!   care (DRC tiles) rebase to a local origin first, which also keeps every
//!   product in the edge math small.

use crate::types::{Bbox, Point};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::i_float::int::point::IntPoint;

/// One polygon with holes: `rings[0]` is the outer boundary (CCW), the rest are
/// holes (CW). Open rings.
pub type Polygon = Vec<Vec<Point>>;

/// A set of disjoint polygons (the result of a boolean).
pub type Region = Vec<Polygon>;

/// The boolean operation to apply between the `a` and `b` operand sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    Union,
    Intersect,
    /// `a` minus `b`.
    Difference,
    Xor,
}

/// Twice the signed area of a ring (shoelace, i128 — exact for any i32 input).
/// Positive = counter-clockwise. Tolerates a closing duplicate (contributes 0).
pub fn signed_area2(ring: &[Point]) -> i128 {
    let n = ring.len();
    if n < 3 {
        return 0;
    }
    let mut s: i128 = 0;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        s += p.x as i128 * q.y as i128 - q.x as i128 * p.y as i128;
    }
    s
}

/// The ring without its closing duplicate (GDS convention: last == first).
pub fn open_ring(pts: &[Point]) -> &[Point] {
    match (pts.first(), pts.last()) {
        (Some(f), Some(l)) if pts.len() > 1 && f == l => &pts[..pts.len() - 1],
        _ => pts,
    }
}

/// Add one ring to `ov`, normalised to CCW and shifted by `-origin`. Degenerate
/// (zero-area) rings are skipped — they cover nothing.
fn add_ring(ov: &mut Overlay<i32>, ring: &[Point], origin: Point, ty: ShapeType) {
    let r = open_ring(ring);
    let a = signed_area2(r);
    if a == 0 {
        return;
    }
    let to = |p: &Point| IntPoint::new(p.x - origin.x, p.y - origin.y);
    if a > 0 {
        ov.add_path_iter(r.iter().map(to), ty);
    } else {
        ov.add_path_iter(r.iter().rev().map(to), ty);
    }
}

fn rect_ring(b: &Bbox) -> [Point; 4] {
    [
        Point::new(b.min.x, b.min.y),
        Point::new(b.max.x, b.min.y),
        Point::new(b.max.x, b.max.y),
        Point::new(b.min.x, b.max.y),
    ]
}

/// Convert i_overlay's output back to nm (undoing the rebase). i_overlay emits
/// outer rings CCW and holes CW (its default `output_direction`), open.
fn to_region(shapes: Vec<Vec<Vec<IntPoint<i32>>>>, origin: Point) -> Region {
    shapes
        .into_iter()
        .map(|shape| {
            shape
                .into_iter()
                .map(|c| c.into_iter().map(|p| Point::new(p.x + origin.x, p.y + origin.y)).collect())
                .collect()
        })
        .collect()
}

/// Union of `rings`, optionally clipped to the rectangle `clip`. The merge-first
/// DRC primitive: `clip` is a tile's check window, so the result is exactly the
/// merged layer inside the window (plus the window's own border as edges where
/// geometry crosses it — the caller recognises and skips those).
///
/// `origin` is subtracted from every coordinate before the boolean and added
/// back after (keep it near the data so the i32 engine's range is never a
/// concern and its arithmetic stays small).
pub fn union_clipped<'a, I>(rings: I, clip: Option<Bbox>, origin: Point) -> Region
where
    I: IntoIterator<Item = &'a [Point]>,
{
    let mut ov: Overlay<i32> = Overlay::new(64);
    for r in rings {
        add_ring(&mut ov, r, origin, ShapeType::Subject);
    }
    let shapes = match clip {
        Some(c) => {
            add_ring(&mut ov, &rect_ring(&c), origin, ShapeType::Clip);
            ov.overlay(OverlayRule::Intersect, FillRule::NonZero)
        }
        None => ov.overlay(OverlayRule::Subject, FillRule::NonZero),
    };
    to_region(shapes, origin)
}

/// General boolean between two ring sets (each set is first unioned within
/// itself by the non-zero fill rule).
pub fn boolean(a: &[&[Point]], b: &[&[Point]], op: BoolOp) -> Region {
    let origin = a.iter().chain(b.iter()).find_map(|r| r.first().copied()).unwrap_or(Point::new(0, 0));
    let mut ov: Overlay<i32> = Overlay::new(64);
    for r in a {
        add_ring(&mut ov, r, origin, ShapeType::Subject);
    }
    for r in b {
        add_ring(&mut ov, r, origin, ShapeType::Clip);
    }
    let rule = match op {
        BoolOp::Union => OverlayRule::Union,
        BoolOp::Intersect => OverlayRule::Intersect,
        BoolOp::Difference => OverlayRule::Difference,
        BoolOp::Xor => OverlayRule::Xor,
    };
    to_region(ov.overlay(rule, FillRule::NonZero), origin)
}

/// One polygon with holes → ONE ring, for formats that cannot hold holes (a GDS
/// BOUNDARY): each hole is bridged into the outer ring along a zero-width slit
/// between existing vertices (no new coordinates, so everything stays on the
/// nm grid). Holes are taken leftmost-first; each is joined from its leftmost
/// vertex to the nearest vertex of the ring built so far that it can *see* —
/// the bridge crosses no edge, touches no other vertex, and runs through the
/// polygon's interior. Returns the open ring (CCW, like the outer input).
///
/// Every renderer / boolean in FluidEDA reads the slit as intended: the two
/// bridge edges coincide in opposite directions and cancel under any fill rule.
pub fn keyhole(poly: &Polygon) -> Vec<Point> {
    let Some(outer) = poly.first() else { return Vec::new() };
    let mut ring = outer.clone();
    let mut holes: Vec<&Vec<Point>> = poly[1..].iter().filter(|h| h.len() >= 3).collect();
    holes.sort_by_key(|h| h.iter().map(|p| (p.x, p.y)).min().unwrap());
    for (k, hole) in holes.iter().enumerate() {
        // Leftmost (then lowest) vertex of the hole.
        let hi = (0..hole.len()).min_by_key(|&i| (hole[i].x, hole[i].y)).unwrap();
        let h = hole[hi];
        // Every edge currently in play: the ring so far + all holes still open.
        let mut edges: Vec<(Point, Point)> = Vec::new();
        let mut push_ring = |r: &[Point]| {
            for i in 0..r.len() {
                edges.push((r[i], r[(i + 1) % r.len()]));
            }
        };
        push_ring(&ring);
        for other in &holes[k..] {
            push_ring(other);
        }
        // Candidate ring vertices nearest first; take the first with a clear,
        // interior line of sight to `h`.
        let mut cand: Vec<usize> = (0..ring.len()).collect();
        cand.sort_by_key(|&i| {
            let (dx, dy) = ((ring[i].x - h.x) as i64, (ring[i].y - h.y) as i64);
            dx * dx + dy * dy
        });
        let visible = |v: Point| -> bool {
            edges.iter().all(|&(a, b)| !seg_blocks(h, v, a, b))
                && inside_region(mid(h, v), &ring, &holes[k..])
        };
        let Some(&vi) = cand.iter().find(|&&i| ring[i] != h && visible(ring[i])) else {
            continue; // no bridge found (degenerate input): leave this hole out
        };
        // ring[..=vi] → h → around the hole → h → ring[vi] → ring[vi+1..]
        let v = ring[vi];
        let mut out = Vec::with_capacity(ring.len() + hole.len() + 3);
        out.extend_from_slice(&ring[..=vi]);
        for j in 0..=hole.len() {
            out.push(hole[(hi + j) % hole.len()]);
        }
        out.push(v);
        out.extend_from_slice(&ring[vi + 1..]);
        ring = out;
    }
    ring
}

fn mid(a: Point, b: Point) -> (f64, f64) {
    ((a.x as f64 + b.x as f64) / 2.0, (a.y as f64 + b.y as f64) / 2.0)
}

/// Does edge `ab` block the bridge `hv`? Any contact other than a shared
/// endpoint blocks it (a crossing, or `ab` touching the bridge's interior).
fn seg_blocks(h: Point, v: Point, a: Point, b: Point) -> bool {
    let orient = |p: Point, q: Point, r: Point| -> i128 {
        ((q.x - p.x) as i128 * (r.y - p.y) as i128 - (q.y - p.y) as i128 * (r.x - p.x) as i128).signum()
    };
    let on = |p: Point, q: Point, r: Point| {
        r.x >= p.x.min(q.x) && r.x <= p.x.max(q.x) && r.y >= p.y.min(q.y) && r.y <= p.y.max(q.y)
    };
    let shared = |p: Point| p == h || p == v;
    let (o1, o2, o3, o4) = (orient(h, v, a), orient(h, v, b), orient(a, b, h), orient(a, b, v));
    // Endpoint-only contact is fine.
    if o1 == 0 && on(h, v, a) && !shared(a) {
        return true;
    }
    if o2 == 0 && on(h, v, b) && !shared(b) {
        return true;
    }
    if (o3 == 0 && on(a, b, h) && !shared(a) && !shared(b)) || (o4 == 0 && on(a, b, v) && !shared(a) && !shared(b)) {
        return true;
    }
    o1 * o2 < 0 && o3 * o4 < 0
}

/// Is `p` inside the outer `ring` and outside every hole? (Even-odd, f64.)
fn inside_region(p: (f64, f64), ring: &[Point], holes: &[&Vec<Point>]) -> bool {
    let inside = |r: &[Point]| {
        let mut c = false;
        let n = r.len();
        for i in 0..n {
            let (a, b) = (r[i], r[(i + n - 1) % n]);
            let (ay, by) = (a.y as f64, b.y as f64);
            if (ay > p.1) != (by > p.1) {
                let x = a.x as f64 + (p.1 - ay) * (b.x as f64 - a.x as f64) / (by - ay);
                if p.0 < x {
                    c = !c;
                }
            }
        }
        c
    };
    inside(ring) && !holes.iter().any(|h| inside(h))
}

/// Total area of a region (nm², holes subtracted). For tests and sanity checks.
pub fn region_area2(region: &Region) -> i128 {
    region.iter().flat_map(|poly| poly.iter()).map(|r| signed_area2(r)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h), Point::new(x, y)]
    }

    #[test]
    fn abutting_fracture_pieces_merge_into_one_rectangle() {
        // The P&R fracture case: a 1000×200 wire split into three abutting
        // pieces (one a 30 nm sliver) must merge back into ONE 4-vertex ring.
        let pieces = [rect(0, 0, 400, 200), rect(400, 0, 30, 200), rect(430, 0, 570, 200)];
        let r = union_clipped(pieces.iter().map(|p| p.as_slice()), None, Point::new(0, 0));
        assert_eq!(r.len(), 1, "one polygon");
        assert_eq!(r[0].len(), 1, "no holes");
        assert_eq!(r[0][0].len(), 4, "collinear fracture vertices removed: {:?}", r[0][0]);
        assert_eq!(region_area2(&r), 2 * 1000 * 200);
    }

    #[test]
    fn opposite_windings_do_not_cancel() {
        // Two overlapping squares, one CW: a naive non-zero union would carve
        // the overlap out as a hole. Normalisation must prevent that.
        let a = rect(0, 0, 100, 100);
        let mut b = rect(50, 0, 100, 100);
        b.reverse();
        let r = union_clipped([a.as_slice(), b.as_slice()], None, Point::new(0, 0));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 1, "no false hole");
        assert_eq!(region_area2(&r), 2 * 150 * 100);
    }

    #[test]
    fn output_orientation_puts_interior_on_the_left() {
        // A square ring with a square hole: outer CCW (area > 0), hole CW.
        let outer = rect(0, 0, 1000, 1000);
        let hole = rect(400, 400, 200, 200);
        let r = boolean(&[outer.as_slice()], &[hole.as_slice()], BoolOp::Difference);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 2, "outer + hole");
        assert!(signed_area2(&r[0][0]) > 0, "outer ring is CCW");
        assert!(signed_area2(&r[0][1]) < 0, "hole ring is CW");
        assert_eq!(region_area2(&r), 2 * (1000 * 1000 - 200 * 200));
    }

    #[test]
    fn clip_window_cuts_to_the_rectangle() {
        let long = rect(-5000, 0, 10000, 100);
        let w = Bbox { min: Point::new(0, -50), max: Point::new(1000, 500) };
        let r = union_clipped([long.as_slice()], Some(w), Point::new(123, 456));
        assert_eq!(region_area2(&r), 2 * 1000 * 100, "only the part inside the window");
        for p in &r[0][0] {
            assert!(p.x >= 0 && p.x <= 1000, "clipped to window: {p:?}");
        }
    }

    #[test]
    fn keyhole_joins_holes_into_one_ring_with_the_same_area() {
        // A square with two square holes (the `Difference` output shape).
        let outer = rect(0, 0, 1000, 1000);
        let h1 = rect(100, 100, 200, 200);
        let h2 = rect(600, 500, 300, 300);
        let r = boolean(&[outer.as_slice()], &[h1.as_slice(), h2.as_slice()], BoolOp::Difference);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 3, "outer + 2 holes");
        let ring = keyhole(&r[0]);
        assert_eq!(ring.len(), 4 + 2 * (4 + 2), "outer + each hole + its two bridge points");
        // Same area (the slits have none), and the union of the keyholed ring
        // recovers the original polygon with its holes.
        assert_eq!(signed_area2(&ring), region_area2(&r));
        let back = union_clipped([ring.as_slice()], None, Point::new(0, 0));
        assert_eq!(region_area2(&back), region_area2(&r));
        assert_eq!(back[0].len(), 3, "the holes come back out of the slit ring");
    }

    #[test]
    fn keyhole_of_a_holeless_polygon_is_its_outer_ring() {
        let r = union_clipped([rect(0, 0, 10, 10).as_slice()], None, Point::new(0, 0));
        assert_eq!(keyhole(&r[0]), r[0][0]);
    }

    #[test]
    fn boolean_ops_areas() {
        let a = rect(0, 0, 100, 100);
        let b = rect(50, 50, 100, 100);
        let area = |op| region_area2(&boolean(&[a.as_slice()], &[b.as_slice()], op)) / 2;
        assert_eq!(area(BoolOp::Union), 10000 + 10000 - 2500);
        assert_eq!(area(BoolOp::Intersect), 2500);
        assert_eq!(area(BoolOp::Difference), 7500);
        assert_eq!(area(BoolOp::Xor), 15000);
    }
}
