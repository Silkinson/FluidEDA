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
/// 2: each cell also carries its placement chunk table (Phase 7.3).
/// 3: a cell's instances are inline, or *remote* — only counted here, and
/// fetched by placement chunk (a streamed design, Phase 7.3 SLICE B).
/// 4: each cell's shape chunks' looks — layer coverage — when computed
/// (SLICE C).
const VERSION: u32 = 4;

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
    let meta = write_meta(
        db,
        |cell| {
            let (c, s, l) = cell.shapes.columns();
            coords.extend_from_slice(c);
            starts.extend(s.iter().copied());
            layers.extend(l.iter().map(|&lay| pack_layer(lay)));
        },
        |_| true,
    );
    DbColumns { coords, starts, layers, meta }
}

/// Just the metadata blob — the skeleton — without copying any geometry
/// (Phase 6 SLICE 3: a design pack's skeleton; its geometry is laid out per
/// chunk instead, see [`crate::pack`]).
pub fn skeleton_meta(db: &CellDb) -> Vec<u8> {
    write_meta(db, |_| {}, |_| true)
}

/// The skeleton of a streamed design (Phase 7.3 SLICE B): like
/// [`skeleton_meta`], but a cell with more than `inline_max` instances lists
/// only their count — its placements are fetched by placement chunk
/// ([`encode_instances`] records, see [`crate::pack`]).
pub fn cell_table_meta(db: &CellDb, inline_max: usize) -> Vec<u8> {
    write_meta(db, |_| {}, |cell| cell.instances.len() <= inline_max)
}

/// A run of instances as a self-contained record: `[n u32]` then `n`
/// instances in the metadata blob's encoding (a placement chunk of a
/// streamed design).
pub fn encode_instances(insts: &[Instance]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(insts.len() as u32);
    for inst in insts {
        write_instance(&mut w, inst);
    }
    w.into_vec()
}

/// The inverse of [`encode_instances`]; every child id must be below
/// `cell_count`.
pub fn decode_instances(bytes: &[u8], cell_count: usize) -> Result<Vec<Instance>> {
    let mut r = Reader::new(bytes);
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(bytes.len() / 57 + 1));
    for _ in 0..n {
        out.push(read_instance(&mut r, cell_count)?);
    }
    if r.pos != bytes.len() {
        bail!("placement record: {} trailing bytes", bytes.len() - r.pos);
    }
    Ok(out)
}

fn write_instance(w: &mut Writer, inst: &Instance) {
    w.u32(inst.child as u32);
    w.transform(&inst.transform);
    match inst.kind {
        InstanceKind::Single => w.u8(0),
        InstanceKind::Array { cols, rows, col_step, row_step } => {
            w.u8(1);
            w.i32(cols);
            w.i32(rows);
            w.f64(col_step[0]);
            w.f64(col_step[1]);
            w.f64(row_step[0]);
            w.f64(row_step[1]);
        }
    }
}

fn read_instance(r: &mut Reader, cell_count: usize) -> Result<Instance> {
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
    if child >= cell_count {
        bail!("CellDb blob: instance of unknown cell {child}");
    }
    Ok(Instance { child, transform, kind })
}

/// The metadata blob, calling `columns` on each cell in order (where
/// [`serialize`] concatenates the geometry columns); `inline` says whether a
/// cell's instances are written out or only counted.
fn write_meta(db: &CellDb, mut columns: impl FnMut(&Cell), inline: impl Fn(&Cell) -> bool) -> Vec<u8> {
    let mut meta = Writer::new();

    meta.u32(MAGIC);
    meta.u32(VERSION);
    meta.u32(db.top as u32);
    meta.u32(db.cells.len() as u32);

    for cell in &db.cells {
        let (c, _, l) = cell.shapes.columns();
        // Per-cell column lengths — the splitter needs these on the way back.
        meta.u32(c.len() as u32); // coord count (= 2 * point count)
        meta.u32(l.len() as u32); // shape count (starts has this + 1)
        columns(cell);

        // Name.
        meta.u32(cell.name.len() as u32);
        meta.bytes(cell.name.as_bytes());

        // Instances: written out, or only counted (remote — Phase 7.3).
        meta.u32(cell.instances.len() as u32);
        if inline(cell) {
            meta.u8(0);
            for inst in &cell.instances {
                write_instance(&mut meta, inst);
            }
        } else {
            meta.u8(1);
        }

        // Chunk tables: shapes, then placements (Phase 7.3).
        for table in [&cell.chunks, &cell.inst_chunks] {
            meta.u32(table.len() as u32);
            for ch in table {
                meta.u32(ch.start);
                meta.u32(ch.end);
                meta.bbox(&ch.bbox);
            }
        }
        // Shape chunks' looks (Phase 7.3 SLICE C): none, or one per chunk —
        // a count, then (packed layer, coverage /255) pairs.
        meta.u32(cell.chunk_looks.len() as u32);
        for look in &cell.chunk_looks {
            meta.u8(look.len() as u8);
            for &(l, c) in look {
                meta.u32(pack_layer(l));
                meta.u8(c);
            }
        }
    }

    // Local bbox column (one per cell).
    for bb in db.local_bbox_column() {
        meta.bbox(bb);
    }

    meta.into_vec()
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
    let (mut db, mut load) = ProgressiveLoad::new(&meta)?;
    let mut done = Vec::new();
    while !load.step(src, usize::MAX, &[], &mut done)? {}
    for (c, store) in done {
        db.cells[c].set_shapes(store);
    }
    Ok(db)
}

