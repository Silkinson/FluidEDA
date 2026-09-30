//! Edge-pair checks over a **merged** region — the measuring half of
//! merge-first DRC (the merging half is [`crate::boolean`]).
//!
//! After a layer's shapes are unioned, every boundary edge has the region's
//! interior on its **left** (outer rings CCW, holes CW). Width and spacing are
//! then the same question asked of opposite sides:
//!
//! * **width** — two boundary edges facing each other *across the interior*
//!   closer than `min_width` (a thin arm, a narrow neck, an acute tip);
//! * **space** — two boundary edges facing each other *across the exterior*
//!   closer than `min_spacing`. This covers the gap between two separate
//!   polygons AND a notch inside one polygon (a U whose arms are too close) —
//!   the case the old per-shape-pair check could never see.
//!
//! Touching/overlapping shapes need no special case any more: they are one
//! polygon after the union, so there is no edge pair between them to measure.
//!
//! ## The pair test (Euclidean metric, KLayout-style)
//!
//! For edges `a`, `b` (not adjacent in their ring):
//!   1. **angle filter** — they must run in opposite directions
//!      (`dir(a)·dir(b) < 0`, i.e. under 90° between `a` and reversed `b`), so
//!      perpendicular neighbours at a corner never pair up;
//!   2. **facing** — clip each edge to the other's open half-plane on the
//!      checked side (interior for width, exterior for space); both clipped
//!      pieces must be non-empty;
//!   3. **distance** — the clipped pieces' minimum distance must be `< d`.
//!
//! Adjacent edges are checked for an **acute** vertex instead: an interior
//! angle under 90° is a width violation (the region pinches to nothing at the
//! tip), an exterior angle under 90° is a space violation (an acute notch).
//! Manhattan and 45°-bend geometry never has either.
//!
//! The reported marker is the bbox of the edge **portions** within `d` of each
//! other (exact convex intervals, not just the closest points), so a violation
//! region is intrinsic to the geometry — which is what lets a tiled check clip
//! markers to disjoint tile cores without losing or double-counting any part.
//!
//! Decisions are exact: every sign test is integer (i128); distances are f64
//! over coordinate *differences*, which are exact for any realistic tile.

use crate::boolean::Region;
use crate::types::{Bbox, Point};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

/// Which side of the boundary a check measures across.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facing {
    /// Across the interior — min **width**.
    Interior,
    /// Across the exterior — min **space** (incl. same-polygon notches).
    Exterior,
}

/// A float box `[x0, y0, x1, y1]` (a violation marker before tile clipping).
pub type BoxF = [f64; 4];

/// A boundary edge `p0 → p1` (region interior on its left), plus its position
/// in its ring for adjacency tests.
#[derive(Debug, Clone, Copy)]
struct Edge {
    p0: Point,
    p1: Point,
    ring: u32,
    idx: u32,
    len: u32,
}

/// A segment in f64, for the clipped pieces.
#[derive(Debug, Clone, Copy)]
struct Seg {
    p: (f64, f64),
    q: (f64, f64),
}

impl Seg {
    fn at(&self, t: f64) -> (f64, f64) {
        (self.p.0 + t * (self.q.0 - self.p.0), self.p.1 + t * (self.q.1 - self.p.1))
    }
}

/// Collect the region's boundary edges, skipping edges that lie ON the clip
/// window's border — those are artifacts of clipping a tile, not real geometry
/// (a real edge exactly on the border is at least `reach` from the tile core, so
/// it cannot take part in any violation the tile reports).
fn collect_edges(region: &Region, window: Option<&Bbox>) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut ring_id = 0u32;
    for poly in region {
        for ring in poly {
            let n = ring.len();
            if n < 2 {
                ring_id += 1;
                continue;
            }
            for i in 0..n {
                let (p0, p1) = (ring[i], ring[(i + 1) % n]);
                if p0 == p1 {
                    continue;
                }
                if let Some(w) = window {
                    let on_border = (p0.x == w.min.x && p1.x == w.min.x)
                        || (p0.x == w.max.x && p1.x == w.max.x)
                        || (p0.y == w.min.y && p1.y == w.min.y)
                        || (p0.y == w.max.y && p1.y == w.max.y);
                    if on_border {
                        continue;
                    }
                }
                edges.push(Edge { p0, p1, ring: ring_id, idx: i as u32, len: n as u32 });
            }
            ring_id += 1;
        }
    }
    edges
}

