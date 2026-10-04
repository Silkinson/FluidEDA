//! "Edit this placement only" — flatten one placement into its parent (Phase 5d).
//!
//! Editing a shape edits its cell **master**: every placement changes. To
//! change just one placement (one element of an AREF, say), a layout editor
//! flattens it: the placement is hidden and its geometry is copied into the
//! parent as ordinary shapes, which every edit tool already handles (move,
//! vertex edit, booleans, DRC, export). This module computes what to copy.
//!
//! **Which hop.** A clicked shape sits under a placement *path* (TOP → … →
//! its cell). Hiding an element inside a cell that is itself placed many times
//! would hide it in all of them. So the hop flattened is the **deepest one
//! whose parent is placed exactly once** in the whole design ([`flatten_hop`])
//! — hiding that element removes exactly the one placement the user clicked.
//! A standard cell inside caravel_core (placed once) is flattened into
//! caravel_core; an element of a nested AREF is flattened into the nearest
//! ancestor that is unique.
//!
//! **What.** The placement's *composite* subtree (base minus tombstones,
//! moved / vertex-edited copies, drawn shapes, hidden sub-placements skipped),
//! mapped into the parent's frame — exactly what is drawn there now, so the
//! flatten itself changes nothing visible (tested). Bounded by a shape limit:
//! flattening a 2.5 M-shape block by accident is refused, not attempted.

use crate::db::{CellDb, CellId, InstanceKind};
use crate::edit::{EditOverlay, PlacementKey, Shape};
use crate::transform::Transform;
use crate::types::Point;

/// Largest flattened subtree "edit this placement only" will copy.
pub const FLATTEN_LIMIT: usize = 50_000;

/// How many times each cell is placed in the flattened design (TOP = 1;
/// unreachable cells 0). Top-down over a topological order, O(cells + instances).
pub fn placement_counts(db: &CellDb) -> Vec<u64> {
    let n = db.cells.len();
    // Post-order DFS from TOP; its reverse lists parents before children.
    let mut order = Vec::with_capacity(n);
    let mut state = vec![0u8; n]; // 0 new, 1 on stack, 2 done
    let mut stack = vec![(db.top, false)];
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            state[id] = 2;
            order.push(id);
            continue;
        }
        if state[id] != 0 {
            continue;
        }
        state[id] = 1;
        stack.push((id, true));
        for inst in &db.cell(id).instances {
            if state[inst.child] == 0 {
                stack.push((inst.child, false));
            }
        }
    }
    let mut counts = vec![0u64; n];
    counts[db.top] = 1;
    for &id in order.iter().rev() {
        let c = counts[id];
        if c == 0 {
            continue;
        }
        for inst in &db.cell(id).instances {
            counts[inst.child] = counts[inst.child].saturating_add(c.saturating_mul(inst.count()));
        }
    }
    counts
}

/// The hop of `path` to flatten: the deepest whose parent cell is placed
/// exactly once (so hiding the element hides exactly the clicked placement).
/// `None` for an empty path (a TOP-cell shape — nothing to flatten).
pub fn flatten_hop(path: &[PlacementKey], counts: &[u64]) -> Option<usize> {
    (0..path.len()).rev().find(|&i| counts.get(path[i].0).copied() == Some(1))
}

/// The composite geometry of placement `key`'s subtree, in the parent's frame
/// (see the module note). `Err(n)` if it exceeds `limit` shapes (`n` = how many
/// were counted before giving up).
pub fn flatten_placement(db: &CellDb, overlay: &EditOverlay, key: PlacementKey, limit: usize) -> Result<Vec<Shape>, usize> {
    let (parent, ii, c, r) = key;
    let Some(inst) = db.cell(parent).resident_instances().find(|&(i, _)| i == ii).map(|(_, inst)| inst) else {
        return Ok(Vec::new());
    };
    let in_range = match inst.kind {
        InstanceKind::Single => c == 0 && r == 0,
        InstanceKind::Array { cols, rows, .. } => (0..cols).contains(&c) && (0..rows).contains(&r),
    };
    if !in_range {
        return Ok(Vec::new()); // no such element — nothing to flatten
    }
    let mut out = Vec::new();
    collect(db, overlay, inst.child, inst.placement(c, r), limit, &mut out)?;
    Ok(out)
}