/// Coordinates copied per coord-slice of one big cell (4 M × 4 B = 16 MB): the
/// unit that lets a 100 MB cell (caravel_core) span several steps.
const COORD_SLICE: usize = 4 << 20;

/// Where one cell's shapes live in the concatenated columns.
#[derive(Debug, Clone, Copy)]
struct CellSpan {
    n_coords: usize,
    n_shapes: usize,
    coord_off: usize,
    start_off: usize,
    layer_off: usize,
}

/// A cell whose coordinates are part-way through copying.
struct PartialCell {
    cell: CellId,
    coords: Vec<i32>,
}

/// Budgeted parse of the metadata blob into a skeleton `CellDb` (Phase 5c).
///
/// The blob is not small: caravel's is 24.6 MB — ~460 k instance records, most
/// of them caravel_core's standard cells — and parsing it whole was a 14 ms
/// block of its own. [`step`](Self::step) parses cells until about `max_bytes`
/// of the blob are consumed; [`finish`](Self::finish) yields the skeleton and
/// the column [`ProgressiveLoad`].
pub struct SkeletonParse {
    meta: Vec<u8>,
    pos: usize,
    top: CellId,
    cell_count: usize,
    cells: Vec<Cell>,
    spans: Vec<CellSpan>,
    offs: (usize, usize, usize),
}

impl SkeletonParse {
    /// Validate the header.
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
        if top >= cell_count && cell_count > 0 {
            bail!("CellDb blob: top cell out of range");
        }
        let pos = r.pos;
        Ok(SkeletonParse {
            meta,
            pos,
            top,
            cell_count,
            cells: Vec::with_capacity(cell_count),
            spans: Vec::with_capacity(cell_count),
            offs: (0, 0, 0),
        })
    }

    /// Parse cells until about `max_bytes` of metadata are consumed; `Ok(true)`
    /// once every cell is parsed.
    pub fn step(&mut self, max_bytes: usize) -> Result<bool> {
        let stop = self.pos.saturating_add(max_bytes);
        let mut r = Reader { buf: &self.meta, pos: self.pos };
        while self.cells.len() < self.cell_count {
            if r.pos >= stop {
                self.pos = r.pos;
                return Ok(false);
            }
            let n_coords = r.u32()? as usize;
            let n_shapes = r.u32()? as usize;
            let (coord_off, start_off, layer_off) = self.offs;
            self.spans.push(CellSpan { n_coords, n_shapes, coord_off, start_off, layer_off });
            self.offs = (coord_off + n_coords, start_off + n_shapes + 1, layer_off + n_shapes);

            let name_len = r.u32()? as usize;
            let name = String::from_utf8(r.bytes(name_len)?.to_vec())?;
            let n_inst = r.u32()? as usize;
            let remote = match r.u8()? {
                0 => false,
                1 => true,
                t => bail!("bad instance list tag {t}"),
            };
            let mut instances = Vec::with_capacity(if remote { 0 } else { n_inst.min(self.meta.len() / 57 + 1) });
            if !remote {
                for _ in 0..n_inst {
                    instances.push(read_instance(&mut r, self.cell_count)?);
                }
            }
            let chunks = read_chunks(&mut r, n_shapes)?;
            let inst_chunks = read_chunks(&mut r, n_inst)?;
            let n_looks = r.u32()? as usize;
            if n_looks != 0 && n_looks != chunks.len() {
                bail!("CellDb blob: {n_looks} chunk looks for {} chunks", chunks.len());
            }
            let mut chunk_looks = Vec::with_capacity(n_looks);
            for _ in 0..n_looks {
                let k = r.u8()? as usize;
                let mut look = Vec::with_capacity(k);
                for _ in 0..k {
                    let v = r.u32()?;
                    look.push((Layer::new((v & 0xffff) as u16, (v >> 16) as u16), r.u8()?));
                }
                chunk_looks.push(look);
            }
            let mut cell = Cell::from_parts(name, ShapeStore::new(), instances, chunks, inst_chunks);
            cell.chunk_looks = chunk_looks;
            if remote {
                cell.make_remote();
            }
            self.cells.push(cell);
        }
        self.pos = r.pos;
        Ok(true)
    }

    /// Fraction of the metadata parsed (progress readout).
    pub fn progress(&self) -> f64 {
        self.pos as f64 / self.meta.len().max(1) as f64
    }

    /// The skeleton `CellDb` (every cell's hierarchy, chunk table and bbox; no
    /// shapes) and the loader for its columns. Call once `step` returned `true`.
    pub fn finish(self) -> Result<(CellDb, ProgressiveLoad)> {
        if self.cells.len() != self.cell_count {
            bail!("CellDb skeleton finished early");
        }
        let mut r = Reader { buf: &self.meta, pos: self.pos };
        let mut local_bboxes = Vec::with_capacity(self.cell_count);
        for _ in 0..self.cell_count {
            local_bboxes.push(r.bbox()?);
        }
        let db = CellDb::from_parts(self.cells, self.top, local_bboxes);
        Ok((db, ProgressiveLoad::from_spans(self.spans)))
    }
}

