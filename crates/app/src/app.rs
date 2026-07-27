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
    cull, cull_with_overlay, pick_in_cell, BaseShapeId, CellDb, CellId, ChunkId, CullStats,
    EditOverlay, Layer, Point, VisibleSet,
};
use fluideda_render::Camera;
use fluideda_sync::{color_from_id, Marker, Presence, Session};
use glam::Vec2;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Zero-config default: the tiny embedded hierarchical sample.
const HIER_GDS: &[u8] = include_bytes!("../../../assets/hier.gds");

/// A cell projecting smaller than this many pixels becomes an LOD proxy.
const MIN_PIXEL: f64 = 4.0;

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
    tess_accum_ms: f64,
    /// Cumulative uploaded GPU geometry bytes so far.
    gpu_bytes: usize,

    // --- Phase 3 editing ---
    /// Copy-on-write edits over the immutable base. Empty ⇒ pristine Phase 2b
    /// render path (the cull hot path is unchanged; see `recull`).
    overlay: EditOverlay,
    /// When on, a left-click selects a TOP-cell shape instead of doing nothing;
    /// pan/zoom still work. Move/delete of the selection arrive in step 3.
    edit_mode: bool,
    /// The currently selected base shape (TOP cell only, this skeleton).
    selected: Option<BaseShapeId>,
    /// Current translation (nm) applied to the selection, measured from its base
    /// position. Accumulated by nudges/drag; the moved copy is always base+this.
    move_delta: (i32, i32),
    /// World point where a move-drag began (`None` when not dragging a shape).
    drag_move_from: Option<Vec2>,
    /// `move_delta` at the moment a move-drag began.
    drag_move_base: (i32, i32),
    /// Base chunks whose geometry an edit invalidated — re-tessellated (with
    /// suppressions) on the next `ensure_uploaded`. Only these rebuild, not the
    /// whole cell: the incremental-edit property.
    dirty_keys: HashSet<(CellId, ChunkId)>,
    /// Overlay geometry needs re-upload (set when the overlay changed).
    overlay_dirty: bool,
    /// Last hit-test latency, shown in the panel (the lazy-index-build cost).
    last_pick_us: f64,
    /// Instrumentation for the last edit: chunks re-tessellated + wall time.
    last_edit_chunks: usize,
    last_edit_ms: f64,

    session: Option<Session>,
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

        // Web: honor ?file=<url> by fetching it into the load slot.
        if let Some(url) = webio::query_file() {
            log::info!("autoloading ?file={url}");
            webio::spawn_fetch(url, load_slot.clone());
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
            load_instant: web_time::Instant::now(),
            awaiting_first_frame: true, // measure TTFF for the default too
            status: String::new(),
            forced_zoom: webio::query_zoom(),
            debug_stats: webio::query_flag("stats"),
            uploaded: HashSet::new(),
            tess_accum_ms: 0.0,
            gpu_bytes: 0,
            overlay: EditOverlay::new(),
            edit_mode: false,
            selected: None,
            move_delta: (0, 0),
            drag_move_from: None,
            drag_move_base: (0, 0),
            dirty_keys: HashSet::new(),
            overlay_dirty: false,
            last_pick_us: 0.0,
            last_edit_chunks: 0,
            last_edit_ms: 0.0,
            session: None,
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
        // A fresh design has its own shape ids — drop any edits/selection.
        self.overlay = EditOverlay::new();
        self.selected = None;
        self.move_delta = (0, 0);
        self.drag_move_from = None;
        self.dirty_keys.clear();
        self.overlay_dirty = false;

        if let Some(rs) = frame.wgpu_render_state() {
            init_renderer(rs);
            self.renderer_ready = true;
        }
        self.awaiting_first_frame = true;
    }

    /// Hit-test a world point against the TOP cell and update the selection.
    /// TOP is at identity, so world nm == TOP-local nm (skeleton scope cut).
    fn pick_at(&mut self, world: Vec2) {
        let top = self.db.top;
        let p = Point::new(world.x.round() as i32, world.y.round() as i32);
        let t = web_time::Instant::now();
        let hit = pick_in_cell(self.db.cell(top), p);
        self.last_pick_us = t.elapsed().as_secs_f64() * 1e6;
        self.selected = hit.map(|idx| (top, idx));
        // Seed the move delta from any existing move of the newly selected shape,
        // so a nudge/drag continues from where it sits rather than snapping back.
        self.move_delta = self.selected.map_or((0, 0), |id| self.existing_delta(id));
    }

    /// Does world point `w` land inside the selection's *current* geometry (the
    /// moved copy if moved, else the base)? Used to decide drag = move vs pan.
    fn point_hits(&self, id: BaseShapeId, w: Vec2) -> bool {
        let p = Point::new(w.x.round() as i32, w.y.round() as i32);
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

    /// Set the selection's translation to `(dx, dy)` from its base — the single
    /// move entry point (nudge and drag both call it). Copy-on-write: reads the
    /// base geometry, translates, and records it in the overlay; the base column
    /// is never touched. Invalidates only the chunk holding the shape.
    fn set_move(&mut self, id: BaseShapeId, dx: i32, dy: i32) {
        let cell = self.db.cell(id.0);
        let layer = cell.shapes.layer(id.1 as usize);
        let pts = cell.shapes.points(id.1 as usize);
        let moved = fluideda_core::edit::translate(layer, pts, dx, dy);
        self.overlay.move_shape(id, moved);
        self.move_delta = (dx, dy);
        self.invalidate(id);
    }

    /// Nudge step (nm): a visible fraction of the design, so panel nudges are
    /// obvious at any zoom. Kept modest so repeated nudges stay on-screen.
    fn nudge_step(&self) -> i32 {
        let w = (self.bbox_max.x - self.bbox_min.x).max(1.0);
        ((w * 0.02) as i32).max(1)
    }

    /// Nudge the selection by `(dx, dy)` on top of its current move delta.
    fn nudge(&mut self, id: BaseShapeId, dx: i32, dy: i32) {
        let (cx, cy) = self.move_delta;
        self.set_move(id, cx + dx, cy + dy);
    }

    /// Delete the selection (tombstone). Copy-on-write path: no base mutation, the
    /// containing chunk just re-tessellates without the shape.
    fn delete_shape(&mut self, id: BaseShapeId) {
        self.overlay.delete(id);
        self.invalidate(id);
        if self.selected == Some(id) {
            self.selected = None;
            self.move_delta = (0, 0);
        }
    }

    /// Mark the chunk that owns `id`'s base shape for re-tessellation, and flag
    /// the overlay geometry for re-upload. This is the whole of "incremental":
    /// one chunk (of possibly dozens in the cell) rebuilds, not the cell.
    fn invalidate(&mut self, id: BaseShapeId) {
        if let Some(chunk) = self.db.cell(id.0).chunk_of(id.1) {
            self.dirty_keys.insert((id.0, chunk));
        }
        self.overlay_dirty = true;
    }

    /// Tessellate + upload any visible cell **chunk** not yet on the GPU (lazy).
    /// Runs after culling, before paint, so the paint callback sees uploaded
    /// buffers. First view of a chunk costs one tessellation on the main thread
    /// (the source of any pan hitch); thereafter it's cached.
    fn ensure_uploaded(&mut self, frame: &mut eframe::Frame) {
        let Some(rs) = frame.wgpu_render_state() else { return };
        let device = rs.device.clone();
        let mut guard = rs.renderer.write();
        let Some(renderer) = renderer_mut(&mut guard.callback_resources) else { return };

        // Phase 3 invalidation: drop the chunks an edit dirtied so the loop below
        // re-tessellates just those (with suppressions), not the whole cell.
        let editing = !self.dirty_keys.is_empty() || self.overlay_dirty;
        let edit_t = web_time::Instant::now();
        let mut rebuilt = 0usize;
        for key in self.dirty_keys.drain() {
            renderer.drop_chunk(key);
            self.uploaded.remove(&key);
        }

        let empty = std::collections::HashSet::new();
        let visible = self.visible.clone();
        for d in &visible.draws {
            let key = (d.cell, d.chunk);
            if self.uploaded.contains(&key) {
                continue;
            }
            let cell = &self.db.cells[d.cell];
            let chunk = &cell.chunks[d.chunk as usize];
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

        // Async slot (fetch / picker) — take at most one per frame.
        let pending = self.load_slot.borrow_mut().take();
        if let Some((name, bytes)) = pending {
            self.status = format!("loading {name} ({} KB)…", bytes.len() / 1024);
            self.load_instant = web_time::Instant::now();
            match design::load_design(&name, &bytes) {
                Ok(design) => {
                    self.install(design, frame);
                    self.status.clear();
                }
                Err(e) => self.status = format!("parse failed: {e}"),
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

    fn connect(&mut self, ctx: &egui::Context) {
        let url = server_url();
        let id = ctx.viewport_id().0.value();
        let local = Presence {
            cursor: Vec2::ZERO,
            name: self.name.clone(),
            color: color_from_id(id),
        };
        match Session::connect(&url, local) {
            Ok(s) => {
                log::info!("connected to {url}");
                self.session = Some(s);
            }
            Err(e) => log::error!("connect failed: {e}"),
        }
    }
}

impl eframe::App for FluidApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = ctx.input(|i| i.time);
        let dt = (now - self.last_frame).max(1e-4);
        self.fps = 0.9 * self.fps + 0.1 * (1.0 / dt) as f32;
        self.last_frame = now;

        // Finalize TTFF at the top of the frame *after* a load, so a frame has
        // actually rendered before we stamp the number.
        self.finish_first_frame();

        self.drain_loads(ctx, frame);

        if let Some(s) = self.session.as_mut() {
            if s.poll() {
                ctx.request_repaint();
            }
        }

        self.side_panel(ctx, frame);
        self.canvas(ctx);
        self.recull();
        self.ensure_uploaded(frame);

        ctx.request_repaint();
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
            if self.session.is_none() {
                if ui.button("Join room").clicked() {
                    self.connect(ctx);
                }
            } else {
                ui.colored_label(egui::Color32::GREEN, "● connected");
                ui.label(format!("{} peers", self.session.as_ref().unwrap().remote.len()));
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
                    let d = world_at(p, &self.camera) - from;
                    self.set_move(
                        id,
                        self.drag_move_base.0 + d.x.round() as i32,
                        self.drag_move_base.1 + d.y.round() as i32,
                    );
                } else if !shift {
                    let d = resp.drag_delta();
                    self.camera.pan_pixels(d.x, d.y);
                    self.dragging = true;
                }
            }
            if resp.drag_stopped() {
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
                if let Some(s) = self.session.as_mut() {
                    s.set_cursor(world);
                }
                if resp.drag_started() && ui.input(|i| i.modifiers.shift) {
                    if let Some(s) = self.session.as_mut() {
                        let half = 2000.0;
                        s.add_marker(Marker {
                            min: world - Vec2::splat(half),
                            max: world + Vec2::splat(half),
                            color: [1.0, 1.0, 1.0, 0.6],
                        });
                    }
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

            // Highlight the selected shape's outline (screen-space; TOP is at
            // identity so its shape points are world nm directly). Uses the moved
            // copy's geometry when the selection has been moved, so the highlight
            // tracks the shape.
            if let Some((cell, idx)) = self.selected {
                let moved = self.overlay.edited.get(&(cell, idx));
                let pts: Vec<Point> = match moved {
                    Some(shape) => shape.points.clone(),
                    None => self.db.cell(cell).shapes.points(idx as usize).to_vec(),
                };
                let screen: Vec<egui::Pos2> = pts
                    .iter()
                    .map(|p| self.world_to_screen(Vec2::new(p.x as f32, p.y as f32), rect))
                    .collect();
                if screen.len() >= 2 {
                    let stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 230, 0));
                    painter.add(egui::Shape::closed_line(screen, stroke));
                }
            }

            if let Some(s) = self.session.as_ref() {
                for (_, pres) in s.remote.iter() {
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
            }
        });
    }

    fn world_to_screen(&self, world: Vec2, rect: egui::Rect) -> egui::Pos2 {
        let half = self.camera.viewport * 0.5;
        let d = (world - self.camera.center) * self.camera.scale;
        egui::pos2(rect.min.x + half.x + d.x, rect.min.y + half.y - d.y)
    }
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