#[inline]
fn delta(e: &Edge) -> (i64, i64) {
    ((e.p1.x - e.p0.x) as i64, (e.p1.y - e.p0.y) as i64)
}

#[inline]
fn dot(a: (i64, i64), b: (i64, i64)) -> i128 {
    a.0 as i128 * b.0 as i128 + a.1 as i128 * b.1 as i128
}

#[inline]
fn cross(a: (i64, i64), b: (i64, i64)) -> i128 {
    a.0 as i128 * b.1 as i128 - a.1 as i128 * b.0 as i128
}

/// Check `region` for boundary edges facing each other across `facing` closer
/// than `d` nm; push one marker box per violating edge pair / acute vertex.
/// `window` is the clip window the region was cut to (its border edges are
/// skipped), or `None` for an unclipped region.
pub fn check_region(region: &Region, d: i32, facing: Facing, window: Option<&Bbox>, out: &mut Vec<BoxF>) {
    if d <= 0 {
        return;
    }
    let edges = collect_edges(region, window);
    if edges.len() < 2 {
        return;
    }
    let df = d as f64;
    let mut test = |a: &Edge, b: &Edge| {
        let m = if adjacent(a, b) { acute_vertex(a, b, df, facing) } else { pair(a, b, df, facing) };
        if let Some(m) = m {
            out.push(m);
        }
    };

    // Partition by orientation. Facing edges run in opposite directions, so a
    // horizontal edge can only pair with another horizontal (or a diagonal),
    // a vertical with a vertical (or a diagonal) — never H with V. That makes
    // Manhattan geometry two 1-D problems (strip sweeps below); only the
    // diagonal edges, rare in practice, need the general R-tree.
    let (mut h, mut v, mut diag) = (Vec::new(), Vec::new(), Vec::new());
    for (i, e) in edges.iter().enumerate() {
        if e.p0.y == e.p1.y {
            h.push(i as u32);
        } else if e.p0.x == e.p1.x {
            v.push(i as u32);
        } else {
            diag.push(i as u32);
        }
    }
    // Horizontal edges: position = y, extent along x.
    strip_sweep(&edges, &h, d, |e| (e.p0.y, e.p0.x.min(e.p1.x), e.p0.x.max(e.p1.x)), &mut test);
    // Vertical edges: position = x, extent along y (the same sweep, transposed).
    strip_sweep(&edges, &v, d, |e| (e.p0.x, e.p0.y.min(e.p1.y), e.p0.y.max(e.p1.y)), &mut test);

    if diag.is_empty() {
        return;
    }
    // Every pair involving a diagonal: an R-tree over all edges, queried with
    // each diagonal's bbox inflated by `d` (a lower bound on the distance).
    let tree = RTree::bulk_load(
        edges
            .iter()
            .enumerate()
            .map(|(i, e)| {
                GeomWithData::new(
                    Rectangle::from_corners(
                        [e.p0.x.min(e.p1.x) as f64, e.p0.y.min(e.p1.y) as f64],
                        [e.p0.x.max(e.p1.x) as f64, e.p0.y.max(e.p1.y) as f64],
                    ),
                    i as u32,
                )
            })
            .collect::<Vec<_>>(),
    );
    let is_diag = |e: &Edge| e.p0.x != e.p1.x && e.p0.y != e.p1.y;
    for &i in &diag {
        let a = &edges[i as usize];
        let env = AABB::from_corners(
            [a.p0.x.min(a.p1.x) as f64 - df, a.p0.y.min(a.p1.y) as f64 - df],
            [a.p0.x.max(a.p1.x) as f64 + df, a.p0.y.max(a.p1.y) as f64 + df],
        );
        for cand in tree.locate_in_envelope_intersecting(&env) {
            let j = cand.data;
            let b = &edges[j as usize];
            // Diagonal×diagonal once (from the lower index); diagonal×H/V always
            // (the sweeps never see those pairs).
            if j == i || (is_diag(b) && j < i) {
                continue;
            }
            test(a, b);
        }
    }
}

