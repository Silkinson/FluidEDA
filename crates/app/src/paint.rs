//! egui-wgpu paint callback that renders the hierarchical layout.
//!
//! The `HierRenderer` lives in egui's `paint_callback_resources` (built once, on
//! first paint, when we have a `wgpu::Device`). Each frame the app runs the
//! culler and hands us the `VisibleSet`; `prepare` packs the per-instance
//! transforms and `paint` issues the instanced draws inside the egui render pass.

use eframe::egui_wgpu::{self, CallbackTrait};
use fluideda_core::VisibleSet;
use fluideda_render::{Camera, HierRenderer};
use std::sync::Arc;

// Note: the Phase 2b STEP 2 groundwork wrapped `HierRenderer` in a
// `MainThreadOnly` marker so it could enter egui's `Send + Sync` resource map
// under a `+atomics` build (where wgpu's wasm handles become `!Send + !Sync`).
// The shipped worker is a *plain* worker — a separate wasm module, no atomics —
// so wgpu keeps its `fragile-send-sync-non-atomic-wasm` shim and `HierRenderer`
// is `Send + Sync` again. The wrapper is gone; the renderer goes in directly.

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

/// Build an empty `HierRenderer` and stash it in egui resources. Cell geometry is
/// tessellated + uploaded lazily as cells become visible (see `app::ensure_uploaded`).
pub fn init_renderer(wgpu_state: &egui_wgpu::RenderState) {
    let device = &wgpu_state.device;
    let format = wgpu_state.target_format;
    let renderer = HierRenderer::new(device, format);
    wgpu_state
        .renderer
        .write()
        .callback_resources
        .insert(renderer);
}

/// Borrow the renderer out of egui's callback resources.
pub fn renderer_mut(resources: &mut egui_wgpu::CallbackResources) -> Option<&mut HierRenderer> {
    resources.get_mut::<HierRenderer>()
}
