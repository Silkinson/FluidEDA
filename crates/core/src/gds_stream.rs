//! Streaming GDSII reader — a hand-rolled record parser that builds the columnar
//! `CellDb` **incrementally**, never materialising the whole file's structured
//! form.
//!
//! ## Why this exists
//!
//! `gds21` parses the entire library into an in-memory `GdsLibrary` (every
//! boundary an enum + `Vec<GdsPoint>` + optional fields) before we convert it to
//! our compact model. On caravel that intermediate is ~1.4 GB of the 1.68 GB wasm
//! heap — the columnar `CellDb` it converts *to* is only ~196 MB. Phase 2b step 1
//! showed parse is both 92% of leaf TTFF and essentially all of the heap, and
//! that a compact model does not cost the memory. This reader removes the
//! intermediate: each record is decoded, appended straight into the current
//! cell's columnar store, and dropped. Peak parse memory becomes file bytes +
//! the columnar store, not file bytes + a clone + the full library.
//!
//! ## Format (this is the whole of GDSII's framing)
//!
//! A GDSII file is a flat sequence of records. Each record is:
//!
//! ```text
//!   u16  length   (big-endian, INCLUDES this 4-byte header)
//!   u8   rtype    (record type, e.g. 0x08 = BOUNDARY)
//!   u8   dtype    (data type: 0=none 1=bitarr 2=i16 3=i32 5=real 6=ascii)
//!   ...  payload  (length - 4 bytes)
//! ```
//!
//! Elements (BOUNDARY/SREF/AREF/…) are opened by their record and closed by
//! ENDEL; the fields in between (LAYER, XY, SNAME, STRANS, …) arrive as their own
//! records in no guaranteed order, so we accumulate into [`ElemAcc`] and commit
//! on ENDEL.
//!
//! ## Cell-id parity with gds21 (load-bearing)
//!
//! The renderer draws cell-major, and the pipeline alpha-blends, so the id
//! assigned to each cell affects the final image when cells on different layers
//! overlap. gds21 interns every struct name in file order first, then fills. We
//! do the same in two passes over the bytes (pass 1 reads only STRNAME; pass 2
//! builds), so a given file yields byte-identical cell ids either way — which is
//! what keeps the golden image stable across the parser swap. The
//! [`crate::gds`] oracle test asserts this cell-for-cell against gds21.
//!
//! ## What we read vs skip (same set as the gds21 path)
//!
//! Read: BOUNDARY, SREF, AREF (with STRANS/MAG/ANGLE and AREF COLROW + 3-point
//! XY), and (Phase 6) PATH, converted to its outline polygon
//! ([`crate::path`]). Skipped, but correctly consumed to ENDEL so nothing
//! downstream is misaligned: TEXT, NODE, BOX.

use crate::db::{CellDb, CellDbBuilder, Instance, InstanceKind};
use crate::transform::Transform;
use crate::types::{Layer, Point};
use anyhow::{anyhow, bail, Result};

// --- record types we care about ---------------------------------------------
const ENDLIB: u8 = 0x04;
const STRNAME: u8 = 0x06;
const ENDSTR: u8 = 0x07;
const BOUNDARY: u8 = 0x08;
const PATH: u8 = 0x09;
const SREF: u8 = 0x0A;
const AREF: u8 = 0x0B;
const TEXT: u8 = 0x0C;
const LAYER: u8 = 0x0D;
const DATATYPE: u8 = 0x0E;
const XY: u8 = 0x10;
const ENDEL: u8 = 0x11;
const SNAME: u8 = 0x12;
const COLROW: u8 = 0x13;
const NODE: u8 = 0x15;
const BOX: u8 = 0x2D;
const STRANS: u8 = 0x1A;
const MAG: u8 = 0x1B;
const ANGLE: u8 = 0x1C;
const WIDTH: u8 = 0x0F;
const PATHTYPE: u8 = 0x21;
const BGNEXTN: u8 = 0x30;
const ENDEXTN: u8 = 0x31;

/// One decoded record: type byte + a borrowed payload slice (no copy).
struct Record<'a> {
    rtype: u8,
    payload: &'a [u8],
}

/// Where the reader gets a file's bytes (Phase 7.9): a slice, or anything
/// that can copy a range out — the parse worker's JS `ArrayBuffer`, or a
/// file on the server — so the file never has to sit whole in memory.
pub trait ByteSource {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copy `buf.len()` bytes starting at `off` (the range is in bounds).
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<()>;
}