/// Width of the sweep strips, as a multiple of the rule distance (and at least
/// this many nm): wide enough that a short edge lands in one or two strips,
/// narrow enough that each strip's y-sorted band scan stays tiny.
const STRIP_RULES: i64 = 8;
const STRIP_MIN_NM: i64 = 2_000;

/// All pairs of axis-parallel edges within `d` of each other, where each edge
/// is `(pos, lo, hi)` — `pos` the coordinate across the edge, `[lo, hi]` its
/// extent along it. Edges are bucketed into strips along the extent axis
/// (each into every strip its `[lo − d, hi + d]` touches), each strip sorted by
/// `pos`; a pair is enumerated only in the strip holding `max(lo_a, lo_b)` —
/// a point both inflated extents contain — so it is tested exactly once. Same-
/// `pos` edges are collinear and can never face, so the band scan starts past
/// them. On routed metal the band `(pos, pos + d)` is almost always empty.
fn strip_sweep<K, F>(edges: &[Edge], idx: &[u32], d: i32, key: K, test: &mut F)
where
    K: Fn(&Edge) -> (i32, i32, i32),
    F: FnMut(&Edge, &Edge),
{
    if idx.len() < 2 {
        return;
    }
    let d = d as i64;
    let keys: Vec<(i64, i64, i64)> = idx
        .iter()
        .map(|&i| {
            let (p, lo, hi) = key(&edges[i as usize]);
            (p as i64, lo as i64, hi as i64)
        })
        .collect();
    let lo_min = keys.iter().map(|k| k.1).min().unwrap() - d;
    let width = (STRIP_RULES * d).max(STRIP_MIN_NM);
    let strip = |x: i64| (x - lo_min).div_euclid(width);
    let nstrips = (strip(keys.iter().map(|k| k.2).max().unwrap() + d) + 1) as usize;

    let mut strips: Vec<Vec<u32>> = vec![Vec::new(); nstrips];
    for (k, &(_, lo, hi)) in keys.iter().enumerate() {
        for s in strip(lo - d)..=strip(hi + d) {
            strips[s as usize].push(k as u32);
        }
    }
    for (s, members) in strips.iter_mut().enumerate() {
        if members.len() < 2 {
            continue;
        }
        members.sort_unstable_by_key(|&k| keys[k as usize].0);
        let n = members.len();
        let mut first_above = 0usize; // first member with pos > current pos
        for a in 0..n {
            let ka = keys[members[a] as usize];
            if first_above <= a {
                first_above = a + 1;
            }
            while first_above < n && keys[members[first_above] as usize].0 == ka.0 {
                first_above += 1;
            }
            for b in first_above..n {
                let kb = keys[members[b] as usize];
                let dp = kb.0 - ka.0;
                if dp >= d {
                    break; // sorted by pos: everything further is farther
                }
                // Owner strip: the pair is handled where max(lo) falls.
                let anchor = ka.1.max(kb.1);
                if strip(anchor) != s as i64 {
                    continue;
                }
                let gap = (ka.1.max(kb.1) - ka.2.min(kb.2)).max(0);
                if dp * dp + gap * gap >= d * d {
                    continue;
                }
                test(&edges[idx[members[a] as usize] as usize], &edges[idx[members[b] as usize] as usize]);
            }
        }
    }
}

/// Consecutive edges of the same ring.
fn adjacent(a: &Edge, b: &Edge) -> bool {
    a.ring == b.ring && (b.idx == (a.idx + 1) % a.len || a.idx == (b.idx + 1) % a.len)
}