/// What a streamed design must have in memory before placement `key` can be
/// flattened (Phase 7.4): every shape chunk and placement chunk of its
/// subtree that is not resident. Empty in-core.
pub fn flatten_needs(db: &CellDb, key: PlacementKey) -> Vec<crate::pick::ChunkNeed> {
    let (parent, ii, _, _) = key;
    let mut needs = Vec::new();
    let Some(&root) = db.cell(parent).resident_instances().find(|&(i, _)| i == ii).map(|(_, inst)| inst.child).as_ref()
    else {
        return needs;
    };
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let cell = db.cell(id);
        for ci in 0..cell.chunks.len() {
            if !cell.chunk_resident(ci) {
                needs.push(crate::pick::ChunkNeed::Shapes(id, ci as u32));
            }
        }
        if let Some(remote) = &cell.remote {
            for (ci, c) in remote.chunks.iter().enumerate() {
                if c.is_none() {
                    needs.push(crate::pick::ChunkNeed::Placements(id, ci as u32));
                }
            }
        }
        stack.extend(cell.resident_instances().map(|(_, inst)| inst.child));
    }
    needs
}

/// Everything drawn, flattened to TOP's frame: the composite of base + overlay
/// with hidden placements skipped. (Tests and export verification — do not
/// call on a production chip; it materialises every placement.)
pub fn composite_flat(db: &CellDb, overlay: &EditOverlay) -> Vec<Shape> {
    let mut out = Vec::new();
    let _ = collect(db, overlay, db.top, Transform::IDENTITY, usize::MAX, &mut out);
    out
}

