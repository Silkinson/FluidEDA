//! Entry point. Native = a winit window; wasm = mount on <canvas id="fluid-canvas">.
//! Both go through eframe with the wgpu backend.

mod app;
mod design;
mod paint;
mod webio;

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
        let document = web_sys::window().unwrap().document().unwrap();
        let canvas = document
            .get_element_by_id("fluid-canvas")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();

        let result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(FluidApp::new(cc)))),
            )
            .await;

        // Clear the HTML loading overlay — otherwise it hangs on "Loading…"
        // forever. On failure, show why instead of silently blanking.
        if let Some(loading) = document.get_element_by_id("loading") {
            match &result {
                Ok(_) => loading.remove(),
                Err(e) => {
                    log::error!("failed to start eframe: {e:?}");
                    loading.set_text_content(Some(&format!(
                        "Could not start FluidEDA: {e:?}\n(needs a browser with WebGPU or WebGL2)"
                    )));
                }
            }
        }
    });
}
