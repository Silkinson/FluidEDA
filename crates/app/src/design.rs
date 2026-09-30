//! Runtime design loading + Phase 1.5 instrumentation.
//!
//! Parses GDS bytes (from the embedded default, a dropped file, or a `?file=`
//! fetch) into a `CellDb`, eagerly tessellates every cell def — exactly what the
//! app does today, so the numbers reflect real behavior — and records timing /
//! memory metrics. On wasm the metrics are also pushed to `window` and the
//! console so the Playwright bench can read them.

use fluideda_core::{CellDb, ColumnSource, Layer};
use std::cell::RefCell;
use std::rc::Rc;
use web_time::Instant;

/// The design's SHA-256 (hex) and the worker's ms to compute it, delivered
/// after the parse reply (Phase 6). Empty hash: none was computed.
pub type HashSlot = Rc<RefCell<Option<(String, f64)>>>;

/// Everything the app needs to render one design, plus how expensive it was.
pub struct Design {
    pub db: CellDb,
    pub layers: Vec<Layer>,
    pub bbox_min: glam::Vec2,
    pub bbox_max: glam::Vec2,
    pub total_shapes: u64,
    pub metrics: Metrics,
    /// Where the file came from (Phase 6: read again to upload it to a room).
    pub source: DesignSource,
    /// Worker loads: where the design's hash will land (it follows the parse).
    pub pending_hash: Option<HashSlot>,
    /// A streamed design (Phase 6 SLICE 3): its cells hold no geometry; each
    /// chunk's is fetched from the server when it is on screen.
    pub stream: Option<StreamInfo>,
}

/// Where a streamed design's geometry lives (Phase 6 SLICE 3): chunk `k`'s
/// record is bytes `offsets[k]..offsets[k+1]` of the blob at `geometry_url`
/// (`k` numbered by `fluideda_core::pack::chunk_base`).
#[derive(Clone)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the stats are published on the web
pub struct StreamInfo {
    pub geometry_url: String,
    pub offsets: Vec<u64>,
    pub geometry_bytes: u64,
    pub skeleton_bytes: u64,
    /// Manifest + skeleton + index fetch, wall ms.
    pub fetch_ms: f64,
}

/// Where a design's bytes came from — what the app reads again when a room
/// needs the design in the server's store (Phase 6). Never the bytes
/// themselves: a URL can be fetched again and a local `File` is a disk-backed
/// handle, so keeping either costs nothing.
#[derive(Clone, Default)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // most sources are web-only
pub enum DesignSource {
    #[default]
    Unknown,
    /// The embedded default (`hier.gds`).
    Embedded,
    /// An absolute URL.
    Url(String),
    /// A local file (drag-drop / picker).
    #[cfg(target_arch = "wasm32")]
    File(web_sys::File),
}

/// Load-time measurements. `index_ms`/`ttff_ms` are filled in later (after the
/// first frame) by the app so they don't pollute each other.
#[derive(Clone, Default)]
pub struct Metrics {
    pub name: String,
    pub file_bytes: usize,
    pub parse_ms: f64,
    pub tess_ms: f64,
    pub index_ms: f64,
    pub index_p99_ms: f64,
    pub index_max_ms: f64,
    pub ttff_ms: f64,
    pub cell_defs: usize,
    pub distinct_polys: usize,
    pub flattened: u64,
    pub triangles: usize,
    pub vertex_bytes: usize,
    pub index_bytes: usize,
    pub wasm_heap_bytes: usize,
    /// Wasm linear-memory high-water of the **parse** (wasm memory never shrinks,
    /// so this is the transient peak). Sync path: the main thread's heap after
    /// parse. Worker path (Phase 2b STEP 2): the *worker's* heap — parse no longer
    /// touches the main thread. Combined peak = this + `wasm_heap_bytes` (main).
    pub parse_heap_bytes: usize,
    /// True when the design was parsed in a Web Worker off the main thread.
    pub worker_used: bool,
    /// The GPU backend actually in use (e.g. `BrowserWebGpu`, `Gl`, `Metal`) —
    /// every browser number before the wgpu 30 upgrade was `Gl` (WebGL2).
    pub backend: String,
    /// Main-thread wall time to rebuild the CellDb from the worker's buffers
    /// (worker path only) — the one load-time frame the main thread still spends.
    /// Phase 5c: this now covers the column copies too (they happen inside the
    /// rebuild — one copy per cell column, straight from the transferred JS
    /// buffers), i.e. the whole design-swap handler's geometry work.
    pub reconstruct_ms: f64,
    /// Frames the (budgeted) reconstruction spanned (Phase 5c).
    pub reconstruct_frames: usize,
    /// Worker path (Phase 5c): wall ms from load start until every cell's
    /// geometry is in place. `ttff_ms` is the first frame of the *skeleton*
    /// (the new design's outline as proxies), which now comes much earlier.
    pub geometry_ms: f64,
    /// SHA-256 of the file bytes, lower-case hex (Phase 6: the design's
    /// identity for rooms and the blob store). Empty when unknown — or not
    /// yet known: the worker computes it after replying with the parse.
    pub hash: String,
    /// Worker path: the worker's ms to hash the file (after the parse reply,
    /// so it adds nothing to the load).
    pub hash_ms: f64,
}

