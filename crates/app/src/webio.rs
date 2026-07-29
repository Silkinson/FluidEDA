//! Browser I/O glue for runtime file loading: `?file=` query autoload and
//! `fetch`. Native builds get no-op stubs (native uses the `--file` CLI in the
//! bench/shot harnesses and drag-and-drop in the app).
//!
//! Phase 2b STEP 2 adds an **off-main-thread parse path**: when the browser has
//! Web Workers, file bytes are transferred to a worker that runs the streaming
//! parser and transfers a serialized `CellDb` back, which the main thread rebuilds
//! (see `spawn_worker_*` + [`crate::design::reconstruct`]). Browsers without
//! workers, and native, fall back to the synchronous [`LoadSlot`] byte path.

use std::cell::RefCell;
use std::rc::Rc;

/// Async loads (fetch / picker) drop their (name, bytes) here for the app to
/// parse synchronously on the next frame (fallback path).
pub type LoadSlot = Rc<RefCell<Option<(String, Vec<u8>)>>>;

/// The worker path drops a reconstructed `Design` (or an error string) here.
pub type DesignSlot = Rc<RefCell<Option<Result<crate::design::Design, String>>>>;

/// Does this browser support Web Workers (the off-main-thread parse path)?
#[cfg(target_arch = "wasm32")]
pub fn worker_supported() -> bool {
    js_sys::Reflect::get(&js_sys::global(), &"Worker".into())
        .map(|v| !v.is_undefined())
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn worker_supported() -> bool {
    false
}

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

/// Is `?<name>` (or `?<name>=1`) present in the URL? Used to enable the debug
/// stats readout from the browser bench without a UI click.
#[cfg(target_arch = "wasm32")]
pub fn query_flag(name: &str) -> bool {
    let Some(search) = web_sys::window().and_then(|w| w.location().search().ok()) else {
        return false;
    };
    search
        .trim_start_matches('?')
        .split('&')
        .any(|kv| kv == name || kv.strip_prefix(name).is_some_and(|r| r.starts_with('=')))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query_flag(_name: &str) -> bool {
    false
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

/// Save `bytes` as a download named `name` (Phase 3 SLICE 1 — export edits).
/// Wraps the bytes in a Blob and clicks a transient `<a download>`.
#[cfg(target_arch = "wasm32")]
pub fn save_bytes(name: &str, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen::{JsCast, JsValue};
    let win = web_sys::window().ok_or("no window")?;
    let doc = win.document().ok_or("no document")?;

    // Blob from the bytes (one Uint8Array copy into a JS-owned buffer).
    let arr = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    arr.copy_from(bytes);
    let parts = js_sys::Array::of1(&arr.buffer());
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/octet-stream");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts)
        .map_err(|e| format!("blob: {e:?}"))?;

    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|e| format!("url: {e:?}"))?;
    let a = doc
        .create_element("a")
        .map_err(|e| format!("a: {e:?}"))?
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .map_err(|_| "anchor cast".to_string())?;
    a.set_href(&url);
    a.set_download(name);
    let _ = a.style().set_property("display", "none");
    let _ = doc.body().ok_or("no body")?.append_child(&a);
    a.click();
    let _ = doc.body().unwrap().remove_child(&a);
    let _ = web_sys::Url::revoke_object_url(&url);
    let _ = JsValue::NULL;
    Ok(())
}

/// Native: write `bytes` to `name` in the current directory. (The GUI has no file
/// dialog wired; a headless exporter would take an explicit --out path.)
#[cfg(not(target_arch = "wasm32"))]
pub fn save_bytes(name: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(name, bytes).map_err(|e| e.to_string())?;
    log::info!("wrote {} ({} bytes)", name, bytes.len());
    Ok(())
}

/// Minimal percent-decode for the query value (enough for file paths/URLs).
#[cfg(target_arch = "wasm32")]
fn js_decode(s: &str) -> String {
    js_sys::decode_uri_component(s)
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| s.to_string())
}

// --- Phase 2b STEP 2: off-main-thread parse worker --------------------------

/// Native stubs — the worker path is web-only; `worker_supported()` is false on
/// native so these are never actually called, but the call sites must compile.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_worker_fetch(_url: String, _slot: DesignSlot) {}
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_worker_bytes(_name: String, _bytes: Vec<u8>, _slot: DesignSlot) {}

/// URL of the worker driver (served from `assets/` by Trunk's copy-dir).
#[cfg(target_arch = "wasm32")]
const WORKER_URL: &str = "assets/worker/parse_worker.js";

/// Fetch `url` and hand its bytes to the parse worker; the reconstructed `Design`
/// lands in `slot`. The file bytes are transferred (not copied) into the worker.
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_fetch(url: String, slot: DesignSlot) {
    use wasm_bindgen::JsCast;
    let name = url.rsplit('/').next().unwrap_or("file.gds").to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let Some(win) = web_sys::window() else { return };
        let fut = wasm_bindgen_futures::JsFuture::from(win.fetch_with_str(&url));
        let buf = match fut.await.and_then(|v| v.dyn_into::<web_sys::Response>()) {
            Ok(r) => match r.array_buffer().map(wasm_bindgen_futures::JsFuture::from) {
                Ok(p) => match p.await {
                    Ok(b) => b.dyn_into::<js_sys::ArrayBuffer>().ok(),
                    Err(e) => {
                        fail(&slot, format!("fetch body failed: {e:?}"));
                        return;
                    }
                },
                Err(e) => {
                    fail(&slot, format!("array_buffer() failed: {e:?}"));
                    return;
                }
            },
            Err(e) => {
                fail(&slot, format!("fetch {url} failed: {e:?}"));
                return;
            }
        };
        match buf {
            Some(b) => run_worker(name, b, slot),
            None => fail(&slot, "fetch produced no ArrayBuffer".into()),
        }
    });
}

