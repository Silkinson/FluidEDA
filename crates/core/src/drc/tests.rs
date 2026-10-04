use super::*;
use crate::boolean::{region_area2, union_clipped};
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
    Cell::from_parts("T".into(), store, Vec::new(), chunks, Vec::new())
}

fn rules(w: i32, s: i32) -> DrcRules {
    DrcRules { default_rule: Some(LayerRule { min_width: w, min_spacing: s }), per_layer: HashMap::new() }
}

const L1: Layer = Layer { layer: 1, datatype: 0 };
const L2: Layer = Layer { layer: 2, datatype: 0 };

#[test]
fn narrow_rect_flags_width_wide_passes() {
    let cell = cell_of(vec![
        (L1, rect(0, 0, 50, 1000)),    // 50 nm wide < 100 ⇒ violation
        (L1, rect(5000, 0, 500, 500)), // fine
    ]);
    let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, DrcKind::Width);
    assert_eq!(v[0].layer, L1);
}

#[test]
fn spacing_gap_flags_touching_and_far_do_not() {
    let cell = cell_of(vec![
        (L1, rect(0, 0, 1000, 1000)),
        (L1, rect(1050, 0, 1000, 1000)),    // 50 nm gap < 100 ⇒ violation
        (L1, rect(3000, 0, 1000, 1000)),    // 950 gap to neighbor ⇒ fine
        (L1, rect(4000, 0, 1000, 1000)),    // touches previous (gap 0) ⇒ merged, fine
        (L2, rect(1010, 2000, 1000, 1000)), // different layer ⇒ never pairs
    ]);
    let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
    assert_eq!(v.len(), 1, "exactly the 50 nm gap flags: {v:?}");
    assert_eq!(v[0].kind, DrcKind::Spacing);
    assert!(v[0].bbox.min.x >= 1000 && v[0].bbox.max.x <= 1050, "marker in the gap: {:?}", v[0].bbox);
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
    // Corner-to-corner ≈ 99 nm < 100 ⇒ one (merged) marker.
    let cell = cell_of(vec![(L1, rect(0, 0, 1000, 1000)), (L1, rect(1070, 1070, 1000, 1000))]);
    let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
    assert_eq!(v.len(), 1, "corner distance ~99 nm flags once: {v:?}");
    // At 71,71 offset (distance ~100.4) it must NOT flag.
    let cell2 = cell_of(vec![(L1, rect(0, 0, 1000, 1000)), (L1, rect(1071, 1071, 1000, 1000))]);
    assert!(check_cell(&cell2, 0, &EditOverlay::new(), &rules(100, 100)).is_empty());
}

#[test]
fn fractured_wire_is_not_a_width_violation() {
    // The headline fix: a 200-wide wire delivered as abutting fracture pieces,
    // including 30 nm and 10 nm slivers. Per-shape width flagged the slivers;
    // merge-first sees one 200 nm wire.
    let cell = cell_of(vec![
        (L1, rect(0, 0, 400, 200)),
        (L1, rect(400, 0, 30, 200)),
        (L1, rect(430, 0, 10, 200)),
        (L1, rect(440, 0, 560, 200)),
        (L1, rect(1000, 0, 200, 1500)), // an abutting perpendicular leg
    ]);
    let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
    assert!(v.is_empty(), "merged geometry is clean: {v:?}");
}

#[test]
fn l_shape_thin_arm_flags_where_bbox_width_did_not() {
    let l = vec![
        Point::new(0, 0),
        Point::new(1000, 0),
        Point::new(1000, 200),
        Point::new(60, 200),
        Point::new(60, 1000),
        Point::new(0, 1000),
        Point::new(0, 0),
    ];
    let cell = cell_of(vec![(L1, l)]);
    let v = check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100));
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, DrcKind::Width);
}

