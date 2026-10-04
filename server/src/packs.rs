//! Design packs on the server (Phase 6 SLICE 3): a stored design, parsed once
//! and laid out for streaming ([`fluideda_core::pack`]).
//!
//! `GET /packs/<design sha256>` returns the pack's manifest, building it the
//! first time: the design blob is parsed natively (no wasm memory ceiling
//! here), and its skeleton, per-chunk geometry and index are stored as
//! ordinary content-addressed blobs. Clients then fetch them from `/blobs/` —
//! immutable, so a CDN can cache every byte, geometry byte ranges included.
//! The manifest itself is cached under `packs/<design>.v<PACK_VERSION>.json`.

use crate::store::{hex, Store};
use fluideda_core::pack::{self, PACK_VERSION};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

/// A blob a pack refers to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub hash: String,
    pub bytes: u64,
}

/// What a client needs to stream a design.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The design blob this pack was built from.
    pub design: String,
    pub pack: u32,
    /// The shape-id scheme of the parser that built it.
    pub ids: u32,
    /// The cell table (v3): the skeleton, with big placement lists remote.
    pub skeleton: BlobRef,
    pub geometry: BlobRef,
    pub index: BlobRef,
    /// Placement records of the remote lists, and their byte offsets (v3).
    pub placements: BlobRef,
    pub placement_index: BlobRef,
    /// Cells with more placements than this are remote.
    pub inline_placements: usize,
    pub cells: usize,
    pub chunks: usize,
    /// Distinct polygons (shapes stored in cells).
    pub shapes: u64,
    /// Polygons with the hierarchy flattened.
    pub flat_shapes: u64,
    /// `[min x, min y, max x, max y]`, nm.
    pub world_bbox: [f64; 4],
    /// Every layer, packed `layer | datatype << 16`.
    pub layers: Vec<u32>,
    /// Per cell: how many times it is placed in the whole design, and how
    /// many shapes its subtree flattens to (v5). A streamed client decides
    /// "edit this placement only" with them: it cannot count placements it
    /// has not fetched.
    pub placement_counts: Vec<u64>,
    pub subtree_shapes: Vec<u64>,
    pub build_ms: f64,
}

fn manifest_path(store: &Store, design: &str) -> PathBuf {
    store.root().join("packs").join(format!("{design}.v{PACK_VERSION}.json"))
}

/// The pack of design blob `design` — built now if this server has not built
/// it yet. `None` if there is no such design blob. Blocking (a first build
/// parses the design); callers serialize builds.
pub fn ensure(store: &Store, design: &str) -> io::Result<Option<Manifest>> {
    let path = manifest_path(store, design);
    if let Ok(text) = fs::read_to_string(&path) {
        match serde_json::from_str::<Manifest>(&text) {
            Ok(m)
                if [&m.skeleton, &m.geometry, &m.index, &m.placements, &m.placement_index]
                    .iter()
                    .all(|b| store.has_blob(&b.hash)) =>
            {
                return Ok(Some(m))
            }
            _ => log::warn!("pack {}: cached manifest unusable; rebuilding", &design[..12]),
        }
    }
    if !store.has_blob(design) {
        return Ok(None);
    }
    let m = build(store, design)?;
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = store.tmp_path();
    fs::write(&tmp, serde_json::to_vec_pretty(&m).map_err(io::Error::other)?)?;
    fs::rename(&tmp, &path)?;
    Ok(Some(m))
}

fn build(store: &Store, design: &str) -> io::Result<Manifest> {
    let t0 = Instant::now();
    let bytes = store.read_blob(design)?;
    let mut db = fluideda_core::load_layout_db(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    drop(bytes);
    let parse_ms = t0.elapsed().as_secs_f64() * 1e3;
    // Each shape chunk's look (its layers' coverage): what a chunk too dense
    // to fetch is drawn as (v4).
    pack::annotate_chunk_looks(&mut db);

    let skeleton = fluideda_core::serialize::cell_table_meta(&db, pack::INLINE_PLACEMENTS);
    let skeleton = BlobRef { bytes: skeleton.len() as u64, hash: store.put_blob(&skeleton)? };

    let (geometry, offsets) = write_streamed(store, |w| pack::write_geometry(&db, w))?;
    let index = pack::index_bytes(&offsets);
    let index = BlobRef { bytes: index.len() as u64, hash: store.put_blob(&index)? };

    let (placements, p_offsets) = write_streamed(store, |w| pack::write_placements(&db, pack::INLINE_PLACEMENTS, w))?;
    let placement_index = pack::index_bytes(&p_offsets);
    let placement_index = BlobRef { bytes: placement_index.len() as u64, hash: store.put_blob(&placement_index)? };

    let wb = db.world_bbox();
    let m = Manifest {
        design: design.to_string(),
        pack: PACK_VERSION,
        ids: fluideda_core::SHAPE_ID_SCHEME,
        skeleton,
        geometry,
        index,
        placements,
        placement_index,
        inline_placements: pack::INLINE_PLACEMENTS,
        cells: db.cells.len(),
        chunks: offsets.len() - 1,
        shapes: db.cells.iter().map(|c| c.shapes.len() as u64).sum(),
        flat_shapes: db.flat_shape_count(),
        world_bbox: [wb.min_x, wb.min_y, wb.max_x, wb.max_y],
        layers: db.all_layers().iter().map(|l| l.layer as u32 | (l.datatype as u32) << 16).collect(),
        placement_counts: fluideda_core::placement_counts(&db),
        subtree_shapes: fluideda_core::subtree_shapes(&db),
        build_ms: t0.elapsed().as_secs_f64() * 1e3,
    };
    log::info!(
        "pack {}: {} cells, {} chunks, {} shapes; parse {:.0} ms, total {:.0} ms; cell table {} B, geometry {} B, placements {} B",
        &design[..12],
        m.cells,
        m.chunks,
        m.shapes,
        parse_ms,
        m.build_ms,
        m.skeleton.bytes,
        m.geometry.bytes,
        m.placements.bytes
    );
    Ok(m)
}

/// Write a blob through `write` to a temp file while hashing it — never
/// whole in memory — and store it. Returns the blob and what `write` returned.
fn write_streamed<T>(store: &Store, write: impl FnOnce(&mut HashingWriter<BufWriter<File>>) -> io::Result<T>) -> io::Result<(BlobRef, T)> {
    let tmp = store.tmp_path();
    let mut w = HashingWriter { out: BufWriter::with_capacity(1 << 20, File::create(&tmp)?), hash: Sha256::new(), n: 0 };
    let value = write(&mut w)?;
    let HashingWriter { out, hash, n } = w;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    let blob = BlobRef { hash: hex(&hash.finalize()), bytes: n };
    store.commit_blob(&tmp, &blob.hash)?;
    Ok((blob, value))
}

/// Writes through to `out` while hashing and counting.
struct HashingWriter<W: Write> {
    out: W,
    hash: Sha256,
    n: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let k = self.out.write(buf)?;
        self.hash.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}
