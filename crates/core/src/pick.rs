//! Hit-testing — pick the base shape under a point (Phase 3 selection).
//!
//! ## No R-tree, no landmine
//!
//! A hit-test is a **point** query, and Phase 2b already partitioned each large
//! cell into spatially-bounded chunks (~a few × 10 k shapes each). So the pick is
//! a chunk-bbox prefilter + a linear point-in-polygon scan of just the chunk(s)
//! whose bbox contains the point — no R-tree, no index build. This *eliminates*
//! the skeleton's lazy-index landmine (building `caravel_core`'s 2.5 M-shape
//! R-tree was ~861 ms) rather than mitigating it: a first click into `caravel_core`
//! now scans the containing chunk(s) (~35–70 k shapes, sub-millisecond) and builds
//! nothing. The R-tree stays only for the debug visible-shape count in `cull`.
//!
//! ## Under the hierarchy
//!
//! [`pick_hierarchy`] walks the instance tree with a transform stack (like
//! [`crate::cull`]), pruning subtrees whose world bbox misses the point,
//! inverse-mapping the point into each candidate cell's local frame, and picking
//! the smallest **world-area** hit across the whole hierarchy — so a click lands
//! on the small leaf feature under the cursor, not the big cell containing it.
//! Editing the returned `(cell, shape)` edits the cell **master**, so every
//! placement updates (standard layout-editor semantics).

use crate::db::{Cell, CellDb, CellId};
use crate::transform::Transform;
use crate::types::{Bbox, Point};

/// A hierarchy hit: the base shape (cell master + index) plus the world transform
/// of the placement that was clicked (for the selection highlight and for mapping
/// a world-space drag into the cell's local frame).
#[derive(Debug, Clone, Copy)]
pub struct PickHit {
    pub id: (CellId, u32),
    pub world_tf: Transform,
}

