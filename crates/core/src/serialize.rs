//! Binary (de)serialization of a `CellDb` — the Phase 2b STEP 2 worker transfer
//! format.
//!
//! The worker parses a GDS in its own wasm instance and ships the finished
//! `CellDb` back to the main thread. We do NOT share a heap (STEP 3 dropped the
//! parse peak to ~0.8 GB, so a parse-and-copy worker fits under 4 GB); instead the
//! `CellDb` is serialized to plain buffers, `postMessage`d as **transferable**
//! `ArrayBuffer`s (zero-copy move, not structured-clone copy), and rebuilt.
//!
//! ## Layout — three Pod columns + a metadata blob
//!
//! The bulk of a `CellDb` is the columnar geometry (caravel: ~196 MB). Those three
//! columns are emitted as their own typed arrays so the reconstruction copy on the
//! main thread is a single `wasm-bindgen` aligned memcpy per column, not a
//! per-shape traversal:
//!
//!   * `coords`  — every cell's `i32` coordinate column, concatenated.
//!   * `starts`  — every cell's `u32` offset column, concatenated (each cell's
//!                 offsets are 0-based into its own coord slice).
//!   * `layers`  — every cell's layer column, concatenated, each `Layer` packed
//!                 into one `u32` (`layer | datatype << 16`).
//!
//! Everything else — cell names, instances, chunk tables, local bboxes, and the
//! per-cell column lengths needed to split the concatenated columns back apart —
//! goes in a single little-endian `meta` blob, read with a byte cursor (no
//! alignment requirement, so it survives living in a `Vec<u8>`).
//!
//! The R-tree indices are deliberately NOT serialized: they stay lazy
//! `OnceCell`s, rebuilt on demand on the main thread exactly as for a locally
//! parsed file (Phase 1 behavior). Shipping them would be wasted bytes and work.

use crate::chunk::Chunk;
use crate::db::{Cell, CellDb, CellId, Instance, InstanceKind};
use crate::store::ShapeStore;
use crate::transform::Transform;
use crate::types::{Bbox, Layer, Point};
use anyhow::{bail, Result};

const MAGIC: u32 = u32::from_le_bytes(*b"FEDB");
const VERSION: u32 = 1;

/// A serialized `CellDb`: three Pod columns (transferred as typed arrays) plus a
/// metadata blob. Field order mirrors what the worker hands to `postMessage`.
pub struct DbColumns {
    pub coords: Vec<i32>,
    pub starts: Vec<u32>,
    pub layers: Vec<u32>,
    pub meta: Vec<u8>,
}

/// Pack a `Layer` into one `u32` whose little-endian bytes equal the `Layer`'s
/// own memory layout, so deserialize can cast the column back with no per-element
/// work (see `deserialize`).
fn pack_layer(l: Layer) -> u32 {
    l.layer as u32 | ((l.datatype as u32) << 16)
}

/// Serialize a `CellDb` into transferable columns + metadata.
pub fn serialize(db: &CellDb) -> DbColumns {
    let mut coords = Vec::new();
    let mut starts = Vec::new();
    let mut layers = Vec::new();
    let mut meta = Writer::new();

    meta.u32(MAGIC);
    meta.u32(VERSION);
    meta.u32(db.top as u32);
    meta.u32(db.cells.len() as u32);

    for cell in &db.cells {
        let (c, s, l) = cell.shapes.columns();
        // Per-cell column lengths — the splitter needs these on the way back.
        meta.u32(c.len() as u32); // coord count (= 2 * point count)
        meta.u32(l.len() as u32); // shape count (starts has this + 1)
        coords.extend_from_slice(c);
        starts.extend(s.iter().copied());
        layers.extend(l.iter().map(|&lay| pack_layer(lay)));

        // Name.
        meta.u32(cell.name.len() as u32);
        meta.bytes(cell.name.as_bytes());

        // Instances.
        meta.u32(cell.instances.len() as u32);
        for inst in &cell.instances {
            meta.u32(inst.child as u32);
            meta.transform(&inst.transform);
            match inst.kind {
                InstanceKind::Single => meta.u8(0),
                InstanceKind::Array { cols, rows, col_step, row_step } => {
                    meta.u8(1);
                    meta.i32(cols);
                    meta.i32(rows);
                    meta.f64(col_step[0]);
                    meta.f64(col_step[1]);
                    meta.f64(row_step[0]);
                    meta.f64(row_step[1]);
                }
            }
        }

        // Chunk table.
        meta.u32(cell.chunks.len() as u32);
        for ch in &cell.chunks {
            meta.u32(ch.start);
            meta.u32(ch.end);
            meta.bbox(&ch.bbox);
        }
    }

    // Local bbox column (one per cell).
    for bb in db.local_bbox_column() {
        meta.bbox(bb);
    }

    DbColumns { coords, starts, layers, meta: meta.into_vec() }
}