/// **Progressive** rebuild of a `CellDb` (Phase 5c).
///
/// [`new`](Self::new) parses the whole metadata blob — names, instances, chunk
/// tables, local bboxes; no geometry — into a **skeleton** `CellDb` whose cells
/// have empty shape stores. The skeleton is installable at once: culling needs
/// only bboxes, chunk tables and instances, and the renderer draws every chunk
/// it cannot tessellate as a bbox proxy, so the new design's outline appears
/// the moment the worker replies. [`step`](Self::step) then copies columns a
/// bounded number of bytes at a time — in any cell order, *priority cells
/// first* (the app passes the cells on screen), a big cell's coordinates in
/// slices — and hands each finished `ShapeStore` back to be slotted in with
/// [`crate::db::Cell::set_shapes`]. One copy per cell column, straight from the
/// [`ColumnSource`]; no concatenated intermediate.
pub struct ProgressiveLoad {
    spans: Vec<CellSpan>,
    loaded: Vec<bool>,
    remaining: usize,
    /// Sequential cursor for non-priority loading.
    next: usize,
    partial: Option<PartialCell>,
    bytes_copied: usize,
    total_bytes: usize,
}

impl ProgressiveLoad {
    /// Parse the metadata blob into the skeleton `CellDb` + the load plan, in
    /// one go (see [`SkeletonParse`] for the budgeted form).
    pub fn new(meta: &[u8]) -> Result<(CellDb, ProgressiveLoad)> {
        let mut p = SkeletonParse::new(meta.to_vec())?;
        while !p.step(usize::MAX)? {}
        p.finish()
    }

    /// A loader with nothing to load (a placeholder while swapping phases).
    pub fn empty() -> ProgressiveLoad {
        ProgressiveLoad::from_spans(Vec::new())
    }

    fn from_spans(spans: Vec<CellSpan>) -> ProgressiveLoad {
        let total_bytes = spans.iter().map(|s| s.n_coords * 4 + s.n_shapes * 8).sum();
        let cell_count = spans.len();
        let mut loaded = vec![false; cell_count];
        let mut remaining = cell_count;
        // Cells with no shapes are complete already (the skeleton's empty store
        // IS their store).
        for (i, sp) in spans.iter().enumerate() {
            if sp.n_shapes == 0 && sp.n_coords == 0 {
                loaded[i] = true;
                remaining -= 1;
            }
        }
        ProgressiveLoad { spans, loaded, remaining, next: 0, partial: None, bytes_copied: 0, total_bytes }
    }

    /// Is `cell`'s geometry in place (handed out by `step`, or shapeless)?
    pub fn is_loaded(&self, cell: CellId) -> bool {
        self.loaded.get(cell).copied().unwrap_or(true)
    }

    /// Fraction of the column bytes copied (progress readout).
    pub fn progress(&self) -> f64 {
        (self.bytes_copied as f64 / self.total_bytes.max(1) as f64).min(1.0)
    }