/// An acute vertex between adjacent edges: interior angle < 90° is a width
/// violation, exterior angle < 90° a space violation. The marker covers the
/// stretch of both edges, from the vertex, over which they are closer than `d`.
fn acute_vertex(a: &Edge, b: &Edge, d: f64, facing: Facing) -> Option<BoxF> {
    // Order as (incoming, outgoing) around the shared vertex.
    let (e_in, e_out) = if a.p1 == b.p0 { (a, b) } else if b.p1 == a.p0 { (b, a) } else { return None };
    let (din, dout) = (delta(e_in), delta(e_out));
    let dt = dot(din, dout);
    let cr = cross(din, dout);
    if dt >= 0 {
        return None; // turn of at most 90° — no acute angle on either side
    }
    let acute_side = if cr > 0 { Facing::Interior } else if cr < 0 { Facing::Exterior } else { return None };
    if acute_side != facing {
        return None;
    }
    let v = (e_in.p1.x as f64, e_in.p1.y as f64);
    let (lin, lout) = (len(din), len(dout));
    // The acute angle α = π − |turn|; points x from the vertex along both edges
    // are 2·x·sin(α/2) apart, so they are closer than d for x < d / (2 sin(α/2)).
    let turn = (cr.unsigned_abs() as f64).atan2(dt as f64); // in (π/2, π]
    let alpha = std::f64::consts::PI - turn;
    let reach = if alpha > 1e-12 { d / (2.0 * (alpha * 0.5).sin()) } else { f64::INFINITY };
    let back = (-(din.0 as f64) / lin, -(din.1 as f64) / lin);
    let fwd = (dout.0 as f64 / lout, dout.1 as f64 / lout);
    let (ri, ro) = (reach.min(lin), reach.min(lout));
    let pts = [v, (v.0 + back.0 * ri, v.1 + back.1 * ri), (v.0 + fwd.0 * ro, v.1 + fwd.1 * ro)];
    Some(bbox_of(&pts))
}

fn len(d: (i64, i64)) -> f64 {
    ((d.0 as f64).powi(2) + (d.1 as f64).powi(2)).sqrt()
}

/// The non-adjacent pair test (see the module note).
fn pair(a: &Edge, b: &Edge, d: f64, facing: Facing) -> Option<BoxF> {
    let (da, db) = (delta(a), delta(b));
    // 1. Angle filter: facing edges run in opposite directions.
    if dot(da, db) >= 0 {
        return None;
    }
    // 2. Facing: each edge clipped to the other's open half-plane on `facing`.
    let bs = clip_to_side(b, a, facing)?;
    let as_ = clip_to_side(a, b, facing)?;
    // 3. Distance.
    let (d2, ta, tb) = closest(as_, bs);
    if d2 >= d * d {
        return None;
    }
    let (a0, a1) = within(as_, bs, d, ta);
    let (b0, b1) = within(bs, as_, d, tb);
    Some(bbox_of(&[as_.at(a0), as_.at(a1), bs.at(b0), bs.at(b1)]))
}

/// The piece of `s` strictly on `facing` side of `l`'s supporting line (left =
/// interior). `None` if nothing of `s` is there. Sign tests are exact integers.
fn clip_to_side(s: &Edge, l: &Edge, facing: Facing) -> Option<Seg> {
    let dl = delta(l);
    let side = |p: Point| -> i128 {
        let c = cross(dl, ((p.x - l.p0.x) as i64, (p.y - l.p0.y) as i64));
        match facing {
            Facing::Interior => c,
            Facing::Exterior => -c,
        }
    };
    let (v0, v1) = (side(s.p0), side(s.p1));
    let p = (s.p0.x as f64, s.p0.y as f64);
    let q = (s.p1.x as f64, s.p1.y as f64);
    match (v0 > 0, v1 > 0) {
        (false, false) => None,
        (true, true) => Some(Seg { p, q }),
        _ => {
            // Crosses the line: keep the part on the positive side.
            let t = v0 as f64 / (v0 as f64 - v1 as f64);
            let full = Seg { p, q };
            let x = full.at(t);
            Some(if v0 > 0 { Seg { p, q: x } } else { Seg { p: x, q } })
        }
    }
}