/// Rebuild a `CellDb` from the worker's transferred columns. Does no geometry
/// work — the columns and chunk tables are already final.
pub fn deserialize(cols: DbColumns) -> Result<CellDb> {
    let DbColumns { coords, starts, layers, meta } = cols;
    let mut r = Reader::new(&meta);

    if r.u32()? != MAGIC {
        bail!("bad CellDb blob: magic mismatch");
    }
    if r.u32()? != VERSION {
        bail!("unsupported CellDb blob version");
    }
    let top = r.u32()? as CellId;
    let cell_count = r.u32()? as usize;

    let mut cells = Vec::with_capacity(cell_count);
    let mut coord_off = 0usize;
    let mut start_off = 0usize;
    let mut layer_off = 0usize;

    for _ in 0..cell_count {
        let n_coords = r.u32()? as usize;
        let n_shapes = r.u32()? as usize;

        // Slice this cell's columns out of the concatenated buffers (memcpy each,
        // not an element loop — the reconstruction runs on the main thread and
        // must stay inside a frame).
        let c = slice(&coords, &mut coord_off, n_coords)?.to_vec();
        // `starts` holds n_shapes + 1 entries per cell.
        let s = slice(&starts, &mut start_off, n_shapes + 1)?.to_vec();
        // A packed layer u32 has the exact little-endian byte layout of `Layer`
        // (layer | datatype << 16 — see pack_layer), so on any LE target (wasm +
        // all our natives) this cast is a memcpy, not 3.9 M unpack calls.
        let lu = slice(&layers, &mut layer_off, n_shapes)?;
        let l = bytemuck::cast_slice::<u32, Layer>(lu).to_vec();
        let shapes = ShapeStore::from_columns(c, s, l);

        let name_len = r.u32()? as usize;
        let name = String::from_utf8(r.bytes(name_len)?.to_vec())?;

        let n_inst = r.u32()? as usize;
        let mut instances = Vec::with_capacity(n_inst);
        for _ in 0..n_inst {
            let child = r.u32()? as CellId;
            let transform = r.transform()?;
            let kind = match r.u8()? {
                0 => InstanceKind::Single,
                1 => InstanceKind::Array {
                    cols: r.i32()?,
                    rows: r.i32()?,
                    col_step: [r.f64()?, r.f64()?],
                    row_step: [r.f64()?, r.f64()?],
                },
                t => bail!("bad instance kind tag {t}"),
            };
            instances.push(Instance { child, transform, kind });
        }

        let n_chunks = r.u32()? as usize;
        let mut chunks = Vec::with_capacity(n_chunks);
        for _ in 0..n_chunks {
            let start = r.u32()?;
            let end = r.u32()?;
            let bbox = r.bbox()?;
            chunks.push(Chunk { start, end, bbox });
        }

        cells.push(Cell::from_parts(name, shapes, instances, chunks));
    }

    let mut local_bboxes = Vec::with_capacity(cell_count);
    for _ in 0..cell_count {
        local_bboxes.push(r.bbox()?);
    }

    Ok(CellDb::from_parts(cells, top, local_bboxes))
}

/// Advance `off` by `n` and return the slice, erroring if it runs past the end.
fn slice<'a, T>(buf: &'a [T], off: &mut usize, n: usize) -> Result<&'a [T]> {
    let end = off.checked_add(n).filter(|&e| e <= buf.len());
    match end {
        Some(e) => {
            let s = &buf[*off..e];
            *off = e;
            Ok(s)
        }
        None => bail!("CellDb blob column underrun"),
    }
}

// --- little-endian byte cursor ----------------------------------------------

