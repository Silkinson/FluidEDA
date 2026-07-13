//! egui-wgpu paint callback that renders the hierarchical layout.
//!
//! The `HierRenderer` lives in egui's `paint_callback_resources` (built once, on
//! first paint, when we have a `wgpu::Device`). Each frame the app runs the
//! culler and hands us the `VisibleSet`; `prepare` packs the per-instance
//! transforms and `paint` issues the instanced draws inside the egui render pass.

use eframe::egui_wgpu::{self, CallbackTrait};
use fluideda_core::{CellId, LayerMesh, VisibleSet};
use fluideda_render::{Camera, HierRenderer};
use std::collections::HashMap;
use std::sync::Arc;

/// egui paint callback carrying the per-frame camera and visible set.
pub struct LayoutPaint {
    pub camera: Camera,
    pub visible: Arc<VisibleSet>,
}

impl CallbackTrait for LayoutPaint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = resources.get_mut::<HierRenderer>() {
            r.prepare(device, queue, &self.camera, &self.visible);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(r) = resources.get::<HierRenderer>() {
            r.draw(pass);
        }
    }
}

/// Build the `HierRenderer` once (per-cell meshes) and stash it in egui resources.
pub fn init_renderer(
    wgpu_state: &egui_wgpu::RenderState,
    per_cell: Arc<HashMap<CellId, Vec<LayerMesh>>>,
) {
    let device = &wgpu_state.device;
    let format = wgpu_state.target_format;
    let renderer = HierRenderer::new(device, format, &per_cell);
    wgpu_state
        .renderer
        .write()
        .callback_resources
        .insert(renderer);
}
