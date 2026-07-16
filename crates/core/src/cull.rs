//! Viewport culling: walk the instance tree with a transform stack and emit only
//! what is visible, grouped for GPU instancing.
//!
//! Strategy (all three required moves of Phase 1 culling):
//!   1. **Inverse-transform the viewport** into each cell's local frame and query
//!      that cell's own R-tree there — so one index serves every instance.
//!   2. **Cull subtrees** whose world bbox misses the viewport, and compute AREF
//!      column/row ranges analytically instead of iterating a million elements.
//!   3. **Stop descending** when a cell projects below `min_pixel` and emit a
//!      bbox/LOD proxy instead of its geometry.
//!
//! Output is grouped by cell definition: one `CellDraw { cell, transforms }` per
//! visible def, feeding a single instanced draw of that cell's mesh.

use crate::chunk::ChunkId;
use crate::db::{CellDb, CellId, InstanceKind};
use crate::transform::{Bboxf, Transform};
use std::collections::HashMap;

/// One instanced draw: a **chunk** of a cell def, and every world transform at
/// which that chunk's geometry is visible this frame.
///
/// Phase 2b made the chunk — not the whole cell — the unit here, so a viewport
/// over a die-spanning cell selects only the chunks it touches.
#[derive(Debug, Clone)]
pub struct CellDraw {
    pub cell: CellId,
    pub chunk: ChunkId,
    pub transforms: Vec<Transform>,
}

/// Result of a viewport query.
#[derive(Debug, Default)]
pub struct VisibleSet {
    pub draws: Vec<CellDraw>,
    /// World-space boxes for cells too small to draw as geometry (LOD proxies).
    pub proxies: Vec<Bboxf>,
    pub stats: CullStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CullStats {
    /// Flattened shapes actually inside the viewport (index-confirmed) at cells
    /// drawn as real geometry — the culling-proof numerator.
    pub visible_shapes: u64,
    /// Cell placements emitted as instanced draws.
    pub instances_emitted: u64,
    /// LOD proxies emitted.
    pub proxy_count: usize,
    /// Nodes visited in the instance-tree walk (a rough work measure).
    pub nodes_visited: u64,
    /// Chunk selections emitted from *chunked* (large) cells only — the measure
    /// of how much of a die-spanning cell a viewport actually pulled in.
    pub chunks_emitted: u64,
}

/// Cull the design against a world-space viewport.
///
/// * `view` — viewport rectangle in world nm.
/// * `px_per_nm` — camera scale (pixels per nm); sets the LOD cutoff.
/// * `min_pixel` — a cell projecting smaller than this (max dimension) becomes a
///   proxy instead of recursing into its geometry.
///
/// `stats.visible_shapes` is left at 0 (see [`cull_counted`]); the render path
/// never needs it, and counting it walks every visible cell's R-tree — ~89% of a
/// fit-view frame on a real chip.
pub fn cull(db: &CellDb, view: &Bboxf, px_per_nm: f64, min_pixel: f64) -> VisibleSet {
    cull_inner(db, view, px_per_nm, min_pixel, false)
}

/// Like [`cull`] but also fills `stats.visible_shapes` by querying each visible
/// cell's index. **Off the hot path** — for the bench/shot harnesses and an
/// opt-in debug readout, never per frame.
pub fn cull_counted(db: &CellDb, view: &Bboxf, px_per_nm: f64, min_pixel: f64) -> VisibleSet {
    cull_inner(db, view, px_per_nm, min_pixel, true)
}

fn cull_inner(db: &CellDb, view: &Bboxf, px_per_nm: f64, min_pixel: f64, count_visible: bool) -> VisibleSet {
    let mut ctx = CullCtx {
        db,
        view: *view,
        px_per_nm,
        min_pixel,
        count_visible,
        grouped: HashMap::new(),
        set: VisibleSet::default(),
    };
    ctx.walk(db.top, Transform::IDENTITY);

    // Materialise the grouped instancing map into the output list, ordered by
    // (cell, chunk). The sort is load-bearing, not cosmetic: `grouped` is a
    // HashMap, so iterating it yields a per-process-random order, and the
    // renderer draws in exactly that order. With alpha blending, two cells whose
    // geometry overlaps blend to different pixels depending on which is drawn
    // first — so an unsorted walk makes the same scene render differently run to
    // run (measured: 118 px of sky130 at fit). Sorting makes the frame
    // reproducible, which is what lets the golden-image check mean anything.
    let mut grouped: Vec<((CellId, ChunkId), Vec<Transform>)> = ctx.grouped.into_iter().collect();
    grouped.sort_unstable_by_key(|&(key, _)| key);
    for ((cell, chunk), transforms) in grouped {
        ctx.set.stats.instances_emitted += transforms.len() as u64;
        ctx.set.draws.push(CellDraw { cell, chunk, transforms });
    }
    ctx.set.stats.proxy_count = ctx.set.proxies.len();
    ctx.set
}

struct CullCtx<'a> {
    db: &'a CellDb,
    view: Bboxf,
    px_per_nm: f64,
    min_pixel: f64,
    /// When false, skip the per-cell index query that fills `visible_shapes`.
    count_visible: bool,
    grouped: HashMap<(CellId, ChunkId), Vec<Transform>>,
    set: VisibleSet,
}

