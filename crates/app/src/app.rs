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
    cull, cull_with_overlay, pick_hierarchy, AddId, BaseShapeId, CellDb, CellId, ChunkId,
    CullStats, EditOverlay, Layer, Point, Transform, VisibleSet,
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

/// Drawing grid (Phase 5 SLICE 1): every authored coordinate snaps to this many
/// nm. Small enough to be invisible at any sane zoom, big enough that a drawn
/// vertex is reproducible across peers and sessions. Coordinates stay i32 nm.
const SNAP_NM: i32 = 5;

/// Snap a world coordinate (nm, f32 out of the camera) onto the drawing grid.
fn snap_nm(v: f32) -> i32 {
    ((v / SNAP_NM as f32).round() as i64 * SNAP_NM as i64) as i32
}

/// What a left-click selects / a drag edits (Phase 5: base shapes and drawn
/// shapes are both editable, but they are different id spaces — an added shape
/// has no base index, so it is referenced by its [`AddId`] everywhere).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Selection {
    /// A base shape: cell master + index (editing updates every placement).
    Base(BaseShapeId),
    /// A shape drawn this session (TOP-cell, identity frame).
    Added(AddId),
}

/// `drc_bench` measurements (Phase 5b), published to `window.__fluideda_perf`
/// (wasm only — natively nothing reads them).
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
struct DrcBench {
    cell: i64,
    shapes: usize,
    grid_ms: f64,
    cold_ms: f64,
    warm_ms: f64,
    warm_tiles: usize,
    tiles: usize,
    tile_max_ms: f64,
    violations: usize,
}

/// The active canvas tool (Phase 5 SLICE 1). Select = Phase 3 behavior; the two
/// draw tools author new TOP-cell shapes on the active layer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tool {
    Select,
    Rect,
    Poly,
}

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

/// Per-frame DRC budget (ms). Phase 5b: the work is cut into tile jobs and
/// grid-build slices (a few ms each on caravel_core's densest tiles), so the
/// driver loop below stops within about one job of this — there is no atomic
/// index build left (the lazy R-tree is replaced by the budgeted tile grid).
const DRC_BUDGET_MS: f64 = 8.0;

/// DRC worker pool (Phase 5b SLICE 3): at most this many workers, and this many
/// jobs queued per worker — enough that a worker never idles between the main
/// thread's once-per-frame refills (a tile is ~1–5 ms; a frame is ~16 ms),
/// small enough that an edit's tiles, which jump the driver's queue, wait
/// behind only a few already-posted jobs.
const DRC_POOL_MAX: usize = 8;
const DRC_JOBS_PER_WORKER: usize = 8;

/// Per-frame budget (ms) for rebuilding a worker-parsed design (Phase 5c): the
/// columns are copied a few MB at a time until done, then the design installs.
const RECON_BUDGET_MS: f64 = 6.0;

