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
use crate::edit::{EditOverlay, Shape};
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
    /// Phase 3: this chunk belongs to a cell with overlay edits, so its base
    /// geometry must be tessellated with the cell's suppressed indices removed
    /// (see [`crate::tess::tessellate_chunk_filtered`]). Always `false` when
    /// there is no overlay — the hot path is untouched.
    pub dirty: bool,
    /// The chunk's loose bbox in **cell-local** nm (a copy of `Chunk::bbox`).
    /// Phase 4 STEP 2: when this chunk's geometry hasn't been tessellated yet
    /// (a worker job is in flight), the renderer draws this box — transformed to
    /// world at each placement — as a coarse proxy until the real mesh arrives.
    pub bbox: crate::types::Bbox,
}

/// Result of a viewport query.
#[derive(Debug, Default)]
pub struct VisibleSet {
    pub draws: Vec<CellDraw>,
    /// World-space boxes for cells too small to draw as geometry (LOD proxies).
    pub proxies: Vec<Bboxf>,
    /// Phase 3: overlay geometry (moved copies + added shapes) visible this
    /// frame, in world nm. Empty unless an overlay was passed. Drawn on top of
    /// the base by the renderer; the base half of every move is suppressed via
    /// [`CellDraw::dirty`], so nothing double-draws.
    pub overlay: Vec<Shape>,
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
    cull_inner(db, view, px_per_nm, min_pixel, false, None)
}

/// Like [`cull`] but also fills `stats.visible_shapes` by querying each visible
/// cell's index. **Off the hot path** — for the bench/shot harnesses and an
/// opt-in debug readout, never per frame.
pub fn cull_counted(db: &CellDb, view: &Bboxf, px_per_nm: f64, min_pixel: f64) -> VisibleSet {
    cull_inner(db, view, px_per_nm, min_pixel, true, None)
}

/// Cull with an editing overlay composited in (Phase 3). Identical to [`cull`]
/// for cells the overlay does not touch — the per-draw cost is one `HashSet`
/// lookup, and only for the dozens of cells actually emitted — so a viewport over
/// unedited geometry pays effectively nothing. An **empty** overlay produces a
/// byte-identical result to [`cull`] (the golden invariant).
pub fn cull_with_overlay(
    db: &CellDb,
    view: &Bboxf,
    px_per_nm: f64,
    min_pixel: f64,
    overlay: &EditOverlay,
) -> VisibleSet {
    cull_inner(db, view, px_per_nm, min_pixel, false, Some(overlay))
}

