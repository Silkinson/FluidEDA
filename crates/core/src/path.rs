//! Path → polygon (Phase 6): the outline of a wire given by its centerline.
//!
//! GDSII PATH and OASIS PATH elements are centerlines with a width and per-end
//! extensions. FluidEDA's model is polygons only, so both readers convert here.
//! (Before Phase 6 the GDS reader skipped PATHs — 480 of them in caravel and 854
//! in the sky130 standard-cell library were silently missing.)
//!
//! Outline: each segment offset by ±half-width, consecutive offsets joined at
//! their **miter** point (square corners for Manhattan wires — GDS semantics),
//! the ends pushed out along the first/last segment by the begin/end extension.
//! Coordinates round to the nm grid (exact for even widths on Manhattan paths).

use crate::types::Point;

/// The closed polygon ring (GDS convention: last point = first) of a path with
/// centerline `center`, half-width `hw` and end extensions `ext_begin` /
/// `ext_end` (nm; negative pulls the end in). `None` for a degenerate path
/// (fewer than two distinct points, or zero width).
pub fn path_to_polygon(center: &[Point], hw: f64, ext_begin: f64, ext_end: f64) -> Option<Vec<Point>> {
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(center.len());
    for p in center {
        let q = (p.x as f64, p.y as f64);
        if pts.last() != Some(&q) {
            pts.push(q);
        }
    }
    if pts.len() < 2 || hw <= 0.0 {
        return None;
    }
    let k = pts.len() - 1; // segments
    let dir: Vec<(f64, f64)> = (0..k)
        .map(|i| {
            let (dx, dy) = (pts[i + 1].0 - pts[i].0, pts[i + 1].1 - pts[i].1);
            let l = (dx * dx + dy * dy).sqrt();
            (dx / l, dy / l)
        })
        .collect();
    let nrm: Vec<(f64, f64)> = dir.iter().map(|&(dx, dy)| (-dy, dx)).collect();

    // One side of the outline (`s` = +1 left, −1 right), start to end.
    let side = |s: f64| -> Vec<(f64, f64)> {
        let mut out = Vec::with_capacity(k + 1);
        let (p0, d0, n0) = (pts[0], dir[0], nrm[0]);
        out.push((p0.0 - d0.0 * ext_begin + s * hw * n0.0, p0.1 - d0.1 * ext_begin + s * hw * n0.1));
        for j in 1..k {
            let (na, nb) = (nrm[j - 1], nrm[j]);
            let denom = 1.0 + na.0 * nb.0 + na.1 * nb.1;
            let p = pts[j];
            if denom.abs() < 1e-9 {
                continue; // a 180° reversal has no miter point; skip it
            }
            // Miter: the offset lines of the two segments meet here.
            let m = s * hw / denom;
            out.push((p.0 + (na.0 + nb.0) * m, p.1 + (na.1 + nb.1) * m));
        }
        let (pk, dk, nk) = (pts[k], dir[k - 1], nrm[k - 1]);
        out.push((pk.0 + dk.0 * ext_end + s * hw * nk.0, pk.1 + dk.1 * ext_end + s * hw * nk.1));
        out
    };
    let left = side(1.0);
    let mut right = side(-1.0);
    right.reverse();
    let mut ring: Vec<Point> = left
        .into_iter()
        .chain(right)
        .map(|(x, y)| Point::new(x.round() as i32, y.round() as i32))
        .collect();
    ring.dedup();
    if ring.len() < 3 {
        return None;
    }
    ring.push(ring[0]);
    Some(ring)
}

/// A GDSII PATH's outline: `width` (negative = absolute, i.e. not scaled by a
/// placement's magnification — the outline is the same here), PATHTYPE (0
/// flush, 1 round, 2 half-width extension, 4 explicit BGNEXTN/ENDEXTN). Round
/// ends (type 1) are approximated by half-width square extensions — none of
/// the test chips use them.
pub fn gds_path_polygon(xy: &[Point], width: i32, pathtype: i16, bgnextn: i32, endextn: i32) -> Option<Vec<Point>> {
    let hw = width.unsigned_abs() as f64 / 2.0;
    let (b, e) = match pathtype {
        1 | 2 => (hw, hw),
        4 => (bgnextn as f64, endextn as f64),
        _ => (0.0, 0.0),
    };
    path_to_polygon(xy, hw, b, e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(r: &[Point]) -> Vec<(i32, i32)> {
        let mut v: Vec<(i32, i32)> = r[..r.len() - 1].iter().map(|p| (p.x, p.y)).collect();
        v.sort();
        v
    }

    #[test]
    fn two_point_flush_path_is_its_rectangle() {
        let r = path_to_polygon(&[Point::new(0, 0), Point::new(1000, 0)], 240.0, 0.0, 0.0).unwrap();
        assert_eq!(norm(&r), vec![(0, -240), (0, 240), (1000, -240), (1000, 240)]);
        assert_eq!(r.first(), r.last(), "closed ring");
    }

    #[test]
    fn extensions_push_the_ends_out() {
        let r = path_to_polygon(&[Point::new(0, 0), Point::new(0, 500)], 50.0, 50.0, 20.0).unwrap();
        assert_eq!(norm(&r), vec![(-50, -50), (-50, 520), (50, -50), (50, 520)]);
    }

    #[test]
    fn manhattan_bend_gets_a_square_corner() {
        // An L: right 1000, then up 1000, width 200 → a 6-vertex L outline.
        let r = path_to_polygon(&[Point::new(0, 0), Point::new(1000, 0), Point::new(1000, 1000)], 100.0, 0.0, 0.0)
            .unwrap();
        assert_eq!(norm(&r), vec![(0, -100), (0, 100), (900, 100), (900, 1000), (1100, -100), (1100, 1000)]);
        let area2 = crate::boolean::signed_area2(&r).abs();
        assert_eq!(area2, 2 * (1100 * 200 + 900 * 200), "L area = two arms");
    }

    #[test]
    fn degenerate_paths_are_none() {
        assert!(path_to_polygon(&[Point::new(5, 5)], 10.0, 0.0, 0.0).is_none());
        assert!(path_to_polygon(&[Point::new(5, 5), Point::new(5, 5)], 10.0, 0.0, 0.0).is_none());
        assert!(path_to_polygon(&[Point::new(0, 0), Point::new(9, 0)], 0.0, 0.0, 0.0).is_none());
    }
}