impl ByteSource for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let o = off as usize;
        buf.copy_from_slice(&self[o..o + buf.len()]);
        Ok(())
    }
}

/// How much of a [`ByteSource`] the reader holds at once.
const WINDOW: usize = 1 << 20;

/// What [`Records`] reads (Phase 7.9): a whole file in place, or a window
/// onto a [`ByteSource`]. The reader is generic over it, so reading a slice
/// costs what it did before windows existed.
trait Window {
    /// The bytes in the window.
    fn bytes(&self) -> &[u8];
    /// File offset of the window's first byte.
    fn base(&self) -> u64;
    /// Make at least `n` bytes from `pos` available, or all that remain;
    /// returns where `pos` now is in the window.
    fn refill(&mut self, pos: usize, n: usize) -> Result<usize>;
}

impl Window for &[u8] {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn base(&self) -> u64 {
        0
    }
    fn refill(&mut self, pos: usize, _: usize) -> Result<usize> {
        Ok(pos) // the whole file is here already
    }
}

/// A window of about `size` bytes onto a [`ByteSource`] (a record is at most
/// 64 KiB; the window grows to fit one).
struct SourceWindow<'s> {
    src: &'s mut dyn ByteSource,
    len: u64,
    size: usize,
    /// File offset of `buf[0]`.
    base: u64,
    buf: Vec<u8>,
}

impl<'s> SourceWindow<'s> {
    fn new(src: &'s mut dyn ByteSource, size: usize) -> Self {
        let len = src.len();
        Self { src, len, size, base: 0, buf: Vec::new() }
    }
}

impl Window for SourceWindow<'_> {
    fn bytes(&self) -> &[u8] {
        &self.buf
    }
    fn base(&self) -> u64 {
        self.base
    }
    #[cold]
    fn refill(&mut self, pos: usize, n: usize) -> Result<usize> {
        self.buf.drain(..pos);
        self.base += pos as u64;
        let have = self.buf.len();
        let left = self.len - (self.base + have as u64);
        let more = (self.size.max(n).saturating_sub(have) as u64).min(left) as usize;
        if more > 0 {
            self.buf.resize(have + more, 0);
            self.src.read_at(self.base + have as u64, &mut self.buf[have..])?;
        }
        Ok(0)
    }
}

/// Cursor over a file's bytes, yielding one [`Record`] at a time.
struct Records<W> {
    win: W,
    /// Read position in the window.
    pos: usize,
}

impl<W: Window> Records<W> {
    fn new(win: W) -> Self {
        Self { win, pos: 0 }
    }

    /// Have at least `n` bytes from `pos` in the window, or all that remain.
    #[inline]
    fn want(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.win.bytes().len() {
            self.pos = self.win.refill(self.pos, n)?;
        }
        Ok(())
    }

    /// Next record, or `Ok(None)` at a clean end (ENDLIB or exhausted bytes).
    /// GDS files are commonly zero-padded to a 2048-byte block after ENDLIB, so
    /// the caller stops on ENDLIB and never sees that padding.
    #[inline(always)]
    fn next(&mut self) -> Result<Option<Record<'_>>> {
        self.want(4)?;
        let (pos, data) = (self.pos, self.win.bytes());
        if pos + 4 > data.len() {
            return Ok(None); // no room for another header — treat as end
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 4 {
            bail!("GDS record at byte {} has invalid length {len}", self.win.base() + pos as u64);
        }
        self.want(len)?;
        let (start, data) = (self.pos, self.win.bytes());
        if start + len > data.len() {
            bail!(
                "GDS record at byte {} claims length {len} but only {} bytes remain",
                self.win.base() + start as u64,
                data.len() - start
            );
        }
        self.pos = start + len;
        Ok(Some(Record { rtype: data[start + 2], payload: &data[start + 4..start + len] }))
    }
}

/// Parse GDSII bytes into a hierarchical cell database, streaming (see module
/// docs). Chunking + bbox finalisation is the caller's job via
/// [`CellDbBuilder::build`], run after the reader returns.
pub fn stream_gds_db(bytes: &[u8]) -> Result<CellDb> {
    let mut b = CellDbBuilder::new();
    intern_struct_names(Records::new(bytes), &mut b)?; // pass 1: file-order ids, matching gds21
    fill(Records::new(bytes), &mut b)?; // pass 2: shapes + instances
    b.build()
}

