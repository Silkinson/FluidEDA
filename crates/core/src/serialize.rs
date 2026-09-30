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

/// Where the deserializer gets each cell's slice of the three columns. The
/// columns live wherever the transfer left them — in wasm memory (a `DbColumns`)
/// or, on the main thread, still in the JS `ArrayBuffer`s the worker
/// transferred (Phase 5c: single-copy reconstruction copies each cell's slice
/// straight from there into its own `Vec`, with no concatenated intermediate).
/// Offsets and lengths are in elements of the column.
pub trait ColumnSource {
    /// Append `len` coordinates (i32) starting at element `off` of the coord
    /// column to `out` (which the caller has reserved).
    fn coords_into(&mut self, off: usize, len: usize, out: &mut Vec<i32>) -> Result<()>;
    /// `len` point offsets (u32) starting at element `off` of the starts column.
    fn starts(&mut self, off: usize, len: usize) -> Result<Vec<u32>>;
    /// `len` packed layers starting at element `off` of the layer column.
    fn layers(&mut self, off: usize, len: usize) -> Result<Vec<Layer>>;
}

/// The in-memory source: slices of already-copied `Vec` columns.
struct VecSource<'a> {
    coords: &'a [i32],
    starts: &'a [u32],
    layers: &'a [u32],
}

impl ColumnSource for VecSource<'_> {
    fn coords_into(&mut self, off: usize, len: usize, out: &mut Vec<i32>) -> Result<()> {
        out.extend_from_slice(take(self.coords, off, len)?);
        Ok(())
    }
    fn starts(&mut self, off: usize, len: usize) -> Result<Vec<u32>> {
        Ok(take(self.starts, off, len)?.to_vec())
    }
    fn layers(&mut self, off: usize, len: usize) -> Result<Vec<Layer>> {
        // A packed layer u32 has the exact little-endian byte layout of `Layer`
        // (layer | datatype << 16 — see pack_layer), so on any LE target (wasm +
        // all our natives) this cast is a memcpy, not per-shape unpack calls.
        Ok(bytemuck::cast_slice::<u32, Layer>(take(self.layers, off, len)?).to_vec())
    }
}

fn take<T>(buf: &[T], off: usize, len: usize) -> Result<&[T]> {
    match off.checked_add(len).filter(|&e| e <= buf.len()) {
        Some(e) => Ok(&buf[off..e]),
        None => bail!("CellDb blob column underrun"),
    }
}

/// Rebuild a `CellDb` from the worker's transferred columns. Does no geometry
/// work — the columns and chunk tables are already final.
pub fn deserialize(cols: DbColumns) -> Result<CellDb> {
    let DbColumns { coords, starts, layers, meta } = cols;
    deserialize_from(meta, &mut VecSource { coords: &coords, starts: &starts, layers: &layers })
}

/// Rebuild a `CellDb` in one go from its metadata blob, pulling each cell's
/// column slices from `src` (see [`ColumnSource`]).
pub fn deserialize_from(meta: Vec<u8>, src: &mut dyn ColumnSource) -> Result<CellDb> {
    let mut d = Deserializer::new(meta)?;
    while !d.step(src, usize::MAX)? {}
    d.finish()
}

/// Coordinates copied per coord-slice of one big cell (4 M × 4 B = 16 MB): the
/// unit that lets a 100 MB cell (caravel_core) span several steps.
const COORD_SLICE: usize = 4 << 20;

/// A cell whose columns are part-way through copying.
struct PartialCell {
    n_coords: usize,
    n_shapes: usize,
    coords: Vec<i32>,
}

/// **Incremental** rebuild of a `CellDb` (Phase 5c): each [`step`](Self::step)
/// copies at most about `max_bytes` of columns, splitting even a single huge
/// cell's coordinate column into slices, so the main thread can spread the
/// design swap over frames instead of blocking one. Owns the metadata blob and
/// a cursor into it; the columns stay in the caller's [`ColumnSource`].
pub struct Deserializer {
    meta: Vec<u8>,
    pos: usize,
    top: CellId,
    cell_count: usize,
    cells: Vec<Cell>,
    coord_off: usize,
    start_off: usize,
    layer_off: usize,
    partial: Option<PartialCell>,
    bytes_copied: usize,
}

