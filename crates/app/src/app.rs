//! The eframe application: central canvas (hierarchical layout + overlay) and a
//! side panel (layers, room, fps, cull stats). Phase 1 loads a *hierarchical*
//! GDS into a `CellDb`, tessellates each cell def once, and every frame runs the
//! culler to feed the instanced renderer only what's visible.

use crate::paint::{init_renderer, LayoutPaint};
use eframe::egui;
use fluideda_core::{Bboxf, CellDb, CullStats, Layer, VisibleSet};
use fluideda_render::{Camera, HierRenderer};
use fluideda_sync::{color_from_id, Marker, Presence, Session};
use glam::Vec2;
use std::collections::HashMap;
use std::sync::Arc;

/// Embedded hierarchical sample: 980 bytes on disk, 10M effective shapes.
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
    session: Option<Session>,
    name: String,
    last_frame: f64,
    fps: f32,
    dragging: bool,
}

impl FluidApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let db = fluideda_core::gds::load_gds_db(HIER_GDS).expect("hier.gds should parse");
        let layers = db.all_layers();
        let total_shapes = db.flat_shape_count();
        let wb = db.world_bbox();
        let bbox_min = Vec2::new(wb.min_x as f32, wb.min_y as f32);
        let bbox_max = Vec2::new(wb.max_x as f32, wb.max_y as f32);

        // Tessellate each cell definition once (cell-local nm). Reused across
        // every instance by the renderer.
        let mut per_cell: HashMap<usize, Vec<fluideda_core::LayerMesh>> = HashMap::new();
        for (id, cell) in db.cells.iter().enumerate() {
            let meshes = fluideda_core::tessellate_shapes(&cell.shapes);
            if !meshes.is_empty() {
                per_cell.insert(id, meshes);
            }
        }

        if let Some(state) = cc.wgpu_render_state.as_ref() {
            init_renderer(state, Arc::new(per_cell));
        }

        let mut camera = Camera::new(Vec2::new(1280.0, 800.0));
        camera.fit(bbox_min, bbox_max);

        let layer_count = layers.len();
        Self {
            camera,
            db,
            layers,
            layer_visible: vec![true; layer_count],
            renderer_ready: cc.wgpu_render_state.is_some(),
            bbox_min,
            bbox_max,
            visible: Arc::new(VisibleSet::default()),
            cull_stats: CullStats::default(),
            total_shapes,
            session: None,
            name: "guest".to_string(),
            last_frame: 0.0,
            fps: 0.0,
            dragging: false,
        }
    }

    /// Run the culler for the current camera and stash the visible set.
    fn recull(&mut self) {
        // Rebase the world origin near the viewport centre so f32 stays precise.
        self.camera.world_origin = self.camera.center;

        let half_w = (self.camera.viewport.x as f64 * 0.5) / self.camera.scale as f64;
        let half_h = (self.camera.viewport.y as f64 * 0.5) / self.camera.scale as f64;
        let cx = self.camera.center.x as f64;
        let cy = self.camera.center.y as f64;
        let view = Bboxf::new(cx - half_w, cy - half_h, cx + half_w, cy + half_h);

        let vs = fluideda_core::cull(&self.db, &view, self.camera.scale as f64, MIN_PIXEL);
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
        // fps
        let now = ctx.input(|i| i.time);
        let dt = (now - self.last_frame).max(1e-4);
        self.fps = 0.9 * self.fps + 0.1 * (1.0 / dt) as f32;
        self.last_frame = now;

        // pump multiplayer
        if let Some(s) = self.session.as_mut() {
            if s.poll() {
                ctx.request_repaint();
            }
        }

        self.side_panel(ctx, frame);
        self.canvas(ctx);

        // Cull for the (possibly updated) camera after input is applied.
        self.recull();

        ctx.request_repaint(); // continuous for smooth pan + live cursors
    }
}

impl FluidApp {
    fn side_panel(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        egui::SidePanel::left("panel").default_width(240.0).show(ctx, |ui| {
            ui.heading("FluidEDA");
            ui.label(format!("{:.0} fps", self.fps));
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
            ui.small(format!("total shapes:  {}", self.total_shapes));
            ui.small(format!("visible:       {}", self.cull_stats.visible_shapes));
            ui.small(format!("instances:     {}", self.cull_stats.instances_emitted));
            ui.small(format!("LOD proxies:   {}", self.cull_stats.proxy_count));
            ui.small(format!("nodes walked:  {}", self.cull_stats.nodes_visited));

            ui.separator();
            ui.label("Layers");
            for i in 0..self.layers.len() {
                let layer = self.layers[i];
                ui.horizontal(|ui| {
                    let [r, g, b, _] = fluideda_render::layer_color(layer);
                    let sw = egui::Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 2.0, sw);
                    if ui
                        .checkbox(
                            &mut self.layer_visible[i],
                            format!("L{}/{}", layer.layer, layer.datatype),
                        )
                        .changed()
                    {
                        if let Some(rs) = frame.wgpu_render_state() {
                            if let Some(r) = rs
                                .renderer
                                .write()
                                .callback_resources
                                .get_mut::<HierRenderer>()
                            {
                                r.set_layer_visible(layer, self.layer_visible[i]);
                            }
                        }
                    }
                });
            }

            ui.separator();
            if ui.button("Fit view").clicked() {
                self.camera.fit(self.bbox_min, self.bbox_max);
            }
            ui.small("Drag: pan · Wheel: zoom · Shift+drag: drop marker");
        });
    }

    fn canvas(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let size = ui.available_size();
            self.camera.viewport = Vec2::new(size.x, size.y);
            let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());

            // --- input -> camera ---
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

            // --- pointer -> presence + markers ---
            if let Some(p) = resp.hover_pos() {
                let local = Vec2::new(p.x - rect.min.x, p.y - rect.min.y);
                let world = self.camera.screen_to_world(local);
                if let Some(s) = self.session.as_mut() {
                    s.set_cursor(world);
                }
                if resp.drag_started() && ui.input(|i| i.modifiers.shift) {
                    if let Some(s) = self.session.as_mut() {
                        let half = 2000.0; // 2 µm marker
                        s.add_marker(Marker {
                            min: world - Vec2::splat(half),
                            max: world + Vec2::splat(half),
                            color: [1.0, 1.0, 1.0, 0.6],
                        });
                    }
                }
            }

            // --- draw layout via wgpu callback ---
            if self.renderer_ready {
                ui.painter().add(eframe::egui_wgpu::Callback::new_paint_callback(
                    rect,
                    LayoutPaint {
                        camera: self.camera,
                        visible: self.visible.clone(),
                    },
                ));
            }

            // --- overlay (cursors + markers) in screen space ---
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

/// Derive the ws URL from the page location (wasm) or localhost (native).
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
