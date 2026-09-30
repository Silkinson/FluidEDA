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
/// through `status`.
#[cfg(target_arch = "wasm32")]
pub fn read_dropped(name: String, file: egui::DroppedFileHandle, slot: LoadSlot, _status: &mut String) {
    wasm_bindgen_futures::spawn_local(async move {
        match file.bytes_async().await {
            Ok(bytes) => *slot.borrow_mut() = Some((name, bytes)),
            Err(e) => log::error!("read dropped {name} failed: {e}"),
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_dropped(name: String, file: egui::DroppedFileHandle, slot: LoadSlot, status: &mut String) {
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
    input.set_accept(".gds,.gds2,.gdsii,.oas,.oasis");

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

/// Parse the file at `url` in the worker — which **fetches it itself** (Phase
/// 5c), so the body never materialises on the main thread. The pending
/// reconstruction lands in `slot`. `file_bytes` for the metrics comes from the
/// worker's reply.
#[cfg(target_arch = "wasm32")]
pub fn spawn_worker_fetch(url: String, slot: DesignSlot) {
    let name = url.rsplit('/').next().unwrap_or("file.gds").to_string();
    // Absolute URL: a worker resolves relative URLs against its own script.
    let abs = web_sys::window()
        .and_then(|w| w.location().href().ok())
        .and_then(|base| web_sys::Url::new_with_base(&url, &base).ok())
        .map(|u| u.href())
        .unwrap_or(url);
    let payload = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&payload, &"type".into(), &"fetch_parse".into());
    let _ = js_sys::Reflect::set(&payload, &"url".into(), &abs.into());
    run_worker(name, payload, js_sys::Array::new(), slot);
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
    run_worker(name, payload, js_sys::Array::of1(&buf), slot);
}

/// Spawn a one-shot parse worker, post it `payload` (with `transfer`), turn its
/// reply into a pending reconstruction in `slot`, then terminate the worker
/// (reclaiming its parse memory).
#[cfg(target_arch = "wasm32")]
fn run_worker(name: String, payload: js_sys::Object, transfer: js_sys::Array, slot: DesignSlot) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};

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
        // type == "done": rebuild the Design from the transferred columns. The
        // three big columns stay in their transferred JS buffers; the rebuild
        // copies each cell's slice from there exactly once (Phase 5c).
        let columns = JsColumns {
            coords: js_sys::Int32Array::new(&get_val(&data, "coords")),
            starts: js_sys::Uint32Array::new(&get_val(&data, "starts")),
            layers: js_sys::ArrayBuffer::from(get_val(&data, "layers")),
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
        };
        // Hand the pending rebuild to the app (it copies across frames) and
        // terminate the worker now — its ~0.8 GB parse heap is reclaimed while
        // the main thread copies, so the two never peak together.
        match crate::design::Reconstruction::new(msg) {
            Ok(r) => *slot_for_msg.borrow_mut() = Some(Ok(r)),
            Err(err) => *slot_for_msg.borrow_mut() = Some(Err(format!("reconstruct failed: {err}"))),
        }
        worker_for_msg.terminate();
    });
    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget(); // keep alive for the one message; the worker is then dropped

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
                        log::error!("tess worker: {}", get_str(&data, "message").unwrap_or_default())
                    }
                    _ => {}
                }
            });
        worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        Ok(Self { worker, _onmessage: onmessage })
    }

    /// Dispatch a chunk-tessellation job. The three columns are copied once into
    /// JS buffers and **transferred** (moved) to the worker; `cell`/`chunk`/`gen`
    /// ride along so the reply can be keyed and staleness-checked.
    pub fn post(&self, cell: usize, chunk: u32, gen: u32, coords: &[i32], starts: &[u32], layers: &[u32]) {
        use wasm_bindgen::JsValue;
        let carr = js_sys::Int32Array::from(coords);
        let sarr = js_sys::Uint32Array::from(starts);
        let larr = js_sys::Uint32Array::from(layers);
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
        let transfer = js_sys::Array::of3(&carr.buffer(), &sarr.buffer(), &larr.buffer());
        if let Err(e) = self.worker.post_message_with_transfer(&payload, &transfer) {
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
    TessReply { cell, chunk, gen, meshes }
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
    pub fn post(&self, _cell: usize, _chunk: u32, _gen: u32, _coords: &[i32], _starts: &[u32], _layers: &[u32]) {}
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
