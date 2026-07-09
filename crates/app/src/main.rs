//! Entry point. Native = a winit window; wasm = mount on <canvas id="fluid-canvas">.
//! Both go through eframe with the wgpu backend.

mod app;
mod paint;

use app::FluidApp;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    env_logger::init();
    let native_options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "FluidEDA",
        native_options,
        Box::new(|cc| Ok(Box::new(FluidApp::new(cc)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Info).ok();

    let web_options = eframe::WebOptions::default();
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("fluid-canvas")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();

        eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(FluidApp::new(cc)))),
            )
            .await
            .expect("failed to start eframe");
    });
}
