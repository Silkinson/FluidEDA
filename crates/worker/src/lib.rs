//! FluidEDA parse worker — the Phase 2b STEP 2 off-main-thread parser.
//!
//! This crate compiles to a **separate** wasm module (its own linear memory) that
//! runs in a Web Worker. It links `fluideda-core` ONLY — no wgpu, no egui — so it
//! is small and never touches the GPU or canvas (neither exists in a worker).
//!
//! Protocol: the main thread `postMessage`s the raw GDS bytes (transferred in);
//! [`parse`] runs the streaming parser + chunking, serializes the finished
//! `CellDb` to plain columns (see `fluideda_core::serialize`), and returns them as
//! typed arrays. The worker JS (`assets/worker/parse_worker.js`) transfers those
//! arrays' `ArrayBuffer`s back to the main thread (zero-copy), then the worker is
//! terminated so its ~0.8 GB parse memory is reclaimed (wasm memory never shrinks
//! in-place). No shared heap, no atomics — STEP 3's ~0.8 GB parse peak makes a
//! parse-and-copy worker fit under 4 GB combined.

#[cfg(target_arch = "wasm32")]
mod wasm {
    use js_sys::{Int32Array, Uint32Array, Uint8Array};
    use wasm_bindgen::prelude::*;

    /// The serialized `CellDb`, ready to transfer. Each field is a typed array
    /// backed by its own `ArrayBuffer` (copied out of wasm memory once), so the
    /// worker JS can move them to the main thread with a transfer list.
    #[wasm_bindgen]
    pub struct ParseResult {
        coords: Int32Array,
        starts: Uint32Array,
        layers: Uint32Array,
        meta: Uint8Array,
        worker_heap_bytes: f64,
        parse_ms: f64,
        // Summary the app would otherwise recompute on the main thread — layers
        // (all_layers is an 80 ms scan of every shape's layer on caravel), the
        // world bbox, and shape counts. Computed here (in the worker) so main-
        // thread reconstruction stays inside one frame.
        world_bbox: Vec<f64>, // [min_x, min_y, max_x, max_y]
        total_shapes: f64,
        distinct_polys: f64,
        cell_defs: f64,
        layer_set: Uint32Array, // distinct layers, packed (layer | datatype<<16)
    }

    #[wasm_bindgen]
    impl ParseResult {
        #[wasm_bindgen(getter)]
        pub fn coords(&self) -> Int32Array {
            self.coords.clone()
        }
        #[wasm_bindgen(getter)]
        pub fn starts(&self) -> Uint32Array {
            self.starts.clone()
        }
        #[wasm_bindgen(getter)]
        pub fn layers(&self) -> Uint32Array {
            self.layers.clone()
        }
        #[wasm_bindgen(getter)]
        pub fn meta(&self) -> Uint8Array {
            self.meta.clone()
        }
        /// The worker's wasm linear-memory high-water (bytes). Sampled after
        /// parse+serialize; since wasm memory only grows, this is the peak.
        #[wasm_bindgen(getter)]
        pub fn worker_heap_bytes(&self) -> f64 {
            self.worker_heap_bytes
        }
        #[wasm_bindgen(getter)]
        pub fn parse_ms(&self) -> f64 {
            self.parse_ms
        }
        /// `[min_x, min_y, max_x, max_y]` world bbox in nm.
        #[wasm_bindgen(getter)]
        pub fn world_bbox(&self) -> Vec<f64> {
            self.world_bbox.clone()
        }
        #[wasm_bindgen(getter)]
        pub fn total_shapes(&self) -> f64 {
            self.total_shapes
        }
        #[wasm_bindgen(getter)]
        pub fn distinct_polys(&self) -> f64 {
            self.distinct_polys
        }
        #[wasm_bindgen(getter)]
        pub fn cell_defs(&self) -> f64 {
            self.cell_defs
        }
        #[wasm_bindgen(getter)]
        pub fn layer_set(&self) -> Uint32Array {
            self.layer_set.clone()
        }
    }

    /// Parse GDS bytes into a serialized `CellDb`. Runs entirely in the worker.
    #[wasm_bindgen]
    pub fn parse(bytes: Vec<u8>) -> Result<ParseResult, JsValue> {
        console_error_panic_hook::set_once();

        let t0 = now_ms();
        let db = fluideda_core::gds::load_gds_db(&bytes)
            .map_err(|e| JsValue::from_str(&format!("parse failed: {e}")))?;
        drop(bytes); // free the raw file (278 MB on caravel) before serializing
        let parse_ms = now_ms() - t0;

        let cols = fluideda_core::serialize(&db);

        // Summary the main thread would otherwise recompute (all_layers alone is
        // ~80 ms on caravel). Cheap here since the db is in hand.
        let wb = db.world_bbox();
        let world_bbox = vec![wb.min_x, wb.min_y, wb.max_x, wb.max_y];
        let total_shapes = db.flat_shape_count() as f64;
        let distinct_polys = db.cells.iter().map(|c| c.shapes.len()).sum::<usize>() as f64;
        let cell_defs = db.cells.len() as f64;
        let packed: Vec<u32> = db
            .all_layers()
            .iter()
            .map(|l| l.layer as u32 | ((l.datatype as u32) << 16))
            .collect();
        let layer_set = Uint32Array::from(&packed[..]);

        // Sample the high-water AFTER parse+serialize (worst case both alive).
        let worker_heap_bytes = wasm_heap_bytes();

        // Copy each column out of wasm memory into its own ArrayBuffer (the one
        // worker-side copy). These become transferable.
        let coords = Int32Array::from(&cols.coords[..]);
        let starts = Uint32Array::from(&cols.starts[..]);
        let layers = Uint32Array::from(&cols.layers[..]);
        let meta = Uint8Array::from(&cols.meta[..]);

        Ok(ParseResult {
            coords,
            starts,
            layers,
            meta,
            worker_heap_bytes,
            parse_ms,
            world_bbox,
            total_shapes,
            distinct_polys,
            cell_defs,
            layer_set,
        })
    }

    /// Current wasm linear-memory size in bytes.
    fn wasm_heap_bytes() -> f64 {
        wasm_bindgen::memory()
            .dyn_into::<js_sys::WebAssembly::Memory>()
            .ok()
            .and_then(|m| m.buffer().dyn_into::<js_sys::ArrayBuffer>().ok())
            .map(|b| b.byte_length() as f64)
            .unwrap_or(0.0)
    }

    fn now_ms() -> f64 {
        js_sys::Date::now()
    }
}
