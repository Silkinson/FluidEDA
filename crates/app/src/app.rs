//! The eframe application: hierarchical layout canvas + side panel (layers,
//! room, fps, cull stats, load metrics). Phase 1.5 adds **runtime file loading**
//! (drag-and-drop, a file picker, and `?file=` autoload on web) so real chips —
//! which cannot be embedded in the wasm — can be opened, and instruments the
//! load path (parse / tessellation / index / TTFF / wasm heap).

use crate::design::{self, Design, Metrics};
use crate::paint::{init_renderer, LayoutPaint};
use crate::webio::{self, LoadSlot};
use eframe::egui;
use fluideda_core::{cull, CellDb, CellId, CullStats, Layer, VisibleSet};
use fluideda_render::{Camera, HierRenderer};
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
    /// Cell defs whose geometry has been tessellated + uploaded (lazy, STEP 3).
    uploaded: HashSet<CellId>,
    /// Cumulative main-thread tessellation time so far (ms) — the lazy TTFF cost.
    tess_accum_ms: f64,
    /// Cumulative uploaded GPU geometry bytes so far.
    gpu_bytes: usize,

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

        if let Some(rs) = frame.wgpu_render_state() {
            init_renderer(rs);
            self.renderer_ready = true;
        }
        self.awaiting_first_frame = true;
    }

    /// Tessellate + upload any visible cell def not yet on the GPU (lazy). Runs
    /// after culling, before paint, so the paint callback sees uploaded buffers.
    /// First-view of a def costs one tessellation on the main thread (the source
    /// of any pan hitch); thereafter it's cached.
    fn ensure_uploaded(&mut self, frame: &mut eframe::Frame) {
        let Some(rs) = frame.wgpu_render_state() else { return };
        let device = rs.device.clone();
        let mut guard = rs.renderer.write();
        let Some(renderer) = guard.callback_resources.get_mut::<HierRenderer>() else { return };

        let visible = self.visible.clone();
        for d in &visible.draws {
            if self.uploaded.contains(&d.cell) {
                continue;
            }
            let t = web_time::Instant::now();
            let meshes = fluideda_core::tessellate_shapes(&self.db.cells[d.cell].shapes);
            self.tess_accum_ms += t.elapsed().as_secs_f64() * 1e3;
            renderer.upload_cell(&device, d.cell, &meshes);
            self.uploaded.insert(d.cell);
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
        // the per-cell index queries that fill visible_shapes.
        let vs = if self.debug_stats {
            fluideda_core::cull_counted(&self.db, &view, self.camera.scale as f64, MIN_PIXEL)
        } else {
            cull(&self.db, &view, self.camera.scale as f64, MIN_PIXEL)
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
                    "tess (lazy): {:.1} ms, {} defs uploaded",
                    self.tess_accum_ms,
                    self.uploaded.len()
                ));
                ui.small(format!("GPU geo:     {} MB (visible only)", self.gpu_bytes / 1_048_576));
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
                                if let Some(r) = rs.renderer.write().callback_resources.get_mut::<HierRenderer>() {
                                    r.set_layer_visible(layer, self.layer_visible[i]);
                                }
                            }
                        }
                    });
                }
            });

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

            if resp.dragged() && !ui.input(|i| i.modifiers.shift) {
                let d = resp.drag_delta();
                self.camera.pan_pixels(d.x, d.y);
                self.dragging = true;
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
