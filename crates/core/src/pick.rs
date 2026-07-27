//! Hit-testing — pick the base shape under a point (Phase 3 selection).
//!
//! ## The lazy-index landmine
//!
//! Each cell's R-tree is a lazy `OnceCell` built by nothing today, so the first
//! hit-test that queries a cell pays its build. On caravel's `caravel_core` (2.5 M
//! shapes) that build is ~861 ms — a UI freeze if it lands on a click. Two
//! mitigations were on the table:
//!
//!   (a) background-build every cell's index across idle frames after TTFF;
//!   (b) a coarse chunk-bbox prefilter, then build **only the queried cell's**
//!       index on demand.
//!
//! We take (b). The reason is the skeleton's scope: editing is TOP-cell only, and
//! caravel's TOP (`caravel`) has just **128 shapes — its index builds in ~0.2 ms**.
//! Option (a) would eagerly build all 1014 indices, *including* the 861 ms
//! `caravel_core` monster and 1012 others that editing never touches — pure waste.
//! Option (b) builds exactly the one small index a click needs and nothing else,
//! and the chunk-bbox prefilter returns early (no R-tree build at all) when the
//! click misses the cell's geometry entirely.
//!
//! **Worst-case first-click latency on caravel: ~0.2 ms** (building TOP's index).
//! The 861 ms cost is only reachable if a future phase lets you hit-test
//! `caravel_core` directly; when per-cell editing is un-deferred, that cell needs
//! either background building or a chunked (per-chunk) index — noted, not built.

use crate::db::Cell;
use crate::types::{Bbox, Point};

/// Pick the shape under cell-local point `p`, or `None`. When several shapes
/// overlap the point, the one with the **smallest bbox area** wins — the most
/// specific / visually topmost pick, which is what a user expects when clicking
/// a small feature sitting on a big one.
///
/// Builds `cell`'s R-tree lazily on first call (see the module landmine note);
/// a click outside every chunk returns before any build.
pub fn pick_in_cell(cell: &Cell, p: Point) -> Option<u32> {
    // Coarse prefilter: a click outside all chunk bboxes hits nothing, and must
    // not trigger the (possibly expensive) R-tree build.
    if !cell.chunks.iter().any(|c| bbox_contains(&c.bbox, p)) {
        return None;
    }

    let mut best: Option<(u32, i64)> = None;
    for i in cell.index().at_point(p.x as f64, p.y as f64) {
        let pts = cell.shapes.points(i as usize);
        if point_in_polygon(pts, p) {
            let area = bbox_area(pts);
            if best.map_or(true, |(_, a)| area < a) {
                best = Some((i, area));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Even-odd ray-cast point-in-polygon on a closed ring, exact in `i64` (no
/// float, no overflow: `i32 × i32` fits `i64`). A repeated closing point just
/// contributes a zero-length edge, so both closed and open rings work.
pub fn point_in_polygon(pts: &[Point], p: Point) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        // Does a horizontal ray at p.y cross edge a→b?
        if (a.y > p.y) != (b.y > p.y) {
            let dy = (b.y - a.y) as i64;
            let lhs = (p.x - a.x) as i64 * dy;
            let rhs = (b.x - a.x) as i64 * (p.y - a.y) as i64;
            // p is left of the edge ⇒ ray crosses. Sign depends on dy's direction.
            if (dy > 0 && lhs < rhs) || (dy < 0 && lhs > rhs) {
                inside = !inside;
            }
        }
    }
    inside
}

fn bbox_contains(b: &Bbox, p: Point) -> bool {
    b.min.x <= b.max.x && p.x >= b.min.x && p.x <= b.max.x && p.y >= b.min.y && p.y <= b.max.y
}

fn bbox_area(pts: &[Point]) -> i64 {
    let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in pts {
        lo_x = lo_x.min(p.x);
        lo_y = lo_y.min(p.y);
        hi_x = hi_x.max(p.x);
        hi_y = hi_y.max(p.y);
    }
    (hi_x - lo_x) as i64 * (hi_y - lo_y) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(x: i32, y: i32, s: i32) -> Vec<Point> {
        // Closed ring with the repeated closing point, as GDS boundaries carry.
        vec![
            Point::new(x, y),
            Point::new(x + s, y),
            Point::new(x + s, y + s),
            Point::new(x, y + s),
            Point::new(x, y),
        ]
    }

    #[test]
    fn inside_and_outside() {
        let r = sq(0, 0, 100);
        assert!(point_in_polygon(&r, Point::new(50, 50)));
        assert!(!point_in_polygon(&r, Point::new(150, 50)));
        assert!(!point_in_polygon(&r, Point::new(-1, 50)));
    }

    #[test]
    fn concave_L_shape() {
        // An L: point in the notch must read as outside.
        let l = vec![
            Point::new(0, 0),
            Point::new(100, 0),
            Point::new(100, 40),
            Point::new(40, 40),
            Point::new(40, 100),
            Point::new(0, 100),
            Point::new(0, 0),
        ];
        assert!(point_in_polygon(&l, Point::new(20, 20)));
        assert!(point_in_polygon(&l, Point::new(70, 20)));
        assert!(!point_in_polygon(&l, Point::new(70, 70)), "notch is outside");
    }

    #[test]
    fn negative_coords() {
        let r = sq(-500, -500, 200);
        assert!(point_in_polygon(&r, Point::new(-400, -400)));
        assert!(!point_in_polygon(&r, Point::new(-100, -100)));
    }
}