impl CullCtx<'_> {
    /// Visit one cell placement (`world_tf` = child-local → world).
    fn walk(&mut self, id: CellId, world_tf: Transform) {
        self.set.stats.nodes_visited += 1;

        let local_bb = self.db.local_bbox(id);
        // Empty cell (no geometry anywhere): nothing to do.
        if local_bb.min.x > local_bb.max.x {
            return;
        }
        let world_bb = world_tf.apply_bbox(&local_bb);
        if !world_bb.intersects(&self.view) {
            return; // whole subtree off-screen
        }

        // LOD: if the cell projects below the pixel threshold, draw a proxy and
        // do not descend into its geometry or children.
        let px = world_bb.width().max(world_bb.height()) * self.px_per_nm;
        if px < self.min_pixel {
            self.set.proxies.push(clip(&world_bb, &self.view));
            return;
        }

        let cell = self.db.cell(id);

        // Emit this cell's visible chunks as instanced draws at `world_tf`. Only
        // when explicitly counting do we query the index for the visible-shape
        // metric — that query (and the lazy index build it triggers) is the bulk
        // of a fit-view frame on a real chip, and the renderer never needs it.
        if !cell.chunks.is_empty() {
            if self.count_visible {
                let local_view = world_tf.inverse().apply_bboxf(&self.view);
                self.set.stats.visible_shapes += cell.index().count(&local_view) as u64;
            }
            if cell.chunks.len() == 1 {
                // The overwhelmingly common case (every small cell). Emit without
                // inverting the transform: Phase 2a took that inverse off the hot
                // path and chunking must not put it back for cells it can't help.
                self.grouped.entry((id, 0)).or_default().push(world_tf);
            } else {
                // Chunked cell: test each chunk's loose bbox in the cell's own
                // frame. Only large defs reach here, and they are placed few
                // times, so the inverse is paid rarely.
                let local_view = world_tf.inverse().apply_bboxf(&self.view);
                for (ci, chunk) in cell.chunks.iter().enumerate() {
                    if bbox_intersects(&chunk.bbox, &local_view) {
                        self.grouped.entry((id, ci as ChunkId)).or_default().push(world_tf);
                        self.set.stats.chunks_emitted += 1;
                    }
                }
            }
        }

        // Descend into child instances.
        for inst in &cell.instances {
            match inst.kind {
                InstanceKind::Single => {
                    self.walk(inst.child, world_tf.then(&inst.transform));
                }
                InstanceKind::Array {
                    cols,
                    rows,
                    col_step,
                    row_step,
                } => {
                    self.walk_array(inst.child, world_tf, inst.transform, cols, rows, col_step, row_step);
                }
            }
        }
    }

    /// Handle an AREF: emit one proxy if each element is sub-pixel, otherwise
    /// recurse only into the column/row range that can intersect the viewport.
    #[allow(clippy::too_many_arguments)]
    fn walk_array(
        &mut self,
        child: CellId,
        parent_world: Transform,
        base: Transform,
        cols: i32,
        rows: i32,
        col_step: [f64; 2],
        row_step: [f64; 2],
    ) {
        if cols <= 0 || rows <= 0 {
            return;
        }
        let child_bb = self.db.local_bbox(child);
        if child_bb.min.x > child_bb.max.x {
            return;
        }
        // World transform of element (0,0), used to size a single element.
        let base_world = parent_world.then(&base);
        let elem_world_bb = base_world.apply_bbox(&child_bb);
        let elem_px = elem_world_bb.width().max(elem_world_bb.height()) * self.px_per_nm;

        // Whole-array world bbox (fold the four corner placements).
        let mut array_bb = Bboxf::empty();
        for &(c, r) in &[(0, 0), (cols - 1, 0), (0, rows - 1), (cols - 1, rows - 1)] {
            let t = parent_world.then(&placement(&base, col_step, row_step, c, r));
            let bb = t.apply_bbox(&child_bb);
            array_bb.expand(bb.min_x, bb.min_y);
            array_bb.expand(bb.max_x, bb.max_y);
        }
        if !array_bb.intersects(&self.view) {
            return; // whole array off-screen
        }

        // Each element sub-pixel: collapse the entire array into one proxy.
        if elem_px < self.min_pixel {
            self.set.proxies.push(clip(&array_bb, &self.view));
            return;
        }

        // Otherwise recurse into the intersecting (col,row) window only.
        let (c0, c1, r0, r1) =
            visible_range(&self.view, parent_world, base, &child_bb, col_step, row_step, cols, rows);
        for r in r0..=r1 {
            for c in c0..=c1 {
                let t = parent_world.then(&placement(&base, col_step, row_step, c, r));
                self.walk(child, t);
            }
        }
    }
}

