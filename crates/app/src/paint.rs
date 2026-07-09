//! egui-wgpu paint callback that renders the layout into the egui canvas.
//!
//! The `LayoutRenderer` lives in egui's render-state `paint_callback_resources`
//! (created once, on first paint, when we have a `wgpu::Device`). Each frame we
//! push the current `Camera` and issue the draw inside the egui render pass.

use eframe::egui_wgpu::{self, CallbackTrait};
use fluideda_core::LayerMesh;
use fluideda_render::{Camera, LayoutRenderer};
use std::sync::Arc;

/// egui paint callback carrying the per-frame camera.
pub struct LayoutPaint {
    pub camera: Camera,
}

impl CallbackTrait for LayoutPaint {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = resources.get::<LayoutRenderer>() {
            r.update_camera(queue, &self.camera);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(r) = resources.get::<LayoutRenderer>() {
            r.draw(pass);
        }
    }
}

/// Build the `LayoutRenderer` once and stash it in egui's callback resources.
pub fn init_renderer(
    wgpu_state: &egui_wgpu::RenderState,
    meshes: Arc<Vec<LayerMesh>>,
) {
    let device = &wgpu_state.device;
    let format = wgpu_state.target_format;
    let renderer = LayoutRenderer::new(device, format, &meshes);
    wgpu_state
        .renderer
        .write()
        .callback_resources
        .insert(renderer);
}