/// Closest points between two segments (Ericson, *Real-Time Collision
/// Detection* §5.1.9): returns `(dist², s, t)` with `s`, `t` the parameters on
/// `p` and `q`. Handles parallel and degenerate segments.
fn closest(p: Seg, q: Seg) -> (f64, f64, f64) {
    let d1 = (p.q.0 - p.p.0, p.q.1 - p.p.1);
    let d2 = (q.q.0 - q.p.0, q.q.1 - q.p.1);
    let r = (p.p.0 - q.p.0, p.p.1 - q.p.1);
    let a = d1.0 * d1.0 + d1.1 * d1.1;
    let e = d2.0 * d2.0 + d2.1 * d2.1;
    let f = d2.0 * r.0 + d2.1 * r.1;
    let eps = 1e-12;
    let (s, t);
    if a <= eps && e <= eps {
        s = 0.0;
        t = 0.0;
    } else if a <= eps {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = d1.0 * r.0 + d1.1 * r.1;
        if e <= eps {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = d1.0 * d2.0 + d1.1 * d2.1;
            let denom = a * e - b * b;
            let mut s0 = if denom > 0.0 { ((b * f - c * e) / denom).clamp(0.0, 1.0) } else { 0.0 };
            let mut t0 = (b * s0 + f) / e;
            if t0 < 0.0 {
                t0 = 0.0;
                s0 = (-c / a).clamp(0.0, 1.0);
            } else if t0 > 1.0 {
                t0 = 1.0;
                s0 = ((b - c) / a).clamp(0.0, 1.0);
            }
            s = s0;
            t = t0;
        }
    }
    let (c1, c2) = (p.at(s), q.at(t));
    let d2_ = (c1.0 - c2.0).powi(2) + (c1.1 - c2.1).powi(2);
    (d2_, s, t)
}