/// Everything the parse worker sends back: the serialized columns (left where
/// the transfer put them, behind a [`ColumnSource`]) and metadata blob, plus the
/// summary it computed (so the main thread doesn't rescan 3.9 M shapes). The
/// worker path turns this into a skeleton [`Design`] + [`Reconstruction`].
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
pub struct WorkerMsg {
    pub name: String,
    pub file_bytes: usize,
    /// The three geometry columns. On the web: the transferred `ArrayBuffer`s,
    /// read per cell (Phase 5c single-copy reconstruction).
    pub columns: Box<dyn ColumnSource>,
    pub meta: Vec<u8>,
    pub bbox: [f64; 4],
    pub total_shapes: u64,
    pub distinct_polys: usize,
    pub cell_defs: usize,
    pub layers_packed: Vec<u32>,
    pub parse_ms: f64,
    pub worker_heap_bytes: usize,
    /// Where the file's SHA-256 lands once the worker has computed it.
    pub hash: HashSlot,
    pub source: DesignSource,
    /// Streaming: the skeleton came from a pack; geometry stays on the server.
    pub stream: Option<StreamInfo>,
}

/// Main-thread bytes copied (columns) or parsed (metadata) per step call
/// between clock checks.
const RECON_STEP_BYTES: usize = 4 << 20;
const SKELETON_STEP_BYTES: usize = 1 << 20;

enum Phase {
    /// Parsing the metadata blob into the skeleton (the old design still shows).
    Skeleton(fluideda_core::SkeletonParse),
    /// The skeleton is installed; copying cell geometry into it.
    Loading(fluideda_core::ProgressiveLoad),
}

/// What one [`Reconstruction::step`] produced.
pub enum ReconStep {
    /// More to do next frame.
    Progress,
    /// The skeleton is complete: install this design now (it renders as proxies
    /// until its cells' geometry arrives through the following steps).
    SkeletonReady(Design),
    /// Every cell's geometry is in place.
    Done,
}

/// A worker-parsed design arriving **progressively** (Phase 5c). Each
/// [`step`](Self::step) spends at most a frame budget: first parsing the
/// metadata into a *skeleton* design — the whole hierarchy, chunk tables and
/// bboxes, no geometry — which the app installs as soon as it is complete
/// (every chunk draws as a bbox proxy); then copying cell geometry into the
/// live design, visible cells first. One copy per cell column, straight from
/// the transferred JS buffers; nothing on the main thread takes more than the
/// budget plus one slice.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
pub struct Reconstruction {
    phase: Phase,
    columns: Box<dyn ColumnSource>,
    /// The worker's summary, consumed when the skeleton becomes a `Design`.
    summary: Option<WorkerMsg>,
    spent_ms: f64,
    frames: usize,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
impl Reconstruction {
    /// Validate the metadata header; nothing is parsed or copied yet.
    pub fn new(mut m: WorkerMsg) -> anyhow::Result<Reconstruction> {
        let parse = fluideda_core::SkeletonParse::new(std::mem::take(&mut m.meta))?;
        let columns = std::mem::replace(&mut m.columns, Box::new(NoColumns));
        Ok(Reconstruction { phase: Phase::Skeleton(parse), columns, summary: Some(m), spent_ms: 0.0, frames: 0 })
    }

