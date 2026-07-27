//! Per-cell spatial index (R-tree via `rstar`).
//!
//! One index is built **per cell definition**, over that cell's *own* shapes in
//! **cell-local** coordinates — never over flattened geometry. That is the whole
//! point of hierarchy: a cell instanced a million times is indexed once, and
//! every instance reuses it by inverse-transforming the viewport into the cell's
//! local frame before querying (see `cull.rs`).
//!
//! Built lazily on first query (see `CellDb::index`), so opening a file with
//! thousands of cell defs doesn't pay for indexes never looked at.

use crate::store::ShapeStore;
use crate::transform::Bboxf;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

/// Each entry is a shape's bbox tagged with its index into the cell's shape list.
type Entry = GeomWithData<Rectangle<[f64; 2]>, u32>;

/// R-tree over one cell definition's local shapes.
pub struct ShapeIndex {
    tree: RTree<Entry>,
}

impl ShapeIndex {
    /// Bulk-load from a cell's shape store. Each shape contributes its
    /// axis-aligned bbox (cell-local nm, cast to f64 — exact for i32 inputs).
    pub fn build(store: &ShapeStore) -> Self {
        let entries: Vec<Entry> = (0..store.len())
            .filter_map(|i| {
                let pts = store.points(i);
                if pts.len() < 3 {
                    return None;
                }
                let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
                for p in pts {
                    lo[0] = lo[0].min(p.x as f64);
                    lo[1] = lo[1].min(p.y as f64);
                    hi[0] = hi[0].max(p.x as f64);
                    hi[1] = hi[1].max(p.y as f64);
                }
                Some(GeomWithData::new(Rectangle::from_corners(lo, hi), i as u32))
            })
            .collect();
        ShapeIndex {
            tree: RTree::bulk_load(entries),
        }
    }

    pub fn len(&self) -> usize {
        self.tree.size()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }

    /// Indices of shapes whose bbox intersects `query` (cell-local nm).
    /// The query rect is the world viewport inverse-transformed into this cell's
    /// frame, then axis-aligned — so it is conservative under rotation, which is
    /// correct (never drops a visible shape).
    pub fn query(&self, query: &Bboxf) -> Vec<u32> {
        let env = AABB::from_corners([query.min_x, query.min_y], [query.max_x, query.max_y]);
        self.tree
            .locate_in_envelope_intersecting(&env)
            .map(|e| e.data)
            .collect()
    }

    /// Count-only query — avoids allocating a Vec when we only need the number
    /// of visible shapes (the culling-proof metric).
    pub fn count(&self, query: &Bboxf) -> usize {
        let env = AABB::from_corners([query.min_x, query.min_y], [query.max_x, query.max_y]);
        self.tree.locate_in_envelope_intersecting(&env).count()
    }

    /// Indices of shapes whose bbox contains `(x, y)` — the hit-test candidate set
    /// (Phase 3). Only a bbox test; the caller refines with point-in-polygon.
    pub fn at_point(&self, x: f64, y: f64) -> Vec<u32> {
        self.tree
            .locate_all_at_point(&[x, y])
            .map(|e| e.data)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Layer, Point};

    fn rect(store: &mut ShapeStore, x: i32, y: i32, w: i32, h: i32) {
        store.push(
            Layer::new(0, 0),
            [
                Point::new(x, y),
                Point::new(x + w, y),
                Point::new(x + w, y + h),
                Point::new(x, y + h),
            ],
        );
    }

    #[test]
    fn queries_hit_only_overlapping() {
        let mut shapes = ShapeStore::new();
        rect(&mut shapes, 0, 0, 10, 10);
        rect(&mut shapes, 100, 100, 10, 10);
        rect(&mut shapes, 1000, 1000, 10, 10);
        let idx = ShapeIndex::build(&shapes);
        assert_eq!(idx.len(), 3);
        // A window around the origin sees only the first rect.
        let hits = idx.query(&Bboxf::new(-5.0, -5.0, 20.0, 20.0));
        assert_eq!(hits, vec![0]);
        // A window covering everything sees all three.
        assert_eq!(idx.count(&Bboxf::new(-1.0, -1.0, 2000.0, 2000.0)), 3);
    }
}