/// Hand already-in-wasm `bytes` (drag-drop / picker) to the parse worker. Copies
/// them once into a JS `ArrayBuffer` to transfer (drag-drop bytes live in wasm
/// memory, which can't be transferred directly).
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_bytes(name: String, bytes: Vec<u8>, slot: DesignSlot) {
    let arr = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    arr.copy_from(&bytes);
    run_worker(name, arr.buffer(), slot);
}

/// Spawn a one-shot worker to parse `buf`, reconstruct the `Design` on the main
/// thread from its reply, then terminate the worker (reclaiming its parse memory).
#[cfg(target_arch = "wasm32")]
fn run_worker(name: String, buf: js_sys::ArrayBuffer, slot: DesignSlot) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};

    let file_bytes = buf.byte_length() as usize;
    let worker = match web_sys::Worker::new(WORKER_URL) {
        Ok(w) => w,
        Err(e) => {
            fail(&slot, format!("worker spawn failed: {e:?}"));
            return;
        }
    };

    let worker_for_msg = worker.clone();
    let slot_for_msg = slot.clone();
    let name_for_msg = name.clone();
    let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        let data = e.data();
        let kind = get_str(&data, "type").unwrap_or_default();
        if kind == "error" {
            fail(&slot_for_msg, get_str(&data, "message").unwrap_or_else(|| "worker error".into()));
            worker_for_msg.terminate();
            return;
        }
        // type == "done": rebuild the Design from the transferred columns.
        let msg = crate::design::WorkerMsg {
            name: name_for_msg.clone(),
            file_bytes,
            coords: get_i32_vec(&data, "coords"),
            starts: get_u32_vec(&data, "starts"),
            layers: get_u32_vec(&data, "layers"),
            meta: get_u8_vec(&data, "meta"),
            bbox: get_bbox(&data),
            total_shapes: get_f64(&data, "total_shapes") as u64,
            distinct_polys: get_f64(&data, "distinct_polys") as usize,
            cell_defs: get_f64(&data, "cell_defs") as usize,
            layers_packed: get_u32_vec(&data, "layer_set"),
            parse_ms: get_f64(&data, "parse_ms"),
            worker_heap_bytes: get_f64(&data, "worker_heap_bytes") as usize,
        };
        // Combined-peak proof: sample the main heap while the worker is still
        // alive (its parse memory not yet reclaimed), then reconstruct + terminate.
        match crate::design::reconstruct(msg) {
            Ok(design) => *slot_for_msg.borrow_mut() = Some(Ok(design)),
            Err(err) => *slot_for_msg.borrow_mut() = Some(Err(format!("reconstruct failed: {err}"))),
        }
        worker_for_msg.terminate();
    });
    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget(); // keep alive for the one message; the worker is then dropped

    // Post the bytes with a transfer list (move, not structured-clone copy).
    let payload = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&payload, &"type".into(), &"parse".into());
    let _ = js_sys::Reflect::set(&payload, &"bytes".into(), &buf);
    let transfer = js_sys::Array::of1(&buf);
    if let Err(e) = worker.post_message_with_transfer(&payload, &transfer) {
        fail(&slot, format!("worker postMessage failed: {e:?}"));
        worker.terminate();
    }
    let _ = JsValue::NULL; // silence unused import in some cfgs
}

#[cfg(target_arch = "wasm32")]
fn fail(slot: &DesignSlot, msg: String) {
    log::error!("{msg}");
    *slot.borrow_mut() = Some(Err(msg));
}

// --- message field extraction (all wasm-only) -------------------------------

#[cfg(target_arch = "wasm32")]
fn get_val(data: &wasm_bindgen::JsValue, key: &str) -> wasm_bindgen::JsValue {
    js_sys::Reflect::get(data, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

#[cfg(target_arch = "wasm32")]
fn get_str(data: &wasm_bindgen::JsValue, key: &str) -> Option<String> {
    get_val(data, key).as_string()
}

#[cfg(target_arch = "wasm32")]
fn get_f64(data: &wasm_bindgen::JsValue, key: &str) -> f64 {
    get_val(data, key).as_f64().unwrap_or(0.0)
}

#[cfg(target_arch = "wasm32")]
fn get_bbox(data: &wasm_bindgen::JsValue) -> [f64; 4] {
    let arr = js_sys::Array::from(&get_val(data, "world_bbox"));
    let g = |i: u32| arr.get(i).as_f64().unwrap_or(0.0);
    [g(0), g(1), g(2), g(3)]
}

/// Wrap a transferred `ArrayBuffer` as a typed array and copy it into an aligned
/// wasm `Vec` (one memcpy — the main-thread reconstruction cost).
#[cfg(target_arch = "wasm32")]
fn get_i32_vec(data: &wasm_bindgen::JsValue, key: &str) -> Vec<i32> {
    js_sys::Int32Array::new(&get_val(data, key)).to_vec()
}

#[cfg(target_arch = "wasm32")]
fn get_u32_vec(data: &wasm_bindgen::JsValue, key: &str) -> Vec<u32> {
    js_sys::Uint32Array::new(&get_val(data, key)).to_vec()
}

#[cfg(target_arch = "wasm32")]
fn get_u8_vec(data: &wasm_bindgen::JsValue, key: &str) -> Vec<u8> {
    js_sys::Uint8Array::new(&get_val(data, key)).to_vec()
}
