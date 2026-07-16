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

/// Marks a value as "only ever touched on the main thread", making it storable
/// in egui's `Send + Sync` callback-resource map.
///
/// Needed for the Phase 2b worker. A shared wasm heap requires building with
/// `-C target-feature=+atomics`, which makes the target genuinely multi-threaded
/// — so wgpu can no longer apply its `fragile-send-sync-non-atomic-wasm` shim and
/// its wasm handles (which wrap JS objects, and JS objects cannot cross a worker
/// boundary) become `!Send + !Sync`. egui's resource map requires `Send + Sync`
/// unconditionally, and that single bound is the *only* thing in the whole
/// dependency tree that blocks the atomics build.
///
/// Sound here because the wrapped value never leaves the main thread: eframe runs
/// its paint callbacks only on the main thread, and the geometry worker links
/// `fluideda-core` alone — it has no handle to egui, wgpu, or this map. Do not
/// use this to move a `HierRenderer` to a worker; it would hand a JS object to a
/// thread that cannot legally touch it.
struct MainThreadOnly<T>(T);

// SAFETY: see the type docs — main-thread-only access is an invariant of the
// architecture, not of the type. Never send one of these anywhere.
unsafe impl<T> Send for MainThreadOnly<T> {}
unsafe impl<T> Sync for MainThreadOnly<T> {}

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
        if let Some(r) = resources.get_mut::<MainThreadOnly<HierRenderer>>() {
            r.0.prepare(device, queue, &self.camera, &self.visible);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(r) = resources.get::<MainThreadOnly<HierRenderer>>() {
            r.0.draw(pass);
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
        .insert(MainThreadOnly(renderer));
}

/// Borrow the renderer out of egui's callback resources.
///
/// The one place that unwraps [`MainThreadOnly`], so callers never mention it and
/// cannot accidentally move a renderer off this thread.
pub fn renderer_mut(resources: &mut egui_wgpu::CallbackResources) -> Option<&mut HierRenderer> {
    resources.get_mut::<MainThreadOnly<HierRenderer>>().map(|r| &mut r.0)
}