/// Transform for array element `(c,r)`: base placement plus the parent-frame
/// lattice offset (a pure translation, so it only shifts the base translation).
fn placement(base: &Transform, col_step: [f64; 2], row_step: [f64; 2], c: i32, r: i32) -> Transform {
    let mut t = *base;
    t.tx += c as f64 * col_step[0] + r as f64 * row_step[0];
    t.ty += c as f64 * col_step[1] + r as f64 * row_step[1];
    t
}

/// Analytically bound the array indices whose element footprint can intersect
/// the viewport. Works for arbitrary (even sheared) lattices by inverse-mapping
/// the viewport rectangle (expanded by one element's footprint) into lattice
/// (col,row) space and taking the bounding range; the per-element `walk` then
/// drops any conservative extras via its own intersect test.
#[allow(clippy::too_many_arguments)]
fn visible_range(
    view: &Bboxf,
    parent_world: Transform,
    base: Transform,
    child_bb: &crate::types::Bbox,
    col_step: [f64; 2],
    row_step: [f64; 2],
    cols: i32,
    rows: i32,
) -> (i32, i32, i32, i32) {
    // Work in the parent-local frame: the lattice offsets live there directly.
    let parent_local_view = parent_world.inverse().apply_bboxf(view);
    // Element (0,0)'s footprint in parent-local space.
    let base_bb = base.apply_bbox(child_bb);

    // We need lattice offset O = c*U + r*V such that (base_bb + O) intersects the
    // (parent-local) viewport, i.e. O lies in the rectangle R below.
    let rx0 = parent_local_view.min_x - base_bb.max_x;
    let rx1 = parent_local_view.max_x - base_bb.min_x;
    let ry0 = parent_local_view.min_y - base_bb.max_y;
    let ry1 = parent_local_view.max_y - base_bb.min_y;

    let (ux, uy) = (col_step[0], col_step[1]);
    let (vx, vy) = (row_step[0], row_step[1]);
    let det = ux * vy - uy * vx;

    let full = (0, cols - 1, 0, rows - 1);
    if det.abs() < 1e-9 {
        // Degenerate lattice (collinear/zero steps, e.g. a 1×N array): fall back
        // to the full range — the per-element intersect test keeps it correct,
        // and one axis is trivially small.
        return full;
    }

    // Map the four corners of R through [U V]^{-1} to get (c,r); bound them.
    let inv = 1.0 / det;
    let map = |ox: f64, oy: f64| -> (f64, f64) {
        let c = (vy * ox - vx * oy) * inv;
        let r = (-uy * ox + ux * oy) * inv;
        (c, r)
    };
    let corners = [map(rx0, ry0), map(rx1, ry0), map(rx1, ry1), map(rx0, ry1)];
    let (mut cmin, mut cmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut rmin, mut rmax) = (f64::INFINITY, f64::NEG_INFINITY);
    for (c, r) in corners {
        cmin = cmin.min(c);
        cmax = cmax.max(c);
        rmin = rmin.min(r);
        rmax = rmax.max(r);
    }
    let c0 = (cmin.floor() as i32).clamp(0, cols - 1);
    let c1 = (cmax.ceil() as i32).clamp(0, cols - 1);
    let r0 = (rmin.floor() as i32).clamp(0, rows - 1);
    let r1 = (rmax.ceil() as i32).clamp(0, rows - 1);
    if c0 > c1 || r0 > r1 {
        // Range collapsed (array touches but analytic bound excluded it): defer
        // to full — cheap, since this only happens for tiny arrays.
        return full;
    }
    (c0, c1, r0, r1)
}

/// Does an integer (cell-local) chunk bbox overlap a float query rect? An empty
/// bbox (`min > max`, i.e. a chunk of only degenerate shapes) never hits.
fn bbox_intersects(b: &crate::types::Bbox, q: &Bboxf) -> bool {
    if b.min.x > b.max.x {
        return false;
    }
    (b.min.x as f64) <= q.max_x
        && (b.max.x as f64) >= q.min_x
        && (b.min.y as f64) <= q.max_y
        && (b.max.y as f64) >= q.min_y
}

/// Intersect two boxes (for clipping a proxy to the viewport).
fn clip(b: &Bboxf, view: &Bboxf) -> Bboxf {
    Bboxf::new(
        b.min_x.max(view.min_x),
        b.min_y.max(view.min_y),
        b.max_x.min(view.max_x),
        b.max_y.min(view.max_y),
    )
}