    /// Spend up to `budget_ms`. In the loading phase, cells in `priority` load
    /// first and finished cell stores are pushed to `out`.
    pub fn step(
        &mut self,
        budget_ms: f64,
        priority: &[fluideda_core::CellId],
        out: &mut Vec<(fluideda_core::CellId, fluideda_core::ShapeStore)>,
    ) -> anyhow::Result<ReconStep> {
        let t = Instant::now();
        self.frames += 1;
        let within = |t: &Instant| t.elapsed().as_secs_f64() * 1e3 < budget_ms;
        let result = match &mut self.phase {
            Phase::Skeleton(p) => {
                let mut done = false;
                while !done && within(&t) {
                    done = p.step(SKELETON_STEP_BYTES)?;
                }
                if done {
                    let Phase::Skeleton(p) =
                        std::mem::replace(&mut self.phase, Phase::Loading(fluideda_core::ProgressiveLoad::empty()))
                    else {
                        unreachable!()
                    };
                    let (db, load) = p.finish()?;
                    // A streamed design loads no geometry: it is done once installed.
                    let streamed = self.summary.as_ref().is_some_and(|m| m.stream.is_some());
                    self.phase = Phase::Loading(if streamed { fluideda_core::ProgressiveLoad::empty() } else { load });
                    ReconStep::SkeletonReady(self.design(db))
                } else {
                    ReconStep::Progress
                }
            }
            Phase::Loading(load) => {
                let mut done = false;
                while !done && within(&t) {
                    done = load.step(self.columns.as_mut(), RECON_STEP_BYTES, priority, out)?;
                }
                if done { ReconStep::Done } else { ReconStep::Progress }
            }
        };
        self.spent_ms += t.elapsed().as_secs_f64() * 1e3;
        Ok(result)
    }

    /// The installable skeleton design, from the worker's summary.
    fn design(&mut self, db: fluideda_core::CellDb) -> Design {
        let m = self.summary.take().expect("skeleton built once");
        let layers = m.layers_packed.iter().map(|&v| Layer::new((v & 0xffff) as u16, (v >> 16) as u16)).collect();
        let metrics = Metrics {
            name: m.name,
            file_bytes: m.file_bytes,
            parse_ms: m.parse_ms,
            cell_defs: m.cell_defs,
            distinct_polys: m.distinct_polys,
            flattened: m.total_shapes,
            parse_heap_bytes: m.worker_heap_bytes, // the parse peak lives in the worker
            worker_used: true,
            ..Default::default()
        };
        Design {
            db,
            layers,
            bbox_min: glam::Vec2::new(m.bbox[0] as f32, m.bbox[1] as f32),
            bbox_max: glam::Vec2::new(m.bbox[2] as f32, m.bbox[3] as f32),
            total_shapes: m.total_shapes,
            metrics,
            source: m.source,
            pending_hash: Some(m.hash),
            stream: m.stream,
        }
    }

    /// Has the skeleton been handed out (so the live design is this one)?
    pub fn installed(&self) -> bool {
        matches!(self.phase, Phase::Loading(_))
    }

    /// Is `cell`'s geometry in place? Meaningful once [`installed`](Self::installed).
    pub fn is_loaded(&self, cell: fluideda_core::CellId) -> bool {
        match &self.phase {
            Phase::Skeleton(_) => false,
            Phase::Loading(l) => l.is_loaded(cell),
        }
    }

    /// Progress in `[0, 1]` of the current phase, and its name (panel readout).
    pub fn progress(&self) -> (&'static str, f64) {
        match &self.phase {
            Phase::Skeleton(p) => ("reading hierarchy", p.progress()),
            Phase::Loading(l) => ("loading geometry", l.progress()),
        }
    }

