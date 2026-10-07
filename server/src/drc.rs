//! Server-side in-context DRC (Phase 7.5): a stored design, with the edits of
//! a document state, checked as it is built — world tiles with geometry
//! gathered through the hierarchy ([`fluideda_core::check_world_tiles`]), on
//! every core. A streamed client cannot check what it does not hold; the
//! server holds the whole design.
//!
//! The parsed design stays in memory between requests, so the re-check of
//! one area after an edit costs only that area's tiles.

use crate::store::Store;
use fluideda_core::{CellDb, DrcKind, DrcRules, Violation};
use serde::Serialize;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One marker, world nm.
#[derive(Debug, Serialize, serde::Deserialize, PartialEq)]
pub struct Marker {
    /// `width` or `space`.
    pub kind: String,
    pub layer: u16,
    pub datatype: u16,
    pub bbox: [i32; 4],
}

/// What a check found, and what it cost.
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Checked {
    pub violations: Vec<Marker>,
    /// The area checked (`[x0, y0, x1, y1]`, world nm), when not the whole
    /// design: what these results replace.
    pub area: Option<[i32; 4]>,
    pub tiles: usize,
    /// Tiles whose content repeated a checked one's.
    pub shared: usize,
    /// Edits the check composited: base shapes edited, shapes drawn, placements hidden.
    pub edited: usize,
    pub added: usize,
    pub hidden: usize,
    /// Parse (when not cached) + check, ms.
    pub ms: f64,
    pub threads: usize,
}

#[derive(Debug)]
pub enum DrcError {
    NoDesign,
    BadState(String),
    BadDesign(String),
    Io(io::Error),
}

impl From<io::Error> for DrcError {
    fn from(e: io::Error) -> Self {
        DrcError::Io(e)
    }
}

/// The last design parsed for a check (one: caravel's is about 1 GB).
static PARSED: Mutex<Option<(String, Arc<CellDb>)>> = Mutex::new(None);

fn parsed(store: &Store, design: &str) -> Result<Arc<CellDb>, DrcError> {
    if let Some((d, db)) = PARSED.lock().unwrap().as_ref() {
        if d == design {
            return Ok(db.clone());
        }
    }
    if !store.has_blob(design) {
        return Err(DrcError::NoDesign);
    }
    let mut src = store.open_blob(design)?;
    let db = Arc::new(fluideda_core::load_layout_db_from(&mut src).map_err(|e| DrcError::BadDesign(e.to_string()))?);
    *PARSED.lock().unwrap() = Some((design.to_string(), db.clone()));
    Ok(db)
}

/// Check `design` with the edits in `state` (a v1 document update) under
/// `rules`, the whole design or only `area`. Blocking.
pub fn check(store: &Store, design: &str, state: &[u8], rules: &DrcRules, area: Option<[i32; 4]>) -> Result<Checked, DrcError> {
    let t0 = Instant::now();
    let session = fluideda_sync::Session::from_state(state).map_err(|e| DrcError::BadState(e.to_string()))?;
    let db = parsed(store, design)?;
    let (overlay, _) = session.overlay(&db);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let report = match area {
        Some([x0, y0, x1, y1]) => {
            let a = fluideda_core::Bbox { min: fluideda_core::Point::new(x0, y0), max: fluideda_core::Point::new(x1, y1) };
            fluideda_core::check_world_tiles(&db, &overlay, rules, &fluideda_core::world_tiles(&a, fluideda_core::WORLD_TILE), threads)
        }
        None => fluideda_core::check_world(&db, &overlay, rules, threads),
    };
    Ok(Checked {
        violations: report.violations.iter().map(marker).collect(),
        // The tiles checked cover the area rounded out to whole tiles.
        area: area.map(|[x0, y0, x1, y1]| {
            let t = fluideda_core::WORLD_TILE;
            [x0.div_euclid(t) * t, y0.div_euclid(t) * t, (x1.div_euclid(t) + 1) * t, (y1.div_euclid(t) + 1) * t]
        }),
        tiles: report.tiles,
        shared: report.shared,
        edited: overlay.deleted.len(),
        added: overlay.added.len(),
        hidden: overlay.hidden.len(),
        ms: t0.elapsed().as_secs_f64() * 1e3,
        threads,
    })
}

fn marker(v: &Violation) -> Marker {
    Marker {
        kind: if v.kind == DrcKind::Width { "width" } else { "space" }.into(),
        layer: v.layer.layer,
        datatype: v.layer.datatype,
        bbox: [v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y],
    }
}
