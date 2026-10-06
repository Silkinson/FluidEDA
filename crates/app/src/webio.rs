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

// --- Phase 7.1a: waking an idle UI ----------------------------------------------

thread_local! {
    /// The UI to wake when an async result lands. The app repaints only while
    /// work is pending, so a reply arriving while it idles must ask for a frame.
    static WAKER: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// Remember the UI context [`wake`] repaints.
pub fn set_waker(ctx: &egui::Context) {
    WAKER.with(|w| *w.borrow_mut() = Some(ctx.clone()));
}

/// Ask for a frame: an async result (a worker reply, a fetch) just landed.
pub fn wake() {
    WAKER.with(|w| {
        if let Some(ctx) = w.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// The test harness drives the app by writing `window.__fluideda_cmd`; make
/// that write wake the UI too, or an idle app would take each command up to
/// a heartbeat later. (A property with a setter: the harness is unchanged.)
#[cfg(target_arch = "wasm32")]
pub fn wake_on_test_commands() {
    use wasm_bindgen::JsCast;
    let wake_fn = wasm_bindgen::closure::Closure::<dyn Fn()>::new(wake);
    let define = js_sys::Function::new_with_args(
        "wake",
        "let v = window.__fluideda_cmd || ''; \
         Object.defineProperty(window, '__fluideda_cmd', { configurable: true, \
           get() { return v; }, set(x) { v = x; if (x) wake(); } });",
    );
    let _ = define.call1(&wasm_bindgen::JsValue::NULL, wake_fn.as_ref().unchecked_ref());
    wake_fn.forget();
}

#[cfg(not(target_arch = "wasm32"))]
pub fn wake_on_test_commands() {}

/// Async loads (fetch / picker) drop their (name, bytes) here for the app to
/// parse synchronously on the next frame (fallback path).
pub type LoadSlot = Rc<RefCell<Option<(String, Vec<u8>)>>>;

/// Local files (drag-drop / picker) on the worker path: the name and a
/// disk-backed handle, handed to the parse worker as-is (Phase 6).
pub type SourceSlot = Rc<RefCell<Option<(String, crate::design::DesignSource)>>>;

/// The worker path drops the new design's progressive [`Reconstruction`](crate::design::Reconstruction)
/// (or an error string) here; the app advances it within the frame budget —
/// skeleton first (installed as soon as it is complete), then geometry.
pub type DesignSlot = Rc<RefCell<Option<Result<crate::design::Reconstruction, String>>>>;

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

/// Read query parameter `name`, if present.
#[cfg(target_arch = "wasm32")]
pub fn query_param(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let prefix = format!("{name}=");
    search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix(prefix.as_str())).map(js_decode)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query_param(_name: &str) -> Option<String> {
    None
}

/// Read the `room` query parameter (Phase 6: the room to join once the
/// design is in), if any.
#[cfg(target_arch = "wasm32")]
pub fn query_room() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix("room=")).map(js_decode)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn query_room() -> Option<String> {
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

/// Read a drag-and-dropped file into `slot` (egui 0.36 hands out a file handle):
/// asynchronously on the web, synchronously natively. A read error is reported
/// through `status`. On the web worker path the browser `File` goes to
/// `sources` instead — the worker reads it, and it stays the design's source.
#[cfg(target_arch = "wasm32")]
pub fn read_dropped(name: String, file: egui::DroppedFileHandle, slot: LoadSlot, sources: SourceSlot, _status: &mut String) {
    if let (true, Some(f)) = (worker_supported(), file.web_file()) {
        *sources.borrow_mut() = Some((name, crate::design::DesignSource::File(f.clone())));
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        match file.bytes_async().await {
            Ok(bytes) => {
                *slot.borrow_mut() = Some((name, bytes));
                wake();
            }
            Err(e) => log::error!("read dropped {name} failed: {e}"),
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_dropped(name: String, file: egui::DroppedFileHandle, slot: LoadSlot, _sources: SourceSlot, status: &mut String) {
    match file.bytes() {
        Ok(bytes) => *slot.borrow_mut() = Some((name, bytes)),
        Err(e) => *status = format!("read failed: {e}"),
    }
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
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_fetch(_url: String, _slot: LoadSlot) {}

/// Open the browser file picker (wasm) and deposit the chosen file into
/// `sources` (worker path: the handle) or its bytes into `slot`. Uses a
/// transient `<input type=file>` — the browser File API.
#[cfg(target_arch = "wasm32")]
pub fn spawn_picker(slot: LoadSlot, sources: SourceSlot) {
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
    input.set_accept(".gds,.gds2,.gdsii,.oas,.oasis");

    let input_clone = input.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let Some(files) = input_clone.files() else { return };
        let Some(file) = files.get(0) else { return };
        let name = file.name();
        if worker_supported() {
            *sources.borrow_mut() = Some((name, crate::design::DesignSource::File(file)));
            wake();
            return;
        }
        let slot = slot.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                Ok(buf) => {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    log::info!("picked {} ({} bytes)", name, bytes.len());
                    *slot.borrow_mut() = Some((name, bytes));
                    wake();
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
pub fn spawn_picker(_slot: LoadSlot, _sources: SourceSlot) {}

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
pub fn spawn_worker_fetch(_url: String, _name: Option<String>, _slot: DesignSlot) {}
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_worker_source(_name: String, _source: crate::design::DesignSource, _slot: DesignSlot) {}
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_worker_bytes(_name: String, _bytes: Vec<u8>, _slot: DesignSlot) {}

/// URL of the worker driver (served from `assets/` by Trunk's copy-dir).
#[cfg(target_arch = "wasm32")]
const WORKER_URL: &str = "assets/worker/parse_worker.js";

/// Parse the file at `url` in the worker — which **fetches it itself** (Phase
/// 5c), so the body never materialises on the main thread. The pending
/// reconstruction lands in `slot`. `file_bytes` for the metrics comes from the
/// worker's reply. `name` defaults to the URL's last segment (a blob URL
/// names a hash, so a room load passes the design's file name).
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_fetch(url: String, name: Option<String>, slot: DesignSlot) {
    let name = name.unwrap_or_else(|| url.rsplit('/').next().unwrap_or("file.gds").to_string());
    // Absolute URL: a worker resolves relative URLs against its own script.
    let abs = web_sys::window()
        .and_then(|w| w.location().href().ok())
        .and_then(|base| web_sys::Url::new_with_base(&url, &base).ok())
        .map(|u| u.href())
        .unwrap_or(url);
    let payload = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&payload, &"type".into(), &"fetch_parse".into());
    let _ = js_sys::Reflect::set(&payload, &"url".into(), &abs.clone().into());
    run_worker(name, payload, js_sys::Array::new(), slot, crate::design::DesignSource::Url(abs));
}

/// Parse a design from its source in the worker: a URL is fetched there, a
/// local `File` handle is posted as-is and read there (Phase 6 — the main
/// thread never holds the bytes). Embedded/unknown sources have no worker path.
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_source(name: String, source: crate::design::DesignSource, slot: DesignSlot) {
    use crate::design::DesignSource;
    match source {
        DesignSource::Url(url) => spawn_worker_fetch(url, Some(name), slot),
        DesignSource::File(file) => {
            let payload = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&payload, &"type".into(), &"file_parse".into());
            let _ = js_sys::Reflect::set(&payload, &"file".into(), &file);
            run_worker(name, payload, js_sys::Array::new(), slot, DesignSource::File(file));
        }
        DesignSource::Embedded | DesignSource::Unknown => fail(&slot, format!("{name}: no worker source")),
    }
}

/// Hand already-in-wasm `bytes` (drag-drop / picker) to the parse worker. Copies
/// them once into a JS `ArrayBuffer` to transfer (drag-drop bytes live in wasm
/// memory, which can't be transferred directly).
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_bytes(name: String, bytes: Vec<u8>, slot: DesignSlot) {
    let arr = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    arr.copy_from(&bytes);
    let buf = arr.buffer();
    // Post the bytes with a transfer list (move, not structured-clone copy).
    let payload = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&payload, &"type".into(), &"parse".into());
    let _ = js_sys::Reflect::set(&payload, &"bytes".into(), &buf);
    run_worker(name, payload, js_sys::Array::of1(&buf), slot, crate::design::DesignSource::Unknown);
}

/// Spawn a one-shot parse worker, post it `payload` (with `transfer`), turn its
/// reply into a pending reconstruction in `slot`, then terminate the worker
/// (reclaiming its parse memory).
#[cfg(target_arch = "wasm32")]
fn run_worker(
    name: String,
    payload: js_sys::Object,
    transfer: js_sys::Array,
    slot: DesignSlot,
    source: crate::design::DesignSource,
) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};

    // `?norects`: keep every shape a ring (Phase 7.7 — the memory A/B knob).
    if query_flag("norects") {
        let _ = js_sys::Reflect::set(&payload, &"norects".into(), &JsValue::TRUE);
    }
    // `?nohash`: skip the design digest (Phase 6 — the load-time A/B knob).
    if query_flag("nohash") {
        let _ = js_sys::Reflect::set(&payload, &"nohash".into(), &JsValue::TRUE);
    }

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
    let hash_slot: crate::design::HashSlot = Rc::new(RefCell::new(None));
    let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        let data = e.data();
        let kind = get_str(&data, "type").unwrap_or_default();
        if kind == "error" {
            fail(&slot_for_msg, get_str(&data, "message").unwrap_or_else(|| "worker error".into()));
            worker_for_msg.terminate();
            return;
        }
        // Phase 6: the design's hash follows the parse reply; the worker is
        // done after it (it also closes itself).
        if kind == "hash" {
            *hash_slot.borrow_mut() = Some((get_str(&data, "hash").unwrap_or_default(), get_f64(&data, "hash_ms")));
            worker_for_msg.terminate();
            wake();
            return;
        }
        // type == "done": rebuild the Design from the transferred columns. The
        // three big columns stay in their transferred JS buffers; the rebuild
        // copies each cell's slice from there exactly once (Phase 5c).
        let columns = JsColumns {
            coords: js_sys::Int32Array::new(&get_val(&data, "coords")),
            starts: js_sys::Uint32Array::new(&get_val(&data, "starts")),
            layers: js_sys::ArrayBuffer::from(get_val(&data, "layers")),
            kinds: js_sys::Uint8Array::new(&get_val(&data, "kinds")),
        };
        let msg = crate::design::WorkerMsg {
            name: name_for_msg.clone(),
            file_bytes: get_f64(&data, "file_bytes") as usize,
            columns: Box::new(columns),
            meta: get_u8_vec(&data, "meta"),
            bbox: get_bbox(&data),
            total_shapes: get_f64(&data, "total_shapes") as u64,
            distinct_polys: get_f64(&data, "distinct_polys") as usize,
            cell_defs: get_f64(&data, "cell_defs") as usize,
            layers_packed: get_u32_vec(&data, "layer_set"),
            parse_ms: get_f64(&data, "parse_ms"),
            worker_heap_bytes: get_f64(&data, "worker_heap_bytes") as usize,
            hash: hash_slot.clone(),
            source: source.clone(),
            stream: None,
        };
        // Hand the pending rebuild to the app (it copies across frames). The
        // worker lives on only to hash the file (Phase 6, a few hundred ms
        // for the largest chip) and is terminated when the hash arrives — its
        // ~0.8 GB parse heap is reclaimed early in the main thread's copying.
        match crate::design::Reconstruction::new(msg) {
            Ok(r) => *slot_for_msg.borrow_mut() = Some(Ok(r)),
            Err(err) => {
                *slot_for_msg.borrow_mut() = Some(Err(format!("reconstruct failed: {err}")));
                worker_for_msg.terminate();
            }
        }
        wake();
    });
    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget(); // keep alive for the worker's two messages (done, hash)

    if let Err(e) = worker.post_message_with_transfer(&payload, &transfer) {
        fail(&slot, format!("worker postMessage failed: {e:?}"));
        worker.terminate();
    }
    let _ = JsValue::NULL; // silence unused import in some cfgs
}

/// The parse worker's three geometry columns, still in the `ArrayBuffer`s the
/// worker transferred. [`fluideda_core::ColumnSource`] over them: each cell's
/// slice is copied once, directly into that cell's `Vec` (`subarray().to_vec()`
/// allocates uninitialised and memcpys — no zero-fill, no whole-column copy).
#[cfg(target_arch = "wasm32")]
struct JsColumns {
    coords: js_sys::Int32Array,
    starts: js_sys::Uint32Array,
    /// Packed layers (u32 each = the `Layer` byte layout); read as u16 pairs.
    layers: js_sys::ArrayBuffer,
    /// One shape kind per shape (Phase 7.7).
    kinds: js_sys::Uint8Array,
}

#[cfg(target_arch = "wasm32")]
fn check_range(off: usize, len: usize, total: u32, what: &str) -> anyhow::Result<(u32, u32)> {
    match off.checked_add(len).filter(|&e| e <= total as usize) {
        Some(e) => Ok((off as u32, e as u32)),
        None => anyhow::bail!("CellDb blob {what} column underrun"),
    }
}

#[cfg(target_arch = "wasm32")]
impl fluideda_core::ColumnSource for JsColumns {
    fn coords_into(&mut self, off: usize, len: usize, out: &mut Vec<i32>) -> anyhow::Result<()> {
        let (a, b) = check_range(off, len, self.coords.length(), "coords")?;
        out.reserve(len);
        let at = out.len();
        // SAFETY: `reserve` guarantees room for `len` more i32s past `at`; the
        // subarray holds exactly `len` elements, all written by the copy.
        unsafe {
            self.coords.subarray(a, b).raw_copy_to_ptr(out.as_mut_ptr().add(at));
            out.set_len(at + len);
        }
        Ok(())
    }
    fn starts(&mut self, off: usize, len: usize) -> anyhow::Result<Vec<u32>> {
        let (a, b) = check_range(off, len, self.starts.length(), "starts")?;
        Ok(self.starts.subarray(a, b).to_vec())
    }
    fn layers(&mut self, off: usize, len: usize) -> anyhow::Result<Vec<fluideda_core::Layer>> {
        let total = self.layers.byte_length() / 4;
        let (a, _) = check_range(off, len, total, "layers")?;
        // `Layer` is `repr(C) { u16, u16 }` — exactly one packed u32's bytes on
        // a little-endian target — so view the buffer as u16 pairs and copy
        // straight into the Vec's storage.
        let view = js_sys::Uint16Array::new_with_byte_offset_and_length(&self.layers, a * 4, (len * 2) as u32);
        let mut v: Vec<fluideda_core::Layer> = Vec::with_capacity(len);
        // SAFETY: `v` has capacity for `len` Layers = `2 * len` u16s, which is
        // exactly `view`'s length; `raw_copy_to_ptr` writes all of them, and
        // every bit pattern is a valid `Layer` (Pod).
        unsafe {
            view.raw_copy_to_ptr(v.as_mut_ptr() as *mut u16);
            v.set_len(len);
        }
        Ok(v)
    }
    fn kinds(&mut self, off: usize, len: usize) -> anyhow::Result<Vec<u8>> {
        let (a, b) = check_range(off, len, self.kinds.length(), "kinds")?;
        Ok(self.kinds.subarray(a, b).to_vec())
    }
}

#[cfg(target_arch = "wasm32")]
fn fail(slot: &DesignSlot, msg: String) {
    log::error!("{msg}");
    *slot.borrow_mut() = Some(Err(msg));
    wake();
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

// --- Phase 4 STEP 2: persistent tessellation worker -------------------------
//
// A long-lived worker (separate from the one-shot parse worker) that tessellates
// chunks off the main thread, so fit-view no longer freezes on the giant flat
// cells. Each job ships the chunk's raw Pod columns in and transfers finished
// vertex/index buffers back; the reply is reconstructed here into per-layer
// meshes the renderer uploads. Unlike the parse worker it is NOT terminated per
// job — the app keeps one per design and respawns it on load / re-partition.

/// A tessellated chunk from the worker, reconstructed into per-layer meshes on
/// the main thread and ready for `HierRenderer::upload_chunk`. `gen` stamps the
/// design/partition epoch it was requested under, so the app drops stale replies.
pub struct TessReply {
    pub cell: usize,
    pub chunk: u32,
    pub gen: u32,
    pub meshes: Vec<fluideda_core::LayerMesh>,
    /// Streaming (Phase 6 SLICE 3): the chunk record's size and fetch time.
    pub fetch_bytes: usize,
    pub fetch_ms: f64,
    /// The job failed (logged): nothing to upload; the app may retry it.
    pub failed: bool,
}

/// Completed tessellation jobs land here for the app to drain each frame (like
/// [`DesignSlot`], but a queue — several chunks can finish between frames).
pub type TessSlot = Rc<RefCell<Vec<TessReply>>>;

#[cfg(target_arch = "wasm32")]
const TESS_WORKER_URL: &str = "assets/worker/tess_worker.js";

/// Handle to the persistent tessellation worker plus its live `onmessage`
/// closure. Dropping it terminates the worker (reclaiming its wasm memory and
/// discarding any in-flight jobs) — how the app invalidates on a new load.
#[cfg(target_arch = "wasm32")]
pub struct TessWorker {
    worker: web_sys::Worker,
    _onmessage: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
}

#[cfg(target_arch = "wasm32")]
impl TessWorker {
    /// Spawn the tessellation worker; completed jobs push into `slot`.
    pub fn new(slot: TessSlot) -> Result<Self, String> {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;
        let worker =
            web_sys::Worker::new(TESS_WORKER_URL).map_err(|e| format!("tess worker spawn: {e:?}"))?;
        let onmessage =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let data = e.data();
                match get_str(&data, "type").as_deref() {
                    Some("tess_done") => slot.borrow_mut().push(reconstruct_tess(&data)),
                    Some("tess_error") => {
                        log::error!("tess worker: {}", get_str(&data, "message").unwrap_or_default());
                        // Reported, so the chunk is not left pending forever.
                        slot.borrow_mut().push(TessReply {
                            cell: get_f64(&data, "cell") as usize,
                            chunk: get_f64(&data, "chunk") as u32,
                            gen: get_f64(&data, "gen") as u32,
                            meshes: Vec::new(),
                            fetch_bytes: 0,
                            fetch_ms: 0.0,
                            failed: true,
                        });
                    }
                    _ => {}
                }
                wake();
            });
        worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        Ok(Self { worker, _onmessage: onmessage })
    }

    /// Dispatch a chunk-tessellation job. The three columns are copied once into
    /// JS buffers and **transferred** (moved) to the worker; `cell`/`chunk`/`gen`
    /// ride along so the reply can be keyed and staleness-checked.
    #[allow(clippy::too_many_arguments)]
    pub fn post(&self, cell: usize, chunk: u32, gen: u32, coords: &[i32], starts: &[u32], layers: &[u32], kinds: &[u8]) {
        use wasm_bindgen::JsValue;
        let carr = js_sys::Int32Array::from(coords);
        let sarr = js_sys::Uint32Array::from(starts);
        let larr = js_sys::Uint32Array::from(layers);
        let karr = js_sys::Uint8Array::from(kinds);
        let payload = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&payload, &k.into(), v);
        };
        set("type", &"tess".into());
        set("cell", &JsValue::from_f64(cell as f64));
        set("chunk", &JsValue::from_f64(chunk as f64));
        set("gen", &JsValue::from_f64(gen as f64));
        set("coords", &carr.buffer());
        set("starts", &sarr.buffer());
        set("layers", &larr.buffer());
        set("kinds", &karr.buffer());
        let transfer = js_sys::Array::of4(&carr.buffer(), &sarr.buffer(), &larr.buffer(), &karr.buffer());
        if let Err(e) = self.worker.post_message_with_transfer(&payload, &transfer) {
            log::error!("tess post failed: {e:?}");
        }
    }

    /// Streaming (Phase 6 SLICE 3): have the worker fetch the chunk's record —
    /// bytes `start..end` of the pack's geometry blob at `url` — and
    /// tessellate it. The geometry never touches the main thread.
    pub fn post_fetch(&self, cell: usize, chunk: u32, gen: u32, url: &str, start: u64, end: u64) {
        use wasm_bindgen::JsValue;
        let payload = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&payload, &k.into(), v);
        };
        set("type", &"tess_fetch".into());
        set("cell", &JsValue::from_f64(cell as f64));
        set("chunk", &JsValue::from_f64(chunk as f64));
        set("gen", &JsValue::from_f64(gen as f64));
        set("url", &url.into());
        set("start", &JsValue::from_f64(start as f64));
        set("end", &JsValue::from_f64(end as f64));
        if let Err(e) = self.worker.post_message(&payload) {
            log::error!("tess post failed: {e:?}");
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for TessWorker {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

/// Rebuild the per-layer meshes from a `tess_done` message's transferred buffers:
/// split the concatenated vertex/index buffers by the per-mesh side tables.
#[cfg(target_arch = "wasm32")]
fn reconstruct_tess(data: &wasm_bindgen::JsValue) -> TessReply {
    use fluideda_core::{Layer, LayerMesh, Vertex};
    let cell = get_f64(data, "cell") as usize;
    let chunk = get_f64(data, "chunk") as u32;
    let gen = get_f64(data, "gen") as u32;
    let mesh_layers = get_u32_vec(data, "mesh_layers");
    let vert_counts = get_u32_vec(data, "vert_counts");
    let index_counts = get_u32_vec(data, "index_counts");
    let verts = get_f32_vec(data, "verts");
    let indices = get_u32_vec(data, "indices");

    let mut meshes = Vec::with_capacity(mesh_layers.len());
    let (mut vo, mut io) = (0usize, 0usize);
    for i in 0..mesh_layers.len() {
        let vc = vert_counts[i] as usize;
        let ic = index_counts[i] as usize;
        let packed = mesh_layers[i];
        let layer = Layer::new((packed & 0xffff) as u16, (packed >> 16) as u16);
        let vertices: Vec<Vertex> = verts[vo..vo + vc]
            .chunks_exact(2)
            .map(|c| Vertex { pos: [c[0], c[1]] })
            .collect();
        let mesh_indices = indices[io..io + ic].to_vec();
        meshes.push(LayerMesh { layer, vertices, indices: mesh_indices });
        vo += vc;
        io += ic;
    }
    TessReply {
        cell,
        chunk,
        gen,
        meshes,
        fetch_bytes: get_f64(data, "fetch_bytes") as usize,
        fetch_ms: get_f64(data, "fetch_ms"),
        failed: false,
    }
}

#[cfg(target_arch = "wasm32")]
fn get_f32_vec(data: &wasm_bindgen::JsValue, key: &str) -> Vec<f32> {
    js_sys::Float32Array::new(&get_val(data, key)).to_vec()
}

/// Native stub — the tessellation worker is web-only (`worker_supported()` is
/// false natively, so `TessWorker::new` is never actually called), but the app's
/// call sites must compile.
#[cfg(not(target_arch = "wasm32"))]
pub struct TessWorker;

#[cfg(not(target_arch = "wasm32"))]
impl TessWorker {
    pub fn new(_slot: TessSlot) -> Result<Self, String> {
        Err("tessellation worker is web-only".into())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn post(&self, _cell: usize, _chunk: u32, _gen: u32, _coords: &[i32], _starts: &[u32], _layers: &[u32], _kinds: &[u8]) {}
    pub fn post_fetch(&self, _cell: usize, _chunk: u32, _gen: u32, _url: &str, _start: u64, _end: u64) {}
}

// --- Phase 5b SLICE 3: DRC worker pool ---------------------------------------
//
// A pool of persistent, stateless DRC workers (assets/worker/drc_worker.js over
// the shared core-only wasm). The app's `DrcDriver` produces tile jobs; each is
// posted round-robin with its Pod columns transferred in, and the reply — flat
// violation records — lands in a `DrcSlot` for the app to feed back to the
// driver next frame.

/// A finished DRC tile from the pool.
pub struct DrcReply {
    pub epoch: u32,
    pub cell: usize,
    pub tile: fluideda_core::TileKey,
    pub gen: u32,
    pub violations: Vec<fluideda_core::Violation>,
    /// The worker threw (logged); the tile is accepted as clean so the pass
    /// completes, and the panel counts the errors.
    pub error: bool,
}

/// Completed DRC jobs, drained each frame by the app.
pub type DrcSlot = Rc<RefCell<Vec<DrcReply>>>;

/// Logical cores the browser reports (1 natively — the pool is web-only).
#[cfg(target_arch = "wasm32")]
pub fn hardware_concurrency() -> usize {
    web_sys::window().map(|w| w.navigator().hardware_concurrency() as usize).unwrap_or(1).max(1)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn hardware_concurrency() -> usize {
    1
}

#[cfg(target_arch = "wasm32")]
const DRC_WORKER_URL: &str = "assets/worker/drc_worker.js";

/// The DRC worker pool. Dropping it terminates every worker (in-flight jobs are
/// discarded; the app re-queues them through the driver).
#[cfg(target_arch = "wasm32")]
pub struct DrcPool {
    workers: Vec<web_sys::Worker>,
    _onmessage: Vec<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>>,
    next: std::cell::Cell<usize>,
}

#[cfg(target_arch = "wasm32")]
impl DrcPool {
    /// Spawn `n` DRC workers; replies push into `slot`.
    pub fn new(n: usize, slot: DrcSlot) -> Result<Self, String> {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;
        let mut workers = Vec::with_capacity(n);
        let mut handlers = Vec::with_capacity(n);
        for _ in 0..n.max(1) {
            let w = web_sys::Worker::new(DRC_WORKER_URL).map_err(|e| format!("drc worker spawn: {e:?}"))?;
            let slot = slot.clone();
            let h = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let data = e.data();
                let kind = get_str(&data, "type");
                let error = match kind.as_deref() {
                    Some("drc_done") => false,
                    Some("drc_error") => {
                        log::error!("drc worker: {}", get_str(&data, "message").unwrap_or_default());
                        true
                    }
                    _ => return,
                };
                let tile = fluideda_core::TileKey::new(
                    get_f64(&data, "tx") as i32,
                    get_f64(&data, "ty") as i32,
                    get_f64(&data, "sx") as u16,
                    get_f64(&data, "sy") as u16,
                );
                let violations = if error { Vec::new() } else { decode_violations(&get_i32_vec(&data, "viol")) };
                slot.borrow_mut().push(DrcReply {
                    epoch: get_f64(&data, "epoch") as u32,
                    cell: get_f64(&data, "cell") as usize,
                    tile,
                    gen: get_f64(&data, "gen") as u32,
                    violations,
                    error,
                });
                wake();
            });
            w.set_onmessage(Some(h.as_ref().unchecked_ref()));
            workers.push(w);
            handlers.push(h);
        }
        Ok(Self { workers, _onmessage: handlers, next: std::cell::Cell::new(0) })
    }

    pub fn size(&self) -> usize {
        self.workers.len()
    }

    /// Post one tile job to the next worker. The three columns are copied once
    /// into JS buffers and transferred; `rules` is the packed deck.
    pub fn post(&self, epoch: u32, job: &fluideda_core::TileJob, rules: &[i32]) {
        use wasm_bindgen::JsValue;
        let core = js_sys::Int32Array::from(
            &[job.core.min.x, job.core.min.y, job.core.max.x, job.core.max.y][..],
        );
        let rules = js_sys::Int32Array::from(rules);
        let carr = js_sys::Int32Array::from(&job.input.coords[..]);
        let sarr = js_sys::Uint32Array::from(&job.input.starts[..]);
        let larr = js_sys::Uint32Array::from(&job.input.layers[..]);
        let payload = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&payload, &k.into(), v);
        };
        set("type", &"drc".into());
        set("epoch", &JsValue::from_f64(epoch as f64));
        set("cell", &JsValue::from_f64(job.cell as f64));
        set("tx", &JsValue::from_f64(job.tile.tx as f64));
        set("ty", &JsValue::from_f64(job.tile.ty as f64));
        set("sx", &JsValue::from_f64(job.tile.sx as f64));
        set("sy", &JsValue::from_f64(job.tile.sy as f64));
        set("gen", &JsValue::from_f64(job.gen as f64));
        set("core", &core);
        set("rules", &rules);
        set("coords", &carr.buffer());
        set("starts", &sarr.buffer());
        set("layers", &larr.buffer());
        let transfer = js_sys::Array::of3(&carr.buffer(), &sarr.buffer(), &larr.buffer());
        let i = self.next.get();
        self.next.set((i + 1) % self.workers.len());
        if let Err(e) = self.workers[i].post_message_with_transfer(&payload, &transfer) {
            log::error!("drc post failed: {e:?}");
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for DrcPool {
    fn drop(&mut self) {
        for w in &self.workers {
            w.terminate();
        }
    }
}

/// Decode the worker's flat violation records (see `fluideda_worker::drc_tile`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn decode_violations(v: &[i32]) -> Vec<fluideda_core::Violation> {
    v.chunks_exact(6)
        .map(|r| fluideda_core::Violation {
            kind: if r[0] == 0 { fluideda_core::DrcKind::Width } else { fluideda_core::DrcKind::Spacing },
            layer: fluideda_core::drc::unpack_layer(r[1] as u32),
            bbox: fluideda_core::Bbox {
                min: fluideda_core::Point::new(r[2], r[3]),
                max: fluideda_core::Point::new(r[4], r[5]),
            },
        })
        .collect()
}

/// Native stub — the DRC pool is web-only (the app runs DRC inline natively).
#[cfg(not(target_arch = "wasm32"))]
pub struct DrcPool;

#[cfg(not(target_arch = "wasm32"))]
impl DrcPool {
    pub fn new(_n: usize, _slot: DrcSlot) -> Result<Self, String> {
        Err("DRC worker pool is web-only".into())
    }
    pub fn size(&self) -> usize {
        0
    }
    pub fn post(&self, _epoch: u32, _job: &fluideda_core::TileJob, _rules: &[i32]) {}
}

// --- Phase 6: rooms and the blob store --------------------------------------
//
// The server (reached through Trunk's proxy, same origin as the page) binds
// each room to one design and keeps designs as immutable, content-addressed
// blobs. Two small async calls: look a room up (to open `?room=` links without
// a file), and make sure the server has our design (so later joiners can load
// it from `/blobs/<sha256>`).

/// What `GET /rooms/<name>` says about a room's design.
#[derive(Debug, Clone, Default)]
pub struct RoomInfo {
    /// (sha256, file name, shape-id scheme) — `None` for an unbound room.
    pub design: Option<(String, String, u32)>,
}

pub type RoomSlot = Rc<RefCell<Option<Result<RoomInfo, String>>>>;

/// A server-side export (Phase 7.4 SLICE C), as the app follows it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the export is web-only
pub enum ExportState {
    Idle,
    Running,
    /// Written into the blob store as `hash` and handed to the browser's
    /// download as `name`.
    Done { hash: String, bytes: u64, ms: f64, name: String },
    Failed(String),
}

pub type ExportSlot = Rc<RefCell<ExportState>>;

/// Export design `design` with the edits in document state `state` on the
/// server (`POST /export/<design>`), then download the result straight from
/// the blob store as `name` — the bytes never pass through this page.
#[cfg(target_arch = "wasm32")]
pub fn export_on_server(design: String, oasis: bool, state: Vec<u8>, name: String, slot: ExportSlot) {
    *slot.borrow_mut() = ExportState::Running;
    wasm_bindgen_futures::spawn_local(async move {
        let result = async {
            let format = if oasis { "oas" } else { "gds" };
            let url = format!("/export/{design}?format={format}&ids={}", fluideda_core::SHAPE_ID_SCHEME);
            let body: wasm_bindgen::JsValue = js_sys::Uint8Array::from(state.as_slice()).into();
            let resp = http(&url, "POST", Some(&body)).await?;
            if !resp.ok() {
                let text = wasm_bindgen_futures::JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
                    .await
                    .ok()
                    .and_then(|t| t.as_string())
                    .unwrap_or_default();
                return Err(format!("POST {url}: HTTP {} {text}", resp.status()));
            }
            let m = wasm_bindgen_futures::JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
                .await
                .map_err(|e| format!("POST {url}: {e:?}"))?;
            let hash = get_str(&m, "hash").ok_or("export: no hash")?;
            download_url(&format!("/blobs/{hash}"), &name)?;
            Ok::<_, String>(ExportState::Done { hash, bytes: get_f64(&m, "bytes") as u64, ms: get_f64(&m, "ms"), name })
        }
        .await;
        *slot.borrow_mut() = result.unwrap_or_else(ExportState::Failed);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn export_on_server(_design: String, _oasis: bool, _state: Vec<u8>, _name: String, slot: ExportSlot) {
    *slot.borrow_mut() = ExportState::Failed("server export is web-only".into());
}

/// Hand `url` (same origin) to the browser's download as `name`.
#[cfg(target_arch = "wasm32")]
fn download_url(url: &str, name: &str) -> Result<(), String> {
    use wasm_bindgen::JsCast;
    let doc = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let a = doc
        .create_element("a")
        .map_err(|e| format!("a: {e:?}"))?
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .map_err(|_| "anchor cast".to_string())?;
    a.set_href(url);
    a.set_download(name);
    let _ = a.style().set_property("display", "none");
    let body = doc.body().ok_or("no body")?;
    let _ = body.append_child(&a);
    a.click();
    let _ = body.remove_child(&a);
    Ok(())
}

/// Where the design stands in the server's blob store.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the upload is web-only
pub enum UploadState {
    Idle,
    Checking,
    Uploading,
    /// The server already had it.
    Present,
    /// We uploaded it.
    Stored,
    /// Nothing to upload from (a source that cannot be read again).
    Unavailable,
    Failed(String),
}

impl UploadState {
    pub fn label(&self) -> String {
        match self {
            UploadState::Idle => "idle".into(),
            UploadState::Checking => "checking".into(),
            UploadState::Uploading => "uploading".into(),
            UploadState::Present => "present".into(),
            UploadState::Stored => "stored".into(),
            UploadState::Unavailable => "unavailable".into(),
            UploadState::Failed(e) => format!("failed: {e}"),
        }
    }
}

pub type UploadSlot = Rc<RefCell<UploadState>>;

#[cfg(target_arch = "wasm32")]
async fn http(url: &str, method: &str, body: Option<&wasm_bindgen::JsValue>) -> Result<web_sys::Response, String> {
    use wasm_bindgen::JsCast;
    let win = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(b) = body {
        init.set_body(b);
    }
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(|e| format!("{method} {url}: {e:?}"))?;
    let resp = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| format!("{method} {url}: {e:?}"))?;
    resp.dyn_into::<web_sys::Response>().map_err(|_| format!("{method} {url}: not a Response"))
}

/// Look up room `room` (`GET /rooms/<room>`); the answer lands in `slot`.
#[cfg(target_arch = "wasm32")]
pub fn fetch_room(room: String, slot: RoomSlot) {
    wasm_bindgen_futures::spawn_local(async move {
        let url = format!("/rooms/{room}");
        let result = async {
            let resp = http(&url, "GET", None).await?;
            if !resp.ok() {
                return Err(format!("GET {url}: HTTP {}", resp.status()));
            }
            let json = wasm_bindgen_futures::JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
                .await
                .map_err(|e| format!("GET {url}: {e:?}"))?;
            let d = get_val(&json, "design");
            let design = (!d.is_null() && !d.is_undefined()).then(|| {
                (get_str(&d, "hash").unwrap_or_default(), get_str(&d, "name").unwrap_or_default(), get_f64(&d, "ids") as u32)
            });
            Ok(RoomInfo { design })
        }
        .await;
        *slot.borrow_mut() = Some(result);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_room(_room: String, _slot: RoomSlot) {}

/// Make sure the server holds blob `hash`: `HEAD` it, and if it is missing,
/// read the design again from `source` and `PUT` it. The progress lands in
/// `slot`. A local `File` streams from disk; a URL is fetched again (usually
/// from the browser cache); `embedded` is the embedded default's bytes.
#[cfg(target_arch = "wasm32")]
pub fn ensure_blob(hash: String, source: crate::design::DesignSource, embedded: &'static [u8], slot: UploadSlot) {
    use crate::design::DesignSource;
    *slot.borrow_mut() = UploadState::Checking;
    wasm_bindgen_futures::spawn_local(async move {
        let url = format!("/blobs/{hash}");
        let result = async {
            let head = http(&url, "HEAD", None).await?;
            if head.ok() {
                return Ok(UploadState::Present);
            }
            if head.status() != 404 {
                return Err(format!("HEAD {url}: HTTP {}", head.status()));
            }
            *slot.borrow_mut() = UploadState::Uploading;
            let body: wasm_bindgen::JsValue = match &source {
                DesignSource::File(f) => f.clone().into(),
                DesignSource::Embedded => js_sys::Uint8Array::from(embedded).into(),
                DesignSource::Url(u) => {
                    let r = http(u, "GET", None).await?;
                    if !r.ok() {
                        return Err(format!("GET {u}: HTTP {}", r.status()));
                    }
                    wasm_bindgen_futures::JsFuture::from(r.blob().map_err(|e| format!("{e:?}"))?)
                        .await
                        .map_err(|e| format!("GET {u}: {e:?}"))?
                }
                DesignSource::Unknown => return Ok(UploadState::Unavailable),
            };
            let put = http(&url, "PUT", Some(&body)).await?;
            if put.ok() {
                Ok(UploadState::Stored)
            } else {
                Err(format!("PUT {url}: HTTP {}", put.status()))
            }
        }
        .await;
        *slot.borrow_mut() = result.unwrap_or_else(UploadState::Failed);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn ensure_blob(_hash: String, _source: crate::design::DesignSource, _embedded: &'static [u8], slot: UploadSlot) {
    *slot.borrow_mut() = UploadState::Unavailable;
}

// --- Phase 6 SLICE 3: streaming a design pack ---------------------------------
//
// Instead of the file, the client fetches the server's *pack* of the design:
// the manifest (`/packs/<sha256>`), the skeleton and the chunk index (both
// immutable blobs). The skeleton installs exactly like a worker-parsed one;
// geometry is then fetched per chunk, by the tessellation worker, as byte
// ranges of the geometry blob — only for chunks on screen.

/// Fetch design `hash`'s pack and queue its skeleton for installation in
/// `slot` (the same slot a worker parse delivers to). `name` labels it.
#[cfg(target_arch = "wasm32")]
pub fn fetch_pack(hash: String, name: String, slot: DesignSlot) {
    wasm_bindgen_futures::spawn_local(async move {
        let result = async {
            let t0 = js_sys::Date::now();
            let url = format!("/packs/{hash}");
            let resp = http(&url, "GET", None).await?;
            if !resp.ok() {
                return Err(format!("GET {url}: HTTP {}", resp.status()));
            }
            let m = wasm_bindgen_futures::JsFuture::from(resp.json().map_err(|e| format!("{e:?}"))?)
                .await
                .map_err(|e| format!("GET {url}: {e:?}"))?;
            let blob = |k: &str| -> (String, f64) {
                let b = get_val(&m, k);
                (get_str(&b, "hash").unwrap_or_default(), get_f64(&b, "bytes"))
            };
            let ((sk, sk_bytes), (geo, geo_bytes), (ix, _)) = (blob("skeleton"), blob("geometry"), blob("index"));
            let ((pl, _), (pix, _)) = (blob("placements"), blob("placement_index"));
            let bytes_of = |h: String| async move {
                let u = format!("/blobs/{h}");
                let r = http(&u, "GET", None).await?;
                if !r.ok() {
                    return Err(format!("GET {u}: HTTP {}", r.status()));
                }
                let buf = wasm_bindgen_futures::JsFuture::from(r.array_buffer().map_err(|e| format!("{e:?}"))?)
                    .await
                    .map_err(|e| format!("GET {u}: {e:?}"))?;
                Ok::<Vec<u8>, String>(js_sys::Uint8Array::new(&buf).to_vec())
            };
            let skeleton = bytes_of(sk).await?;
            let offsets = fluideda_core::pack::parse_index(&bytes_of(ix).await?).map_err(|e| e.to_string())?;
            let placement_offsets =
                fluideda_core::pack::parse_index(&bytes_of(pix).await?).map_err(|e| e.to_string())?;
            let bbox = js_sys::Array::from(&get_val(&m, "world_bbox"));
            // Per-cell counts (pack v5); absent from an older manifest.
            let counts = |k: &str| -> Vec<u64> {
                let v = get_val(&m, k);
                if !v.is_object() {
                    return Vec::new();
                }
                js_sys::Array::from(&v).iter().map(|x| x.as_f64().unwrap_or(0.0) as u64).collect()
            };
            let layers = js_sys::Array::from(&get_val(&m, "layers"));
            let hash_slot: crate::design::HashSlot = Rc::new(RefCell::new(Some((hash.clone(), 0.0))));
            let msg = crate::design::WorkerMsg {
                name,
                file_bytes: geo_bytes as usize,
                columns: Box::new(crate::design::NoColumns),
                meta: skeleton,
                bbox: [0, 1, 2, 3].map(|i| bbox.get(i).as_f64().unwrap_or(0.0)),
                total_shapes: get_f64(&m, "flat_shapes") as u64,
                distinct_polys: get_f64(&m, "shapes") as usize,
                cell_defs: get_f64(&m, "cells") as usize,
                layers_packed: layers.iter().map(|v| v.as_f64().unwrap_or(0.0) as u32).collect(),
                parse_ms: get_f64(&m, "build_ms"),
                worker_heap_bytes: 0,
                hash: hash_slot,
                source: crate::design::DesignSource::Url(format!("/blobs/{hash}")),
                stream: Some(crate::design::StreamInfo {
                    geometry_url: format!("/blobs/{geo}"),
                    offsets,
                    placements_url: format!("/blobs/{pl}"),
                    placement_offsets,
                    geometry_bytes: geo_bytes as u64,
                    skeleton_bytes: sk_bytes as u64,
                    fetch_ms: js_sys::Date::now() - t0,
                    placement_counts: counts("placement_counts"),
                    subtree_shapes: counts("subtree_shapes"),
                }),
            };
            crate::design::Reconstruction::new(msg).map_err(|e| format!("pack skeleton: {e}"))
        }
        .await;
        *slot.borrow_mut() = Some(result);
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_pack(_hash: String, _name: String, _slot: DesignSlot) {}

/// A placement chunk of a streamed design, arrived (Phase 7.3 SLICE B).
pub struct PlacementReply {
    pub cell: usize,
    pub chunk: u32,
    /// The stream it was asked for (a reply for a replaced design is dropped).
    pub gen: u32,
    pub result: Result<Vec<fluideda_core::Instance>, String>,
    pub bytes: usize,
    pub ms: f64,
}

pub type PlacementSlot = Rc<RefCell<Vec<PlacementReply>>>;

/// Fetch placement chunk `(cell, chunk)` — bytes `start..end` of the pack's
/// placements blob — and decode it into `slot`. On the main thread: a record
/// is ~100 KB and decodes in well under a millisecond.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
pub fn fetch_placements(url: String, start: u64, end: u64, cell: usize, chunk: u32, gen: u32, cell_count: usize, slot: PlacementSlot) {
    wasm_bindgen_futures::spawn_local(async move {
        let t0 = js_sys::Date::now();
        let result = async {
            let bytes = fetch_range(&url, start, end).await?;
            let insts = fluideda_core::serialize::decode_instances(&bytes, cell_count).map_err(|e| e.to_string())?;
            Ok::<_, String>((insts, bytes.len()))
        }
        .await;
        let ms = js_sys::Date::now() - t0;
        let (result, bytes) = match result {
            Ok((insts, n)) => (Ok(insts), n),
            Err(e) => (Err(e), 0),
        };
        slot.borrow_mut().push(PlacementReply { cell, chunk, gen, result, bytes, ms });
        wake();
    });
}

/// A shape chunk of a streamed design, arrived on the main thread for
/// picking and editing (Phase 7.4).
pub struct ShapeReply {
    pub cell: usize,
    pub chunk: u32,
    pub gen: u32,
    pub result: Result<fluideda_core::ShapeStore, String>,
    pub bytes: usize,
}

pub type ShapeSlot = Rc<RefCell<Vec<ShapeReply>>>;

/// Fetch shape chunk `(cell, chunk)` — bytes `start..end` of the pack's
/// geometry blob — and decode it into a store of its own (shape `j` of the
/// store is shape `chunk.start + j` of the cell). The tessellation worker
/// fetches the same range for drawing; this copy is what picking and editing
/// read, so it is only asked for when they need it.
#[cfg(target_arch = "wasm32")]
pub fn fetch_shape_chunk(url: String, start: u64, end: u64, cell: usize, chunk: u32, gen: u32, slot: ShapeSlot) {
    wasm_bindgen_futures::spawn_local(async move {
        let result = async {
            let bytes = fetch_range(&url, start, end).await?;
            let store = fluideda_core::pack::decode_store(&bytes).map_err(|e| e.to_string())?;
            Ok::<_, String>((store, bytes.len()))
        }
        .await;
        let (result, bytes) = match result {
            Ok((store, n)) => (Ok(store), n),
            Err(e) => (Err(e), 0),
        };
        slot.borrow_mut().push(ShapeReply { cell, chunk, gen, result, bytes });
        wake();
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_shape_chunk(_url: String, _start: u64, _end: u64, _cell: usize, _chunk: u32, _gen: u32, _slot: ShapeSlot) {}

/// Bytes `start..end` of `url` by HTTP range (a server ignoring Range is
/// handled by cutting the range out of the whole body).
///
/// Chrome's HTTP cache can hand back a short body when the page and a worker
/// read the same range at once (Phase 7.4: a few reads in 600, when a pick
/// and the drawing fetch one chunk together): a short read is read again
/// past the cache.
#[cfg(target_arch = "wasm32")]
async fn fetch_range(url: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let bytes = fetch_range_once(url, start, end, web_sys::RequestCache::Default).await?;
    if bytes.len() as u64 == end - start {
        return Ok(bytes);
    }
    let bytes = fetch_range_once(url, start, end, web_sys::RequestCache::NoStore).await?;
    if bytes.len() as u64 != end - start {
        return Err(format!("GET {url}: {} bytes for a {}-byte range", bytes.len(), end - start));
    }
    Ok(bytes)
}

#[cfg(target_arch = "wasm32")]
async fn fetch_range_once(url: &str, start: u64, end: u64, cache: web_sys::RequestCache) -> Result<Vec<u8>, String> {
    use wasm_bindgen::JsCast;
    let win = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_cache(cache);
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    headers.set("Range", &format!("bytes={start}-{}", end - 1)).map_err(|e| format!("{e:?}"))?;
    init.set_headers(&headers);
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| format!("GET {url}: {e:?}"))?
        .dyn_into()
        .map_err(|_| "not a Response".to_string())?;
    if resp.status() != 206 && resp.status() != 200 {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("GET {url}: {e:?}"))?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    if resp.status() == 200 {
        return bytes.get(start as usize..end as usize).map(<[u8]>::to_vec).ok_or_else(|| "short blob".to_string());
    }
    Ok(bytes)
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub fn fetch_placements(
    _url: String,
    _start: u64,
    _end: u64,
    _cell: usize,
    _chunk: u32,
    _gen: u32,
    _cell_count: usize,
    _slot: PlacementSlot,
) {
}
