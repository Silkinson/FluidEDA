//! The eframe application: hierarchical layout canvas + side panel (layers,
//! room, fps, cull stats, load metrics). Phase 1.5 adds **runtime file loading**
//! (drag-and-drop, a file picker, and `?file=` autoload on web) so real chips —
//! which cannot be embedded in the wasm — can be opened, and instruments the
//! load path (parse / tessellation / index / TTFF / wasm heap).

use crate::design::{self, Design, Metrics};
use crate::paint::{init_renderer, renderer_mut, LayoutPaint};
use crate::webio::{self, LoadSlot};
use eframe::egui;
use fluideda_core::{
    cull, cull_with_overlay, pick_hierarchy, BaseShapeId, CellDb, CellId, ChunkId, CullStats,
    EditOverlay, Layer, Point, Transform, VisibleSet,
};
use fluideda_render::Camera;
use fluideda_sync::{color_from_id, EditKind, Marker, Presence, Session};
use glam::Vec2;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Zero-config default: the tiny embedded hierarchical sample.
const HIER_GDS: &[u8] = include_bytes!("../../../assets/hier.gds");

/// A cell projecting smaller than this many pixels becomes an LOD proxy.
const MIN_PIXEL: f64 = 4.0;

/// Phase 4 STEP 2: a visible **pristine** chunk with at least this many shapes is
/// tessellated in the worker (fit-view / pan offload); smaller chunks tessellate
/// inline, since a worker round-trip costs more than just doing them. Set above
/// `EDIT_CHUNK_TARGET` (3 k) so an edit's finer chunks always stay inline (they
/// need overlay suppressions and are already < 1 ms). Only consulted on web with
/// worker support; native / no-worker always tessellates inline.
const TESS_WORKER_MIN_SHAPES: usize = 8_000;

/// Bound the per-frame main-thread cost of the offload: at most this many new jobs
/// dispatched (each copies one chunk's columns into a transferable buffer) and this
/// many finished chunks uploaded, per frame. Keeps the longest fit-view frame small
/// while the die streams in as geometry over the following frames.
const TESS_JOBS_PER_FRAME: usize = 8;

/// Offload only kicks in when **more** than this many big chunks are unresolved in
/// one frame (the fit-view freeze). At or below it — a leaf zoom or a gentle pan —
/// the few big chunks tessellate inline, so leaf stays byte-for-byte the same
/// experience and no bbox proxy flashes for a small zoom-in. Kept low enough that
/// the inline case (`≤ this × ~9 ms/chunk`) is well under the 100 ms frame target.
const TESS_INLINE_MAX: usize = 6;

pub struct FluidApp {
    camera: Camera,
    db: CellDb,
    layers: Vec<Layer>,
    layer_visible: Vec<bool>,
    renderer_ready: bool,
    bbox_min: Vec2,
    bbox_max: Vec2,
    visible: Arc<VisibleSet>,
    cull_stats: CullStats,
    total_shapes: u64,
    metrics: Metrics,

    // runtime loading
    load_slot: LoadSlot,
    /// Worker-parsed designs land here (Phase 2b STEP 2); drained before load_slot.
    design_slot: webio::DesignSlot,
    load_instant: web_time::Instant,
    awaiting_first_frame: bool,
    status: String,
    /// Optional `?zoom=` viewport override for the browser bench.
    forced_zoom: Option<String>,
    /// Off by default: when on, cull counts visible shapes and TTFF forces all
    /// R-trees. Both are expensive and only feed the stats readout — kept off the
    /// hot path (Phase 2a STEP 2).
    debug_stats: bool,
    /// Cell chunks whose geometry has been tessellated + uploaded (lazy). Keyed
    /// by (cell, chunk) since Phase 2b — a die-spanning cell uploads only the
    /// chunks the viewport has actually touched.
    uploaded: HashSet<(CellId, ChunkId)>,
    /// Cumulative main-thread tessellation time so far (ms) — the lazy TTFF cost.
    /// Phase 4 STEP 2: this now counts only chunks tessellated *inline*; the big
    /// fit-view chunks tessellate in the worker and never touch this number, which
    /// is the point — it is the residual main-thread freeze, now ~0.
    tess_accum_ms: f64,
    /// Cumulative uploaded GPU geometry bytes so far.
    gpu_bytes: usize,

    // --- Phase 4 STEP 2: off-main-thread (worker) tessellation ---
    /// Persistent tessellation worker. `None` on native / no-worker / before the
    /// first offloaded chunk (spawned lazily). Dropping it terminates the worker.
    tess: Option<webio::TessWorker>,
    /// Completed worker jobs, drained (rate-limited) each frame in `ensure_uploaded`.
    tess_results: webio::TessSlot,
    /// Chunks a worker job is in flight for (dispatched, not yet returned) — so a
    /// chunk isn't re-dispatched every frame while its bbox proxy shows.
    tess_pending: HashSet<(CellId, ChunkId)>,
    /// Epoch bumped on load / chunk re-partition and stamped on every job; a reply
    /// whose `gen` differs is stale (its `(cell, chunk)` no longer means the same
    /// geometry) and is dropped on arrival.
    tess_gen: u32,
    /// Chunks tessellated in the worker so far (panel readout; the offload count).
    tess_worker_chunks: usize,
    /// STEP 2 measurement: longest single `update()` (main-thread frame) since the
    /// current design installed, in ms — the freeze metric. Reset on load; the
    /// inline fit path spikes it to seconds, the offload keeps it small. Published
    /// to `window.__fluideda_perf` each frame for the browser harness.
    max_frame_ms: f64,
    perf_frames: u64,
    /// `?tessinline=1`: force the pre-STEP-2 inline path (offload off), so the
    /// before/after fit-view freeze can be measured on one build.
    force_inline: bool,

    // --- Phase 3 editing ---
    /// Copy-on-write edits over the immutable base. Empty ⇒ pristine Phase 2b
    /// render path (the cull hot path is unchanged; see `recull`).
    overlay: EditOverlay,
    /// When on, a left-click selects the shape under the cursor (any cell, through
    /// the instance hierarchy) and drag/nudge/delete edit it; pan/zoom still work.
    edit_mode: bool,
    /// The currently selected base shape — a cell **master** + shape index; editing
    /// it updates every placement of that cell (SLICE 2: any cell, not just TOP).
    selected: Option<BaseShapeId>,
    /// World transform of the placement that was clicked, for the highlight and for
    /// mapping a world-space drag into the selected cell's local frame. Identity
    /// for a TOP-cell selection.
    selected_tf: Transform,
    /// Current translation applied to the selection, in the selected cell's LOCAL
    /// nm, measured from its base. Accumulated by nudges/drag; the moved copy is
    /// always base + this (so it round-trips through the CRDT as "M dx dy").
    move_delta: (i32, i32),
    /// World point where a move-drag began (`None` when not dragging a shape).
    drag_move_from: Option<Vec2>,
    /// `move_delta` at the moment a move-drag began.
    drag_move_base: (i32, i32),
    /// Base chunks whose geometry an edit invalidated — re-tessellated (with
    /// suppressions) on the next `ensure_uploaded`. Only these rebuild, not the
    /// whole cell: the incremental-edit property.
    dirty_keys: HashSet<(CellId, ChunkId)>,
    /// Cells whose chunk table was just re-partitioned (finer, on first big-cell
    /// edit): the renderer must drop all their now-stale (cell, old-chunk) buffers.
    renderer_drop_cells: HashSet<CellId>,
    /// Overlay geometry needs re-upload (set when the overlay changed).
    overlay_dirty: bool,
    /// Last hit-test latency, shown in the panel (the lazy-index-build cost).
    last_pick_us: f64,
    /// Instrumentation for the last edit: chunks re-tessellated + wall time.
    last_edit_chunks: usize,
    last_edit_ms: f64,