/// [`stream_gds_db`] from any [`ByteSource`], read through a window (Phase
/// 7.9): the parse worker reads the file out of its JS buffer twice (one
/// pass for names, one to fill) instead of copying it into wasm memory.
pub fn stream_gds_db_from(src: &mut dyn ByteSource) -> Result<CellDb> {
    stream_gds_db_windowed(src, WINDOW)
}

fn stream_gds_db_windowed(src: &mut dyn ByteSource, window: usize) -> Result<CellDb> {
    let mut b = CellDbBuilder::new();
    intern_struct_names(Records::new(SourceWindow::new(&mut *src, window)), &mut b)?;
    fill(Records::new(SourceWindow::new(src, window)), &mut b)?;
    b.build()
}

/// Pass 1: intern every struct name in file order. Reads only STRNAME payloads;
/// every other record is skipped by length, so this is a cheap linear scan.
fn intern_struct_names<W: Window>(mut recs: Records<W>, b: &mut CellDbBuilder) -> Result<()> {
    while let Some(r) = recs.next()? {
        match r.rtype {
            STRNAME => {
                b.intern(read_name(r.payload)?);
            }
            ENDLIB => break,
            _ => {}
        }
    }
    Ok(())
}

/// Pass 2: walk the records, accumulating each element and committing it into
/// its cell on ENDEL.
fn fill<W: Window>(mut recs: Records<W>, b: &mut CellDbBuilder) -> Result<()> {
    let mut current: Option<usize> = None; // current struct's cell id
    let mut acc = ElemAcc::default();

    while let Some(r) = recs.next()? {
        match r.rtype {
            STRNAME => current = Some(b.intern(read_name(r.payload)?)),
            ENDSTR => current = None,

            // Element openers: reset the accumulator and record which kind.
            BOUNDARY => acc.begin(Kind::Boundary),
            SREF => acc.begin(Kind::Sref),
            AREF => acc.begin(Kind::Aref),
            // Phase 6: paths are converted to their outline polygons.
            PATH => acc.begin(Kind::Path),
            // Consumed but not drawn — accumulator kind stays `Skip`, so ENDEL
            // discards them. Still parsed record-by-record so we stay aligned.
            TEXT | NODE | BOX => acc.begin(Kind::Skip),

            // Element fields.
            LAYER => acc.layer = read_i16(r.payload, "LAYER")? as u16,
            DATATYPE => acc.datatype = read_i16(r.payload, "DATATYPE")? as u16,
            XY => read_i32_into(r.payload, &mut acc.xy, "XY")?,
            SNAME => acc.child = Some(b.intern(read_name(r.payload)?)),
            COLROW => {
                acc.cols = read_i16(&r.payload[0..2], "COLROW cols")? as i32;
                acc.rows = read_i16(&r.payload[2..4], "COLROW rows")? as i32;
            }
            STRANS => acc.reflect = read_u16(r.payload, "STRANS")? & 0x8000 != 0,
            MAG => acc.mag = read_real(r.payload, "MAG")?,
            ANGLE => acc.angle = read_real(r.payload, "ANGLE")?,
            WIDTH => acc.width = read_i32(r.payload, "WIDTH")?,
            PATHTYPE => acc.pathtype = read_i16(r.payload, "PATHTYPE")?,
            BGNEXTN => acc.bgnextn = read_i32(r.payload, "BGNEXTN")?,
            ENDEXTN => acc.endextn = read_i32(r.payload, "ENDEXTN")?,

            ENDEL => {
                let cell = current.ok_or_else(|| anyhow!("element outside any struct"))?;
                acc.commit(b, cell)?;
            }

            ENDLIB => break,
            _ => {} // HEADER/BGNLIB/LIBNAME/UNITS/BGNSTR/PLEX/ELFLAGS/props: ignore
        }
    }
    Ok(())
}

/// Which element is being accumulated.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    None,
    Skip,
    Boundary,
    Sref,
    Aref,
    Path,
}

/// Fields of the element currently being read, committed on ENDEL. Reused across
/// elements (the `xy` buffer is cleared, not reallocated), so a whole file's
/// worth of elements costs one element's transient memory.
struct ElemAcc {
    kind: Kind,
    layer: u16,
    datatype: u16,
    xy: Vec<i32>,
    child: Option<usize>,
    reflect: bool,
    mag: f64,
    angle: f64,
    cols: i32,
    rows: i32,
    width: i32,
    pathtype: i16,
    bgnextn: i32,
    endextn: i32,
}