#[test]
fn overlay_composites_into_the_check() {
    let cell = cell_of(vec![(L1, rect(0, 0, 1000, 1000)), (L1, rect(1200, 0, 1000, 1000))]);
    assert!(check_cell(&cell, 0, &EditOverlay::new(), &rules(100, 100)).is_empty());

    // Move shape 1 left by 150 ⇒ gap 50 ⇒ violation.
    let mut moved = EditOverlay::new();
    moved.move_shape((0, 1), crate::edit::translate(L1, &rect(1200, 0, 1000, 1000), -150, 0));
    let v = check_cell(&cell, 0, &moved, &rules(100, 100));
    assert_eq!(v.len(), 1, "moved-closer pair flags: {v:?}");
    assert_eq!(v[0].kind, DrcKind::Spacing);

    // Move it onto the other (overlap) ⇒ merged ⇒ clean.
    let mut merged = EditOverlay::new();
    merged.move_shape((0, 1), crate::edit::translate(L1, &rect(1200, 0, 1000, 1000), -500, 0));
    assert!(check_cell(&cell, 0, &merged, &rules(100, 100)).is_empty());

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

    // A drawn shape that abuts a base shape merges with it: no width violation
    // even though the drawn piece alone is 40 nm wide (and the gap it leaves to
    // shape 1, 160 nm, is legal).
    let mut abut = EditOverlay::new();
    abut.add((1, 1), 0, crate::edit::Shape { layer: L1, points: rect(1000, 0, 40, 1000) });
    assert!(check_cell(&cell, 0, &abut, &rules(100, 100)).is_empty());
    // Widen it to 140: still merged (no width flag), but now a 60 nm gap.
    let mut abut2 = EditOverlay::new();
    abut2.add((1, 1), 0, crate::edit::Shape { layer: L1, points: rect(1000, 0, 140, 1000) });
    let v = check_cell(&cell, 0, &abut2, &rules(100, 100));
    assert_eq!(v.len(), 1, "only the 60 nm gap: {v:?}");
    assert_eq!(v[0].kind, DrcKind::Spacing);
}

#[test]
fn drawn_shape_beyond_the_base_extent_is_checked() {
    let cell = cell_of(vec![(L1, rect(0, 0, 1000, 1000))]);
    let mut o = EditOverlay::new();
    o.add((9, 0), 0, crate::edit::Shape { layer: L1, points: rect(900_000, 900_000, 30, 30) });
    let v = check_cell(&cell, 0, &o, &rules(100, 100));
    assert_eq!(v.len(), 1, "far-away drawn sliver still flags: {v:?}");
}

#[test]
fn no_rules_means_no_violations() {
    let cell = cell_of(vec![(L1, rect(0, 0, 10, 10))]);
    assert!(check_cell(&cell, 0, &EditOverlay::new(), &DrcRules::default()).is_empty());
}

#[test]
fn rules_pack_round_trip() {
    let mut r = rules(100, 120);
    r.per_layer.insert(Layer::new(68, 20), LayerRule { min_width: 140, min_spacing: 140 });
    r.per_layer.insert(Layer::new(70, 20), LayerRule { min_width: 300, min_spacing: 300 });
    assert_eq!(DrcRules::unpack(&r.pack()), r);
    assert_eq!(DrcRules::unpack(&DrcRules::default().pack()), DrcRules::default());
}

/// A lattice cell big enough to cut into many tiles, with violations placed
/// across tile borders: thin wires, close pairs, and fractured wide wires.
fn lattice_cell() -> Cell {
    let mut shapes = Vec::new();
    for row in 0..30 {
        let y = row * 3_000;
        for col in 0..30 {
            let x = col * 3_000;
            // A wide wire fractured into two abutting pieces (clean).
            shapes.push((L1, rect(x, y, 1_300, 400)));
            shapes.push((L1, rect(x + 1_300, y, 1_200, 400)));
            // Every 7th cell: a thin wire (width) and a close pair (space).
            if (row * 30 + col) % 7 == 0 {
                shapes.push((L1, rect(x, y + 1_000, 2_900, 60)));
                shapes.push((L1, rect(x, y + 1_500, 1_000, 500)));
                shapes.push((L1, rect(x + 1_040, y + 1_500, 1_000, 500)));
            }
        }
    }
    cell_of(shapes)
}

/// Union-of-markers area per kind — tiling fragments markers at tile borders,
/// but the covered region must be identical.
fn coverage(v: &[Violation], kind: DrcKind) -> i128 {
    let rings: Vec<Vec<Point>> = v
        .iter()
        .filter(|x| x.kind == kind)
        .map(|x| rect(x.bbox.min.x, x.bbox.min.y, x.bbox.max.x - x.bbox.min.x, x.bbox.max.y - x.bbox.min.y))
        .collect();
    region_area2(&union_clipped(rings.iter().map(|r| r.as_slice()), None, Point::new(0, 0)))
}