    /// Copy up to about `max_bytes`, finishing cells into `out` as
    /// `(cell, store)`. The next cell to start is the first not-yet-loaded one
    /// in `priority`, else the next in file order. A cell already started is
    /// finished before another begins. `Ok(true)` once every cell is loaded.
    pub fn step(
        &mut self,
        src: &mut dyn ColumnSource,
        max_bytes: usize,
        priority: &[CellId],
        out: &mut Vec<(CellId, ShapeStore)>,
    ) -> Result<bool> {
        let mut budget = max_bytes;
        while self.remaining > 0 {
            if self.partial.is_none() {
                let pick = priority.iter().copied().find(|&c| c < self.loaded.len() && !self.loaded[c]).or_else(|| {
                    while self.next < self.loaded.len() && self.loaded[self.next] {
                        self.next += 1;
                    }
                    (self.next < self.loaded.len()).then_some(self.next)
                });
                let Some(cell) = pick else { break };
                self.partial = Some(PartialCell { cell, coords: Vec::with_capacity(self.spans[cell].n_coords) });
            }
            let p = self.partial.as_mut().unwrap();
            let sp = self.spans[p.cell];
            while p.coords.len() < sp.n_coords {
                if budget == 0 {
                    return Ok(false);
                }
                let n = (sp.n_coords - p.coords.len()).min(COORD_SLICE).min((budget / 4).max(1));
                src.coords_into(sp.coord_off + p.coords.len(), n, &mut p.coords)?;
                budget = budget.saturating_sub(n * 4);
                self.bytes_copied += n * 4;
            }
            let p = self.partial.take().unwrap();
            let s = src.starts(sp.start_off, sp.n_shapes + 1)?;
            let l = src.layers(sp.layer_off, sp.n_shapes)?;
            budget = budget.saturating_sub(sp.n_shapes * 8);
            self.bytes_copied += sp.n_shapes * 8;
            if s.last().copied().unwrap_or(0) as usize * 2 != p.coords.len() {
                bail!("CellDb blob: cell starts/coords mismatch");
            }
            out.push((p.cell, ShapeStore::from_columns(p.coords, s, l)));
            self.loaded[p.cell] = true;
            self.remaining -= 1;
            if budget == 0 && self.remaining > 0 {
                return Ok(false);
            }
        }
        Ok(self.remaining == 0)
    }
}

/// A chunk table: contiguous, in-range runs (checked — a malformed blob must
/// not make the cull index past the end of a cell's shapes or instances).
fn read_chunks(r: &mut Reader, total: usize) -> Result<Vec<Chunk>> {
    let n = r.u32()? as usize;
    let mut chunks = Vec::with_capacity(n.min(total + 1));
    for _ in 0..n {
        let (start, end, bbox) = (r.u32()?, r.u32()?, r.bbox()?);
        if start > end || end as usize > total {
            bail!("CellDb blob: chunk {start}..{end} outside 0..{total}");
        }
        chunks.push(Chunk { start, end, bbox });
    }
    Ok(chunks)
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

    /// Progressive (sliced, priority-ordered) reconstruction must build exactly
    /// the same CellDb as a one-shot one — including a cell whose coords span
    /// several slices — and the skeleton must be complete except for shapes.
    #[test]
    fn progressive_load_matches_one_shot() {
        let db = crate::gds::load_gds_db(FIXTURE).unwrap();
        let cols = serialize(&db);
        let mut src = VecSource { coords: &cols.coords, starts: &cols.starts, layers: &cols.layers };
        // Skeleton parsed in small budgeted steps, as the app drives it.
        let mut sp = SkeletonParse::new(cols.meta.clone()).unwrap();
        let mut sk_steps = 1;
        while !sp.step(1).unwrap() {
            sk_steps += 1;
        }
        assert_eq!(sk_steps, db.cells.len(), "a 1-byte budget parses one cell per step");
        let (mut skel, mut load) = sp.finish().unwrap();
        assert_eq!(skel.cells.len(), db.cells.len());
        assert_eq!(skel.top, db.top);
        for (a, b) in skel.cells.iter().zip(&db.cells) {
            assert_eq!(a.chunks.len(), b.chunks.len(), "skeleton carries the chunk tables");
            assert_eq!(a.instances.len(), b.instances.len(), "skeleton carries the hierarchy");
            assert!(a.shapes.is_empty(), "…but no geometry yet");
        }
        // Load the LAST cell first (a priority request), in 64-byte steps.
        let prio = [db.cells.len() - 1];
        let (mut steps, mut done) = (0, Vec::new());
        while !load.step(&mut src, 64, &prio, &mut done).unwrap() {
            steps += 1;
        }
        assert!(steps > 3, "a 64-byte budget must take several steps ({steps})");
        assert_eq!(done.first().map(|d| d.0), Some(prio[0]), "priority cell finished first");
        for (c, store) in done {
            skel.cells[c].set_shapes(store);
        }
        for (a, b) in skel.cells.iter().zip(&db.cells) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.shapes.columns(), b.shapes.columns());
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
            assert_eq!(a.inst_chunks, b.inst_chunks, "{label}: '{}' placement chunks", a.name);
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