    /// Always present: an offline edit session (shared doc + per-user undo) that
    /// `join` upgrades to a live connection. Editing and undo work before any
    /// connection; joining a room then syncs.
    session: Session,
    name: String,
    last_frame: f64,
    fps: f32,
    dragging: bool,
}

impl FluidApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Load the embedded default synchronously so there is always something.
        let design = design::load_design("hier.gds (embedded)", HIER_GDS)
            .expect("embedded hier.gds should parse");

        if let Some(state) = cc.wgpu_render_state.as_ref() {
            init_renderer(state);
        }

        let mut camera = Camera::new(Vec2::new(1280.0, 800.0));
        camera.fit(design.bbox_min, design.bbox_max);

        let load_slot: LoadSlot = Rc::new(RefCell::new(None));
        let design_slot: webio::DesignSlot = Rc::new(RefCell::new(None));

        // Web: honor ?file=<url>. Prefer the off-main-thread worker parse; the URL
        // path streams the bytes straight into the worker (no main-thread copy).
        if let Some(url) = webio::query_file() {
            log::info!("autoloading ?file={url}");
            if webio::worker_supported() {
                webio::spawn_worker_fetch(url, design_slot.clone());
            } else {
                webio::spawn_fetch(url, load_slot.clone());
            }
        }

        let mut app = Self {
            camera,
            db: design.db,
            layers: design.layers,
            layer_visible: Vec::new(),
            renderer_ready: cc.wgpu_render_state.is_some(),
            bbox_min: design.bbox_min,
            bbox_max: design.bbox_max,
            visible: Arc::new(VisibleSet::default()),
            cull_stats: CullStats::default(),
            total_shapes: design.total_shapes,
            metrics: design.metrics,
            load_slot,
            design_slot,
            load_instant: web_time::Instant::now(),
            awaiting_first_frame: true, // measure TTFF for the default too
            status: String::new(),
            forced_zoom: webio::query_zoom(),
            debug_stats: webio::query_flag("stats"),
            uploaded: HashSet::new(),
            tess_accum_ms: 0.0,
            gpu_bytes: 0,
            tess: None,
            tess_results: Rc::new(RefCell::new(Vec::new())),
            tess_pending: HashSet::new(),
            tess_gen: 0,
            tess_worker_chunks: 0,
            max_frame_ms: 0.0,
            perf_frames: 0,
            force_inline: webio::query_flag("tessinline"),
            overlay: EditOverlay::new(),
            edit_mode: false,
            selected: None,
            selected_tf: Transform::IDENTITY,
            move_delta: (0, 0),
            drag_move_from: None,
            drag_move_base: (0, 0),
            dirty_keys: HashSet::new(),
            renderer_drop_cells: HashSet::new(),
            overlay_dirty: false,
            last_pick_us: 0.0,
            last_edit_chunks: 0,
            last_edit_ms: 0.0,
            session: Session::new(Presence {
                cursor: Vec2::ZERO,
                name: "guest".to_string(),
                color: color_from_id(0),
            }),
            name: "guest".to_string(),
            last_frame: 0.0,
            fps: 0.0,
            dragging: false,
        };
        app.layer_visible = vec![true; app.layers.len()];
        app
    }

    /// Install a freshly parsed design: swap the DB, reset the (empty) GPU
    /// renderer, camera and layer list. Tessellation is deferred — cells upload
    /// lazily as they become visible (STEP 3).
    fn install(&mut self, design: Design, frame: &mut eframe::Frame) {
        self.db = design.db;
        self.layers = design.layers;
        self.layer_visible = vec![true; self.layers.len()];
        self.bbox_min = design.bbox_min;
        self.bbox_max = design.bbox_max;
        self.total_shapes = design.total_shapes;
        self.metrics = design.metrics;
        self.camera.fit(self.bbox_min, self.bbox_max);
        self.apply_forced_zoom();
        self.uploaded.clear();
        self.tess_accum_ms = 0.0;
        self.gpu_bytes = 0;
        // A fresh design has its own shape ids, so old edits are meaningless —
        // reset to a fresh offline session (rejoin to collaborate on the new
        // design). Editing/undo keep working immediately.
        self.overlay = EditOverlay::new();
        self.selected = None;
        self.selected_tf = Transform::IDENTITY;
        self.move_delta = (0, 0);
        self.drag_move_from = None;
        self.dirty_keys.clear();
        self.renderer_drop_cells.clear();
        self.overlay_dirty = false;
        // Drop the old design's tessellation worker + any in-flight jobs.
        self.reset_tess();
        self.tess_worker_chunks = 0;
        // Fresh freeze measurement for this design (from the install frame onward).
        self.max_frame_ms = 0.0;
        self.perf_frames = 0;
        self.session = Session::new(Presence {
            cursor: Vec2::ZERO,
            name: self.name.clone(),
            color: color_from_id(0),
        });

        if let Some(rs) = frame.wgpu_render_state() {
            // Reuse the existing renderer (keeping its compiled pipeline) rather
            // than rebuilding it — a fresh HierRenderer recompiles the shader
            // program, ~50-100 ms in WebGL2, which would land as a stall on the
            // design-swap frame. Reset drops the old design's GPU buffers only.
            let mut guard = rs.renderer.write();
            match renderer_mut(&mut guard.callback_resources) {
                Some(r) => r.reset(),
                None => {
                    drop(guard);
                    init_renderer(rs);
                }
            }
            self.renderer_ready = true;
        }
        self.awaiting_first_frame = true;
    }

    /// Hit-test a world point through the instance hierarchy and update the
    /// selection. Resolves to a cell **master** + shape, plus the clicked
    /// placement's world transform (SLICE 2 — any cell, under a transform stack).
    fn pick_at(&mut self, world: Vec2) {
        let p = Point::new(world.x.round() as i32, world.y.round() as i32);
        let t = web_time::Instant::now();
        let hit = pick_hierarchy(&self.db, p);
        self.last_pick_us = t.elapsed().as_secs_f64() * 1e6;
        match hit {
            Some(h) => {
                self.selected = Some(h.id);
                self.selected_tf = h.world_tf;
                self.move_delta = self.existing_delta(h.id);
            }
            None => {
                self.selected = None;
                self.selected_tf = Transform::IDENTITY;
                self.move_delta = (0, 0);
            }
        }
    }

    /// Does world point `w` land inside the selection's *current* geometry? Tests
    /// in the selected cell's local frame (inverse-mapping through the placement's
    /// transform), against the moved copy if moved else the base. Decides drag =
    /// move vs pan.
    fn point_hits(&self, id: BaseShapeId, w: Vec2) -> bool {
        let (lx, ly) = self.selected_tf.inverse().apply(w.x as f64, w.y as f64);
        let p = Point::new(lx.round() as i32, ly.round() as i32);
        match self.overlay.edited.get(&id) {
            Some(moved) => fluideda_core::point_in_polygon(&moved.points, p),
            None => fluideda_core::point_in_polygon(self.db.cell(id.0).shapes.points(id.1 as usize), p),
        }
    }

    /// The translation already applied to `id` by the overlay, or (0,0). Recovered
    /// by diffing the moved copy's first point against the base's.
    fn existing_delta(&self, id: BaseShapeId) -> (i32, i32) {
        match self.overlay.edited.get(&id) {
            Some(moved) => {
                let base = self.db.cell(id.0).shapes.points(id.1 as usize);
                match (base.first(), moved.points.first()) {
                    (Some(b), Some(m)) => (m.x - b.x, m.y - b.y),
                    _ => (0, 0),
                }
            }
            None => (0, 0),
        }
    }

    /// Live move **preview** during a drag: mutate the local overlay directly for
    /// responsiveness, without a CRDT write per frame. Copy-on-write — reads the
    /// base geometry, translates, records the moved copy; the base is never
    /// touched. The authoritative CRDT write happens once, on drag release
    /// ([`commit_move`](Self::commit_move)).
    fn preview_move(&mut self, id: BaseShapeId, dx: i32, dy: i32) {
        let cell = self.db.cell(id.0);
        let layer = cell.shapes.layer(id.1 as usize);
        let pts = cell.shapes.points(id.1 as usize);
        let moved = fluideda_core::edit::translate(layer, pts, dx, dy);
        self.overlay.move_shape(id, moved);
        self.move_delta = (dx, dy);
        self.invalidate(id);
    }

    /// Commit a move to the CRDT (broadcast to peers + pushed on the undo stack),
    /// then reconcile the overlay from the CRDT truth.
    fn commit_move(&mut self, id: BaseShapeId, dx: i32, dy: i32) {
        self.session.set_edit(id, EditKind::Move { dx, dy });
        self.reconcile_edits();
    }

    /// Nudge step in the selected cell's LOCAL nm — a visible fraction of the
    /// design, divided by the placement's scale so the on-screen move is the same
    /// size regardless of how deep (and scaled) the selected cell is.
    fn nudge_step(&self) -> i32 {
        let world = (self.bbox_max.x - self.bbox_min.x).max(1.0) as f64 * 0.02;
        let scale = self.selected_tf.scale_factor().max(1e-9);
        ((world / scale) as i32).max(1)
    }

    /// Convert a world-space delta into the selected cell's local frame (apply the
    /// inverse of the placement's linear part). For a TOP selection (identity) this
    /// is the identity.
    fn world_delta_to_local(&self, wdx: f64, wdy: f64) -> (i32, i32) {
        let t = &self.selected_tf;
        let det = t.a * t.d - t.b * t.c;
        if det.abs() < 1e-12 {
            return (wdx.round() as i32, wdy.round() as i32);
        }
        let ldx = (t.d * wdx - t.c * wdy) / det;
        let ldy = (-t.b * wdx + t.a * wdy) / det;
        (ldx.round() as i32, ldy.round() as i32)
    }

    /// Nudge the selection by `(dx, dy)` on top of its current move delta (a
    /// discrete, immediately-committed edit).
    fn nudge(&mut self, id: BaseShapeId, dx: i32, dy: i32) {
        let (cx, cy) = self.move_delta;
        self.commit_move(id, cx + dx, cy + dy);
    }

    /// Delete the selection: a CRDT tombstone, then reconcile. No base mutation —
    /// the containing chunk re-tessellates without the shape.
    fn delete_shape(&mut self, id: BaseShapeId) {
        self.session.set_edit(id, EditKind::Delete);
        self.reconcile_edits();
        if self.selected == Some(id) {
            self.selected = None;
            self.move_delta = (0, 0);
        }
    }

    /// Export the current design (base composited with the edit overlay) to GDSII
    /// and save it (web: download; native: write a file). Phase 3 SLICE 1.
    fn save_gds(&mut self) {
        let bytes = fluideda_core::write_gds(&self.db, &self.overlay);
        let name = out_name(&self.metrics.name);
        match webio::save_bytes(&name, &bytes) {
            Ok(()) => self.status = format!("saved {name} ({} KB)", bytes.len() / 1024),
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    /// Undo the last local edit (per-user; a peer's edits are never on our undo
    /// stack) and reconcile the overlay from the reverted CRDT state.
    fn do_undo(&mut self) {
        if self.session.undo() {
            self.selected = None;
            self.reconcile_edits();
        }
    }

    /// Redo the last undone local edit and reconcile.
    fn do_redo(&mut self) {
        if self.session.redo() {
            self.selected = None;
            self.reconcile_edits();
        }
    }

    /// Nudge target: keep an edited chunk small enough to re-tessellate under a
    /// frame. caravel_core's coarse ~35–75 k-shape chunks are re-tessellated whole
    /// on each edit (~20–43 ms); splitting the touched chunk to this size brings a
    /// single-shape edit to ~1–2 ms. Only large chunks of *edited* cells are split.
    const EDIT_CHUNK_TARGET: usize = 3000;

    /// Mark the chunk that owns `id`'s base shape for re-tessellation, and flag the
    /// overlay geometry for re-upload — the "incremental" property: one chunk
    /// rebuilds, not the cell. If that chunk is large (a big flat cell like
    /// caravel_core), first split it into finer contiguous sub-chunks so the
    /// re-tessellation stays under a frame; the split preserves shape indices, so
    /// it is safe for the CRDT and can happen independently on each peer.
    fn invalidate(&mut self, id: BaseShapeId) {
        if let Some(chunk) = self.db.cell(id.0).chunk_of(id.1) {
            let big = {
                let c = &self.db.cells[id.0].chunks[chunk as usize];
                (c.end - c.start) as usize > Self::EDIT_CHUNK_TARGET
            };
            if big {
                // Splitting shifts this cell's chunk ids, so drop all its uploads;
                // ensure_uploaded rebuilds the (now finer) visible chunks.
                self.db.cells[id.0].subdivide_chunk(chunk as usize, Self::EDIT_CHUNK_TARGET);
                self.uploaded.retain(|&(c, _)| c != id.0);
                self.dirty_keys.retain(|&(c, _)| c != id.0);
                // The chunk-id shift invalidates any in-flight worker jobs (their
                // keys now point at different geometry): reset the tess pipeline.
                self.reset_tess();
                self.renderer_drop_cells.insert(id.0);
                if let Some(fine) = self.db.cell(id.0).chunk_of(id.1) {
                    self.dirty_keys.insert((id.0, fine));
                }
            } else {
                self.dirty_keys.insert((id.0, chunk));
            }
        }
        self.overlay_dirty = true;
    }

    /// Invalidate the tessellation-worker pipeline: drop the worker (terminating
    /// it discards in-flight jobs and reclaims its wasm memory), clear the pending
    /// set + result queue, and bump the generation so any reply already queued is
    /// recognised as stale on arrival. Called on a new design load and on a chunk
    /// re-partition — both change what a `(cell, chunk)` key means. The worker
    /// respawns lazily the next time a chunk needs offloading.
    fn reset_tess(&mut self) {
        self.tess = None;
        self.tess_pending.clear();
        self.tess_results.borrow_mut().clear();
        self.tess_gen = self.tess_gen.wrapping_add(1);
    }

    /// Tessellate + upload any visible cell **chunk** not yet on the GPU. Runs
    /// after culling, before paint, so the paint callback sees uploaded buffers.
    ///
    /// Phase 4 STEP 2 routing — this is the single convergence point for fit-view
    /// first-paint AND editing's incremental re-tess:
    ///   * a `dirty` (edited) chunk tessellates **inline** with the overlay's
    ///     suppressions — tiny and < 1 ms, and it needs the overlay state;
    ///   * a big **pristine** chunk (the fit-view freeze) is sent to the worker and
    ///     drawn as a bbox proxy until its geometry arrives;
    ///   * a small pristine chunk tessellates inline — a round-trip isn't worth it.
    /// Dispatches and uploads are rate-limited so no single frame stalls.
    fn ensure_uploaded(&mut self, frame: &mut eframe::Frame) {
        let Some(rs) = frame.wgpu_render_state() else { return };
        let device = rs.device.clone();
        let mut guard = rs.renderer.write();
        let Some(renderer) = renderer_mut(&mut guard.callback_resources) else { return };

        // Phase 3 invalidation: drop the chunks an edit dirtied so the loop below
        // re-tessellates just those (with suppressions), not the whole cell.
        let editing = !self.dirty_keys.is_empty() || self.overlay_dirty || !self.renderer_drop_cells.is_empty();
        let edit_t = web_time::Instant::now();
        let mut rebuilt = 0usize;
        // A re-partitioned cell (SLICE 2 finer chunking) shifted its chunk ids —
        // drop all its stale GPU buffers before re-uploading the finer chunks.
        for cell in self.renderer_drop_cells.drain() {
            renderer.drop_cell(cell);
        }
        for key in self.dirty_keys.drain() {
            renderer.drop_chunk(key);
            self.uploaded.remove(&key);
        }

        // 0. Upload chunks the worker just finished (rate-limited). Stale replies —
        //    a different generation, or no longer expected — are dropped, not drawn.
        let mut ready: Vec<webio::TessReply> = Vec::new();
        {
            let mut q = self.tess_results.borrow_mut();
            let mut i = 0;
            while i < q.len() {
                if q[i].gen != self.tess_gen || !self.tess_pending.contains(&(q[i].cell, q[i].chunk)) {
                    q.swap_remove(i); // stale — drop, re-check the element swapped in
                } else if ready.len() < TESS_JOBS_PER_FRAME {
                    ready.push(q.swap_remove(i));
                } else {
                    i += 1; // over budget this frame; leave for the next
                }
            }
        }
        for reply in &ready {
            let key = (reply.cell, reply.chunk);
            self.tess_pending.remove(&key);
            renderer.upload_chunk(&device, key, &reply.meshes);
            self.uploaded.insert(key);
            self.tess_worker_chunks += 1;
        }

        let empty = std::collections::HashSet::new();
        let visible = self.visible.clone();

        // Decide per frame whether to offload. Only when MANY big chunks are
        // unresolved at once — the fit-view freeze, where the whole die's geometry
        // wants to tessellate in one frame. A leaf/pan view touching a handful of
        // big chunks tessellates them inline exactly as before (~22 ms, under a
        // frame), so leaf stays unchanged and no proxy flashes on a small zoom-in.
        // `tess_pending` (in-flight) chunks are still not-uploaded, so the count
        // stays high across the fit load and the mode doesn't flip mid-stream.
        let big_unresolved = visible
            .draws
            .iter()
            .filter(|d| {
                !d.dirty
                    && !self.uploaded.contains(&(d.cell, d.chunk))
                    && self.db.cells[d.cell].chunks[d.chunk as usize].len() >= TESS_WORKER_MIN_SHAPES
            })
            .count();
        let offload =
            webio::worker_supported() && !self.force_inline && big_unresolved > TESS_INLINE_MAX;

        let mut dispatched = 0usize;
        for d in &visible.draws {
            let key = (d.cell, d.chunk);
            if self.uploaded.contains(&key) || self.tess_pending.contains(&key) {
                continue; // already on the GPU, or a worker job is in flight (proxy shows)
            }
            let cell = &self.db.cells[d.cell];
            let chunk = &cell.chunks[d.chunk as usize];

            // Offload a big pristine chunk to the worker (drawn as a bbox proxy
            // until it lands); a dirty/small chunk — and everything when not in
            // offload mode — tessellates inline. See the fn note for the rationale.
            if offload && !d.dirty && chunk.len() >= TESS_WORKER_MIN_SHAPES {
                if dispatched >= TESS_JOBS_PER_FRAME {
                    continue; // dispatch budget spent; the proxy holds, retry next frame
                }
                if self.tess.is_none() {
                    match webio::TessWorker::new(self.tess_results.clone()) {
                        Ok(w) => self.tess = Some(w),
                        Err(e) => log::error!("tess worker unavailable: {e}"),
                    }
                }
                if let Some(w) = &self.tess {
                    let (coords, starts, layers) = cell.shapes.chunk_columns(chunk.start, chunk.end);
                    w.post(d.cell, d.chunk, self.tess_gen, coords, &starts, layers);
                    self.tess_pending.insert(key);
                    dispatched += 1;
                    continue;
                }
                // Worker spawn failed: fall through to inline so geometry still shows.
            }

            let t = web_time::Instant::now();
            // A dirty chunk (its cell has overlay edits) tessellates with the
            // suppressed base shapes removed; every other chunk is untouched.
            let meshes = if d.dirty {
                let suppressed = self.overlay.suppressed_in(d.cell).unwrap_or(&empty);
                fluideda_core::tessellate_chunk_filtered(&cell.shapes, chunk, suppressed)
            } else {
                fluideda_core::tessellate_chunk(&cell.shapes, chunk)
            };
            self.tess_accum_ms += t.elapsed().as_secs_f64() * 1e3;
            renderer.upload_chunk(&device, key, &meshes);
            self.uploaded.insert(key);
            rebuilt += 1;
        }

        // Re-upload the (tiny) overlay geometry when it changed.
        if self.overlay_dirty {
            let meshes = fluideda_core::tessellate_shapes(&visible.overlay);
            renderer.upload_overlay(&device, &meshes);
            self.overlay_dirty = false;
        }

        if editing {
            self.last_edit_chunks = rebuilt;
            self.last_edit_ms = edit_t.elapsed().as_secs_f64() * 1e3;
        }
        self.gpu_bytes = renderer.uploaded_bytes();
    }

    /// Consume any pending drag-drop / fetched / picked file.
    fn drain_loads(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 0. Worker-parsed design ready (Phase 2b STEP 2). Drained first so a
        //    freshly reconstructed design installs immediately.
        let worker_done = self.design_slot.borrow_mut().take();
        if let Some(res) = worker_done {
            match res {
                Ok(design) => {
                    self.install(design, frame);
                    self.status.clear();
                }
                Err(e) => self.status = format!("load failed: {e}"),
            }
        }

        // egui drag-and-drop (bytes on web, path on native).
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(bytes) = f.bytes {
                let name = if f.name.is_empty() { "dropped.gds".into() } else { f.name.clone() };
                *self.load_slot.borrow_mut() = Some((name, bytes.to_vec()));
            } else if let Some(path) = &f.path {
                match std::fs::read(path) {
                    Ok(bytes) => {
                        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file.gds").to_string();
                        *self.load_slot.borrow_mut() = Some((name, bytes));
                    }
                    Err(e) => self.status = format!("read failed: {e}"),
                }
            }
        }

        // Byte slot (drag-drop / picker). Take at most one per frame. On web with
        // worker support, hand the bytes off-thread instead of parsing here.
        let pending = self.load_slot.borrow_mut().take();
        if let Some((name, bytes)) = pending {
            self.load_instant = web_time::Instant::now();
            if webio::worker_supported() {
                self.status = format!("parsing {name} ({} KB) in worker…", bytes.len() / 1024);
                webio::spawn_worker_bytes(name, bytes, self.design_slot.clone());
            } else {
                self.status = format!("loading {name} ({} KB)…", bytes.len() / 1024);
                match design::load_design(&name, &bytes) {
                    Ok(design) => {
                        self.install(design, frame);
                        self.status.clear();
                    }
                    Err(e) => self.status = format!("parse failed: {e}"),
                }
            }
        }
    }

    /// On the frame *after* a load (so at least one frame has actually rendered),
    /// record wall-clock TTFF, force + time the R-tree build, and publish metrics.
    fn finish_first_frame(&mut self) {
        if !self.awaiting_first_frame || !self.renderer_ready {
            return;
        }
        self.metrics.ttff_ms = self.load_instant.elapsed().as_secs_f64() * 1e3;
        // Lazy tessellation: report what the initial view actually cost, not all defs.
        self.metrics.tess_ms = self.tess_accum_ms;
        self.metrics.vertex_bytes = self.gpu_bytes;
        self.metrics.index_bytes = 0;
        // Forcing every R-tree at load (708 ms on caravel) only served the metric.
        // Indices are lazy and only built when actually queried; measure them here
        // only under the debug flag, so a normal load never pays for them.
        if self.debug_stats {
            let (idx_total, idx_p99, idx_max) = design::measure_indices(&self.db);
            self.metrics.index_ms = idx_total;
            self.metrics.index_p99_ms = idx_p99;
            self.metrics.index_max_ms = idx_max;
        }
        self.metrics.wasm_heap_bytes = design::wasm_heap_bytes();
        self.metrics.publish();
        self.awaiting_first_frame = false;
    }

    /// Publish the per-frame perf snapshot to `window.__fluideda_perf` (wasm only):
    /// the longest main-thread frame since load (the freeze metric), frame count,
    /// worker vs inline tessellation counts. The browser harness polls this to
    /// prove the fit-view freeze is gone and geometry streamed in off-thread.
    #[cfg(target_arch = "wasm32")]
    fn publish_perf(&self) {
        let Some(w) = web_sys::window() else { return };
        let json = format!(
            "{{\"max_frame_ms\":{:.1},\"frames\":{},\"worker_chunks\":{},\"in_flight\":{},\
             \"inline_tess_ms\":{:.1},\"uploaded\":{},\"force_inline\":{},\
             \"last_edit_ms\":{:.2},\"last_edit_chunks\":{}}}",
            self.max_frame_ms,
            self.perf_frames,
            self.tess_worker_chunks,
            self.tess_pending.len(),
            self.tess_accum_ms,
            self.uploaded.len(),
            self.force_inline,
            self.last_edit_ms,
            self.last_edit_chunks,
        );
        let _ = js_sys::Reflect::set(&w, &"__fluideda_perf".into(), &json.into());
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn publish_perf(&self) {}

    /// Apply a `?zoom=` override (fit|mid|leaf) by setting the camera span,
    /// centered on the design. Used by the browser bench to measure fps at a
    /// viewport without input simulation.
    fn apply_forced_zoom(&mut self) {
        let Some(kw) = self.forced_zoom.clone() else { return };
        let w = (self.bbox_max.x - self.bbox_min.x).max(1.0) as f64;
        let h = (self.bbox_max.y - self.bbox_min.y).max(1.0) as f64;
        let span = match kw.as_str() {
            "mid" => (w * 0.03).clamp(20_000.0, 300_000.0),
            "leaf" => 5_000.0,
            _ => w.max(h) * 1.05, // "fit"
        };
        self.camera.center = (self.bbox_min + self.bbox_max) * 0.5;
        self.camera.scale = (self.camera.viewport.x as f64 / span) as f32;
    }

    fn recull(&mut self) {
        self.camera.world_origin = self.camera.center;
        let half_w = (self.camera.viewport.x as f64 * 0.5) / self.camera.scale as f64;
        let half_h = (self.camera.viewport.y as f64 * 0.5) / self.camera.scale as f64;
        let cx = self.camera.center.x as f64;
        let cy = self.camera.center.y as f64;
        let view = fluideda_core::Bboxf::new(cx - half_w, cy - half_h, cx + half_w, cy + half_h);
        // Hot path uses the uncounted cull; only the opt-in debug readout pays for
        // the per-cell index queries that fill visible_shapes. When there are edits
        // we composite the overlay — but only then, so an unedited design takes the
        // exact Phase 2b path (the "unedited cull time unchanged" requirement).
        let vs = if self.debug_stats {
            fluideda_core::cull_counted(&self.db, &view, self.camera.scale as f64, MIN_PIXEL)
        } else if self.overlay.is_empty() {
            cull(&self.db, &view, self.camera.scale as f64, MIN_PIXEL)
        } else {
            cull_with_overlay(&self.db, &view, self.camera.scale as f64, MIN_PIXEL, &self.overlay)
        };
        self.cull_stats = vs.stats;
        self.visible = Arc::new(vs);
    }

    fn connect(&mut self) {
        let url = server_url();
        self.session.local.name = self.name.clone();
        match self.session.join(&url) {
            Ok(()) => log::info!("joined {url}"),
            Err(e) => log::error!("join failed: {e}"),
        }
    }

    /// Rebuild the overlay from the CRDT (the source of truth) after any edit —
    /// local, remote, or undo — and invalidate the chunks whose edit-state
    /// changed. Skipped mid-drag so a live move preview isn't clobbered.
    fn reconcile_edits(&mut self) {
        let new = self.overlay_from_crdt();
        if new.deleted == self.overlay.deleted && new.edited == self.overlay.edited {
            return; // nothing edit-related changed (e.g. a marker-only update)
        }
        let changed: Vec<BaseShapeId> = changed_ids(&self.overlay, &new).into_iter().collect();
        self.overlay = new;
        // Route through the same invalidate() the direct-edit path uses, so a
        // committed or remote edit of a big cell also triggers the finer-chunking
        // (SLICE 2) — not just a local drag preview.
        for id in changed {
            self.invalidate(id);
        }
        // Keep the nudge accumulator + highlight in step with the authoritative state.
        if let Some(id) = self.selected {
            self.move_delta = self.existing_delta(id);
        }
    }

    /// Build an `EditOverlay` from the CRDT's edit records, translating each move
    /// against this client's base geometry. Out-of-range ids (a peer on a
    /// different file) are ignored rather than panicking.
    fn overlay_from_crdt(&self) -> EditOverlay {
        let mut o = EditOverlay::new();
        for (id, kind) in self.session.edits() {
            let (cell, idx) = id;
            if cell >= self.db.cells.len() || idx as usize >= self.db.cell(cell).shapes.len() {
                continue;
            }
            match kind {
                fluideda_sync::EditKind::Delete => o.delete(id),
                fluideda_sync::EditKind::Move { dx, dy } => {
                    let c = self.db.cell(cell);
                    let moved =
                        fluideda_core::edit::translate(c.shapes.layer(idx as usize), c.shapes.points(idx as usize), dx, dy);
                    o.move_shape(id, moved);
                }
            }
        }
        o
    }
}

/// Ids whose edit-state differs between two overlays (added, removed, or moved to
/// a new position) — the exact set of chunks an edit reconcile must invalidate.
fn changed_ids(old: &EditOverlay, new: &EditOverlay) -> std::collections::HashSet<BaseShapeId> {
    let mut ids = std::collections::HashSet::new();
    for id in old.deleted.iter().chain(new.deleted.iter()) {
        if old.deleted.contains(id) != new.deleted.contains(id) {
            ids.insert(*id);
        }
    }
    for id in old.edited.keys().chain(new.edited.keys()) {
        if old.edited.get(id) != new.edited.get(id) {
            ids.insert(*id);
        }
    }
    ids
}

impl eframe::App for FluidApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Time the whole main-thread frame (the fit-view freeze lives inside
        // `ensure_uploaded` below). `max_frame_ms` is the STEP 2 headline number.
        let frame_t = web_time::Instant::now();
        let now = ctx.input(|i| i.time);
        let dt = (now - self.last_frame).max(1e-4);
        self.fps = 0.9 * self.fps + 0.1 * (1.0 / dt) as f32;
        self.last_frame = now;

        // Finalize TTFF at the top of the frame *after* a load, so a frame has
        // actually rendered before we stamp the number.
        self.finish_first_frame();

        self.drain_loads(ctx, frame);

        self.session.local.name = self.name.clone();
        if self.session.poll() {
            // A remote edit (or marker) arrived. Don't rebuild the overlay mid-drag
            // or it would clobber the live move preview; reconcile on drag release.
            if self.drag_move_from.is_none() {
                self.reconcile_edits();
            }
            ctx.request_repaint();
        }

        self.drive_test_hook();

        self.side_panel(ctx, frame);
        self.canvas(ctx);
        self.recull();
        self.ensure_uploaded(frame);

        ctx.request_repaint();

        // Record the longest main-thread frame since load, and publish the perf
        // snapshot for the browser harness (STEP 2 measurement).
        let ms = frame_t.elapsed().as_secs_f64() * 1e3;
        self.max_frame_ms = self.max_frame_ms.max(ms);
        self.perf_frames += 1;
        self.publish_perf();
    }
}