fn check_with_target(cell: &Cell, overlay: &EditOverlay, rules: &DrcRules, target: usize) -> (DrcGrid, Vec<Violation>) {
    let mut b = DrcGridBuilder::new(cell, rules.max_reach(), target);
    let checked = |l: Layer| rules.rule_for(l).is_some();
    // Budgeted in small steps, as the app drives it.
    while !b.step(cell, 97, &checked) {}
    let grid = b.finish();
    let mut out = Vec::new();
    for t in cell_tiles(&grid, 0, overlay, rules) {
        let input = gather_tile(cell, 0, overlay, &grid, rules, t);
        out.extend(check_tile(grid.tile_core(t), rules, &input));
    }
    (grid, out)
}

#[test]
fn tiling_does_not_change_what_is_found() {
    let cell = lattice_cell();
    let r = rules(100, 100);
    let (g1, one) = check_with_target(&cell, &EditOverlay::new(), &r, usize::MAX);
    let (gn, many) = check_with_target(&cell, &EditOverlay::new(), &r, 150);
    assert_eq!(g1.nx * g1.ny, 1, "single-tile reference");
    assert!(gn.nx * gn.ny > 9, "the tiled run really is tiled: {}×{}", gn.nx, gn.ny);
    let n_bad = (0..900).filter(|k| k % 7 == 0).count();
    assert_eq!(one.iter().filter(|v| v.kind == DrcKind::Width).count(), n_bad, "one thin wire each");
    assert_eq!(one.iter().filter(|v| v.kind == DrcKind::Spacing).count(), n_bad, "one close pair each");
    for kind in [DrcKind::Width, DrcKind::Spacing] {
        assert_eq!(coverage(&one, kind), coverage(&many, kind), "{kind:?} coverage differs under tiling");
    }
}

#[test]
fn edit_rechecks_only_nearby_tiles_and_matches_a_full_check() {
    let cell = lattice_cell();
    let r = rules(100, 100);
    let target = 150;
    let (grid, _) = check_with_target(&cell, &EditOverlay::new(), &r, target);
    let per_tile = |o: &EditOverlay| -> std::collections::BTreeMap<TileKey, Vec<(DrcKind, Bbox)>> {
        cell_tiles(&grid, 0, o, &r)
            .into_iter()
            .map(|t| {
                let v = check_tile(grid.tile_core(t), &r, &gather_tile(&cell, 0, o, &grid, &r, t));
                (t, v.into_iter().map(|x| (x.kind, x.bbox)).collect())
            })
            .collect()
    };
    let before = per_tile(&EditOverlay::new());

    // Edit: move base shape 0 up by 450 so it nearly touches its neighbour
    // row... and draw a sliver elsewhere.
    let mut o = EditOverlay::new();
    let old = cell.shapes.points(0).to_vec();
    let moved = crate::edit::translate(cell.shapes.layer(0), &old, 0, 2_560);
    let (ob, nb) = (ring_bbox(&old), ring_bbox(&moved.points));
    o.move_shape((0, 0), moved);
    let sliver = rect(40_000, 40_000, 30, 900);
    o.add((5, 0), 0, crate::edit::Shape { layer: L1, points: sliver.clone() });

    // Incremental: re-check only the tiles near what changed.
    let mut dirty: Vec<TileKey> = Vec::new();
    for b in [ob, nb, ring_bbox(&sliver)] {
        dirty.extend(grid.tiles_near(&b));
    }
    dirty.sort_unstable();
    dirty.dedup();
    let mut incremental = before.clone();
    for t in &dirty {
        let v = check_tile(grid.tile_core(*t), &r, &gather_tile(&cell, 0, &o, &grid, &r, *t));
        incremental.insert(*t, v.into_iter().map(|x| (x.kind, x.bbox)).collect());
    }
    incremental.retain(|_, v| !v.is_empty());
    let mut full = per_tile(&o);
    full.retain(|_, v| !v.is_empty());
    assert_eq!(incremental, full, "incremental re-check must equal a full re-check");
    assert!(dirty.len() < grid.nonempty_tiles().len() / 4, "an edit touches few tiles ({} dirty)", dirty.len());
}
