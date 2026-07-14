//! Browser I/O glue for runtime file loading: `?file=` query autoload and
//! `fetch`. Native builds get no-op stubs (native uses the `--file` CLI in the
//! bench/shot harnesses and drag-and-drop in the app).

use std::cell::RefCell;
use std::rc::Rc;

/// Async loads (fetch / picker) drop their (name, bytes) here for the app to
/// pick up on the next frame.
pub type LoadSlot = Rc<RefCell<Option<(String, Vec<u8>)>>>;

/// Read the `file` query parameter from the page URL, if any.
#[cfg(target_arch = "wasm32")]
pub fn query_file() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let search = search.trim_start_matches('?');
    for kv in search.split('&') {
        if let Some(v) = kv.strip_prefix("file=") {
            return Some(js_decode(v));
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query_file() -> Option<String> {
    None
}

/// Read the `zoom` query parameter (fit|mid|leaf) — lets the browser bench set a
/// viewport without simulating input.
#[cfg(target_arch = "wasm32")]
pub fn query_zoom() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    for kv in search.trim_start_matches('?').split('&') {
        if let Some(v) = kv.strip_prefix("zoom=") {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query_zoom() -> Option<String> {
    None
}

/// Fetch a URL and deposit the bytes into `slot` when done (wasm).
#[cfg(target_arch = "wasm32")]
pub fn spawn_fetch(url: String, slot: LoadSlot) {
    use wasm_bindgen::JsCast;
    let name = url.rsplit('/').next().unwrap_or("file.gds").to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let win = match web_sys::window() {
            Some(w) => w,
            None => return,
        };
        let fut = wasm_bindgen_futures::JsFuture::from(win.fetch_with_str(&url));
        let resp = match fut.await.and_then(|v| v.dyn_into::<web_sys::Response>()) {
            Ok(r) => r,
            Err(e) => {
                log::error!("fetch {url} failed: {e:?}");
                return;
            }
        };
        let buf = match resp.array_buffer() {
            Ok(p) => match wasm_bindgen_futures::JsFuture::from(p).await {
                Ok(b) => b,
                Err(e) => {
                    log::error!("array_buffer failed: {e:?}");
                    return;
                }
            },
            Err(e) => {
                log::error!("array_buffer() failed: {e:?}");
                return;
            }
        };
        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
        log::info!("fetched {} ({} bytes)", name, bytes.len());
        *slot.borrow_mut() = Some((name, bytes));
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_fetch(_url: String, _slot: LoadSlot) {}

/// Open the browser file picker (wasm) and deposit the chosen file's bytes into
/// `slot`. Uses a transient `<input type=file>` — the browser File API.
#[cfg(target_arch = "wasm32")]
pub fn spawn_picker(slot: LoadSlot) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    let doc = match web_sys::window().and_then(|w| w.document()) {
        Some(d) => d,
        None => return,
    };
    let input = doc
        .create_element("input")
        .unwrap()
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap();
    input.set_type("file");
    input.set_accept(".gds,.gds2,.gdsii");

    let input_clone = input.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let Some(files) = input_clone.files() else { return };
        let Some(file) = files.get(0) else { return };
        let name = file.name();
        let slot = slot.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                Ok(buf) => {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    log::info!("picked {} ({} bytes)", name, bytes.len());
                    *slot.borrow_mut() = Some((name, bytes));
                }
                Err(e) => log::error!("read picked file failed: {e:?}"),
            }
        });
    });
    input.set_onchange(Some(cb.as_ref().unchecked_ref()));
    cb.forget(); // keep the closure alive for the async callback
    input.click();
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_picker(_slot: LoadSlot) {}

/// Minimal percent-decode for the query value (enough for file paths/URLs).
#[cfg(target_arch = "wasm32")]
fn js_decode(s: &str) -> String {
    js_sys::decode_uri_component(s)
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| s.to_string())
}