impl Default for ElemAcc {
    fn default() -> Self {
        Self {
            kind: Kind::None,
            layer: 0,
            datatype: 0,
            xy: Vec::new(),
            child: None,
            reflect: false,
            mag: 1.0,
            angle: 0.0,
            cols: 0,
            rows: 0,
            width: 0,
            pathtype: 0,
            bgnextn: 0,
            endextn: 0,
        }
    }
}

impl ElemAcc {
    /// Reset for a new element (STRANS defaults reapplied: no reflect, mag 1,
    /// angle 0 — matches gds21's `unwrap_or` defaults when a field is absent).
    fn begin(&mut self, kind: Kind) {
        self.kind = kind;
        self.layer = 0;
        self.datatype = 0;
        self.xy.clear();
        self.child = None;
        self.reflect = false;
        self.mag = 1.0;
        self.angle = 0.0;
        self.cols = 0;
        self.rows = 0;
        self.width = 0;
        self.pathtype = 0;
        self.bgnextn = 0;
        self.endextn = 0;
    }

    fn points(&self) -> impl Iterator<Item = Point> + '_ {
        self.xy.chunks_exact(2).map(|c| Point::new(c[0], c[1]))
    }

    /// Commit the accumulated element into `cell`.
    fn commit(&mut self, b: &mut CellDbBuilder, cell: usize) -> Result<()> {
        match self.kind {
            Kind::Boundary => {
                // Match the gds21 path: keep every point verbatim, including the
                // repeated closing point, and drop rings with < 3 points.
                if self.xy.len() / 2 >= 3 {
                    let layer = Layer::new(self.layer, self.datatype);
                    let points: Vec<Point> = self.points().collect();
                    b.cell_mut(cell).shapes.push(layer, points);
                }
            }
            Kind::Sref => {
                let child = self.child.ok_or_else(|| anyhow!("SREF without SNAME"))?;
                if self.xy.len() < 2 {
                    bail!("SREF without an XY origin");
                }
                let (x, y) = (self.xy[0] as f64, self.xy[1] as f64);
                let transform = Transform::from_strans(x, y, self.angle, self.reflect, self.mag);
                b.cell_mut(cell).instances.push(Instance::new(child, transform, InstanceKind::Single));
            }
            Kind::Aref => {
                let child = self.child.ok_or_else(|| anyhow!("AREF without SNAME"))?;
                if self.xy.len() < 6 {
                    bail!("AREF needs a 3-point XY, got {} coords", self.xy.len());
                }
                // GDSII AREF three-point xy: p0 = element [0,0]; p1 encodes the
                // TOTAL column span, p2 the TOTAL row span. Per-step vectors are
                // (p1-p0)/cols and (p2-p0)/rows — dividing by cols/rows, not using
                // p1/p2 as steps, is the classic AREF bug.
                let (p0x, p0y) = (self.xy[0], self.xy[1]);
                let (p1x, p1y) = (self.xy[2], self.xy[3]);
                let (p2x, p2y) = (self.xy[4], self.xy[5]);
                let cols = self.cols;
                let rows = self.rows;
                let col_step = [
                    (p1x - p0x) as f64 / cols.max(1) as f64,
                    (p1y - p0y) as f64 / cols.max(1) as f64,
                ];
                let row_step = [
                    (p2x - p0x) as f64 / rows.max(1) as f64,
                    (p2y - p0y) as f64 / rows.max(1) as f64,
                ];
                let transform =
                    Transform::from_strans(p0x as f64, p0y as f64, self.angle, self.reflect, self.mag);
                b.cell_mut(cell).instances.push(Instance::new(
                    child,
                    transform,
                    InstanceKind::Array { cols, rows, col_step, row_step },
                ));
            }
            Kind::Path => {
                let center: Vec<Point> = self.points().collect();
                if let Some(ring) =
                    crate::path::gds_path_polygon(&center, self.width, self.pathtype, self.bgnextn, self.endextn)
                {
                    b.cell_mut(cell).shapes.push(Layer::new(self.layer, self.datatype), ring);
                }
            }
            Kind::Skip | Kind::None => {}
        }
        self.kind = Kind::None;
        Ok(())
    }
}

// --- payload decoders --------------------------------------------------------