/// Cap on violation markers drawn per frame (the panel shows the counts).
const DRC_MARKER_CAP: usize = 300;

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
    /// A worker-parsed design being rebuilt across frames (Phase 5c); installed
    /// when complete. The old design keeps rendering meanwhile.
    pending_design: Option<design::Reconstruction>,
    /// A rebuilt design waiting for next frame's install.
    ready_design: Option<Design>,
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
    /// The active canvas tool (Phase 5): Select, or one of the draw tools.
    tool: Tool,
    /// Index into `layers` of the layer new shapes are drawn on.
    active_layer: usize,
    /// Rect tool: the snapped world anchor of an in-progress corner drag.
    rect_anchor: Option<Point>,
    /// Poly tool: the snapped vertices accumulated so far (world = TOP-local nm).
    poly_pts: Vec<Point>,
    /// The current selection — a base shape (cell **master** + index; editing it
    /// updates every placement) or an added shape (by its stable AddId).
    selected: Option<Selection>,
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
    /// Geometry snapshot at drag start, for drags whose CRDT record carries
    /// geometry rather than an op: an added shape's move, a vertex-edited base
    /// shape's move, and any vertex drag. Previews transform this snapshot; the
    /// record is rewritten once, on release.
    drag_geom_start: Option<Vec<Point>>,
    /// A vertex drag in progress: index of the grabbed vertex (into the ring,
    /// closing duplicate excluded — moving vertex 0 moves the duplicate too).
    vertex_drag: Option<usize>,
    /// Base shapes whose current CRDT record is `SetPoints` (a vertex edit).
    /// Their later translations must rewrite the point list (a new `SetPoints`),
    /// not commit a `Move` op — a `Move` rebuilds from the BASE on every peer and
    /// would silently discard the vertex edit (LWW takes the whole record).
    geom_edited: HashSet<BaseShapeId>,
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

    // --- Phase 5 SLICE 3 / Phase 5b: DRC ---
    /// DRC on/off. Off ⇒ zero cost (no checks queued, nothing drawn); switching
    /// it off drops all DRC state.
    drc_enabled: bool,
    /// The merge-first tiled DRC scheduler: rule deck, per-cell tile grids,
    /// per-tile results, the pending queue (edits first). Jobs run inline under
    /// `DRC_BUDGET_MS` each frame.
    drc: fluideda_core::DrcDriver,
    /// The active rule deck (auto-detected from the design's layers on load;
    /// the panel can override). Changing it drops all DRC state.
    drc_deck: fluideda_core::Deck,
    /// `drc_bench` results (see `run_test_command`).
    drc_bench: Option<DrcBench>,
    /// Phase 5b SLICE 3: the DRC worker pool (web; spawned on first work).
    /// `None` natively / without workers / under `?drcinline=1` — jobs then run
    /// inline under the frame budget, through the same driver.
    drc_pool: Option<webio::DrcPool>,
    /// Pool replies, drained into the driver each frame.
    drc_replies: webio::DrcSlot,
    /// `?drcinline=1`: never use the pool (before/after measurement).
    drc_force_inline: bool,
    /// The deck packed for worker jobs (refreshed on deck change).
    drc_rules_packed: Vec<i32>,
    /// Tiles a worker failed on (accepted as clean; shown in the panel).
    drc_tile_errors: usize,
    /// Monotonic origin for the driver's pass timing.
    app_t0: web_time::Instant,

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

        let deck = fluideda_core::Deck::detect(&design.layers);
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
            pending_design: None,
            ready_design: None,
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
            tool: Tool::Select,
            active_layer: 0,
            rect_anchor: None,
            poly_pts: Vec::new(),
            selected: None,
            selected_tf: Transform::IDENTITY,
            move_delta: (0, 0),
            drag_move_from: None,
            drag_move_base: (0, 0),
            drag_geom_start: None,
            vertex_drag: None,
            geom_edited: HashSet::new(),
            drc_enabled: false,
            drc: fluideda_core::DrcDriver::new(deck.rules()),
            drc_deck: deck,
            drc_bench: None,
            drc_pool: None,
            drc_replies: Rc::new(RefCell::new(Vec::new())),
            drc_force_inline: webio::query_flag("drcinline"),
            drc_rules_packed: deck.rules().pack(),
            drc_tile_errors: 0,
            app_t0: web_time::Instant::now(),
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
        self.drag_geom_start = None;
        self.vertex_drag = None;
        self.geom_edited.clear();
        // A new design picks its own deck (sky130 layers ⇒ the PDK table).
        self.drc_deck = fluideda_core::Deck::detect(&self.layers);
        self.drc.set_rules(self.drc_deck.rules());
        self.drc_rules_packed = self.drc.rules().pack();
        self.drc_bench = None;
        self.drc_tile_errors = 0;
        self.tool = Tool::Select;
        self.active_layer = 0;
        self.rect_anchor = None;
        self.poly_pts.clear();
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

    /// Hit-test a world point and update the selection. Added shapes draw on top
    /// of the base, so they are tested first (a linear scan — there are few, and
    /// they are TOP-cell so world == local); otherwise the pick walks the
    /// instance hierarchy and resolves to a cell **master** + shape, plus the
    /// clicked placement's world transform.
    fn pick_at(&mut self, world: Vec2) {
        let p = Point::new(world.x.round() as i32, world.y.round() as i32);
        let t = web_time::Instant::now();
        let top = self.db.top;
        // Last-drawn wins when adds overlap (matches their draw order: on top).
        for a in self.overlay.added.iter().rev() {
            if a.cell == top && fluideda_core::point_in_polygon(&a.shape.points, p) {
                self.last_pick_us = t.elapsed().as_secs_f64() * 1e6;
                self.selected = Some(Selection::Added(a.id));
                self.selected_tf = Transform::IDENTITY;
                self.move_delta = (0, 0);
                return;
            }
        }
        let hit = pick_hierarchy(&self.db, p);
        self.last_pick_us = t.elapsed().as_secs_f64() * 1e6;
        match hit {
            Some(h) => {
                self.selected = Some(Selection::Base(h.id));
                self.selected_tf = h.world_tf;
                self.move_delta =
                    if self.geom_edited.contains(&h.id) { (0, 0) } else { self.existing_delta(h.id) };
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
    /// transform), against the current overlay copy when one exists. Decides
    /// drag = move vs pan.
    fn point_hits(&self, sel: Selection, w: Vec2) -> bool {
        let (lx, ly) = self.selected_tf.inverse().apply(w.x as f64, w.y as f64);
        let p = Point::new(lx.round() as i32, ly.round() as i32);
        match sel {
            Selection::Added(id) => self
                .overlay
                .find_added(id)
                .is_some_and(|a| fluideda_core::point_in_polygon(&a.shape.points, p)),
            Selection::Base(id) => match self.overlay.edited.get(&id) {
                Some(moved) => fluideda_core::point_in_polygon(&moved.points, p),
                None => fluideda_core::point_in_polygon(self.db.cell(id.0).shapes.points(id.1 as usize), p),
            },
        }
    }

    /// The selection's *current* geometry (layer + ring): the overlay copy when
    /// one exists, else the base. Cell-local nm.
    fn selection_geometry(&self, sel: Selection) -> Option<(Layer, Vec<Point>)> {
        match sel {
            Selection::Added(id) => self.overlay.find_added(id).map(|a| (a.shape.layer, a.shape.points.clone())),
            Selection::Base(id) => match self.overlay.edited.get(&id) {
                Some(s) => Some((s.layer, s.points.clone())),
                None => {
                    let c = self.db.cell(id.0);
                    if (id.1 as usize) < c.shapes.len() {
                        Some((c.shapes.layer(id.1 as usize), c.shapes.points(id.1 as usize).to_vec()))
                    } else {
                        None
                    }
                }
            },
        }
    }

    // --- Phase 5 SLICE 2: vertex editing -------------------------------------

    /// Screen-space pixel radius within which a pointer grabs a vertex handle.
    const HANDLE_GRAB_PX: f32 = 8.0;

    /// How many draggable vertices the ring has: the closing duplicate (GDS
    /// convention: last == first) is not its own handle — dragging vertex 0
    /// moves both ends.
    fn handle_count(pts: &[Point]) -> usize {
        match (pts.first(), pts.last()) {
            (Some(f), Some(l)) if pts.len() > 3 && f == l => pts.len() - 1,
            _ => pts.len(),
        }
    }

    /// The handle (vertex index) under screen point `p`, if any — a cheap
    /// screen-space proximity scan of the selection's ring (no index needed).
    fn vertex_at(&self, sel: Selection, p: egui::Pos2, rect: egui::Rect) -> Option<usize> {
        let (_, pts) = self.selection_geometry(sel)?;
        let n = Self::handle_count(&pts);
        let mut best: Option<(usize, f32)> = None;
        for (i, q) in pts.iter().take(n).enumerate() {
            let (wx, wy) = self.selected_tf.apply_point(*q);
            let s = self.world_to_screen(Vec2::new(wx as f32, wy as f32), rect);
            let d2 = (s.x - p.x).powi(2) + (s.y - p.y).powi(2);
            if d2 <= Self::HANDLE_GRAB_PX * Self::HANDLE_GRAB_PX && best.map_or(true, |(_, b)| d2 < b) {
                best = Some((i, d2));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Move vertex `vi` of the drag-start snapshot to `local` (the pointer,
    /// inverse-mapped into the cell frame and snapped), keeping the closing
    /// duplicate in step. Returns the full new ring.
    fn ring_with_vertex(start: &[Point], vi: usize, local: Point) -> Vec<Point> {
        let mut pts = start.to_vec();
        let closed = pts.len() > 3 && pts.first() == pts.last();
        if vi < pts.len() {
            pts[vi] = local;
            if closed && vi == 0 {
                let last = pts.len() - 1;
                pts[last] = local;
            }
        }
        pts
    }

    /// Live vertex-drag preview: write the new ring into the overlay (the same
    /// copy-on-write path a move uses — tombstone base + edited copy), without a
    /// CRDT write per frame.
    fn preview_ring(&mut self, sel: Selection, pts: Vec<Point>) {
        match sel {
            Selection::Base(id) => {
                let layer = self.db.cell(id.0).shapes.layer(id.1 as usize);
                self.overlay.move_shape(id, fluideda_core::Shape { layer, points: pts });
                self.invalidate(id);
            }
            Selection::Added(id) => {
                if let Some(a) = self.overlay.find_added_mut(id) {
                    a.shape.points = pts;
                    self.overlay_dirty = true;
                }
            }
        }
    }

    /// Commit the selection's current (previewed) ring to the CRDT once, on
    /// release: a base shape ships `SetPoints` (the second geometry amendment);
    /// an added shape rewrites its geometry record.
    fn commit_ring(&mut self, sel: Selection) {
        match sel {
            Selection::Base(id) => {
                if let Some(s) = self.overlay.edited.get(&id) {
                    let pts = s.points.clone();
                    self.session.set_edit(id, EditKind::SetPoints(pts));
                    self.reconcile_edits();
                }
            }
            Selection::Added(id) => {
                if let Some(a) = self.overlay.find_added(id) {
                    let (cell, layer, pts) = (a.cell, a.shape.layer, a.shape.points.clone());
                    self.session.set_added_geometry(id, cell, layer, &pts);
                    self.reconcile_edits();
                }
            }
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

    /// Nudge the selection by `(dx, dy)` (a discrete, immediately-committed
    /// edit). Base shapes commit a translation op on top of the current delta;
    /// added shapes rewrite their geometry (geometry IS their CRDT state).
    fn nudge(&mut self, sel: Selection, dx: i32, dy: i32) {
        match sel {
            Selection::Base(id) => {
                if self.geom_edited.contains(&id) {
                    // A vertex-edited shape: its record carries geometry, so a
                    // translation rewrites the point list (a Move op would LWW
                    // the whole record back to translated-BASE, silently undoing
                    // the vertex edit).
                    if let Some(s) = self.overlay.edited.get(&id) {
                        let moved = fluideda_core::edit::translate(s.layer, &s.points, dx, dy);
                        self.session.set_edit(id, EditKind::SetPoints(moved.points));
                        self.reconcile_edits();
                    }
                } else {
                    let (cx, cy) = self.move_delta;
                    self.commit_move(id, cx + dx, cy + dy);
                }
            }
            Selection::Added(id) => {
                let Some(a) = self.overlay.find_added(id) else { return };
                let moved = fluideda_core::edit::translate(a.shape.layer, &a.shape.points, dx, dy);
                self.session.set_added_geometry(id, a.cell, moved.layer, &moved.points);
                self.reconcile_edits();
            }
        }
    }

    /// Delete the selection: a CRDT tombstone, then reconcile. No base mutation —
    /// a base shape's chunk re-tessellates without it; an added shape's record
    /// tombstones by its AddId.
    fn delete_shape(&mut self, sel: Selection) {
        match sel {
            Selection::Base(id) => self.session.set_edit(id, EditKind::Delete),
            Selection::Added(id) => self.session.delete_added(id),
        }
        self.reconcile_edits();
        if self.selected == Some(sel) {
            self.selected = None;
            self.move_delta = (0, 0);
        }
    }

    // --- Phase 5 SLICE 1: draw tools -----------------------------------------

    /// Commit a drawn rectangle spanning snapped corners `a`..`b` (world = TOP
    /// nm). Degenerate spans (a dimension that snapped to zero) are dropped.
    fn commit_rect(&mut self, a: Point, b: Point) {
        let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
        let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));
        if x1 - x0 < SNAP_NM || y1 - y0 < SNAP_NM {
            return;
        }
        self.commit_add(vec![
            Point::new(x0, y0),
            Point::new(x1, y0),
            Point::new(x1, y1),
            Point::new(x0, y1),
            Point::new(x0, y0), // closed ring, GDS boundary convention
        ]);
    }

    /// Close and commit the in-progress polygon if it has ≥ 3 distinct vertices.
    fn commit_poly(&mut self) {
        let mut pts = std::mem::take(&mut self.poly_pts);
        if pts.len() < 3 {
            return;
        }
        let first = pts[0];
        pts.push(first); // close the ring
        self.commit_add(pts);
    }

    /// Commit a brand-new TOP-cell shape on the active layer: mint its AddId via
    /// the CRDT (full geometry — the documented amendment), reconcile so the
    /// overlay/upload paths pick it up, and select it.
    fn commit_add(&mut self, pts: Vec<Point>) {
        let layer = self.layers.get(self.active_layer).copied().unwrap_or(Layer::new(1, 0));
        let top = self.db.top;
        let id = self.session.add_shape(top, layer, &pts);
        self.reconcile_edits();
        self.selected = Some(Selection::Added(id));
        self.selected_tf = Transform::IDENTITY;
        self.move_delta = (0, 0);
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

    // --- Phase 5 SLICE 3 / Phase 5b: DRC driver ------------------------------

    /// Switch the rule deck: grids and results are deck-dependent, so the driver
    /// starts over (any cells the user asked to check must be asked again).
    fn set_deck(&mut self, deck: fluideda_core::Deck) {
        self.drc_deck = deck;
        self.drc.set_rules(deck.rules());
        self.drc_rules_packed = self.drc.rules().pack();
        self.drc_bench = None;
    }

    /// Milliseconds since app start (the driver's clock; it has none itself).
    fn now_ms(&self) -> f64 {
        self.app_t0.elapsed().as_secs_f64() * 1e3
    }

    /// Geometry inside `region` of `cell` changed: re-check the tiles near it on
    /// the next frames, ahead of any background sweep. The incremental unit is
    /// the tile (Phase 5b) — an edit to caravel_core re-checks a few tiles, not
    /// the 2.5 M-shape cell.
    fn drc_invalidate(&mut self, cell: CellId, region: fluideda_core::Bbox) {
        if !self.drc_enabled {
            return;
        }
        let now = self.now_ms();
        self.drc.invalidate(&self.db, cell, region, now);
    }

    /// Request a full check of every currently-visible cell (plus TOP, where
    /// drawn shapes live) — the "check visible" button. Cells already fully
    /// checked are skipped by the driver.
    fn drc_enqueue_visible(&mut self) {
        let mut want: Vec<CellId> = self.visible.draws.iter().map(|d| d.cell).collect();
        want.push(self.db.top);
        want.sort_unstable();
        want.dedup();
        let now = self.now_ms();
        for cell in want {
            self.drc.request_full(&self.db, &self.overlay, cell, now);
        }
    }

    /// Run queued DRC work within the per-frame budget: feed pool replies back
    /// to the driver, then advance grid-build slices and hand out tile jobs —
    /// posted to the worker pool when there is one, else checked inline. Either
    /// way the main thread only gathers (~0.1 ms/tile) and never blocks past
    /// about one job of `DRC_BUDGET_MS`.
    fn run_drc_budget(&mut self) {
        if !self.drc_enabled {
            return;
        }
        // 1. Replies from the pool. A reply from before a clear (new design,
        //    deck change, DRC toggled) carries an old epoch: drop it — its
        //    (cell, tile, gen) may collide with the new state's.
        let replies: Vec<webio::DrcReply> = std::mem::take(&mut *self.drc_replies.borrow_mut());
        let now = self.now_ms();
        for r in replies {
            if r.epoch != self.drc.epoch() {
                continue;
            }
            self.drc_tile_errors += r.error as usize;
            self.drc.accept(r.cell, r.tile, r.gen, r.violations, now);
        }
        if !self.drc.is_busy() {
            return;
        }
        // 2. The pool: spawned on first work (web, workers supported, not forced
        //    inline). One worker per spare core, capped.
        if self.drc_pool.is_none() && !self.drc_force_inline && webio::worker_supported() {
            let n = webio::hardware_concurrency().saturating_sub(1).clamp(1, DRC_POOL_MAX);
            match webio::DrcPool::new(n, self.drc_replies.clone()) {
                Ok(p) => {
                    log::info!("DRC worker pool: {n} worker(s)");
                    self.drc.max_in_flight = n * DRC_JOBS_PER_WORKER;
                    self.drc_pool = Some(p);
                }
                Err(e) => {
                    log::error!("DRC pool unavailable, running inline: {e}");
                    self.drc_force_inline = true;
                }
            }
        }
        // 3. Grid slices + jobs within the budget.
        let t0 = web_time::Instant::now();
        while t0.elapsed().as_secs_f64() * 1e3 < DRC_BUDGET_MS {
            let now = self.now_ms();
            match self.drc.step(&self.db, &self.overlay, now) {
                fluideda_core::DrcStep::Progress => {}
                fluideda_core::DrcStep::Job(job) => match &self.drc_pool {
                    Some(pool) => pool.post(self.drc.epoch(), &job, &self.drc_rules_packed),
                    None => {
                        let v = fluideda_core::check_tile(job.core, self.drc.rules(), &job.input);
                        let now = self.now_ms();
                        self.drc.accept(job.cell, job.tile, job.gen, v, now);
                    }
                },
                fluideda_core::DrcStep::Idle => break,
            }
        }
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
        // 0. Worker-parsed design (Phase 2b STEP 2): the reply queues a
        //    reconstruction; advance it within the frame budget and install the
        //    design once complete (Phase 5c — the swap spans a few frames
        //    instead of blocking one).
        let worker_done = self.design_slot.borrow_mut().take();
        if let Some(res) = worker_done {
            match res {
                Ok(r) => self.pending_design = Some(r),
                Err(e) => self.status = format!("load failed: {e}"),
            }
        }
        // A design finished rebuilding last frame: install it now, on a frame
        // of its own (install + first-view tessellation are ~20 ms by
        // themselves; stacking them on the last copy slice made one long frame).
        if let Some(design) = self.ready_design.take() {
            self.install(design, frame);
            self.status.clear();
        }
        if let Some(r) = self.pending_design.as_mut() {
            match r.step(RECON_BUDGET_MS) {
                Ok(true) => {
                    let r = self.pending_design.take().unwrap();
                    match r.finish() {
                        Ok(design) => self.ready_design = Some(design),
                        Err(e) => self.status = format!("reconstruct failed: {e}"),
                    }
                }
                Ok(false) => self.status = format!("rebuilding design… {:.0}%", r.progress() * 100.0),
                Err(e) => {
                    self.pending_design = None;
                    self.status = format!("reconstruct failed: {e}");
                }
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
        let b = self.drc_bench.unwrap_or(DrcBench { cell: -1, ..Default::default() });
        let (drc_last_ms, drc_last_tiles) = self.drc.last_pass.map_or((-1.0, 0), |p| (p.ms, p.tiles));
        let json = format!(
            "{{\"max_frame_ms\":{:.1},\"frames\":{},\"worker_chunks\":{},\"in_flight\":{},\
             \"inline_tess_ms\":{:.1},\"uploaded\":{},\"force_inline\":{},\
             \"last_edit_ms\":{:.2},\"last_edit_chunks\":{},\
             \"drc_cell\":{},\"drc_shapes\":{},\"drc_grid_ms\":{:.1},\"drc_cold_ms\":{:.1},\
             \"drc_warm_ms\":{:.2},\"drc_warm_tiles\":{},\"drc_tiles\":{},\"drc_tile_max_ms\":{:.2},\
             \"drc_violations\":{},\"drc_busy\":{},\"drc_pending\":{},\"drc_total\":{},\
             \"drc_last_pass_ms\":{:.1},\"drc_last_pass_tiles\":{},\"drc_deck\":\"{:?}\",\
             \"drc_workers\":{},\"drc_errors\":{}}}",
            self.max_frame_ms,
            self.perf_frames,
            self.tess_worker_chunks,
            self.tess_pending.len(),
            self.tess_accum_ms,
            self.uploaded.len(),
            self.force_inline,
            self.last_edit_ms,
            self.last_edit_chunks,
            b.cell,
            b.shapes,
            b.grid_ms,
            b.cold_ms,
            b.warm_ms,
            b.warm_tiles,
            b.tiles,
            b.tile_max_ms,
            b.violations,
            self.drc.is_busy(),
            self.drc.pending_tiles(),
            self.drc.violation_count(),
            drc_last_ms,
            drc_last_tiles,
            self.drc_deck,
            self.drc_pool.as_ref().map_or(0, |p| p.size()),
            self.drc_tile_errors,
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
        // Which base shapes currently carry a geometry record (SetPoints) — their
        // later translations must rewrite geometry, not commit a Move op.
        self.geom_edited = self
            .session
            .edits()
            .into_iter()
            .filter_map(|(id, k)| matches!(k, fluideda_sync::EditKind::SetPoints(_)).then_some(id))
            .collect();
        let new = self.overlay_from_crdt();
        let added_changed = new.added != self.overlay.added;
        if !added_changed && new.deleted == self.overlay.deleted && new.edited == self.overlay.edited {
            return; // nothing edit-related changed (e.g. a marker-only update)
        }
        let changed: Vec<BaseShapeId> = changed_ids(&self.overlay, &new).into_iter().collect();
        // Where the composite just changed — the DRC re-check set (Phase 5b:
        // regions, not cells): old ∪ new geometry of every changed base shape
        // and of every added shape that appeared, vanished, or moved.
        let drc_regions = edit_regions(&self.db, &self.overlay, &new, &changed);
        self.overlay = new;
        // Route through the same invalidate() the direct-edit path uses, so a
        // committed or remote edit of a big cell also triggers the finer-chunking
        // (SLICE 2) — not just a local drag preview.
        for id in changed {
            self.invalidate(id);
        }
        // Adds live only in the overlay geometry — no base chunk to dirty, just
        // the (tiny) overlay buffer to re-upload.
        if added_changed {
            self.overlay_dirty = true;
        }
        // DRC is incremental at tile granularity: re-check only the tiles near
        // what changed. Runs on commit/remote convergence, not per preview
        // frame — violations update on release, keeping drags at full rate.
        for (cell, region) in drc_regions {
            self.drc_invalidate(cell, region);
        }
        // Keep the nudge accumulator + highlight in step with the authoritative
        // state. A geometry-record (vertex-edited) shape has no meaningful delta.
        match self.selected {
            Some(Selection::Base(id)) => {
                self.move_delta =
                    if self.geom_edited.contains(&id) { (0, 0) } else { self.existing_delta(id) }
            }
            Some(Selection::Added(id)) => {
                // The selection may have been deleted remotely (or by undo).
                if self.overlay.find_added(id).is_none() {
                    self.selected = None;
                    self.move_delta = (0, 0);
                }
            }
            None => {}
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
                // A vertex edit ships the full ring (SLICE 2 amendment); the layer
                // comes from the receiver's own base — it never changes.
                fluideda_sync::EditKind::SetPoints(points) => {
                    let layer = self.db.cell(cell).shapes.layer(idx as usize);
                    o.move_shape(id, fluideda_core::Shape { layer, points });
                }
            }
        }
        // Added shapes (Phase 5 SLICE 1): geometry ships in the record; tombstoned
        // adds return None and are skipped. `session.adds()` is sorted by id, so
        // every peer rebuilds the same deterministic draw order.
        for (id, geom) in self.session.adds() {
            let Some(g) = geom else { continue };
            if g.cell >= self.db.cells.len() {
                continue;
            }
            o.add(id, g.cell, fluideda_core::Shape { layer: g.layer, points: g.points });
        }
        o
    }
}

/// Bbox of a ring (cell-local nm).
fn ring_bbox(pts: &[Point]) -> fluideda_core::Bbox {
    let mut b = fluideda_core::Bbox::empty();
    for p in pts {
        b.expand(*p);
    }
    b
}

/// The regions (per cell) whose composite geometry differs between two overlays:
/// for each changed base id, its base ∪ old copy ∪ new copy; for each added
/// shape that appeared, vanished or changed, its old and new footprints. The
/// DRC re-check set of a reconcile.
fn edit_regions(
    db: &CellDb,
    old: &EditOverlay,
    new: &EditOverlay,
    changed: &[BaseShapeId],
) -> Vec<(CellId, fluideda_core::Bbox)> {
    let mut out = Vec::new();
    for &id in changed {
        let mut b = ring_bbox(db.cell(id.0).shapes.points(id.1 as usize));
        for s in [old.edited.get(&id), new.edited.get(&id)].into_iter().flatten() {
            let r = ring_bbox(&s.points);
            b.expand(r.min);
            b.expand(r.max);
        }
        out.push((id.0, b));
    }
    let olds: std::collections::HashMap<AddId, &fluideda_core::AddedShape> = old.added.iter().map(|a| (a.id, a)).collect();
    let news: std::collections::HashMap<AddId, &fluideda_core::AddedShape> = new.added.iter().map(|a| (a.id, a)).collect();
    for (id, a) in &olds {
        if news.get(id) != Some(a) {
            out.push((a.cell, ring_bbox(&a.shape.points)));
        }
    }
    for (id, a) in &news {
        if olds.get(id) != Some(a) {
            out.push((a.cell, ring_bbox(&a.shape.points)));
        }
    }
    out
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
        self.run_drc_budget();

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
        // Base-selection fields keep their Phase 3 semantics (the harness relies
        // on them); an added-shape selection reports its index in the (sorted,
        // cross-peer-deterministic) added list instead.
        let (sel, sel_cell, sel_added) = match self.selected {
            Some(Selection::Base((c, i))) => (i as i64, c as i64, -1i64),
            Some(Selection::Added(id)) => (
                -1,
                -1,
                self.overlay.added.iter().position(|a| a.id == id).map_or(-1, |i| i as i64),
            ),
            None => (-1, -1, -1),
        };
        // Every base edit tombstones its base (move records a copy too), so
        // `deleted` holds ALL edited base shapes and `edited` is the moved subset:
        //   edited shapes = deleted.len(); moves = edited.len();
        //   pure deletes  = deleted.len() - edited.len().
        let moves = self.overlay.edited.len();
        let edited = self.overlay.deleted.len();
        let state = format!(
            "{{\"connected\":{},\"peers\":{},\"edited\":{},\"moves\":{},\"deletes\":{},\"adds\":{},\"geom_edits\":{},\"selected\":{},\"selected_cell\":{},\"selected_added\":{},\"top_shapes\":{},\"drc_viols\":{},\"drc_queue\":{}}}",
            self.session.is_connected(),
            self.session.remote.len(),
            edited,
            moves,
            edited - moves,
            self.overlay.added.len(),
            self.geom_edited.len(),
            sel,
            sel_cell,
            sel_added,
            self.db.cell(top).shapes.len(),
            self.drc.violation_count(),
            self.drc.pending_tiles(),
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
                    self.selected = Some(Selection::Base((cell, idx)));
                    self.selected_tf = Transform::IDENTITY;
                    self.move_delta = self.existing_delta((cell, idx));
                }
            }
            Some("select_added") => {
                // `select_added <i>` — select the i-th live added shape. The added
                // list is sorted by AddId, so the index means the same on every
                // peer (the harness uses this to reference a peer's drawn shape).
                if let Some(i) = parts.next().and_then(|s| s.parse::<usize>().ok()) {
                    if let Some(a) = self.overlay.added.get(i) {
                        self.selected = Some(Selection::Added(a.id));
                        self.selected_tf = Transform::IDENTITY;
                        self.move_delta = (0, 0);
                    }
                }
            }
            Some("tool") => match parts.next() {
                Some("rect") => self.tool = Tool::Rect,
                Some("poly") => self.tool = Tool::Poly,
                _ => self.tool = Tool::Select,
            },
            Some("layer") => {
                // `layer <index>` or `layer <layer>/<datatype>` (e.g. `layer 68/20`).
                let arg = parts.next().unwrap_or("");
                let idx = match arg.split_once('/') {
                    Some((l, d)) => match (l.parse::<u16>(), d.parse::<u16>()) {
                        (Ok(l), Ok(d)) => self.layers.iter().position(|&x| x == Layer::new(l, d)),
                        _ => None,
                    },
                    None => arg.parse::<usize>().ok(),
                };
                if let Some(i) = idx.filter(|&i| i < self.layers.len()) {
                    self.active_layer = i;
                }
            }
            Some("draw_rect") => {
                // `draw_rect x0 y0 x1 y1` — commit a rectangle directly (dodges
                // pixel-accurate synthetic drags, same stance as `select`).
                let mut n = || parts.next().and_then(|s| s.parse::<i32>().ok());
                if let (Some(x0), Some(y0), Some(x1), Some(y1)) = (n(), n(), n(), n()) {
                    self.commit_rect(
                        Point::new(snap_nm(x0 as f32), snap_nm(y0 as f32)),
                        Point::new(snap_nm(x1 as f32), snap_nm(y1 as f32)),
                    );
                }
            }
            Some("draw_poly") => {
                // `draw_poly x0 y0 x1 y1 x2 y2 [...]` — commit a polygon directly.
                let coords: Vec<i32> = parts.filter_map(|s| s.parse::<i32>().ok()).collect();
                if coords.len() >= 6 && coords.len() % 2 == 0 {
                    self.poly_pts = coords
                        .chunks_exact(2)
                        .map(|c| Point::new(snap_nm(c[0] as f32), snap_nm(c[1] as f32)))
                        .collect();
                    self.commit_poly();
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
            Some("vertex") => {
                // `vertex <i> <dx> <dy>` — move vertex i of the selection by a
                // cell-local delta and commit (the headless vertex-edit driver,
                // SLICE 2). Works for base (SetPoints) and added (geometry LWW).
                let i = parts.next().and_then(|s| s.parse::<usize>().ok());
                let dx = parts.next().and_then(|s| s.parse::<i32>().ok());
                let dy = parts.next().and_then(|s| s.parse::<i32>().ok());
                if let (Some(vi), Some(dx), Some(dy), Some(sel)) = (i, dx, dy, self.selected) {
                    if let Some((_, pts)) = self.selection_geometry(sel) {
                        if vi < Self::handle_count(&pts) {
                            let np = Point::new(pts[vi].x + dx, pts[vi].y + dy);
                            self.preview_ring(sel, Self::ring_with_vertex(&pts, vi, np));
                            self.commit_ring(sel);
                        }
                    }
                }
            }
            Some("select_biggest") => {
                // Select shape 0 of the cell with the most shapes (caravel_core on
                // caravel) — a data-independent handle on the big flat cell.
                let biggest = (0..self.db.cells.len())
                    .max_by_key(|&c| self.db.cell(c).shapes.len())
                    .filter(|&c| self.db.cell(c).shapes.len() > 0);
                if let Some(cell) = biggest {
                    self.selected = Some(Selection::Base((cell, 0)));
                    self.selected_tf = Transform::IDENTITY;
                    self.move_delta = self.existing_delta((cell, 0));
                }
            }
            Some("drc_on") => self.drc_enabled = true,
            Some("drc_full_biggest") => {
                // Full check of the biggest cell through the live driver (pool
                // or inline) — the harness times it via `drc_last_pass_ms`.
                let biggest = (0..self.db.cells.len())
                    .max_by_key(|&c| self.db.cell(c).shapes.len())
                    .filter(|&c| self.db.cell(c).shapes.len() > 0);
                if let Some(cell) = biggest {
                    let now = self.now_ms();
                    self.drc.request_full(&self.db, &self.overlay, cell, now);
                }
            }
            Some("drc_deck") => {
                // `drc_deck sky130|blanket` — switch the rule deck.
                if let Some(d) = parts.next().and_then(fluideda_core::Deck::from_name) {
                    self.set_deck(d);
                }
            }
            Some("perf_reset") => {
                // Restart the freeze measurement (longest frame) from now — lets
                // the harness measure a window (e.g. a background DRC pass).
                self.max_frame_ms = 0.0;
                self.perf_frames = 0;
            }
            Some("drc_check") => self.drc_enqueue_visible(),
            Some("drc_bench") => {
                // Measure merge-first tiled DRC on the biggest cell (caravel_core
                // on caravel), synchronously and single-threaded, as the inline
                // path runs it: GRID = the tile-grid build (what replaced the
                // atomic lazy R-tree), COLD = grid + every tile, WARM = the
                // incremental re-check after one edit (the tiles near shape 0 —
                // what an edit to this cell actually costs now). Published to
                // __fluideda_perf for the harness.
                let biggest = (0..self.db.cells.len())
                    .max_by_key(|&c| self.db.cell(c).shapes.len())
                    .filter(|&c| self.db.cell(c).shapes.len() > 0);
                if let Some(cell) = biggest {
                    let rules = self.drc.rules().clone();
                    let cref = self.db.cell(cell);
                    let t0 = web_time::Instant::now();
                    let grid = fluideda_core::build_grid(cref, &rules);
                    let grid_ms = t0.elapsed().as_secs_f64() * 1e3;
                    let tiles = fluideda_core::cell_tiles(&grid, cell, &self.overlay, &rules);
                    let (mut violations, mut tile_max_ms) = (0usize, 0.0f64);
                    for &t in &tiles {
                        let tt = web_time::Instant::now();
                        let input = fluideda_core::gather_tile(cref, cell, &self.overlay, &grid, &rules, t);
                        violations += fluideda_core::check_tile(grid.tile_core(t), &rules, &input).len();
                        tile_max_ms = tile_max_ms.max(tt.elapsed().as_secs_f64() * 1e3);
                    }
                    let cold_ms = t0.elapsed().as_secs_f64() * 1e3;
                    let near = grid.tiles_near(&ring_bbox(cref.shapes.points(0)));
                    let t1 = web_time::Instant::now();
                    for &t in &near {
                        let input = fluideda_core::gather_tile(cref, cell, &self.overlay, &grid, &rules, t);
                        let _ = fluideda_core::check_tile(grid.tile_core(t), &rules, &input);
                    }
                    let warm_ms = t1.elapsed().as_secs_f64() * 1e3;
                    let shapes = cref.shapes.len();
                    log::info!(
                        "DRC bench cell {cell} ({shapes} shapes): grid {grid_ms:.0} ms, full {cold_ms:.0} ms over {} tiles (max tile {tile_max_ms:.1} ms), one-edit re-check {warm_ms:.1} ms over {} tiles, {violations} violations",
                        tiles.len(),
                        near.len()
                    );
                    self.drc_bench = Some(DrcBench {
                        cell: cell as i64,
                        shapes,
                        grid_ms,
                        cold_ms,
                        warm_ms,
                        warm_tiles: near.len(),
                        tiles: tiles.len(),
                        tile_max_ms,
                        violations,
                    });
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
            if self.edit_mode {
                // Phase 5 SLICE 1: the canvas tools + the layer new shapes draw on.
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tool, Tool::Select, "Select");
                    ui.selectable_value(&mut self.tool, Tool::Rect, "▭ Rect");
                    ui.selectable_value(&mut self.tool, Tool::Poly, "⬠ Poly");
                });
                if self.tool != Tool::Select && !self.layers.is_empty() {
                    self.active_layer = self.active_layer.min(self.layers.len() - 1);
                    let cur = self.layers[self.active_layer];
                    egui::ComboBox::from_label("draw layer")
                        .selected_text(format!("L{}/{}", cur.layer, cur.datatype))
                        .show_ui(ui, |ui| {
                            for i in 0..self.layers.len() {
                                let l = self.layers[i];
                                ui.selectable_value(&mut self.active_layer, i, format!("L{}/{}", l.layer, l.datatype));
                            }
                        });
                    if self.tool == Tool::Poly {
                        ui.small("click vertices · Enter/double-click closes · Esc cancels");
                    } else {
                        ui.small("drag corner-to-corner");
                    }
                    ui.small(format!("snap: {SNAP_NM} nm grid"));
                }
            }
            match self.selected {
                Some(Selection::Base((cell, idx))) => ui.small(format!("selected: cell {cell}, shape {idx}")),
                Some(Selection::Added((client, seq))) => ui.small(format!("selected: drawn #{client:x}:{seq}")),
                None => ui.small("selected: (none)"),
            };
            if let Some(sel) = self.selected {
                // Nudge = a fixed translation step, the deterministic move path.
                let step = self.nudge_step();
                ui.horizontal(|ui| {
                    if ui.button("←").clicked() { self.nudge(sel, -step, 0); }
                    if ui.button("→").clicked() { self.nudge(sel, step, 0); }
                    if ui.button("↑").clicked() { self.nudge(sel, 0, step); }
                    if ui.button("↓").clicked() { self.nudge(sel, 0, -step); }
                });
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() { self.delete_shape(sel); }
                    if ui.button("Deselect").clicked() { self.selected = None; }
                });
                ui.small(format!("move Δ: ({}, {}) nm", self.move_delta.0, self.move_delta.1));
            }
            if self.last_pick_us > 0.0 {
                ui.small(format!("last hit-test: {:.0} µs", self.last_pick_us));
            }
            let edits = self.overlay.deleted.len() + self.overlay.edited.len() + self.overlay.added.len();
            ui.small(format!("edits: {edits} (moved {}, deleted {}, drawn {})",
                self.overlay.edited.len(),
                self.overlay.deleted.len().saturating_sub(self.overlay.edited.len()),
                self.overlay.added.len()));
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
            ui.label("DRC (merge-first, intra-cell)");
            if ui.checkbox(&mut self.drc_enabled, "enable").changed() && !self.drc_enabled {
                self.drc.clear(); // off ⇒ zero cost, and no stale results on re-enable
            }
            if self.drc_enabled {
                let mut deck = self.drc_deck;
                egui::ComboBox::from_label("deck").selected_text(deck.label()).show_ui(ui, |ui| {
                    for d in fluideda_core::Deck::ALL {
                        ui.selectable_value(&mut deck, d, d.label());
                    }
                });
                if deck != self.drc_deck {
                    self.set_deck(deck);
                }
                if ui.button("Check visible cells").clicked() {
                    self.drc_enqueue_visible();
                }
                ui.small(format!(
                    "violations: {} across {} cell(s)",
                    self.drc.violation_count(),
                    self.drc.cell_count()
                ));
                if let Some((c, f)) = self.drc.grid_progress(&self.db) {
                    ui.small(format!("indexing cell {c}… {:.0}%", f * 100.0));
                }
                if self.drc.pending_tiles() > 0 {
                    ui.small(format!("checking… {} tile(s) queued", self.drc.pending_tiles()));
                }
                match &self.drc_pool {
                    Some(p) => ui.small(format!("{} worker(s), {} tile(s) in flight", p.size(), self.drc.in_flight())),
                    None => ui.small("inline (no worker pool)"),
                };
                if self.drc_tile_errors > 0 {
                    ui.colored_label(egui::Color32::YELLOW, format!("{} tile error(s) — see console", self.drc_tile_errors));
                }
                if let Some(p) = self.drc.last_pass {
                    ui.small(format!(
                        "last: cell {} — {} tile(s) in {:.0} ms → {} violation(s)",
                        p.cell, p.tiles, p.ms, p.violations
                    ));
                }
                // First few violations, cell-sorted for a stable list.
                let mut all: Vec<(CellId, &fluideda_core::Violation)> = self.drc.violations().collect();
                all.sort_by_key(|(c, v)| (*c, v.bbox.min.x, v.bbox.min.y));
                for (i, (cell, v)) in all.iter().enumerate() {
                    if i >= 8 {
                        ui.small("…");
                        break;
                    }
                    // Named rule when the deck has one ("met1 m1.1 width").
                    let what = match self.drc_deck.entry(v.layer) {
                        Some(e) => match v.kind {
                            fluideda_core::DrcKind::Width => format!("{} {} width", e.name, e.width_rule),
                            fluideda_core::DrcKind::Spacing => format!("{} {} space", e.name, e.space_rule),
                        },
                        None => format!("{:?} L{}/{}", v.kind, v.layer.layer, v.layer.datatype),
                    };
                    ui.small(format!(
                        "{what} · cell {} @ ({:.2}, {:.2}) µm",
                        cell,
                        v.bbox.min.x as f64 / 1000.0,
                        v.bbox.min.y as f64 / 1000.0
                    ));
                }
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

            // Tool bookkeeping: an in-progress polygon only lives in the Poly tool.
            if self.tool != Tool::Poly && !self.poly_pts.is_empty() {
                self.poly_pts.clear();
            }
            let drawing = self.edit_mode && self.tool != Tool::Select;

            // Draw tools (Phase 5 SLICE 1). Rect: drag corner-to-corner. Poly:
            // click vertices; Enter/double-click closes; Escape cancels. Every
            // authored coordinate snaps to the SNAP_NM grid.
            if drawing && !shift {
                match self.tool {
                    Tool::Rect => {
                        if resp.drag_started() {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let w = world_at(p, &self.camera);
                                self.rect_anchor = Some(Point::new(snap_nm(w.x), snap_nm(w.y)));
                            }
                        }
                        if resp.drag_stopped() {
                            if let (Some(a), Some(p)) = (self.rect_anchor.take(), resp.interact_pointer_pos()) {
                                let w = world_at(p, &self.camera);
                                self.commit_rect(a, Point::new(snap_nm(w.x), snap_nm(w.y)));
                            }
                        }
                    }
                    Tool::Poly => {
                        if resp.double_clicked() {
                            self.commit_poly();
                        } else if resp.clicked() {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let w = world_at(p, &self.camera);
                                let v = Point::new(snap_nm(w.x), snap_nm(w.y));
                                if self.poly_pts.last() != Some(&v) {
                                    self.poly_pts.push(v);
                                }
                            }
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.commit_poly();
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            self.poly_pts.clear();
                        }
                    }
                    Tool::Select => {}
                }
            }

            // Edit mode (Select tool): a drag that begins on a vertex handle of
            // the selection drags that vertex (SLICE 2); a drag that begins inside
            // the selected shape moves it (copy-on-write); any other drag pans.
            if resp.drag_started() && self.edit_mode && !drawing && !shift {
                if let (Some(sel), Some(p)) = (self.selected, resp.interact_pointer_pos()) {
                    if let Some(vi) = self.vertex_at(sel, p, rect) {
                        // Vertex drag takes precedence over a body move (handles
                        // sit on the outline, inside the grab radius).
                        self.vertex_drag = Some(vi);
                        self.drag_geom_start = self.selection_geometry(sel).map(|(_, pts)| pts);
                    } else if self.point_hits(sel, world_at(p, &self.camera)) {
                        self.drag_move_from = Some(world_at(p, &self.camera));
                        self.drag_move_base = self.move_delta;
                        // Geometry-record shapes (adds, vertex-edited bases) have no
                        // base to rebuild from — snapshot so the preview translates
                        // from a fixed origin and the commit rewrites geometry.
                        self.drag_geom_start = match sel {
                            Selection::Added(id) => self.overlay.find_added(id).map(|a| a.shape.points.clone()),
                            Selection::Base(id) => self
                                .geom_edited
                                .contains(&id)
                                .then(|| self.overlay.edited.get(&id).map(|s| s.points.clone()))
                                .flatten(),
                        };
                    }
                }
            }
            if resp.dragged() {
                if let (Some(vi), Some(sel), Some(p)) =
                    (self.vertex_drag, self.selected, resp.interact_pointer_pos())
                {
                    // Vertex drag: pointer → world → the cell's LOCAL frame → snap.
                    // Identity for TOP; exact under any placement transform.
                    let w = world_at(p, &self.camera);
                    let (lx, ly) = self.selected_tf.inverse().apply(w.x as f64, w.y as f64);
                    let local = Point::new(snap_nm(lx as f32), snap_nm(ly as f32));
                    if let Some(start) = self.drag_geom_start.clone() {
                        self.preview_ring(sel, Self::ring_with_vertex(&start, vi, local));
                    }
                } else if let (Some(from), Some(sel), Some(p)) =
                    (self.drag_move_from, self.selected, resp.interact_pointer_pos())
                {
                    let d = world_at(p, &self.camera) - from;
                    match sel {
                        Selection::Base(id) => {
                            // World drag delta → the selected cell's LOCAL frame (the
                            // move is stored cell-local), so at the clicked placement
                            // the shape follows the cursor even under rotation/scale.
                            let (ldx, ldy) = self.world_delta_to_local(d.x as f64, d.y as f64);
                            if let Some(start) = self.drag_geom_start.clone() {
                                // Vertex-edited shape: translate the whole snapshot
                                // (the commit rewrites its geometry record).
                                let pts =
                                    start.iter().map(|q| Point::new(q.x + ldx, q.y + ldy)).collect();
                                self.preview_ring(sel, pts);
                                self.move_delta = (ldx, ldy);
                            } else {
                                self.preview_move(id, self.drag_move_base.0 + ldx, self.drag_move_base.1 + ldy);
                            }
                        }
                        Selection::Added(id) => {
                            // TOP-cell adds: world == local. Preview by translating
                            // the drag-start snapshot; the CRDT write happens once,
                            // on release.
                            let (ddx, ddy) = (d.x.round() as i32, d.y.round() as i32);
                            if let Some(start) = self.drag_geom_start.clone() {
                                if let Some(a) = self.overlay.find_added_mut(id) {
                                    a.shape.points =
                                        start.iter().map(|q| Point::new(q.x + ddx, q.y + ddy)).collect();
                                    self.move_delta = (ddx, ddy);
                                    self.overlay_dirty = true;
                                }
                            }
                        }
                    }
                } else if !shift && !drawing {
                    let d = resp.drag_delta();
                    self.camera.pan_pixels(d.x, d.y);
                    self.dragging = true;
                }
            }
            if resp.drag_stopped() {
                // Commit the previewed edit to the CRDT once (broadcast + undoable).
                if self.vertex_drag.take().is_some() {
                    if let Some(sel) = self.selected {
                        self.commit_ring(sel);
                    }
                } else if self.drag_move_from.is_some() {
                    match self.selected {
                        Some(Selection::Base(id)) => {
                            if self.drag_geom_start.is_some() {
                                // Geometry-record shape: the move ships new points.
                                self.commit_ring(Selection::Base(id));
                            } else {
                                self.commit_move(id, self.move_delta.0, self.move_delta.1);
                            }
                        }
                        Some(sel @ Selection::Added(_)) => self.commit_ring(sel),
                        None => {}
                    }
                }
                self.drag_move_from = None;
                self.drag_geom_start = None;
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

            // Edit mode, Select tool: a plain click selects the shape under the
            // cursor (drag still pans, so navigation is unaffected). Clicking
            // empty space deselects. The draw tools consume clicks instead.
            if self.edit_mode && self.tool == Tool::Select && resp.clicked() {
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
            // Uses the current overlay geometry (moved copy / added shape), so
            // the highlight tracks the shape.
            if let Some(sel) = self.selected {
                let pts: Vec<Point> = match sel {
                    Selection::Base((cell, idx)) => match self.overlay.edited.get(&(cell, idx)) {
                        Some(shape) => shape.points.clone(),
                        None => self.db.cell(cell).shapes.points(idx as usize).to_vec(),
                    },
                    Selection::Added(id) => self
                        .overlay
                        .find_added(id)
                        .map(|a| a.shape.points.clone())
                        .unwrap_or_default(),
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

            // Vertex handles (SLICE 2): small squares on the selection's ring,
            // draggable one at a time. Only in the Select tool, where a drag near
            // a handle means "move this vertex", not "move the shape".
            if self.edit_mode && self.tool == Tool::Select {
                if let Some(sel) = self.selected {
                    if let Some((_, hpts)) = self.selection_geometry(sel) {
                        let n = Self::handle_count(&hpts);
                        for q in hpts.iter().take(n) {
                            let (wx, wy) = self.selected_tf.apply_point(*q);
                            let s = self.world_to_screen(Vec2::new(wx as f32, wy as f32), rect);
                            painter.rect_filled(
                                egui::Rect::from_center_size(s, egui::vec2(6.0, 6.0)),
                                1.0,
                                egui::Color32::from_rgb(255, 230, 0),
                            );
                        }
                    }
                }
            }

            // DRC violation markers (SLICE 3): red boxes over the gap/shape, at
            // every visible placement of the violating cell (TOP at identity —
            // where drawn shapes live — is always included). Capped; the panel
            // carries the full counts.
            if self.drc_enabled && self.drc.violation_count() > 0 {
                let vis = self.visible.clone();
                let stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 64, 64));
                let mut placements: std::collections::HashMap<CellId, &Vec<Transform>> =
                    std::collections::HashMap::new();
                for d in vis.draws.iter() {
                    placements.entry(d.cell).or_insert(&d.transforms);
                }
                let identity = vec![Transform::IDENTITY];
                let mut drawn = 0usize;
                'markers: for (cell, v) in self.drc.violations() {
                    let tfs: &Vec<Transform> = if cell == self.db.top {
                        &identity
                    } else if let Some(t) = placements.get(&cell) {
                        t
                    } else {
                        continue; // cell not on screen this frame
                    };
                    {
                        for tf in tfs.iter() {
                            if drawn >= DRC_MARKER_CAP {
                                break 'markers;
                            }
                            let (x0, y0) = tf.apply_point(v.bbox.min);
                            let (x1, y1) = tf.apply_point(v.bbox.max);
                            let r = egui::Rect::from_two_pos(
                                self.world_to_screen(Vec2::new(x0 as f32, y0 as f32), rect),
                                self.world_to_screen(Vec2::new(x1 as f32, y1 as f32), rect),
                            );
                            if r.intersects(rect) {
                                painter.rect_stroke(r.expand(2.0), 0.0, stroke);
                                drawn += 1;
                            }
                        }
                    }
                }
            }

            // Draw-tool previews (Phase 5 SLICE 1): the in-progress rect corner
            // drag and the open polygon, screen-space like the highlight.
            let preview_stroke = egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(120, 220, 255));
            if let Some(a) = self.rect_anchor {
                if let Some(p) = resp.interact_pointer_pos().or_else(|| resp.hover_pos()) {
                    let w = world_at(p, &self.camera);
                    let b = Point::new(snap_nm(w.x), snap_nm(w.y));
                    let r = egui::Rect::from_two_pos(
                        self.world_to_screen(Vec2::new(a.x as f32, a.y as f32), rect),
                        self.world_to_screen(Vec2::new(b.x as f32, b.y as f32), rect),
                    );
                    painter.rect_stroke(r, 0.0, preview_stroke);
                }
            }
            if !self.poly_pts.is_empty() {
                let mut screen: Vec<egui::Pos2> = self
                    .poly_pts
                    .iter()
                    .map(|q| self.world_to_screen(Vec2::new(q.x as f32, q.y as f32), rect))
                    .collect();
                if let Some(p) = resp.hover_pos() {
                    screen.push(p); // rubber band to the cursor
                }
                painter.add(egui::Shape::line(screen, preview_stroke));
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