fn cull_inner(
    db: &CellDb,
    view: &Bboxf,
    px_per_nm: f64,
    min_pixel: f64,
    count_visible: bool,
    overlay: Option<&EditOverlay>,
) -> VisibleSet {
    let mut ctx = CullCtx {
        db,
        view: *view,
        px_per_nm,
        min_pixel,
        count_visible,
        overlay,
        grouped: HashMap::new(),
        dirty_cells: std::collections::HashSet::new(),
        edited_placements: HashMap::new(),
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
    let dirty_cells = std::mem::take(&mut ctx.dirty_cells);
    let mut grouped: Vec<((CellId, ChunkId), Vec<Transform>)> = ctx.grouped.into_iter().collect();
    grouped.sort_unstable_by_key(|&(key, _)| key);
    for ((cell, chunk), transforms) in grouped {
        ctx.set.stats.instances_emitted += transforms.len() as u64;
        let dirty = dirty_cells.contains(&cell);
        let bbox = db.cell(cell).chunks[chunk as usize].bbox;
        ctx.set.draws.push(CellDraw { cell, chunk, transforms, dirty, bbox });
    }
    ctx.set.stats.proxy_count = ctx.set.proxies.len();

    // Emit overlay geometry (moved copies + added), in world nm, at EVERY placement
    // of each edited cell — so an edit to a cell master shows at all its instances
    // (the base half of every move is suppressed at all placements too, via the
    // cell-local dirty chunk). For the TOP cell this is one placement at identity,
    // so it reduces to the skeleton's behavior byte-for-byte.
    let edited_placements = std::mem::take(&mut ctx.edited_placements);
    if let Some(o) = overlay {
        emit_overlay(o, &edited_placements, view, &mut ctx.set.overlay);
    }

    ctx.set
}

/// Collect the overlay's moved/added shapes, each transformed to world at every
/// placement of its owning cell, that fall in `view`.
fn emit_overlay(
    overlay: &EditOverlay,
    placements: &HashMap<CellId, Vec<Transform>>,
    view: &Bboxf,
    out: &mut Vec<Shape>,
) {
    let mut emit = |cell: CellId, shape: &Shape| {
        let Some(tfs) = placements.get(&cell) else { return };
        for tf in tfs {
            let world = transform_shape(shape, tf);
            if shape_in_view(&world, view) {
                out.push(world);
            }
        }
    };
    for (&(cell, _), shape) in &overlay.edited {
        emit(cell, shape);
    }
    for (cell, shape) in &overlay.added {
        emit(*cell, shape);
    }
}

/// Apply `tf` to every point of a cell-local shape, producing a world-nm shape.
fn transform_shape(s: &Shape, tf: &Transform) -> Shape {
    Shape {
        layer: s.layer,
        points: s
            .points
            .iter()
            .map(|&p| {
                let (x, y) = tf.apply_point(p);
                crate::types::Point::new(x.round() as i32, y.round() as i32)
            })
            .collect(),
    }
}

/// Cheap axis-aligned overlap of a polygon's bbox against the viewport.
fn shape_in_view(s: &Shape, view: &Bboxf) -> bool {
    let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in &s.points {
        lo_x = lo_x.min(p.x);
        lo_y = lo_y.min(p.y);
        hi_x = hi_x.max(p.x);
        hi_y = hi_y.max(p.y);
    }
    if lo_x > hi_x {
        return false;
    }
    (lo_x as f64) <= view.max_x
        && (hi_x as f64) >= view.min_x
        && (lo_y as f64) <= view.max_y
        && (hi_y as f64) >= view.min_y
}

struct CullCtx<'a> {
    db: &'a CellDb,
    view: Bboxf,
    px_per_nm: f64,
    min_pixel: f64,
    /// When false, skip the per-cell index query that fills `visible_shapes`.
    count_visible: bool,
    /// Phase 3 overlay, or `None` for the pristine Phase 2b path.
    overlay: Option<&'a EditOverlay>,
    grouped: HashMap<(CellId, ChunkId), Vec<Transform>>,
    /// Cells emitted this frame that the overlay edits — their base chunks are
    /// marked `dirty` so tessellation drops the suppressed shapes.
    dirty_cells: std::collections::HashSet<CellId>,
    /// World transforms at which each edited cell is drawn this frame — where the
    /// overlay's moved/added geometry must be emitted (one entry per placement).
    edited_placements: HashMap<CellId, Vec<Transform>>,
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
            // Phase 3: flag cells the overlay edits so their base chunks are
            // re-tessellated with suppressed shapes removed, and record this
            // placement so the overlay's moved/added geometry is drawn here too.
            // One hash lookup, only for emitted cells and only when an overlay is
            // present — the pristine path (overlay = None) never runs this.
            if self.overlay.map_or(false, |o| o.edits_cell(id)) {
                self.dirty_cells.insert(id);
                self.edited_placements.entry(id).or_default().push(world_tf);
            }
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
/// `pub(crate)` so the hit-test walk ([`crate::pick`]) can place AREF elements.
pub(crate) fn placement(base: &Transform, col_step: [f64; 2], row_step: [f64; 2], c: i32, r: i32) -> Transform {
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
/// `pub(crate)` so the hit-test walk ([`crate::pick`]) can find the AREF elements
/// under a point by passing a tiny box around it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn visible_range(
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