    /// Main-thread ms spent so far, and frames spanned.
    pub fn cost(&self) -> (f64, usize) {
        (self.spent_ms, self.frames)
    }
}

/// Placeholder left in a consumed `WorkerMsg` (its columns moved out).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
/// A column source with nothing in it (a streamed design keeps no columns).
pub struct NoColumns;
impl ColumnSource for NoColumns {
    fn coords_into(&mut self, _: usize, _: usize, _: &mut Vec<i32>) -> anyhow::Result<()> {
        anyhow::bail!("columns already taken")
    }
    fn starts(&mut self, _: usize, _: usize) -> anyhow::Result<Vec<u32>> {
        anyhow::bail!("columns already taken")
    }
    fn layers(&mut self, _: usize, _: usize) -> anyhow::Result<Vec<Layer>> {
        anyhow::bail!("columns already taken")
    }
}

/// Parse only. Tessellation is now **lazy** (Phase 2a STEP 3): the app
/// tessellates + uploads a cell def the first time it's actually drawn, so the
/// load path no longer blocks on tessellating every def. `tess_ms` and the GPU-
/// geometry byte counts are filled in by the app as cells become visible.
pub fn load_design(name: &str, bytes: &[u8]) -> anyhow::Result<Design> {
    let t = Instant::now();
    let db = fluideda_core::load_layout_db(bytes)?; // GDSII or OASIS (Phase 6)
    let parse_ms = t.elapsed().as_secs_f64() * 1e3;
    // Sampled here, before tessellation/index — wasm memory never shrinks, so
    // this is the parse high-water (the streaming-parser headline).
    let parse_heap_bytes = wasm_heap_bytes();

    let cell_defs = db.cells.len();
    let distinct_polys: usize = db.cells.iter().map(|c| c.shapes.len()).sum();
    let flattened = db.flat_shape_count();
    let wb = db.world_bbox();
    let layers = db.all_layers();

    let metrics = Metrics {
        name: name.to_string(),
        file_bytes: bytes.len(),
        parse_ms,
        cell_defs,
        distinct_polys,
        flattened,
        wasm_heap_bytes: wasm_heap_bytes(),
        parse_heap_bytes,
        ..Default::default()
    };

    Ok(Design {
        db,
        layers,
        bbox_min: glam::Vec2::new(wb.min_x as f32, wb.min_y as f32),
        bbox_max: glam::Vec2::new(wb.max_x as f32, wb.max_y as f32),
        total_shapes: flattened,
        metrics,
        source: DesignSource::Unknown,
        pending_hash: None,
        stream: None,
    })
}

/// Force + time every cell's R-tree (normally lazy). Reports total, p99, max.
/// Run *after* the first frame so it doesn't inflate TTFF.
pub fn measure_indices(db: &CellDb) -> (f64, f64, f64) {
    let mut durs = Vec::with_capacity(db.cells.len());
    let total = Instant::now();
    for c in &db.cells {
        let t = Instant::now();
        let _ = c.index().len();
        durs.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let total_ms = total.elapsed().as_secs_f64() * 1e3;
    durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p99 = pct(&durs, 0.99);
    let max = durs.last().copied().unwrap_or(0.0);
    (total_ms, p99, max)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// --- wasm memory + JS bridge -------------------------------------------------

/// Current wasm linear-memory size in bytes (the number that hits the ~2 GB
/// browser ceiling). 0 on native.
#[cfg(target_arch = "wasm32")]
pub fn wasm_heap_bytes() -> usize {
    use wasm_bindgen::JsCast;
    wasm_bindgen::memory()
        .dyn_into::<js_sys::WebAssembly::Memory>()
        .ok()
        .and_then(|m| m.buffer().dyn_into::<js_sys::ArrayBuffer>().ok())
        .map(|b| b.byte_length() as usize)
        .unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn wasm_heap_bytes() -> usize {
    0
}

impl Metrics {
    /// Serialize to a compact JSON object (hand-rolled to avoid a serde dep).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"name\":\"{}\",\"file_bytes\":{},\"parse_ms\":{:.1},\"tess_ms\":{:.1},\
             \"index_ms\":{:.1},\"index_p99_ms\":{:.3},\"index_max_ms\":{:.1},\"ttff_ms\":{:.1},\
             \"cell_defs\":{},\"distinct_polys\":{},\"flattened\":{},\"triangles\":{},\
             \"vertex_bytes\":{},\"index_bytes\":{},\"wasm_heap_bytes\":{},\
             \"parse_heap_bytes\":{},\"worker_used\":{},\"reconstruct_ms\":{:.1},\
             \"reconstruct_frames\":{},\"geometry_ms\":{:.1},\"backend\":\"{}\",\
             \"hash\":\"{}\",\"hash_ms\":{:.1}}}",
            self.name.replace('"', "'"),
            self.file_bytes,
            self.parse_ms,
            self.tess_ms,
            self.index_ms,
            self.index_p99_ms,
            self.index_max_ms,
            self.ttff_ms,
            self.cell_defs,
            self.distinct_polys,
            self.flattened,
            self.triangles,
            self.vertex_bytes,
            self.index_bytes,
            self.wasm_heap_bytes,
            self.parse_heap_bytes,
            self.worker_used,
            self.reconstruct_ms,
            self.reconstruct_frames,
            self.geometry_ms,
            self.backend,
            self.hash,
            self.hash_ms,
        )
    }

    /// Publish to `window.__fluideda_metrics` + console (wasm only). The bench
    /// harness waits on the window global.
    pub fn publish(&self) {
        let json = self.to_json();
        log::info!("FLUIDEDA_METRICS {json}");
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(w) = web_sys::window() {
                let _ = js_sys::Reflect::set(
                    &w,
                    &wasm_bindgen::JsValue::from_str("__fluideda_metrics"),
                    &wasm_bindgen::JsValue::from_str(&json),
                );
            }
        }
    }
}
