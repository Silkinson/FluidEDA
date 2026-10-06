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
use crate::db::{Cell, CellDb, CellId, Instance, InstanceKind, RemotePlacements};
use crate::edit::{EditOverlay, Shape};
use crate::transform::{Bboxf, Transform};
use crate::types::Bbox;
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
    /// The colour this chunk's proxy draws in, packed straight RGBA8 — what
    /// its layers make from far away (a streamed design, Phase 7.3 SLICE C);
    /// 0 = the neutral proxy.
    pub rgba: u32,
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
    /// Streamed designs (Phase 7.3 SLICE B): placement chunks on screen that
    /// have not arrived (drawn as proxies until they do) …
    pub placements_wanted: Vec<(CellId, u32)>,
    /// … and resident ones walked this frame (their residency's "last used").
    pub placements_used: Vec<(CellId, u32)>,
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
    /// Placement chunks skipped whole because their footprint missed the view
    /// (Phase 7.3).
    pub inst_chunks_skipped: u64,
    /// Streamed cells' placement chunks drawn as one proxy for density.
    pub dense_inst_chunks: u64,
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
    // Phase 5d: each edited cell's footprint grown by its own overlay geometry,
    // so a shape drawn beyond a cell's load-time extent — or into a cell with
    // no geometry at all — is not culled with the old bbox. O(overlay), once.
    let mut grown: HashMap<CellId, Bbox> = HashMap::new();
    if let Some(o) = overlay {
        let mut grow = |cell: CellId, pts: &[crate::types::Point]| {
            let b = grown.entry(cell).or_insert_with(|| db.local_bbox(cell));
            for p in pts {
                b.expand(*p);
            }
        };
        for (&(c, _), sh) in &o.edited {
            grow(c, &sh.points);
        }
        for a in &o.added {
            grow(a.cell, &a.shape.points);
        }
    }
    let mut ctx = CullCtx {
        db,
        view: *view,
        px_per_nm,
        min_pixel,
        count_visible,
        overlay,
        grown,
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
        let c = db.cell(cell);
        let bbox = c.chunks[chunk as usize].bbox;
        let rgba = c.chunk_rgba.get(chunk as usize).copied().unwrap_or(0);
        ctx.set.draws.push(CellDraw { cell, chunk, transforms, dirty, bbox, rgba });
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
    for a in &overlay.added {
        emit(a.cell, &a.shape);
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
    /// Edited cells' bboxes grown by their overlay geometry (see `cull_inner`).
    grown: HashMap<CellId, Bbox>,
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

        let local_bb = self.grown.get(&id).copied().unwrap_or_else(|| self.db.local_bbox(id));
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

        // Phase 3: flag cells the overlay edits so their base chunks are
        // re-tessellated with suppressed shapes removed, and record this
        // placement so the overlay's moved/added geometry is drawn here too.
        // One hash lookup, only for visited cells and only when an overlay is
        // present — the pristine path (overlay = None) never runs this. Phase
        // 5d: recorded even for a cell with no base shapes of its own, so a
        // shape drawn into a pure-hierarchy cell draws at its placements.
        if self.overlay.map_or(false, |o| o.edits_cell(id)) {
            self.dirty_cells.insert(id);
            self.edited_placements.entry(id).or_default().push(world_tf);
        }

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

        // Descend into child instances, a placement chunk at a time (Phase
        // 7.3): a chunk whose footprint misses the view is skipped with one
        // test instead of one per placement — caravel_core's 460 k standard
        // cells were each visited every frame just to be found off-screen.
        // While an edit has grown a cell past its stored bbox, the chunks'
        // precomputed footprints may be too small, so every chunk is walked
        // (exactly the pre-7.3 behavior).
        // Phase 5d: placements hidden by "edit this placement only" are skipped
        // (their geometry is drawn as the parent's added shapes instead).
        let hides = self.overlay.is_some_and(|o| o.hides_in(id));
        if let Some(remote) = &cell.remote {
            self.walk_remote(id, cell, remote, world_tf, hides);
            return;
        }
        let chunk_view =
            (cell.inst_chunks.len() > 1 && self.grown.is_empty()).then(|| world_tf.inverse().apply_bboxf(&self.view));
        for ch in &cell.inst_chunks {
            if chunk_view.as_ref().is_some_and(|v| !bbox_intersects(&ch.bbox, v)) {
                self.set.stats.inst_chunks_skipped += 1;
                continue;
            }
            for ii in ch.start as usize..ch.end as usize {
                self.descend(id, ii, &cell.instances[ii], world_tf, hides);
            }
        }
    }

    /// The placements of a streamed cell (Phase 7.3 SLICE B), a placement
    /// chunk at a time. A chunk whose placements would average under
    /// `min_pixel` across draws as one proxy, resident or not — so the work
    /// at a zoomed-out view is bounded by the screen, not the placement
    /// count. A visible chunk that has not arrived draws as a proxy and is
    /// asked for (`placements_wanted`); a resident one is walked like
    /// in-core placements and reported in `placements_used`.
    fn walk_remote(&mut self, id: CellId, cell: &Cell, remote: &RemotePlacements, world_tf: Transform, hides: bool) {
        let local_view = world_tf.inverse().apply_bboxf(&self.view);
        for (ci, ch) in cell.inst_chunks.iter().enumerate() {
            if !bbox_intersects(&ch.bbox, &local_view) {
                self.set.stats.inst_chunks_skipped += 1;
                continue;
            }
            let wb = world_tf.apply_bbox(&ch.bbox);
            let px = (wb.width() * self.px_per_nm).max(1.0) * (wb.height() * self.px_per_nm).max(1.0);
            if px < ch.len() as f64 * self.min_pixel * self.min_pixel {
                self.set.proxies.push(clip(&wb, &self.view));
                self.set.stats.dense_inst_chunks += 1;
                continue;
            }
            match remote.chunks.get(ci) {
                Some(Some(insts)) => {
                    self.set.placements_used.push((id, ci as u32));
                    for (k, inst) in insts.iter().enumerate() {
                        self.descend(id, ch.start as usize + k, inst, world_tf, hides);
                    }
                }
                _ => {
                    self.set.proxies.push(clip(&wb, &self.view));
                    self.set.placements_wanted.push((id, ci as u32));
                }
            }
        }
    }

    /// Walk placement `inst` (number `ii`) of cell `id` (`world_tf` = the
    /// cell's own world transform).
    fn descend(&mut self, id: CellId, ii: usize, inst: &Instance, world_tf: Transform, hides: bool) {
        match inst.kind() {
            InstanceKind::Single => {
                if hides && self.overlay.is_some_and(|o| o.is_hidden((id, ii as u32, 0, 0))) {
                    return;
                }
                self.walk(inst.child, world_tf.then(&inst.transform()));
            }
            InstanceKind::Array { cols, rows, col_step, row_step } => {
                let hidden_in = hides.then_some((id, ii as u32));
                self.walk_array(inst.child, world_tf, inst.transform(), cols, rows, col_step, row_step, hidden_in);
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
        hidden_in: Option<(CellId, u32)>,
    ) {
        if cols <= 0 || rows <= 0 {
            return;
        }
        let child_bb = self.grown.get(&child).copied().unwrap_or_else(|| self.db.local_bbox(child));
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
                if let (Some((p, ii)), Some(o)) = (hidden_in, self.overlay) {
                    if o.is_hidden((p, ii, c, r)) {
                        continue; // this element was flattened into the parent
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CellDbBuilder, Instance, InstanceKind};
    use crate::edit::{EditOverlay, Shape};
    use crate::types::{Layer, Point};

    /// Phase 5d: a shape drawn into a cell that has NO geometry of its own (and
    /// so an empty load-time bbox) is emitted at every placement of that cell.
    #[test]
    fn drawn_shape_in_an_empty_cell_shows_at_every_placement() {
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let empty = b.intern("EMPTY");
        b.cell_mut(top).shapes.push(Layer::new(1, 0), [
            Point::new(0, 0),
            Point::new(10_000, 0),
            Point::new(10_000, 10_000),
            Point::new(0, 10_000),
        ]);
        for x in [1_000.0, 6_000.0] {
            b.cell_mut(top).instances.push(Instance::new(empty, Transform::translate(x, 1_000.0), InstanceKind::Single));
        }
        let db = b.build().unwrap();
        let mut o = EditOverlay::new();
        let sq = vec![Point::new(0, 0), Point::new(500, 0), Point::new(500, 500), Point::new(0, 500), Point::new(0, 0)];
        o.add((1, 0), empty, Shape { layer: Layer::new(2, 0), points: sq });
        let view = Bboxf::new(-1_000.0, -1_000.0, 11_000.0, 11_000.0);
        let vs = cull_with_overlay(&db, &view, 0.1, 0.5, &o);
        assert_eq!(vs.overlay.len(), 2, "one world copy per placement");
        let mut xs: Vec<i32> = vs.overlay.iter().map(|s| s.points[0].x).collect();
        xs.sort();
        assert_eq!(xs, vec![1_000, 6_000]);
    }

    /// Phase 7.3: culling a placement chunk at a time must select exactly what
    /// walking every placement does — same draws (cell, chunk, transforms) and
    /// same proxies — at every view, including views that cut chunks in half.
    #[test]
    fn placement_chunks_cull_exactly_like_walking_every_placement() {
        use crate::chunk::Chunk;
        fn lattice() -> CellDb {
            let mut b = CellDbBuilder::new();
            let top = b.intern("TOP");
            let leaf = b.intern("LEAF");
            let tall = b.intern("TALL");
            b.cell_mut(leaf).shapes.push(Layer::new(1, 0), sq(0, 0, 800));
            b.cell_mut(tall).shapes.push(Layer::new(2, 0), sq(0, 0, 300));
            // 6,000 scattered single placements (enough to chunk) …
            for i in 0..6_000i32 {
                let (x, y) = ((i * 7_919) % 100_000, (i * 104_729) % 80_000);
                b.cell_mut(top).instances.push(Instance::new(leaf, Transform::translate(x as f64, y as f64), InstanceKind::Single));
            }
            // … and an array, which is one placement record covering many.
            b.cell_mut(top).instances.push(Instance::new(tall, Transform::translate(-5_000.0, 0.0), InstanceKind::Array { cols: 3, rows: 40, col_step: [1_000.0, 0.0], row_step: [0.0, 2_000.0] }));
            b.build().unwrap()
        }
        fn sq(x: i32, y: i32, s: i32) -> Vec<Point> {
            vec![Point::new(x, y), Point::new(x + s, y), Point::new(x + s, y + s), Point::new(x, y + s)]
        }
        type Canon = (Vec<(CellId, ChunkId, Vec<[i64; 6]>)>, Vec<[i64; 4]>);
        fn canon(vs: &VisibleSet) -> Canon {
            let q = |v: f64| (v * 1e3).round() as i64;
            let mut draws: Vec<_> = vs
                .draws
                .iter()
                .map(|d| {
                    let mut t: Vec<[i64; 6]> =
                        d.transforms.iter().map(|t| [q(t.a), q(t.b), q(t.c), q(t.d), q(t.tx), q(t.ty)]).collect();
                    t.sort();
                    (d.cell, d.chunk, t)
                })
                .collect();
            draws.sort();
            let mut proxies: Vec<[i64; 4]> = vs.proxies.iter().map(|p| [q(p.min_x), q(p.min_y), q(p.max_x), q(p.max_y)]).collect();
            proxies.sort();
            (draws, proxies)
        }
        let chunked = lattice();
        let top = chunked.top;
        assert!(chunked.cell(top).inst_chunks.len() > 1, "the lattice must chunk");
        // The reference: same database with every placement in one chunk.
        let mut walked = lattice();
        let n = walked.cells[top].instances.len() as u32;
        walked.cells[top].inst_chunks = vec![Chunk { start: 0, end: n, bbox: walked.local_bbox(top) }];
        let views = [
            Bboxf { min_x: -6_000.0, min_y: -1_000.0, max_x: 101_000.0, max_y: 81_000.0 }, // all
            Bboxf { min_x: 10_000.0, min_y: 10_000.0, max_x: 14_000.0, max_y: 13_000.0 },  // leaf-ish
            Bboxf { min_x: 49_000.0, min_y: -500.0, max_x: 52_000.0, max_y: 80_500.0 },    // a stripe across chunks
            Bboxf { min_x: -5_500.0, min_y: 30_000.0, max_x: -3_000.0, max_y: 50_000.0 },   // the array only
        ];
        for v in views {
            for scale in [0.002, 0.05, 1.0] {
                let a = cull(&chunked, &v, scale, 4.0);
                let b = cull(&walked, &v, scale, 4.0);
                assert_eq!(canon(&a), canon(&b), "view {v:?} at {scale} px/nm");
                assert!(a.stats.nodes_visited <= b.stats.nodes_visited);
            }
        }
        // A small view really skips most of the lattice.
        let small = cull(&chunked, &views[1], 1.0, 4.0);
        assert!(small.stats.inst_chunks_skipped > 0);
    }

}
