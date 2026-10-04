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

/// A chunk a streamed design must have in memory before a pick at some point
/// can be trusted (Phase 7.4): shape chunk or placement chunk `(cell, i)`
/// whose footprint holds the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChunkNeed {
    Shapes(CellId, u32),
    Placements(CellId, u32),
}

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
    pick_in_cell_except(cell, 0, p, None, &mut Vec::new())
}

/// [`pick_in_cell`] skipping base shapes in `skip` (tombstoned / moved away).
/// A streamed cell's chunks that hold the point but are not in memory are
/// skipped and reported in `needs` (Phase 7.4).
fn pick_in_cell_except(
    cell: &Cell,
    id: CellId,
    p: Point,
    skip: Option<&std::collections::HashSet<u32>>,
    needs: &mut Vec<ChunkNeed>,
) -> Option<u32> {
    let mut best: Option<(u32, i64)> = None;
    for (ci, chunk) in cell.chunks.iter().enumerate() {
        if !bbox_contains(&chunk.bbox, p) {
            continue; // point outside this chunk's (loose) bbox
        }
        let Some((store, base)) = cell.chunk_store(ci) else {
            needs.push(ChunkNeed::Shapes(id, ci as u32));
            continue;
        };
        for i in chunk.start..chunk.end {
            if skip.is_some_and(|s| s.contains(&i)) {
                continue;
            }
            let pts = store.points((i - base) as usize);
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
        let area = bbox_area(cell.shape_points(idx as usize).unwrap_or(&[])) as f64 * world_tf.det().abs();
        if best.as_ref().map_or(true, |&(_, a)| area < a) {
            *best = Some((PickHit { id: (id, idx), world_tf }, area));
        }
    }

    // Descend into child instances whose world footprint contains the point.
    for (_, inst) in cell.resident_instances() {
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

/// What a [`pick_composite`] resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    /// A base shape — or its moved / vertex-edited copy (same id).
    Base(crate::edit::BaseShapeId),
    /// A drawn shape, in whatever cell it was drawn into.
    Added(crate::edit::AddId),
}

/// A composite hit: the target, the world transform of the placement it was
/// hit at (identity for the TOP cell), and the placement **path** from TOP down
/// to the hit's cell — one [`crate::edit::PlacementKey`] per hierarchy hop
/// (empty for a TOP-cell hit). "Edit this placement only" flattens along it.
#[derive(Debug, Clone)]
pub struct CompositeHit {
    pub target: PickTarget,
    pub world_tf: Transform,
    pub path: Vec<crate::edit::PlacementKey>,
}

/// Pick what is *drawn* under a world point — Phase 5d: base shapes minus
/// tombstones, moved / vertex-edited copies at their **current** position, and
/// drawn shapes in **any** cell (a shape drawn into a master is hit at every
/// placement) — walking the instance hierarchy like [`pick_hierarchy`]. The
/// smallest world-area candidate wins; on a tie the overlay (drawn on top)
/// wins. A cell's footprint for pruning is its load-time bbox grown by its own
/// overlay geometry, so a shape drawn just past a cell's original extent is
/// still hit (an ancestor's precomputed bbox is not grown — a documented edge).
pub fn pick_composite(db: &CellDb, overlay: &crate::edit::EditOverlay, world: Point) -> Option<CompositeHit> {
    pick_composite_needs(db, overlay, world).0
}

/// [`pick_composite`] for a design that may be streamed (Phase 7.4): also the
/// chunks under the point that are not in memory. The hit is only final when
/// that list is empty — a missing chunk may hold a better candidate, or the
/// placements to descend through — so a streamed design fetches them and
/// picks again. In-core designs never report any.
pub fn pick_composite_needs(
    db: &CellDb,
    overlay: &crate::edit::EditOverlay,
    world: Point,
) -> (Option<CompositeHit>, Vec<ChunkNeed>) {
    let mut grown: std::collections::HashMap<CellId, Bbox> = std::collections::HashMap::new();
    let mut grow = |cell: CellId, pts: &[Point]| {
        let b = grown.entry(cell).or_insert_with(|| db.local_bbox(cell));
        for p in pts {
            b.expand(*p);
        }
    };
    for (&(c, _), s) in &overlay.edited {
        grow(c, &s.points);
    }
    for a in &overlay.added {
        grow(a.cell, &a.shape.points);
    }
    let mut best: Option<(CompositeHit, f64)> = None;
    let mut path = Vec::new();
    let mut needs = Vec::new();
    walk_composite(db, overlay, &grown, db.top, Transform::IDENTITY, world, &mut path, &mut best, &mut needs);
    needs.sort_unstable();
    needs.dedup();
    (best.map(|(h, _)| h), needs)
}

#[allow(clippy::too_many_arguments)]
fn walk_composite(
    db: &CellDb,
    overlay: &crate::edit::EditOverlay,
    grown: &std::collections::HashMap<CellId, Bbox>,
    id: CellId,
    world_tf: Transform,
    world: Point,
    path: &mut Vec<crate::edit::PlacementKey>,
    best: &mut Option<(CompositeHit, f64)>,
    needs: &mut Vec<ChunkNeed>,
) {
    let local_bb = grown.get(&id).copied().unwrap_or_else(|| db.local_bbox(id));
    if local_bb.min.x > local_bb.max.x {
        return;
    }
    if !bboxf_contains(&world_tf.apply_bbox(&local_bb), world) {
        return;
    }
    let cell = db.cell(id);
    let (lx, ly) = world_tf.inverse().apply_point(world);
    let local = Point::new(lx.round() as i32, ly.round() as i32);
    let scale = world_tf.det().abs();
    let mut offer = |target: PickTarget, pts: &[Point], overlay_wins_ties: bool| {
        let area = bbox_area(pts) as f64 * scale;
        let better = best.as_ref().map_or(true, |(_, a)| if overlay_wins_ties { area <= *a } else { area < *a });
        if better {
            *best = Some((CompositeHit { target, world_tf, path: path.clone() }, area));
        }
    };
    if let Some(idx) = pick_in_cell_except(cell, id, local, overlay.suppressed_in(id), needs) {
        offer(PickTarget::Base((id, idx)), cell.shape_points(idx as usize).unwrap_or(&[]), false);
    }
    if overlay.edits_cell(id) {
        for (&(c, i), s) in &overlay.edited {
            if c == id && shape_bbox_contains(&s.points, local) && point_in_polygon(&s.points, local) {
                offer(PickTarget::Base((c, i)), &s.points, true);
            }
        }
        for a in overlay.added.iter().filter(|a| a.cell == id) {
            if shape_bbox_contains(&a.shape.points, local) && point_in_polygon(&a.shape.points, local) {
                offer(PickTarget::Added(a.id), &a.shape.points, true);
            }
        }
    }
    // A streamed cell's placement chunks under the point that are not in
    // memory hide whatever they place there.
    if let Some(remote) = &cell.remote {
        for (ci, ch) in cell.inst_chunks.iter().enumerate() {
            if remote.chunks[ci].is_none() && bbox_contains(&ch.bbox, local) {
                needs.push(ChunkNeed::Placements(id, ci as u32));
            }
        }
    }
    for (ii, inst) in cell.resident_instances() {
        match inst.kind {
            crate::db::InstanceKind::Single => {
                if overlay.is_hidden((id, ii, 0, 0)) {
                    continue;
                }
                path.push((id, ii, 0, 0));
                walk_composite(db, overlay, grown, inst.child, world_tf.then(&inst.transform), world, path, best, needs);
                path.pop();
            }
            crate::db::InstanceKind::Array { cols, rows, col_step, row_step } => {
                if cols <= 0 || rows <= 0 {
                    continue;
                }
                let (wp, hp) = (world.x as f64, world.y as f64);
                let dot = crate::transform::Bboxf::new(wp - 1.0, hp - 1.0, wp + 1.0, hp + 1.0);
                let child_bb = grown.get(&inst.child).copied().unwrap_or_else(|| db.local_bbox(inst.child));
                let (c0, c1, r0, r1) = crate::cull::visible_range(
                    &dot, world_tf, inst.transform, &child_bb, col_step, row_step, cols, rows,
                );
                for r in r0..=r1 {
                    for c in c0..=c1 {
                        if overlay.is_hidden((id, ii, c, r)) {
                            continue;
                        }
                        let t = world_tf.then(&crate::cull::placement(&inst.transform, col_step, row_step, c, r));
                        path.push((id, ii, c, r));
                        walk_composite(db, overlay, grown, inst.child, t, world, path, best, needs);
                        path.pop();
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

    /// The composite pick sees the overlay: a moved shape is hit where it IS,
    /// a deleted one not at all, and a shape drawn into a master is hit at an
    /// arbitrary placement with that placement's transform.
    #[test]
    fn composite_pick_follows_the_overlay_through_the_hierarchy() {
        use crate::edit::{EditOverlay, Shape};
        let db = crate::gds::load_gds_db(HIER).unwrap();
        let leaf = db.id_of("LEAF").expect("LEAF cell");
        let at = Point::new(5_000_100, 5_000_100); // inside element (0,0)'s base tile
        let base = pick_hierarchy(&db, at).unwrap().id;

        // Delete that base shape: whatever is hit now, it is not the deleted one.
        let mut del = EditOverlay::new();
        del.delete(base);
        let h = pick_composite(&db, &del, at);
        assert!(h.map_or(true, |h| h.target != PickTarget::Base(base)), "a deleted shape is not pickable");

        // Move it +50 000 nm in x (LEAF-local): hit at its new place, not the old.
        let mut mv = EditOverlay::new();
        let c = db.cell(leaf);
        mv.move_shape(base, crate::edit::translate(c.shapes.layer(base.1 as usize), c.shapes.points(base.1 as usize), 50_000, 0));
        let moved_at = Point::new(at.x + 50_000, at.y);
        let h = pick_composite(&db, &mv, moved_at).expect("hit the moved copy");
        assert_eq!(h.target, PickTarget::Base(base));

        // Draw a 100 nm square into LEAF at local (50,50): hit through element
        // (2,0) of the array, with that element's transform.
        let mut add = EditOverlay::new();
        let sq = vec![Point::new(50, 50), Point::new(150, 50), Point::new(150, 150), Point::new(50, 150), Point::new(50, 50)];
        add.add((7, 0), leaf, Shape { layer: crate::types::Layer::new(9, 0), points: sq });
        let h = pick_composite(&db, &add, Point::new(5_002_100, 5_000_100)).expect("hit the drawn shape");
        assert_eq!(h.target, PickTarget::Added((7, 0)));
        let (ox, _) = h.world_tf.apply(0.0, 0.0);
        assert!((ox - 5_002_000.0).abs() < 1.0, "resolved at element (2,0)");
    }

    #[test]
    fn empty_space_picks_nothing() {
        let db = crate::gds::load_gds_db(HIER).unwrap();
        // Far outside the block.
        assert!(pick_hierarchy(&db, Point::new(0, 0)).is_none());
    }
}