struct Writer(Vec<u8>);
impl Writer {
    fn new() -> Self {
        Writer(Vec::new())
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn transform(&mut self, t: &Transform) {
        for v in [t.a, t.b, t.c, t.d, t.tx, t.ty] {
            self.f64(v);
        }
    }
    fn bbox(&mut self, b: &Bbox) {
        for v in [b.min.x, b.min.y, b.max.x, b.max.y] {
            self.i32(v);
        }
    }
    fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(e) => {
                let s = &self.buf[self.pos..e];
                self.pos = e;
                Ok(s)
            }
            None => bail!("CellDb blob meta underrun"),
        }
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }
    fn transform(&mut self) -> Result<Transform> {
        Ok(Transform {
            a: self.f64()?,
            b: self.f64()?,
            c: self.f64()?,
            d: self.f64()?,
            tx: self.f64()?,
            ty: self.f64()?,
        })
    }
    fn bbox(&mut self) -> Result<Bbox> {
        Ok(Bbox {
            min: Point::new(self.i32()?, self.i32()?),
            max: Point::new(self.i32()?, self.i32()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
    const FIXTURE: &[u8] = include_bytes!("../../../assets/edit_fixture.gds");

    /// A round-tripped CellDb must be identical in everything the renderer reads —
    /// the whole point of the worker transfer. Compares cells/shapes/instances/
    /// chunks/bboxes/top, the same surface the gds21 oracle checks.
    fn assert_round_trip(bytes: &[u8], label: &str) {
        let db = crate::gds::load_gds_db(bytes).unwrap();
        let cols = serialize(&db);
        let db2 = deserialize(cols).unwrap();

        assert_eq!(db.cells.len(), db2.cells.len(), "{label}: cell count");
        assert_eq!(db.top, db2.top, "{label}: top");
        for id in 0..db.cells.len() {
            let (a, b) = (&db.cells[id], &db2.cells[id]);
            assert_eq!(a.name, b.name, "{label}: cell {id} name");
            assert_eq!(a.shapes.len(), b.shapes.len(), "{label}: '{}' shape count", a.name);
            for i in 0..a.shapes.len() {
                assert_eq!(a.shapes.layer(i), b.shapes.layer(i), "{label}: '{}' shape {i} layer", a.name);
                assert_eq!(a.shapes.points(i), b.shapes.points(i), "{label}: '{}' shape {i} pts", a.name);
            }
            assert_eq!(a.instances.len(), b.instances.len(), "{label}: '{}' inst count", a.name);
            for (j, (x, y)) in a.instances.iter().zip(&b.instances).enumerate() {
                assert_eq!(x.child, y.child, "{label}: '{}' inst {j} child", a.name);
                assert_eq!(x.transform, y.transform, "{label}: '{}' inst {j} transform", a.name);
                match (x.kind, y.kind) {
                    (InstanceKind::Single, InstanceKind::Single) => {}
                    (
                        InstanceKind::Array { cols: c1, rows: r1, col_step: cs1, row_step: rs1 },
                        InstanceKind::Array { cols: c2, rows: r2, col_step: cs2, row_step: rs2 },
                    ) => assert_eq!((c1, r1, cs1, rs1), (c2, r2, cs2, rs2), "{label}: '{}' inst {j} array", a.name),
                    _ => panic!("{label}: '{}' inst {j} kind mismatch", a.name),
                }
            }
            assert_eq!(a.chunks.len(), b.chunks.len(), "{label}: '{}' chunk count", a.name);
            for (j, (x, y)) in a.chunks.iter().zip(&b.chunks).enumerate() {
                assert_eq!((x.start, x.end), (y.start, y.end), "{label}: '{}' chunk {j} range", a.name);
                assert_eq!(
                    (x.bbox.min, x.bbox.max),
                    (y.bbox.min, y.bbox.max),
                    "{label}: '{}' chunk {j} bbox",
                    a.name
                );
            }
            assert_eq!(db.local_bbox(id).min, db2.local_bbox(id).min, "{label}: {id} local bbox min");
            assert_eq!(db.local_bbox(id).max, db2.local_bbox(id).max, "{label}: {id} local bbox max");
        }
    }

    #[test]
    fn round_trip_hier() {
        assert_round_trip(HIER, "hier");
    }

    #[test]
    fn round_trip_fixture() {
        assert_round_trip(FIXTURE, "fixture");
    }

    #[test]
    fn round_trip_sky130_if_present() {
        if let Ok(bytes) = std::fs::read("../../testdata/sky130_fd_sc_hd_merged.gds") {
            assert_round_trip(&bytes, "sky130");
        }
    }
}