/// Pick the shape under cell-local point `p`, or `None`. When several shapes
/// overlap the point, the one with the **smallest bbox area** wins — the most
/// specific / visually topmost pick, which is what a user expects when clicking
/// a small feature sitting on a big one.
///
/// Chunk-bbox prefilter + linear scan of the containing chunk(s) — no R-tree
/// build (see the module note).
pub fn pick_in_cell(cell: &Cell, p: Point) -> Option<u32> {
    let mut best: Option<(u32, i64)> = None;
    for chunk in &cell.chunks {
        if !bbox_contains(&chunk.bbox, p) {
            continue; // point outside this chunk's (loose) bbox
        }
        for i in chunk.start..chunk.end {
            let pts = cell.shapes.points(i as usize);
            // Cheap per-shape bbox reject before the full point-in-polygon test.
            if !shape_bbox_contains(pts, p) {
                continue;
            }
            if point_in_polygon(pts, p) {
                let area = bbox_area(pts);
                if best.map_or(true, |(_, a)| area < a) {
                    best = Some((i, area));
                }
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Pick the shape under a **world** point, walking the instance hierarchy. Returns
/// the master `(cell, shape)` and the clicked placement's world transform.
pub fn pick_hierarchy(db: &CellDb, world: Point) -> Option<PickHit> {
    let mut best: Option<(PickHit, f64)> = None;
    walk_pick(db, db.top, Transform::IDENTITY, world, &mut best);
    best.map(|(hit, _)| hit)
}

/// Recursive pick walk: prune by world bbox, test this cell's own shapes, descend
/// into children (and AREF elements) whose world footprint contains the point.
fn walk_pick(
    db: &CellDb,
    id: CellId,
    world_tf: Transform,
    world: Point,
    best: &mut Option<(PickHit, f64)>,
) {
    let local_bb = db.local_bbox(id);
    if local_bb.min.x > local_bb.max.x {
        return; // empty cell
    }
    let world_bb = world_tf.apply_bbox(&local_bb);
    if !bboxf_contains(&world_bb, world) {
        return; // point outside this subtree
    }

    // Inverse-map the world point into this cell's local frame, then test shapes.
    let cell = db.cell(id);
    let (lx, ly) = world_tf.inverse().apply_point(world);
    let local = Point::new(lx.round() as i32, ly.round() as i32);
    if let Some(idx) = pick_in_cell(cell, local) {
        // Compare hits by WORLD area so a small leaf feature beats the big cell
        // it sits inside, regardless of each cell's local scale.
        let area = bbox_area(cell.shapes.points(idx as usize)) as f64 * world_tf.det().abs();
        if best.as_ref().map_or(true, |&(_, a)| area < a) {
            *best = Some((PickHit { id: (id, idx), world_tf }, area));
        }
    }

    // Descend into child instances whose world footprint contains the point.
    for inst in &cell.instances {
        match inst.kind {
            crate::db::InstanceKind::Single => {
                walk_pick(db, inst.child, world_tf.then(&inst.transform), world, best);
            }
            crate::db::InstanceKind::Array { cols, rows, col_step, row_step } => {
                if cols <= 0 || rows <= 0 {
                    continue;
                }
                // A ~2 nm box around the point picks out the lattice element(s)
                // whose nominal footprint it lands in; each element's own world-bbox
                // prune (in the recursive call) drops any conservative extras.
                let wp = world.x as f64;
                let hp = world.y as f64;
                let dot = crate::transform::Bboxf::new(wp - 1.0, hp - 1.0, wp + 1.0, hp + 1.0);
                let child_bb = db.local_bbox(inst.child);
                let (c0, c1, r0, r1) = crate::cull::visible_range(
                    &dot, world_tf, inst.transform, &child_bb, col_step, row_step, cols, rows,
                );
                for r in r0..=r1 {
                    for c in c0..=c1 {
                        let t = world_tf.then(&crate::cull::placement(&inst.transform, col_step, row_step, c, r));
                        walk_pick(db, inst.child, t, world, best);
                    }
                }
            }
        }
    }
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

fn bboxf_contains(b: &crate::transform::Bboxf, p: Point) -> bool {
    (p.x as f64) >= b.min_x && (p.x as f64) <= b.max_x && (p.y as f64) >= b.min_y && (p.y as f64) <= b.max_y
}

/// Does shape `pts`' axis-aligned bbox contain `p`? A cheap reject before the full
/// point-in-polygon test in the linear scan.
fn shape_bbox_contains(pts: &[Point], p: Point) -> bool {
    let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for q in pts {
        lo_x = lo_x.min(q.x);
        lo_y = lo_y.min(q.y);
        hi_x = hi_x.max(q.x);
        hi_y = hi_y.max(q.y);
    }
    p.x >= lo_x && p.x <= hi_x && p.y >= lo_y && p.y <= hi_y
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

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");

    /// Picking under the transform stack: hier is TOP → AREF(ROW) → AREF(LEAF),
    /// so a click inside a LEAF placement must resolve to the LEAF **master**
    /// (editing it updates all placements). Element (0,0) of the grid sits at the
    /// block origin (5_000_000, 5_000_000); LEAF's base tile is (0,0)–(360,400).
    #[test]
    fn picks_leaf_shape_through_two_arefs() {
        let db = crate::gds::load_gds_db(HIER).unwrap();
        let leaf = db.id_of("LEAF").expect("LEAF cell");

        // A point well inside element (0,0)'s base tile.
        let hit = pick_hierarchy(&db, Point::new(5_000_100, 5_000_100)).expect("a hit");
        assert_eq!(hit.id.0, leaf, "must resolve to the LEAF master, not TOP/ROW");
        // The clicked placement maps LEAF-local (0,0) to ~the block origin.
        let (ox, oy) = hit.world_tf.apply(0.0, 0.0);
        assert!((ox - 5_000_000.0).abs() < 1.0 && (oy - 5_000_000.0).abs() < 1.0);

        // A point in element (2,0): origin + 2 * COL_PITCH(1000) = 5_002_000.
        let hit2 = pick_hierarchy(&db, Point::new(5_002_100, 5_000_100)).expect("a hit in col 2");
        assert_eq!(hit2.id.0, leaf);
        let (ox2, _) = hit2.world_tf.apply(0.0, 0.0);
        assert!((ox2 - 5_002_000.0).abs() < 1.0, "picked the wrong AREF column");
    }

    #[test]
    fn empty_space_picks_nothing() {
        let db = crate::gds::load_gds_db(HIER).unwrap();
        // Far outside the block.
        assert!(pick_hierarchy(&db, Point::new(0, 0)).is_none());
    }
}
