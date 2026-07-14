//! Runtime design loading + Phase 1.5 instrumentation.
//!
//! Parses GDS bytes (from the embedded default, a dropped file, or a `?file=`
//! fetch) into a `CellDb`, eagerly tessellates every cell def — exactly what the
//! app does today, so the numbers reflect real behavior — and records timing /
//! memory metrics. On wasm the metrics are also pushed to `window` and the
//! console so the Playwright bench can read them.

use fluideda_core::{CellDb, Layer};
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
}

/// Parse only. Tessellation is now **lazy** (Phase 2a STEP 3): the app
/// tessellates + uploads a cell def the first time it's actually drawn, so the
/// load path no longer blocks on tessellating every def. `tess_ms` and the GPU-
/// geometry byte counts are filled in by the app as cells become visible.
pub fn load_design(name: &str, bytes: &[u8]) -> anyhow::Result<Design> {
    let t = Instant::now();
    let db = fluideda_core::gds::load_gds_db(bytes)?;
    let parse_ms = t.elapsed().as_secs_f64() * 1e3;

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
             \"vertex_bytes\":{},\"index_bytes\":{},\"wasm_heap_bytes\":{}}}",
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