/// Squared distance from point `x` to segment `s`.
fn point_seg2(x: (f64, f64), s: Seg) -> f64 {
    let v = (s.q.0 - s.p.0, s.q.1 - s.p.1);
    let w = (x.0 - s.p.0, x.1 - s.p.1);
    let l2 = v.0 * v.0 + v.1 * v.1;
    let t = if l2 > 0.0 { ((w.0 * v.0 + w.1 * v.1) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let f = s.at(t);
    (x.0 - f.0).powi(2) + (x.1 - f.1).powi(2)
}

/// The parameter interval of `s` whose points lie within `d` of `other`, given a
/// minimiser `t_star`. Distance-to-a-segment along a segment is convex, so the
/// sub-level set is one interval; its ends are found by bisection.
fn within(s: Seg, other: Seg, d: f64, t_star: f64) -> (f64, f64) {
    let d2 = d * d;
    let g = |t: f64| point_seg2(s.at(t), other);
    let lo = if g(0.0) < d2 {
        0.0
    } else {
        let (mut l, mut h) = (0.0, t_star); // g(l) >= d², g(h) < d²
        for _ in 0..48 {
            let m = 0.5 * (l + h);
            if g(m) < d2 {
                h = m
            } else {
                l = m
            }
        }
        h
    };
    let hi = if g(1.0) < d2 {
        1.0
    } else {
        let (mut l, mut h) = (t_star, 1.0); // g(l) < d², g(h) >= d²
        for _ in 0..48 {
            let m = 0.5 * (l + h);
            if g(m) < d2 {
                l = m
            } else {
                h = m
            }
        }
        l
    };
    (lo, hi)
}

fn bbox_of(pts: &[(f64, f64)]) -> BoxF {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for p in pts {
        b[0] = b[0].min(p.0);
        b[1] = b[1].min(p.1);
        b[2] = b[2].max(p.0);
        b[3] = b[3].max(p.1);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boolean::union_clipped;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h)]
    }

    fn check(rings: &[Vec<Point>], d: i32, facing: Facing) -> Vec<BoxF> {
        let region = union_clipped(rings.iter().map(|r| r.as_slice()), None, Point::new(0, 0));
        let mut out = Vec::new();
        check_region(&region, d, facing, None, &mut out);
        out
    }

    #[test]
    fn thin_arm_of_an_l_shape_is_a_width_violation() {
        // The case per-shape bbox width got wrong: an L whose bbox is wide but
        // whose vertical arm is 60 nm thin.
        let l = vec![
            Point::new(0, 0),
            Point::new(1000, 0),
            Point::new(1000, 200),
            Point::new(60, 200),
            Point::new(60, 1000),
            Point::new(0, 1000),
        ];
        let v = check(&[l], 100, Facing::Interior);
        assert_eq!(v.len(), 1, "exactly the thin arm: {v:?}");
        let m = v[0];
        assert!(m[0] >= 0.0 && m[2] <= 60.0, "marker spans the arm's width: {m:?}");
        // Euclidean metric: the outer edge is within 100 nm of the arm's inner
        // edge from y = 200 − √(100² − 60²) = 120 up, not only above the foot.
        assert!((m[1] - 120.0).abs() < 1e-3 && m[3] == 1000.0, "marker covers the arm: {m:?}");
    }

    #[test]
    fn fractured_slivers_are_not_width_violations_after_merge() {
        // 1000×200 wire fractured into pieces including a 30 nm sliver.
        let v = check(&[rect(0, 0, 400, 200), rect(400, 0, 30, 200), rect(430, 0, 570, 200)], 100, Facing::Interior);
        assert!(v.is_empty(), "merged wire is 200 wide: {v:?}");
    }

    #[test]
    fn notch_inside_one_polygon_is_a_space_violation() {
        // A U whose arms are 50 nm apart: one polygon, an exterior-facing pair.
        let u = vec![
            Point::new(0, 0),
            Point::new(1050, 0),
            Point::new(1050, 1000),
            Point::new(550, 1000),
            Point::new(550, 300),
            Point::new(500, 300),
            Point::new(500, 1000),
            Point::new(0, 1000),
        ];
        let v = check(&[u.clone()], 100, Facing::Exterior);
        assert_eq!(v.len(), 1, "the 50 nm notch: {v:?}");
        assert!(check(&[u], 100, Facing::Interior).is_empty(), "arms are 500 wide");
    }

    #[test]
    fn exact_threshold_is_legal() {
        // Gap of exactly d must pass (violation is strictly < d).
        assert!(check(&[rect(0, 0, 500, 500), rect(600, 0, 500, 500)], 100, Facing::Exterior).is_empty());
        assert_eq!(check(&[rect(0, 0, 500, 500), rect(599, 0, 500, 500)], 100, Facing::Exterior).len(), 1);
    }

    #[test]
    fn acute_tip_flags_width() {
        // A 1000-long triangle with a sharp tip: the region pinches to nothing.
        let tri = vec![Point::new(0, 0), Point::new(1000, 100), Point::new(0, 200)];
        let v = check(&[tri], 50, Facing::Interior);
        assert!(!v.is_empty(), "acute tip must flag");
        assert!(v.iter().any(|m| m[2] >= 999.0), "a marker reaches the tip: {v:?}");
    }

    #[test]
    fn diagonal_neck_between_overlapping_squares() {
        // Squares overlapping in a 50×50 corner patch: the neck is 70.7 nm wide.
        let v = check(&[rect(0, 0, 1000, 1000), rect(950, 950, 1000, 1000)], 100, Facing::Interior);
        assert!(!v.is_empty(), "70.7 nm neck < 100");
        assert!(check(&[rect(0, 0, 1000, 1000), rect(950, 950, 1000, 1000)], 70, Facing::Interior).is_empty());
    }
}