impl Deserializer {
    /// Validate the header. No column is touched yet.
    pub fn new(meta: Vec<u8>) -> Result<Self> {
        let mut r = Reader::new(&meta);
        if r.u32()? != MAGIC {
            bail!("bad CellDb blob: magic mismatch");
        }
        if r.u32()? != VERSION {
            bail!("unsupported CellDb blob version");
        }
        let top = r.u32()? as CellId;
        let cell_count = r.u32()? as usize;
        let pos = r.pos;
        Ok(Deserializer {
            meta,
            pos,
            top,
            cell_count,
            cells: Vec::with_capacity(cell_count),
            coord_off: 0,
            start_off: 0,
            layer_off: 0,
            partial: None,
            bytes_copied: 0,
        })
    }

    /// Copy up to about `max_bytes` more; `Ok(true)` once every cell is built.
    pub fn step(&mut self, src: &mut dyn ColumnSource, max_bytes: usize) -> Result<bool> {
        let mut budget = max_bytes;
        while self.cells.len() < self.cell_count {
            // Start the next cell: read its column lengths.
            let p = match self.partial.as_mut() {
                Some(p) => p,
                None => {
                    let mut r = Reader { buf: &self.meta, pos: self.pos };
                    let n_coords = r.u32()? as usize;
                    let n_shapes = r.u32()? as usize;
                    self.pos = r.pos;
                    self.partial.insert(PartialCell { n_coords, n_shapes, coords: Vec::with_capacity(n_coords) })
                }
            };
            // Coordinates, a slice at a time.
            while p.coords.len() < p.n_coords {
                if budget == 0 {
                    return Ok(false);
                }
                let n = (p.n_coords - p.coords.len()).min(COORD_SLICE).min((budget / 4).max(1));
                src.coords_into(self.coord_off + p.coords.len(), n, &mut p.coords)?;
                budget = budget.saturating_sub(n * 4);
                self.bytes_copied += n * 4;
            }
            // Starts + layers (8 B/shape) and the metadata finish the cell.
            let p = self.partial.take().unwrap();
            let s = src.starts(self.start_off, p.n_shapes + 1)?;
            let l = src.layers(self.layer_off, p.n_shapes)?;
            budget = budget.saturating_sub(p.n_shapes * 8);
            self.bytes_copied += p.n_shapes * 8;
            if s.last().copied().unwrap_or(0) as usize * 2 != p.coords.len() {
                bail!("CellDb blob: cell starts/coords mismatch");
            }
            self.coord_off += p.n_coords;
            self.start_off += p.n_shapes + 1;
            self.layer_off += p.n_shapes;
            let shapes = ShapeStore::from_columns(p.coords, s, l);
            let cell = self.read_cell_meta(shapes)?;
            self.cells.push(cell);
            if budget == 0 && self.cells.len() < self.cell_count {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Name, instances and chunk table of the cell whose shapes just finished.
    fn read_cell_meta(&mut self, shapes: ShapeStore) -> Result<Cell> {
        let mut r = Reader { buf: &self.meta, pos: self.pos };
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
        self.pos = r.pos;
        Ok(Cell::from_parts(name, shapes, instances, chunks))
    }

    /// Fraction of the column bytes copied so far, given the total (readout).
    pub fn bytes_copied(&self) -> usize {
        self.bytes_copied
    }

    /// Assemble the `CellDb` (call once `step` returned `true`).
    pub fn finish(self) -> Result<CellDb> {
        if self.cells.len() != self.cell_count {
            bail!("CellDb deserializer finished early");
        }
        let mut r = Reader { buf: &self.meta, pos: self.pos };
        let mut local_bboxes = Vec::with_capacity(self.cell_count);
        for _ in 0..self.cell_count {
            local_bboxes.push(r.bbox()?);
        }
        Ok(CellDb::from_parts(self.cells, self.top, local_bboxes))
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

    /// Budgeted (sliced) reconstruction must build exactly the same CellDb as a
    /// one-shot one — including a cell whose coords span several slices.
    #[test]
    fn incremental_deserialize_matches_one_shot() {
        let db = crate::gds::load_gds_db(FIXTURE).unwrap();
        let cols = serialize(&db);
        let mut src = VecSource { coords: &cols.coords, starts: &cols.starts, layers: &cols.layers };
        let mut d = Deserializer::new(cols.meta.clone()).unwrap();
        let mut steps = 0;
        while !d.step(&mut src, 64).unwrap() {
            steps += 1;
        }
        assert!(steps > 3, "a 64-byte budget must take several steps ({steps})");
        let inc = d.finish().unwrap();
        let whole = deserialize(serialize(&db)).unwrap();
        assert_eq!(inc.cells.len(), whole.cells.len());
        for (a, b) in inc.cells.iter().zip(&whole.cells) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.shapes.columns(), b.shapes.columns());
            assert_eq!(a.chunks.len(), b.chunks.len());
        }
    }

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
