//! The eframe application: central canvas (layout + overlay) and a side panel
//! (layers, room, fps). This is the glue that closes the Phase 0 loop.

use crate::paint::{init_renderer, LayoutPaint};
use eframe::egui;
use fluideda_render::Camera;
use fluideda_sync::{color_from_id, Marker, Presence, Session};
use glam::Vec2;
use std::sync::Arc;

/// Embedded sample so the app renders something with zero setup.
const SAMPLE_GDS: &[u8] = include_bytes!("../../../assets/sample.gds");

pub struct FluidApp {
    camera: Camera,
    layer_count: usize,
    layer_visible: Vec<bool>,
    renderer_ready: bool,
    meshes: Arc<Vec<fluideda_core::LayerMesh>>,
    bbox_min: Vec2,
    bbox_max: Vec2,
    session: Option<Session>,
    name: String,
    last_frame: f64,
    fps: f32,
    dragging: bool,
}

impl FluidApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (meshes, bbox) = fluideda_core::load_and_tessellate(SAMPLE_GDS)
            .expect("sample.gds should parse");
        let layer_count = meshes.len();
        let bbox_min = Vec2::new(bbox.min.x as f32, bbox.min.y as f32);
        let bbox_max = Vec2::new(bbox.max.x as f32, bbox.max.y as f32);

        // Build the wgpu renderer immediately — we already have the device.
        if let Some(state) = cc.wgpu_render_state.as_ref() {
            init_renderer(state, Arc::new(meshes.clone()));
        }

        let mut camera = Camera::new(Vec2::new(1280.0, 800.0));
        camera.fit(bbox_min, bbox_max);

        Self {
            camera,
            layer_count,
            layer_visible: vec![true; layer_count],
            renderer_ready: cc.wgpu_render_state.is_some(),
            meshes: Arc::new(meshes),
            bbox_min,
            bbox_max,
            session: None,
            name: "guest".to_string(),
            last_frame: 0.0,
            fps: 0.0,
            dragging: false,
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let url = server_url();
        let id = ctx.viewport_id().value();
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        self.side_panel(ctx);
        self.canvas(ctx);
        ctx.request_repaint(); // continuous for smooth pan + live cursors
    }
}

impl FluidApp {
    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("panel").default_width(220.0).show(ctx, |ui| {
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
            ui.label("Layers");
            for i in 0..self.layer_count {
                if ui.checkbox(&mut self.layer_visible[i], format!("layer {i}")).changed() {
                    // Visibility toggles are applied on the renderer via callback
                    // resources; wired in the paint path (omitted for brevity).
                }
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
            let (rect, resp) =
                ui.allocate_exact_size(size, egui::Sense::click_and_drag());

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
                    LayoutPaint { camera: self.camera },
                ));
            }

            // --- draw overlay (cursors + markers) with egui painter, in screen space ---
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
