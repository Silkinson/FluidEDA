//! Server-side export (Phase 7.4 SLICE C): a stored design's base composited
//! with a document's edits, written as GDSII or OASIS into the blob store.
//!
//! A streamed client never holds the whole design, so it cannot write it.
//! The server has the design blob and parses it in full. The edits come
//! from a CRDT document: the client's own state, posted with the request, or
//! the state of a room bound to the design. The overlay is built by
//! `fluideda_sync::Session::overlay`, the same code the app uses, and
//! written by the same writers as an in-core "Save". So the result is the
//! file an in-core client would save, byte for byte.
//!
//! The output is a content-addressed blob. The client downloads it straight
//! from `/blobs/<hash>`, so a big export never passes through its memory.

use crate::store::Store;
use serde::Serialize;
use std::io;
use std::time::Instant;

/// What an export wrote.
#[derive(Debug, Serialize, serde::Deserialize, PartialEq)]
pub struct Exported {
    /// The output blob.
    pub hash: String,
    pub bytes: u64,
    /// `"gds"` or `"oas"`.
    pub format: String,
    /// The edits it carried: base shapes edited, shapes added, placements hidden.
    pub edited: usize,
    pub added: usize,
    pub hidden: usize,
    /// Parse + overlay + write, ms.
    pub ms: f64,
}

/// Why an export could not be made.
#[derive(Debug)]
pub enum ExportError {
    /// No such design blob.
    NoDesign,
    /// The document state does not decode.
    BadState(String),
    /// The design does not parse.
    BadDesign(String),
    Io(io::Error),
}

impl From<io::Error> for ExportError {
    fn from(e: io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// Export design blob `design` with the edits in document state `state`
/// (a v1 update) as GDSII, or OASIS when `oasis`. Blocking: parses the whole
/// design.
pub fn export(store: &Store, design: &str, state: &[u8], oasis: bool) -> Result<Exported, ExportError> {
    let t0 = Instant::now();
    if !store.has_blob(design) {
        return Err(ExportError::NoDesign);
    }
    let session = fluideda_sync::Session::from_state(state).map_err(|e| ExportError::BadState(e.to_string()))?;
    let bytes = store.read_blob(design)?;
    let db = fluideda_core::load_layout_db(&bytes).map_err(|e| ExportError::BadDesign(e.to_string()))?;
    drop(bytes);
    // In-core here, so nothing waits for a chunk.
    let (overlay, _) = session.overlay(&db);
    let out = if oasis { fluideda_core::write_oasis(&db, &overlay) } else { fluideda_core::write_gds(&db, &overlay) };
    let hash = store.put_blob(&out)?;
    let e = Exported {
        hash,
        bytes: out.len() as u64,
        format: if oasis { "oas" } else { "gds" }.to_string(),
        edited: overlay.deleted.len(),
        added: overlay.added.len(),
        hidden: overlay.hidden.len(),
        ms: t0.elapsed().as_secs_f64() * 1e3,
    };
    log::info!(
        "export {} as {}: {} edited, {} added, {} hidden → {} B in {:.0} ms",
        &design[..12],
        e.format,
        e.edited,
        e.added,
        e.hidden,
        e.bytes,
        e.ms
    );
    Ok(e)
}