/// A struct/reference name: ASCII, with one optional trailing NUL pad. gds21
/// strips exactly one trailing 0x00; we match so interned names are identical.
fn read_name(payload: &[u8]) -> Result<&str> {
    let end = if payload.last() == Some(&0) {
        payload.len() - 1
    } else {
        payload.len()
    };
    std::str::from_utf8(&payload[..end]).map_err(|e| anyhow!("non-UTF8 GDS name: {e}"))
}

fn read_i16(payload: &[u8], what: &str) -> Result<i16> {
    if payload.len() < 2 {
        bail!("{what} record too short ({} bytes)", payload.len());
    }
    Ok(i16::from_be_bytes([payload[0], payload[1]]))
}

fn read_i32(payload: &[u8], what: &str) -> Result<i32> {
    let b: [u8; 4] = payload
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow!("{what}: payload too short"))?;
    Ok(i32::from_be_bytes(b))
}

fn read_u16(payload: &[u8], what: &str) -> Result<u16> {
    Ok(read_i16(payload, what)? as u16)
}

/// Append all i32s in `payload` to `out` (cleared first). Used for XY.
fn read_i32_into(payload: &[u8], out: &mut Vec<i32>, what: &str) -> Result<()> {
    if payload.len() % 4 != 0 {
        bail!("{what} record length {} is not a multiple of 4", payload.len());
    }
    out.clear();
    out.reserve(payload.len() / 4);
    for c in payload.chunks_exact(4) {
        out.push(i32::from_be_bytes([c[0], c[1], c[2], c[3]]));
    }
    Ok(())
}

/// 8-byte GDSII real: sign bit, 7-bit excess-64 base-16 exponent, 56-bit
/// mantissa. value = ±(mantissa / 2^56) · 16^(exp-64). Used for MAG and ANGLE.
fn read_real(payload: &[u8], what: &str) -> Result<f64> {
    if payload.len() < 8 {
        bail!("{what} real record too short ({} bytes)", payload.len());
    }
    let sign = if payload[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exp = (payload[0] & 0x7f) as i32 - 64;
    let mut mant: u64 = 0;
    for &byte in &payload[1..8] {
        mant = (mant << 8) | byte as u64;
    }
    Ok(sign * (mant as f64) / ((1u64 << 56) as f64) * 16f64.powi(exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gds_real_decodes_common_values() {
        // 1.0 = mantissa 0x10_00000000000000 (1/16), exponent 65 (16^1).
        let one = [0x41, 0x10, 0, 0, 0, 0, 0, 0];
        assert!((read_real(&one, "t").unwrap() - 1.0).abs() < 1e-12);
        // 0.0 is all-zero bytes.
        assert_eq!(read_real(&[0u8; 8], "t").unwrap(), 0.0);
        // 90.0 (a common ANGLE): 0x42_5A00000000000000 -> 90/256 * 16^2.
        let ninety = [0x42, 0x5A, 0, 0, 0, 0, 0, 0];
        assert!((read_real(&ninety, "t").unwrap() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn read_name_strips_one_trailing_nul() {
        assert_eq!(read_name(b"LEAF\x00").unwrap(), "LEAF");
        assert_eq!(read_name(b"ROW").unwrap(), "ROW");
        // Only ONE trailing nul is padding; interior/extra bytes are kept.
        assert_eq!(read_name(b"A\x00").unwrap(), "A");
    }

    #[test]
    fn truncated_record_errors_not_panics() {
        // length says 12 but only 6 bytes present.
        let bytes = [0x00, 0x0C, 0x08, 0x00, 0xAA, 0xBB];
        assert!(Records::new(&bytes[..]).next().is_err());
        let mut src: &[u8] = &bytes;
        assert!(Records::new(SourceWindow::new(&mut src, 4)).next().is_err());
    }

    /// Phase 7.9: reading through a window — here 64 bytes, so it refills
    /// every few records and grows for the longer ones — builds exactly what
    /// reading the slice in place does.
    #[test]
    fn windowed_read_matches_in_place() {
        let bytes = include_bytes!("../../../assets/hier.gds");
        let whole = crate::serialize(&stream_gds_db(bytes).unwrap());
        let mut src: &[u8] = bytes;
        let windowed = crate::serialize(&stream_gds_db_windowed(&mut src, 64).unwrap());
        assert_eq!(whole.coords, windowed.coords);
        assert_eq!(whole.starts, windowed.starts);
        assert_eq!(whole.layers, windowed.layers);
        assert_eq!(whole.kinds, windowed.kinds);
        assert_eq!(whole.meta, windowed.meta);
    }
}