impl FluidApp {
    /// Headless test surface (wasm only). Reads a one-shot command string from
    /// `window.__fluideda_cmd` and publishes edit state to `window.__fluideda_state`
    /// as JSON, so the two-browser sync test (scripts/edit_sync_test.mjs) can drive
    /// edits and assert convergence without pixel-accurate canvas clicks. No effect
    /// on real use; not compiled natively.
    #[cfg(target_arch = "wasm32")]
    fn drive_test_hook(&mut self) {
        let Some(w) = web_sys::window() else { return };
        // Consume a pending command, then clear it.
        if let Ok(v) = js_sys::Reflect::get(&w, &"__fluideda_cmd".into()) {
            if let Some(cmd) = v.as_string() {
                if !cmd.is_empty() {
                    self.run_test_command(&cmd);
                    let _ = js_sys::Reflect::set(&w, &"__fluideda_cmd".into(), &"".into());
                }
            }
        }
        let top = self.db.top;
        let sel = self.selected.map_or(-1i64, |(_, i)| i as i64);
        let sel_cell = self.selected.map_or(-1i64, |(c, _)| c as i64);
        // Every edit tombstones its base (move records a copy too), so
        // `deleted` holds ALL edited shapes and `edited` is the moved subset:
        //   edited shapes = deleted.len(); moves = edited.len();
        //   pure deletes  = deleted.len() - edited.len().
        let moves = self.overlay.edited.len();
        let edited = self.overlay.deleted.len();
        let state = format!(
            "{{\"connected\":{},\"peers\":{},\"edited\":{},\"moves\":{},\"deletes\":{},\"selected\":{},\"selected_cell\":{},\"top_shapes\":{}}}",
            self.session.is_connected(),
            self.session.remote.len(),
            edited,
            moves,
            edited - moves,
            sel,
            sel_cell,
            self.db.cell(top).shapes.len(),
        );
        let _ = js_sys::Reflect::set(&w, &"__fluideda_state".into(), &state.into());
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn drive_test_hook(&mut self) {}

    /// Execute a scripted test command (see [`drive_test_hook`]).
    #[cfg(target_arch = "wasm32")]
    fn run_test_command(&mut self, cmd: &str) {
        let top = self.db.top;
        let mut parts = cmd.split_whitespace();
        match parts.next() {
            Some("join") => self.connect(),
            Some("edit_mode") => self.edit_mode = true,
            Some("select") => {
                // `select <idx>` selects a TOP shape; `select <cell> <idx>` selects
                // a specific master (SLICE 2 non-TOP editing). The placement
                // transform is left identity — fine for the headless CRDT test,
                // which drives moves/deletes keyed by (cell, idx) and checks
                // convergence, not pixels.
                let a = parts.next().and_then(|s| s.parse::<u32>().ok());
                let b = parts.next().and_then(|s| s.parse::<u32>().ok());
                let (cell, idx) = match (a, b) {
                    (Some(c), Some(i)) => (c as usize, i),
                    (Some(i), None) => (top, i),
                    _ => return,
                };
                if cell < self.db.cells.len() && (idx as usize) < self.db.cell(cell).shapes.len() {
                    self.selected = Some((cell, idx));
                    self.selected_tf = Transform::IDENTITY;
                    self.move_delta = self.existing_delta((cell, idx));
                }
            }
            Some("pick") => {
                // `pick <x> <y>` — run the real hierarchy hit-test at a world point.
                let x = parts.next().and_then(|s| s.parse::<f32>().ok());
                let y = parts.next().and_then(|s| s.parse::<f32>().ok());
                if let (Some(x), Some(y)) = (x, y) {
                    self.pick_at(Vec2::new(x, y));
                }
            }
            Some("select_biggest") => {
                // Select shape 0 of the cell with the most shapes (caravel_core on
                // caravel) — a data-independent handle on the big flat cell.
                let biggest = (0..self.db.cells.len())
                    .max_by_key(|&c| self.db.cell(c).shapes.len())
                    .filter(|&c| self.db.cell(c).shapes.len() > 0);
                if let Some(cell) = biggest {
                    self.selected = Some((cell, 0));
                    self.selected_tf = Transform::IDENTITY;
                    self.move_delta = self.existing_delta((cell, 0));
                }
            }
            Some("edit_bench") => {
                // Viewport-independent edit-frame measurement (STEP 2): time a first
                // edit of the biggest cell — the finer-chunking re-partition plus the
                // incremental re-tessellation of the fine chunk owning the shape,
                // exactly the work `ensure_uploaded` does for a dirty chunk. Records
                // it into `last_edit_ms`/`last_edit_chunks` (published to the harness)
                // without depending on where the cell is placed or what's on screen.
                let biggest = (0..self.db.cells.len())
                    .max_by_key(|&c| self.db.cell(c).shapes.len())
                    .filter(|&c| self.db.cell(c).shapes.len() > 0);
                if let Some(cell) = biggest {
                    let idx = 0u32;
                    let t = web_time::Instant::now();
                    if let Some(ch) = self.db.cell(cell).chunk_of(idx) {
                        if self.db.cells[cell].chunks[ch as usize].len() > Self::EDIT_CHUNK_TARGET {
                            self.db.cells[cell].subdivide_chunk(ch as usize, Self::EDIT_CHUNK_TARGET);
                        }
                    }
                    if let Some(fine) = self.db.cell(cell).chunk_of(idx) {
                        let cref = self.db.cell(cell);
                        let chunk = &cref.chunks[fine as usize];
                        let mut suppressed = std::collections::HashSet::new();
                        suppressed.insert(idx); // as a delete of shape 0 would suppress
                        let _ = fluideda_core::tessellate_chunk_filtered(&cref.shapes, chunk, &suppressed);
                    }
                    self.last_edit_ms = t.elapsed().as_secs_f64() * 1e3;
                    self.last_edit_chunks = 1;
                }
            }
            Some("nudge") => {
                if let Some(id) = self.selected {
                    let step = self.nudge_step();
                    self.nudge(id, step, 0);
                }
            }
            Some("delete") => {
                if let Some(id) = self.selected {
                    self.delete_shape(id);
                }
            }
            Some("undo") => self.do_undo(),
            Some("redo") => self.do_redo(),
            _ => {}
        }
    }
}

impl FluidApp {
    fn side_panel(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        egui::SidePanel::left("panel").default_width(250.0).show(ctx, |ui| {
            ui.heading("FluidEDA");
            ui.label(format!("{:.0} fps", self.fps));
            if !self.status.is_empty() {
                ui.colored_label(egui::Color32::YELLOW, &self.status);
            }
            ui.separator();

            if ui.button("📂 Open GDS…").clicked() {
                webio::spawn_picker(self.load_slot.clone());
            }
            ui.small("or drag a .gds onto the canvas");
            ui.small(format!("file: {}", self.metrics.name));

            ui.separator();
            ui.collapsing("Load metrics", |ui| {
                let m = &self.metrics;
                ui.small(format!("size:        {} KB", m.file_bytes / 1024));
                ui.small(format!("parse:       {:.1} ms", m.parse_ms));
                ui.small(format!("TTFF:        {:.0} ms", m.ttff_ms));
                ui.small(format!("cell defs:   {}", m.cell_defs));
                ui.small(format!("distinct:    {} polys", m.distinct_polys));
                // Live (grow as cells stream in): lazy tessellation + GPU geometry.
                ui.small(format!(
                    "tess (lazy): {:.1} ms, {} chunks uploaded",
                    self.tess_accum_ms,
                    self.uploaded.len()
                ));
                if self.tess_worker_chunks > 0 || !self.tess_pending.is_empty() {
                    ui.small(format!(
                        "tess (worker): {} chunks off-thread, {} in flight",
                        self.tess_worker_chunks,
                        self.tess_pending.len()
                    ));
                }
                ui.small(format!("GPU geo:     {} MB (visible only)", self.gpu_bytes / 1_048_576));
                if m.parse_heap_bytes > 0 {
                    ui.small(format!("parse heap:  {} MB (high-water)", m.parse_heap_bytes / 1_048_576));
                }
                if m.wasm_heap_bytes > 0 {
                    ui.small(format!("wasm heap:   {} MB", m.wasm_heap_bytes / 1_048_576));
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("name");
                ui.text_edit_singleline(&mut self.name);
            });
            if !self.session.is_connected() {
                if ui.button("Join room").clicked() {
                    self.connect();
                }
            } else {
                ui.colored_label(egui::Color32::GREEN, "● connected");
                ui.label(format!("{} peers", self.session.remote.len()));
            }

            ui.separator();
            ui.label("Culling");
            ui.small(format!("total shapes: {}", self.total_shapes));
            ui.small(format!("instances:    {}", self.cull_stats.instances_emitted));
            ui.small(format!("proxies:      {}", self.cull_stats.proxy_count));
            ui.checkbox(&mut self.debug_stats, "count visible (slow)");
            if self.debug_stats {
                ui.small(format!("visible:      {}", self.cull_stats.visible_shapes));
            }

            ui.separator();
            ui.label(format!("Layers ({})", self.layers.len()));
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for i in 0..self.layers.len() {
                    let layer = self.layers[i];
                    ui.horizontal(|ui| {
                        let [r, g, b, _] = fluideda_render::layer_color(layer);
                        let sw = egui::Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, sw);
                        if ui
                            .checkbox(&mut self.layer_visible[i], format!("L{}/{}", layer.layer, layer.datatype))
                            .changed()
                        {
                            if let Some(rs) = frame.wgpu_render_state() {
                                let mut guard = rs.renderer.write();
                                if let Some(r) = renderer_mut(&mut guard.callback_resources) {
                                    r.set_layer_visible(layer, self.layer_visible[i]);
                                }
                            }
                        }
                    });
                }
            });

            ui.separator();
            ui.label("Editing");
            ui.checkbox(&mut self.edit_mode, "Edit mode (click to select)");
            match self.selected {
                Some((cell, idx)) => ui.small(format!("selected: cell {cell}, shape {idx}")),
                None => ui.small("selected: (none)"),
            };
            if let Some(id) = self.selected {
                // Nudge = a fixed translation step, the deterministic move path.
                let step = self.nudge_step();
                ui.horizontal(|ui| {
                    if ui.button("←").clicked() { self.nudge(id, -step, 0); }
                    if ui.button("→").clicked() { self.nudge(id, step, 0); }
                    if ui.button("↑").clicked() { self.nudge(id, 0, step); }
                    if ui.button("↓").clicked() { self.nudge(id, 0, -step); }
                });
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() { self.delete_shape(id); }
                    if ui.button("Deselect").clicked() { self.selected = None; }
                });
                ui.small(format!("move Δ: ({}, {}) nm", self.move_delta.0, self.move_delta.1));
            }
            if self.last_pick_us > 0.0 {
                ui.small(format!("last hit-test: {:.0} µs", self.last_pick_us));
            }
            let edits = self.overlay.deleted.len() + self.overlay.edited.len();
            ui.small(format!("edits: {edits} (moved {}, deleted {})",
                self.overlay.edited.len(), self.overlay.deleted.len()));
            if self.last_edit_ms > 0.0 {
                ui.small(format!("last edit: {} chunk(s) re-tess, {:.2} ms",
                    self.last_edit_chunks, self.last_edit_ms));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(self.session.can_undo(), egui::Button::new("Undo")).clicked() {
                    self.do_undo();
                }
                if ui.add_enabled(self.session.can_redo(), egui::Button::new("Redo")).clicked() {
                    self.do_redo();
                }
            });
            if ui.button("💾 Save GDS").clicked() {
                self.save_gds();
            }

            ui.separator();
            if ui.button("Fit view").clicked() {
                self.camera.fit(self.bbox_min, self.bbox_max);
            }
            ui.small("Drag: pan · Wheel: zoom · Shift+drag: marker");
        });
    }

    fn canvas(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let size = ui.available_size();
            self.camera.viewport = Vec2::new(size.x, size.y);
            let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
            let shift = ui.input(|i| i.modifiers.shift);
            let world_at = |p: egui::Pos2, cam: &Camera| {
                cam.screen_to_world(Vec2::new(p.x - rect.min.x, p.y - rect.min.y))
            };

            // Edit mode: a drag that begins on the selected shape moves it (from
            // the base, via copy-on-write); any other drag pans as usual.
            if resp.drag_started() && self.edit_mode && !shift {
                if let (Some(id), Some(p)) = (self.selected, resp.interact_pointer_pos()) {
                    if self.point_hits(id, world_at(p, &self.camera)) {
                        self.drag_move_from = Some(world_at(p, &self.camera));
                        self.drag_move_base = self.move_delta;
                    }
                }
            }
            if resp.dragged() {
                if let (Some(from), Some(id), Some(p)) =
                    (self.drag_move_from, self.selected, resp.interact_pointer_pos())
                {
                    // World drag delta → the selected cell's LOCAL frame (the move
                    // is stored cell-local), so at the clicked placement the shape
                    // follows the cursor even under rotation/scale.
                    let d = world_at(p, &self.camera) - from;
                    let (ldx, ldy) = self.world_delta_to_local(d.x as f64, d.y as f64);
                    self.preview_move(id, self.drag_move_base.0 + ldx, self.drag_move_base.1 + ldy);
                } else if !shift {
                    let d = resp.drag_delta();
                    self.camera.pan_pixels(d.x, d.y);
                    self.dragging = true;
                }
            }
            if resp.drag_stopped() {
                // Commit the previewed move to the CRDT once (broadcast + undoable).
                if self.drag_move_from.is_some() {
                    if let Some(id) = self.selected {
                        self.commit_move(id, self.move_delta.0, self.move_delta.1);
                    }
                }
                self.drag_move_from = None;
            }

            // Delete key removes the selection.
            if self.edit_mode
                && self.selected.is_some()
                && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
            {
                self.delete_shape(self.selected.unwrap());
            }

            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll.abs() > 0.0 {
                if let Some(p) = resp.hover_pos() {
                    let local = Vec2::new(p.x - rect.min.x, p.y - rect.min.y);
                    self.camera.zoom_at((scroll * 0.002).exp(), local);
                }
            }

            if let Some(p) = resp.hover_pos() {
                let local = Vec2::new(p.x - rect.min.x, p.y - rect.min.y);
                let world = self.camera.screen_to_world(local);
                self.session.set_cursor(world);
                if resp.drag_started() && ui.input(|i| i.modifiers.shift) {
                    let half = 2000.0;
                    self.session.add_marker(Marker {
                        min: world - Vec2::splat(half),
                        max: world + Vec2::splat(half),
                        color: [1.0, 1.0, 1.0, 0.6],
                    });
                }
            }

            // Edit mode: a plain click selects a TOP-cell shape (drag still pans,
            // so navigation is unaffected). Clicking empty space deselects.
            if self.edit_mode && resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let local = Vec2::new(p.x - rect.min.x, p.y - rect.min.y);
                    self.pick_at(self.camera.screen_to_world(local));
                }
            }

            if self.renderer_ready {
                ui.painter().add(eframe::egui_wgpu::Callback::new_paint_callback(
                    rect,
                    LayoutPaint {
                        camera: self.camera,
                        visible: self.visible.clone(),
                    },
                ));
            }

            let painter = ui.painter_at(rect);

            // Highlight the selected shape's outline at the clicked placement:
            // cell-local points → world (via the placement transform) → screen.
            // Uses the moved copy's geometry when the selection has been moved, so
            // the highlight tracks the shape.
            if let Some((cell, idx)) = self.selected {
                let moved = self.overlay.edited.get(&(cell, idx));
                let pts: Vec<Point> = match moved {
                    Some(shape) => shape.points.clone(),
                    None => self.db.cell(cell).shapes.points(idx as usize).to_vec(),
                };
                let screen: Vec<egui::Pos2> = pts
                    .iter()
                    .map(|p| {
                        let (wx, wy) = self.selected_tf.apply_point(*p);
                        self.world_to_screen(Vec2::new(wx as f32, wy as f32), rect)
                    })
                    .collect();
                if screen.len() >= 2 {
                    let stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 230, 0));
                    painter.add(egui::Shape::closed_line(screen, stroke));
                }
            }

            for (_, pres) in self.session.remote.iter() {
                let sp = self.world_to_screen(pres.cursor, rect);
                let col = egui::Color32::from_rgb(
                    (pres.color[0] * 255.0) as u8,
                    (pres.color[1] * 255.0) as u8,
                    (pres.color[2] * 255.0) as u8,
                );
                painter.circle_filled(sp, 4.0, col);
                painter.text(
                    sp + egui::vec2(6.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &pres.name,
                    egui::FontId::proportional(12.0),
                    col,
                );
            }
        });
    }

    fn world_to_screen(&self, world: Vec2, rect: egui::Rect) -> egui::Pos2 {
        let half = self.camera.viewport * 0.5;
        let d = (world - self.camera.center) * self.camera.scale;
        egui::pos2(rect.min.x + half.x + d.x, rect.min.y + half.y - d.y)
    }
}

/// Export filename derived from the loaded file's name: `caravel.gds` →
/// `caravel-edited.gds`. Strips any parenthetical suffix (e.g. the embedded label).
fn out_name(loaded: &str) -> String {
    let base = loaded.split_whitespace().next().unwrap_or("design");
    let stem = base.strip_suffix(".gds").unwrap_or(base);
    format!("{stem}-edited.gds")
}

fn server_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(w) = web_sys::window() {
            if let Ok(host) = w.location().host() {
                return format!("ws://{host}/ws?room=demo");
            }
        }
    }
    "ws://127.0.0.1:9000/ws?room=demo".to_string()
}
