//! Runtime design loading + Phase 1.5 instrumentation.
//!
//! Parses GDS bytes (from the embedded default, a dropped file, or a `?file=`
//! fetch) into a `CellDb`, eagerly tessellates every cell def — exactly what the
//! app does today, so the numbers reflect real behavior — and records timing /
//! memory metrics. On wasm the metrics are also pushed to `window` and the
//! console so the Playwright bench can read them.

use fluideda_core::{CellDb, ColumnSource, Layer};
use web_time::Instant;

/// Everything the app needs to render one design, plus how expensive it was.
pub struct Design {
    pub db: CellDb,
    pub layers: Vec<Layer>,
    pub bbox_min: glam::Vec2,
    pub bbox_max: glam::Vec2,
    pub total_shapes: u64,
    pub metrics: Metrics,
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
    /// Main-thread wall time to rebuild the CellDb from the worker's buffers
    /// (worker path only) — the one load-time frame the main thread still spends.
    /// Phase 5c: this now covers the column copies too (they happen inside the
    /// rebuild — one copy per cell column, straight from the transferred JS
    /// buffers), i.e. the whole design-swap handler's geometry work.
    pub reconstruct_ms: f64,
    /// Frames the (budgeted) reconstruction spanned (Phase 5c).
    pub reconstruct_frames: usize,
}

/// Everything the parse worker sends back: the serialized columns (left where
/// the transfer put them, behind a [`ColumnSource`]) and metadata blob, plus the
/// summary it computed (so the main thread doesn't rescan 3.9 M shapes). The
/// worker path builds a [`Design`] from this via [`Reconstruction`].
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
pub struct WorkerMsg {
    pub name: String,
    pub file_bytes: usize,
    /// The three geometry columns. On the web: the transferred `ArrayBuffer`s,
    /// read per cell (Phase 5c single-copy reconstruction).
    pub columns: Box<dyn ColumnSource>,
    /// Length of the coord column (i32 elements) — progress readout.
    pub coord_count: usize,
    pub meta: Vec<u8>,
    pub bbox: [f64; 4],
    pub total_shapes: u64,
    pub distinct_polys: usize,
    pub cell_defs: usize,
    pub layers_packed: Vec<u32>,
    pub parse_ms: f64,
    pub worker_heap_bytes: usize,
}

/// Main-thread bytes copied per `Deserializer::step` call between clock checks.
const RECON_STEP_BYTES: usize = 4 << 20;

/// A design being rebuilt from the worker's transferred buffers, **across
/// frames** (Phase 5c). The worker reply only creates this; the app then
/// advances it each frame within a time budget ([`step`](Self::step)) — one
/// copy per cell column, straight from the transferred JS buffers, a big cell's
/// coordinates in slices — and installs the finished [`Design`]. The swap no
/// longer lands as one ~30–50 ms task between two frames.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
pub struct Reconstruction {
    deser: fluideda_core::serialize::Deserializer,
    columns: Box<dyn ColumnSource>,
    summary: WorkerMsg,
    total_bytes: usize,
    spent_ms: f64,
    frames: usize,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
impl Reconstruction {
    /// Validate the metadata header; copies nothing yet.
    pub fn new(mut m: WorkerMsg) -> anyhow::Result<Self> {
        let deser = fluideda_core::serialize::Deserializer::new(std::mem::take(&mut m.meta))?;
        // Every column byte gets copied once: coords (4 B/coord) + starts +
        // layers (8 B/shape, near enough) — for the progress readout only.
        let total_bytes = m.coord_count * 4 + m.distinct_polys * 8;
        let columns = std::mem::replace(&mut m.columns, Box::new(NoColumns));
        Ok(Reconstruction { deser, columns, summary: m, total_bytes, spent_ms: 0.0, frames: 0 })
    }

    /// Copy for up to `budget_ms`; `Ok(true)` when the CellDb is complete.
    pub fn step(&mut self, budget_ms: f64) -> anyhow::Result<bool> {
        let t = Instant::now();
        self.frames += 1;
        let mut done = false;
        while !done && t.elapsed().as_secs_f64() * 1e3 < budget_ms {
            done = self.deser.step(self.columns.as_mut(), RECON_STEP_BYTES)?;
        }
        self.spent_ms += t.elapsed().as_secs_f64() * 1e3;
        Ok(done)
    }

    /// Fraction of the column bytes copied (panel readout).
    pub fn progress(&self) -> f64 {
        (self.deser.bytes_copied() as f64 / self.total_bytes.max(1) as f64).min(1.0)
    }

    /// The finished design (call once `step` returned `true`). Adopts the
    /// worker's precomputed summary — no reparse, no shape rescan.
    pub fn finish(self) -> anyhow::Result<Design> {
        let db = self.deser.finish()?;
        let m = self.summary;
        let layers = m.layers_packed.iter().map(|&v| Layer::new((v & 0xffff) as u16, (v >> 16) as u16)).collect();
        let metrics = Metrics {
            name: m.name,
            file_bytes: m.file_bytes,
            parse_ms: m.parse_ms,
            cell_defs: m.cell_defs,
            distinct_polys: m.distinct_polys,
            flattened: m.total_shapes,
            wasm_heap_bytes: wasm_heap_bytes(), // main-thread heap after reconstruction
            parse_heap_bytes: m.worker_heap_bytes, // the parse peak now lives in the worker
            worker_used: true,
            reconstruct_ms: self.spent_ms,
            reconstruct_frames: self.frames,
            ..Default::default()
        };
        Ok(Design {
            db,
            layers,
            bbox_min: glam::Vec2::new(m.bbox[0] as f32, m.bbox[1] as f32),
            bbox_max: glam::Vec2::new(m.bbox[2] as f32, m.bbox[3] as f32),
            total_shapes: m.total_shapes,
            metrics,
        })
    }
}

/// Placeholder left in a consumed `WorkerMsg` (its columns moved out).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // the worker path is web-only
struct NoColumns;
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
    let db = fluideda_core::gds::load_gds_db(bytes)?;
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
             \"reconstruct_frames\":{}}}",
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