fn collect(
    db: &CellDb,
    overlay: &EditOverlay,
    id: CellId,
    tf: Transform,
    limit: usize,
    out: &mut Vec<Shape>,
) -> Result<(), usize> {
    let cell = db.cell(id);
    let map = |pts: &[Point]| -> Vec<Point> {
        pts.iter()
            .map(|&p| {
                let (x, y) = tf.apply_point(p);
                Point::new(x.round() as i32, y.round() as i32)
            })
            .collect()
    };
    let push = |out: &mut Vec<Shape>, s: Shape| -> Result<(), usize> {
        out.push(s);
        if out.len() > limit {
            Err(out.len())
        } else {
            Ok(())
        }
    };
    let suppressed = overlay.suppressed_in(id);
    // Shapes through the chunk stores: a streamed design's subtree must be
    // resident first (`flatten_needs`).
    for ci in 0..cell.chunks.len() {
        let Some((store, base)) = cell.chunk_store(ci) else { continue };
        let ch = cell.chunks[ci];
        for i in ch.start..ch.end {
            if suppressed.is_some_and(|s| s.contains(&i)) {
                continue;
            }
            let j = (i - base) as usize;
            push(out, Shape { layer: store.layer(j), points: map(store.points(j)) })?;
        }
    }
    for s in overlay.live_shapes_in(id) {
        push(out, Shape { layer: s.layer, points: map(&s.points) })?;
    }
    for (ii, inst) in cell.resident_instances() {
        let (cols, rows) = match inst.kind {
            InstanceKind::Single => (1, 1),
            InstanceKind::Array { cols, rows, .. } => (cols.max(0), rows.max(0)),
        };
        for r in 0..rows {
            for c in 0..cols {
                if overlay.is_hidden((id, ii, c, r)) {
                    continue;
                }
                collect(db, overlay, inst.child, tf.then(&inst.placement(c, r)), limit, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CellDbBuilder, Instance};
    use crate::types::Layer;

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h), Point::new(x, y)]
    }

    /// TOP ─ AREF 3×2 of MID ─ SREF of LEAF (+ MID's own shape); TOP has one
    /// shape too. Small enough to flatten whole in a test.
    fn tiny() -> CellDb {
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let mid = b.intern("MID");
        let leaf = b.intern("LEAF");
        b.cell_mut(leaf).shapes.push(Layer::new(1, 0), rect(0, 0, 100, 100));
        b.cell_mut(leaf).shapes.push(Layer::new(2, 0), rect(20, 20, 40, 40));
        b.cell_mut(mid).shapes.push(Layer::new(3, 0), rect(0, 200, 300, 50));
        b.cell_mut(mid).instances.push(Instance {
            child: leaf,
            transform: Transform::translate(150.0, 0.0),
            kind: InstanceKind::Single,
        });
        b.cell_mut(top).shapes.push(Layer::new(1, 0), rect(-500, -500, 100, 100));
        b.cell_mut(top).instances.push(Instance {
            child: mid,
            transform: Transform::translate(0.0, 0.0),
            kind: InstanceKind::Array { cols: 3, rows: 2, col_step: [1000.0, 0.0], row_step: [0.0, 1000.0] },
        });
        b.build().unwrap()
    }

    /// Everything drawn, flattened to world: the composite of base + overlay
    /// with hidden placements skipped. Sorted, for multiset comparison.
    fn drawn(db: &CellDb, overlay: &EditOverlay) -> Vec<(Layer, Vec<(i32, i32)>)> {
        let mut out = Vec::new();
        collect(db, overlay, db.top, Transform::IDENTITY, usize::MAX, &mut out).unwrap();
        let mut v: Vec<_> = out.into_iter().map(|s| (s.layer, s.points.iter().map(|p| (p.x, p.y)).collect())).collect();
        v.sort();
        v
    }

    #[test]
    fn counts_and_hop_choice_on_nested_arrays() {
        let db = crate::gds::load_gds_db(HIER).unwrap();
        let counts = placement_counts(&db);
        let (row, leaf) = (db.id_of("ROW").unwrap(), db.id_of("LEAF").unwrap());
        assert_eq!(counts[db.top], 1);
        assert_eq!(counts[row], 1000, "ROW is arrayed 1×1000 in TOP");
        assert_eq!(counts[leaf], 1000 * 1000 + 1, "1000 per ROW, plus TOP's landmark leaf");
        // A LEAF under TOP → ROW(elem) → LEAF(elem): ROW is not unique, so the
        // element to hide is ROW's, in TOP.
        let path = vec![(db.top, 0, 0, 5), (row, 0, 7, 0)];
        assert_eq!(flatten_hop(&path, &counts), Some(0));
        assert_eq!(flatten_hop(&[], &counts), None);
    }

    #[test]
    fn flattening_a_placement_changes_nothing_that_is_drawn() {
        let db = tiny();
        let before = drawn(&db, &EditOverlay::new());
        assert_eq!(before.len(), 1 + 6 * 3, "TOP's shape + 6 MIDs × (1 own + 2 LEAF)");
        let key = (db.top, 0, 2, 1); // the last element of the 3×2 array
        let shapes = flatten_placement(&db, &EditOverlay::new(), key, FLATTEN_LIMIT).unwrap();
        assert_eq!(shapes.len(), 3, "MID's own shape + its LEAF's two");
        let mut o = EditOverlay::new();
        o.hide_placement(key);
        for (k, s) in shapes.into_iter().enumerate() {
            o.add((99, k as u32), db.top, s);
        }
        assert_eq!(drawn(&db, &o), before, "same geometry, now owned by TOP");
        let mut hidden_only = EditOverlay::new();
        hidden_only.hide_placement(key);
        assert_eq!(drawn(&db, &hidden_only).len(), before.len() - 3, "hiding removes exactly that element");
    }

    #[test]
    fn out_of_range_elements_and_the_limit() {
        let db = tiny();
        assert!(flatten_placement(&db, &EditOverlay::new(), (db.top, 0, 3, 0), FLATTEN_LIMIT).unwrap().is_empty());
        assert!(flatten_placement(&db, &EditOverlay::new(), (db.top, 0, 0, 0), 2).is_err());
    }
}
