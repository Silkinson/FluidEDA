//! OASIS (SEMI P39) reader — Phase 6.
//!
//! OASIS is GDSII's successor: variable-length integers, delta-encoded point
//! lists, **repetitions** (one record for a whole array of shapes or
//! placements), modal variables (a field omitted from a record reuses the last
//! value), name tables, and deflate-compressed CBLOCKs. Production tapeouts are
//! typically 5–10× smaller than the same design in GDSII.
//!
//! This reader decodes a whole file into the same hierarchical [`CellDb`] the
//! GDS readers build (so everything downstream — chunking, culling, editing,
//! DRC, export — is format-blind):
//!
//! * RECTANGLE, POLYGON, TRAPEZOID, CTRAPEZOID (all 26 templates), CIRCLE
//!   (as a 64-gon) → polygons; PATH → its outline ([`crate::path`]);
//! * PLACEMENT → an SREF, or an AREF when its repetition is a regular lattice
//!   (irregular repetitions expand to one placement each);
//! * shape repetitions expand to one shape per position (the store is flat);
//! * TEXT, PROPERTY, XNAME/XELEMENT/XGEOMETRY, LAYERNAME are parsed (to stay
//!   aligned) and dropped;
//! * CELL / PLACEMENT may name cells by reference number *before* the CELLNAME
//!   record that defines the number (legal outside strict mode): references
//!   are resolved once the whole file has been read.
//!
//! Coordinates are converted to nm from the file's `unit` (grid steps per µm).
//! Verified against KLayout — its own reading of the same file, cell for cell,
//! through `scripts/oracle/` — on the standard-cell library, hier and caravel.

use crate::db::{CellDb, CellDbBuilder, Instance, InstanceKind};
use crate::store::ShapeStore;
use crate::transform::Transform;
use crate::types::{Layer, Point};
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

/// The OASIS magic bytes every file starts with.
pub const MAGIC: &[u8] = b"%SEMI-OASIS\r\n";

/// Does `bytes` start with the OASIS magic?
pub fn is_oasis(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Parse an OASIS file into a hierarchical cell database.
pub fn load_oasis_db(bytes: &[u8]) -> Result<CellDb> {
    if !is_oasis(bytes) {
        bail!("not an OASIS file (missing %SEMI-OASIS magic)");
    }
    let mut r = Reader::default();
    let mut cur = Cur { d: bytes, p: MAGIC.len() };
    r.records(&mut cur, true)?;
    r.finish()
}

// --- primitive decoding -------------------------------------------------------

struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn at_end(&self) -> bool {
        self.p >= self.d.len()
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self.d.get(self.p).ok_or_else(|| anyhow!("OASIS: unexpected end of data at byte {}", self.p))?;
        self.p += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.d.len());
        let end = end.ok_or_else(|| anyhow!("OASIS: {n} bytes requested at byte {}, past the end", self.p))?;
        let s = &self.d[self.p..end];
        self.p = end;
        Ok(s)
    }

    /// unsigned-integer: 7 bits per byte, little end first, high bit = more.
    fn uint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift >= 64 || (shift == 63 && (b & 0x7e) != 0) {
                bail!("OASIS: unsigned integer overflow at byte {}", self.p);
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    /// signed-integer: an unsigned whose bit 0 is the sign.
    fn sint(&mut self) -> Result<i64> {
        let u = self.uint()?;
        let m = (u >> 1) as i64;
        Ok(if u & 1 != 0 { -m } else { m })
    }

    /// real: a type code, then its encoding (whole / reciprocal / ratio /
    /// IEEE float or double, little-endian).
    fn real(&mut self) -> Result<f64> {
        Ok(match self.uint()? {
            0 => self.uint()? as f64,
            1 => -(self.uint()? as f64),
            2 => 1.0 / self.uint()? as f64,
            3 => -1.0 / self.uint()? as f64,
            4 => {
                let a = self.uint()? as f64;
                a / self.uint()? as f64
            }
            5 => {
                let a = self.uint()? as f64;
                -a / self.uint()? as f64
            }
            6 => f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()) as f64,
            7 => f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()),
            t => bail!("OASIS: invalid real type {t}"),
        })
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.uint()? as usize;
        self.bytes(n)
    }

    /// 2-delta (Manhattan: E/N/W/S + magnitude).
    fn delta2(&mut self) -> Result<(i64, i64)> {
        let u = self.uint()?;
        Ok(dir(u & 3, (u >> 2) as i64))
    }

    /// 3-delta (octangular).
    fn delta3(&mut self) -> Result<(i64, i64)> {
        let u = self.uint()?;
        Ok(dir(u & 7, (u >> 3) as i64))
    }

    /// g-delta: form 1 = octangular (bit 0 clear), form 2 = explicit x with its
    /// sign in bit 1, then a signed-integer y.
    fn gdelta(&mut self) -> Result<(i64, i64)> {
        let u = self.uint()?;
        if u & 1 == 0 {
            Ok(dir((u >> 1) & 7, (u >> 4) as i64))
        } else {
            let x = (u >> 2) as i64;
            let x = if u & 2 != 0 { -x } else { x };
            Ok((x, self.sint()?))
        }
    }
}

/// The eight OASIS directions: E, N, W, S, NE, NW, SW, SE.
fn dir(code: u64, m: i64) -> (i64, i64) {
    match code {
        0 => (m, 0),
        1 => (0, m),
        2 => (-m, 0),
        3 => (0, -m),
        4 => (m, m),
        5 => (-m, m),
        6 => (-m, -m),
        _ => (m, -m),
    }
}

// --- repetitions --------------------------------------------------------------

/// A decoded repetition: a regular lattice `n × m` of steps `a`, `b`, or an
/// explicit list of displacements (the first element's `(0, 0)` included).
#[derive(Debug, Clone)]
enum Rep {
    Grid { n: u64, m: u64, a: (i64, i64), b: (i64, i64) },
    List(Vec<(i64, i64)>),
}

impl Rep {
    fn offsets(&self) -> Vec<(i64, i64)> {
        match self {
            Rep::Grid { n, m, a, b } => {
                let mut v = Vec::with_capacity((*n * *m) as usize);
                for j in 0..*m as i64 {
                    for i in 0..*n as i64 {
                        v.push((i * a.0 + j * b.0, i * a.1 + j * b.1));
                    }
                }
                v
            }
            Rep::List(v) => v.clone(),
        }
    }
}

fn read_rep(c: &mut Cur, last: &Option<Rep>) -> Result<Rep> {
    let ty = c.uint()?;
    // A regular lattice; a zero step collapses that dimension to one element.
    let grid = |n: u64, m: u64, a: (i64, i64), b: (i64, i64)| Rep::Grid {
        n: if a == (0, 0) { 1 } else { n },
        m: if b == (0, 0) { 1 } else { m },
        a,
        b,
    };
    // An irregular list: `count` cumulative steps after the implicit origin;
    // zero steps (coincident elements) are dropped, as KLayout does.
    let list = |steps: Vec<(i64, i64)>| {
        let mut v = vec![(0, 0)];
        let (mut x, mut y) = (0i64, 0i64);
        for (dx, dy) in steps {
            if (dx, dy) != (0, 0) {
                x += dx;
                y += dy;
                v.push((x, y));
            }
        }
        Rep::List(v)
    };
    Ok(match ty {
        0 => last.clone().ok_or_else(|| anyhow!("OASIS: repetition reuse with no previous repetition"))?,
        1 => {
            let (nx, ny) = (c.uint()? + 2, c.uint()? + 2);
            let (dx, dy) = (c.uint()? as i64, c.uint()? as i64);
            grid(nx, ny, (dx, 0), (0, dy))
        }
        2 => {
            let nx = c.uint()? + 2;
            let dx = c.uint()? as i64;
            grid(nx, 1, (dx, 0), (0, 0))
        }
        3 => {
            let ny = c.uint()? + 2;
            let dy = c.uint()? as i64;
            grid(1, ny, (0, 0), (0, dy))
        }
        4 | 5 | 6 | 7 => {
            let n = c.uint()?;
            let g = if ty == 5 || ty == 7 { c.uint()? as i64 } else { 1 };
            let mut steps = Vec::with_capacity(n as usize + 1);
            for _ in 0..=n {
                let d = c.uint()? as i64 * g;
                steps.push(if ty <= 5 { (d, 0) } else { (0, d) });
            }
            list(steps)
        }
        8 => {
            let (n, m) = (c.uint()? + 2, c.uint()? + 2);
            let (a, b) = (c.gdelta()?, c.gdelta()?);
            grid(n, m, a, b)
        }
        9 => {
            let n = c.uint()? + 2;
            let a = c.gdelta()?;
            grid(n, 1, a, (0, 0))
        }
        10 | 11 => {
            let n = c.uint()?;
            let g = if ty == 11 { c.uint()? as i64 } else { 1 };
            let mut steps = Vec::with_capacity(n as usize + 1);
            for _ in 0..=n {
                let (dx, dy) = c.gdelta()?;
                steps.push((dx * g, dy * g));
            }
            list(steps)
        }
        t => bail!("OASIS: invalid repetition type {t}"),
    })
}

// --- point lists ----------------------------------------------------------------

/// A point list relative to the element's position, starting with `(0, 0)`.
/// For a polygon, Manhattan lists (types 0/1) end with a synthesized vertex
/// that closes the outline with a Manhattan edge.
fn read_point_list(c: &mut Cur, for_polygon: bool) -> Result<Vec<(i64, i64)>> {
    let ty = c.uint()?;
    let n = c.uint()?;
    if n == 0 {
        bail!("OASIS: empty point list");
    }
    let mut pts = Vec::with_capacity(n as usize + 2);
    pts.push((0i64, 0i64));
    let (mut x, mut y) = (0i64, 0i64);
    match ty {
        0 | 1 => {
            let mut horizontal = ty == 0;
            for _ in 0..n {
                let d = c.sint()?;
                if horizontal {
                    x += d;
                } else {
                    y += d;
                }
                horizontal = !horizontal;
                pts.push((x, y));
            }
            if for_polygon {
                pts.push(if horizontal { (0, y) } else { (x, 0) });
            }
        }
        2 | 3 | 4 => {
            for _ in 0..n {
                let (dx, dy) = match ty {
                    2 => c.delta2()?,
                    3 => c.delta3()?,
                    _ => c.gdelta()?,
                };
                x += dx;
                y += dy;
                pts.push((x, y));
            }
        }
        5 => {
            let (mut sx, mut sy) = (0i64, 0i64);
            for _ in 0..n {
                let (dx, dy) = c.gdelta()?;
                sx += dx;
                sy += dy;
                x += sx;
                y += sy;
                pts.push((x, y));
            }
        }
        t => bail!("OASIS: invalid point-list type {t}"),
    }
    Ok(pts)
}

// --- the reader -------------------------------------------------------------------

/// How a record names a cell: by reference number (resolved against the
/// CELLNAME table at the end) or by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CellKey {
    Ref(u64),
    Name(Vec<u8>),
}

/// A cell under construction, before names are resolved.
#[derive(Default)]
struct OCell {
    shapes: ShapeStore,
    instances: Vec<(usize, Transform, InstanceKind)>,
}

/// Modal variables (reset at every CELL record).
#[derive(Default)]
struct Modal {
    relative: bool,
    placement_x: i64,
    placement_y: i64,
    placement_cell: Option<usize>,
    layer: Option<u64>,
    datatype: Option<u64>,
    text_x: i64,
    text_y: i64,
    geometry_x: i64,
    geometry_y: i64,
    geometry_w: Option<i64>,
    geometry_h: Option<i64>,
    polygon: Option<Vec<(i64, i64)>>,
    path_hw: Option<i64>,
    path: Option<Vec<(i64, i64)>>,
    path_start: Option<i64>,
    path_end: Option<i64>,
    ctrap_type: Option<u64>,
    circle_r: Option<i64>,
    repetition: Option<Rep>,
}

#[derive(Default)]
struct Reader {
    /// File grid steps per µm (START record); coordinates scale by 1000 / unit.
    unit: f64,
    scale: f64,
    cells: Vec<OCell>,
    keys: HashMap<CellKey, usize>,
    /// Key of each cell (index-aligned with `cells`).
    cell_keys: Vec<CellKey>,
    cellnames: HashMap<u64, Vec<u8>>,
    next_cellname: u64,
    next_textstring: u64,
    next_propname: u64,
    next_propstring: u64,
    current: Option<usize>,
    m: Modal,
    ended: bool,
}

/// The 26 CTRAPEZOID templates of the OASIS spec: four vertices each, every
/// coordinate a combination `a·w + b·h` given as `[ax, bx, ay, by]`. A
/// repeated last vertex means the shape is a triangle.
const CTRAPS: [[[i64; 4]; 4]; 26] = [
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, -1, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, -1, 0, 0]],
    [[0, 0, 0, 0], [0, 1, 0, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 1, 0, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 1, 0, 1], [1, -1, 0, 1], [1, 0, 0, 0]],
    [[0, 1, 0, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, -1, 0, 0]],
    [[0, 0, 0, 0], [0, 1, 0, 1], [1, 0, 0, 1], [1, -1, 0, 0]],
    [[0, 1, 0, 0], [0, 0, 0, 1], [1, -1, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, 0, -1, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, -1, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, 0, 1, 0]],
    [[0, 0, 1, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, 0, -1, 1], [1, 0, 1, 0]],
    [[0, 0, 1, 0], [0, 0, -1, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, -1, 1], [1, 0, 0, 1], [1, 0, 1, 0]],
    [[0, 0, 1, 0], [0, 0, 0, 1], [1, 0, -1, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 1, 0], [1, 0, 0, 0], [0, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 1, 0], [1, 0, 1, 0], [0, 0, 0, 0]],
    [[0, 0, 0, 0], [1, 0, 1, 0], [1, 0, 0, 0], [0, 0, 0, 0]],
    [[0, 0, 1, 0], [1, 0, 1, 0], [1, 0, 0, 0], [0, 0, 1, 0]],
    [[0, 0, 0, 0], [0, 1, 0, 1], [0, 2, 0, 0], [0, 0, 0, 0]],
    [[0, 0, 0, 1], [0, 2, 0, 1], [0, 1, 0, 0], [0, 0, 0, 1]],
    [[0, 0, 0, 0], [0, 0, 2, 0], [1, 0, 1, 0], [0, 0, 0, 0]],
    [[1, 0, 0, 0], [0, 0, 1, 0], [1, 0, 2, 0], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 0, 1], [1, 0, 0, 1], [1, 0, 0, 0]],
    [[0, 0, 0, 0], [0, 0, 1, 0], [1, 0, 1, 0], [1, 0, 0, 0]],
];

/// Vertices of a circle approximation (KLayout's reader default too).
const CIRCLE_POINTS: usize = 64;

impl Reader {
    fn cell_for(&mut self, key: CellKey) -> usize {
        if let Some(&i) = self.keys.get(&key) {
            return i;
        }
        let i = self.cells.len();
        self.cells.push(OCell::default());
        self.cell_keys.push(key.clone());
        self.keys.insert(key, i);
        i
    }

    /// A coordinate from the file, in nm.
    fn nm(&self, v: i64) -> i64 {
        if self.scale == 1.0 {
            v
        } else {
            (v as f64 * self.scale).round() as i64
        }
    }

    fn xy(&mut self, c: &mut Cur, info: u8, xbit: u8, ybit: u8, which: u8) -> Result<(i64, i64)> {
        // which: 0 = geometry, 1 = placement, 2 = text (each has its own modal).
        let (mx, my) = match which {
            0 => (&mut self.m.geometry_x, &mut self.m.geometry_y),
            1 => (&mut self.m.placement_x, &mut self.m.placement_y),
            _ => (&mut self.m.text_x, &mut self.m.text_y),
        };
        if info & xbit != 0 {
            let v = c.sint()?;
            *mx = if self.m.relative { *mx + v } else { v };
        }
        if info & ybit != 0 {
            let v = c.sint()?;
            *my = if self.m.relative { *my + v } else { v };
        }
        Ok((*mx, *my))
    }

    fn layer_dt(&mut self, c: &mut Cur, info: u8) -> Result<Layer> {
        if info & 0x01 != 0 {
            self.m.layer = Some(c.uint()?);
        }
        if info & 0x02 != 0 {
            self.m.datatype = Some(c.uint()?);
        }
        let l = self.m.layer.ok_or_else(|| anyhow!("OASIS: shape with no layer"))?;
        let d = self.m.datatype.ok_or_else(|| anyhow!("OASIS: shape with no datatype"))?;
        Ok(Layer::new(l as u16, d as u16))
    }

    fn rep(&mut self, c: &mut Cur, info: u8, bit: u8) -> Result<Vec<(i64, i64)>> {
        if info & bit == 0 {
            return Ok(vec![(0, 0)]);
        }
        let r = read_rep(c, &self.m.repetition)?;
        let offs = r.offsets();
        self.m.repetition = Some(r);
        Ok(offs)
    }

    /// Emit a polygon (file coordinates relative to `(x, y)`) at every offset.
    fn emit(&mut self, layer: Layer, rel: &[(i64, i64)], x: i64, y: i64, offs: &[(i64, i64)]) -> Result<()> {
        let cell = self.current.ok_or_else(|| anyhow!("OASIS: geometry outside any cell"))?;
        if rel.len() < 3 {
            return Ok(());
        }
        for &(ox, oy) in offs {
            let mut ring: Vec<Point> = rel
                .iter()
                .map(|&(px, py)| Point::new(self.nm(x + ox + px) as i32, self.nm(y + oy + py) as i32))
                .collect();
            ring.push(ring[0]); // closed, as GDS boundaries are
            self.cells[cell].shapes.push(layer, ring);
        }
        Ok(())
    }

    fn records(&mut self, c: &mut Cur, top_level: bool) -> Result<()> {
        while !c.at_end() && !self.ended {
            let id = c.uint()?;
            match id {
                0 => {} // PAD
                1 => {
                    // START: version, unit, offset flag [+ table offsets].
                    let _version = c.string()?;
                    self.unit = c.real()?;
                    if !(self.unit > 0.0) {
                        bail!("OASIS: invalid unit {}", self.unit);
                    }
                    self.scale = 1000.0 / self.unit;
                    if (self.scale - self.scale.round()).abs() < 1e-9 {
                        self.scale = self.scale.round();
                    }
                    if c.uint()? == 0 {
                        for _ in 0..12 {
                            c.uint()?;
                        }
                    }
                }
                2 => {
                    // END: the rest of the file is padding / validation.
                    self.ended = true;
                }
                3 => {
                    let name = c.string()?.to_vec();
                    self.cellnames.insert(self.next_cellname, name);
                    self.next_cellname += 1;
                }
                4 => {
                    let name = c.string()?.to_vec();
                    let r = c.uint()?;
                    self.cellnames.insert(r, name);
                }
                5 | 7 | 9 => {
                    c.string()?; // TEXTSTRING / PROPNAME / PROPSTRING, implicit number
                    match id {
                        5 => self.next_textstring += 1,
                        7 => self.next_propname += 1,
                        _ => self.next_propstring += 1,
                    }
                }
                6 | 8 | 10 => {
                    c.string()?;
                    c.uint()?;
                }
                11 | 12 => {
                    // LAYERNAME: name, then two intervals.
                    c.string()?;
                    for _ in 0..2 {
                        match c.uint()? {
                            0 => {}
                            1..=3 => {
                                c.uint()?;
                            }
                            4 => {
                                c.uint()?;
                                c.uint()?;
                            }
                            t => bail!("OASIS: invalid interval type {t}"),
                        }
                    }
                }
                13 | 14 => {
                    let key = if id == 13 { CellKey::Ref(c.uint()?) } else { CellKey::Name(c.string()?.to_vec()) };
                    self.current = Some(self.cell_for(key));
                    self.m = Modal::default();
                }
                15 => self.m.relative = false,
                16 => self.m.relative = true,
                17 | 18 => self.placement(c, id)?,
                19 => self.text(c)?,
                20 => self.rectangle(c)?,
                21 => self.polygon(c)?,
                22 => self.path(c)?,
                23..=25 => self.trapezoid(c, id)?,
                26 => self.ctrapezoid(c)?,
                27 => self.circle(c)?,
                28 => self.property(c)?,
                29 => {} // PROPERTY: repeat the last one
                30 => {
                    c.uint()?;
                    c.string()?;
                }
                31 => {
                    c.uint()?;
                    c.string()?;
                    c.uint()?;
                }
                32 => {
                    c.uint()?;
                    c.string()?;
                }
                33 => {
                    // XGEOMETRY: info 000XYRDL, attribute, layer, datatype, string, x, y, rep.
                    let info = c.byte()?;
                    c.uint()?;
                    self.layer_dt(c, info)?;
                    c.string()?;
                    self.xy(c, info, 0x10, 0x08, 0)?;
                    self.rep(c, info, 0x04)?;
                }
                34 => {
                    if !top_level {
                        bail!("OASIS: nested CBLOCK");
                    }
                    let comp = c.uint()?;
                    let uncomp_len = c.uint()? as usize;
                    let comp_len = c.uint()? as usize;
                    let data = c.bytes(comp_len)?;
                    if comp != 0 {
                        bail!("OASIS: unsupported CBLOCK compression {comp}");
                    }
                    let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(data, uncomp_len)
                        .map_err(|e| anyhow!("OASIS: CBLOCK inflate failed: {e:?}"))?;
                    let mut inner = Cur { d: &raw, p: 0 };
                    self.records(&mut inner, false)?;
                }
                other => bail!("OASIS: unknown record type {other} at byte {}", c.p),
            }
        }
        Ok(())
    }

    fn placement(&mut self, c: &mut Cur, id: u64) -> Result<()> {
        let info = c.byte()?;
        if info & 0x80 != 0 {
            let key = if info & 0x40 != 0 { CellKey::Ref(c.uint()?) } else { CellKey::Name(c.string()?.to_vec()) };
            self.m.placement_cell = Some(self.cell_for(key));
        }
        let child = self.m.placement_cell.ok_or_else(|| anyhow!("OASIS: placement with no cell"))?;
        let (mut mag, mut angle) = (1.0f64, 0.0f64);
        if id == 18 {
            if info & 0x04 != 0 {
                mag = c.real()?;
            }
            if info & 0x02 != 0 {
                angle = c.real()?;
            }
        } else {
            angle = ((info >> 1) & 3) as f64 * 90.0;
        }
        let mirror = info & 0x01 != 0;
        let (x, y) = self.xy(c, info, 0x20, 0x10, 1)?;
        let cell = self.current.ok_or_else(|| anyhow!("OASIS: placement outside any cell"))?;
        let (px, py) = (self.nm(x) as f64, self.nm(y) as f64);
        let tf = Transform::from_strans(px, py, angle, mirror, mag);
        if info & 0x08 == 0 {
            self.cells[cell].instances.push((child, tf, InstanceKind::Single));
            return Ok(());
        }
        let r = read_rep(c, &self.m.repetition)?;
        self.m.repetition = Some(r.clone());
        match r {
            Rep::Grid { n, m, a, b } => {
                let kind = InstanceKind::Array {
                    cols: n as i32,
                    rows: m as i32,
                    col_step: [self.nm(a.0) as f64, self.nm(a.1) as f64],
                    row_step: [self.nm(b.0) as f64, self.nm(b.1) as f64],
                };
                let kind = if n * m == 1 { InstanceKind::Single } else { kind };
                self.cells[cell].instances.push((child, tf, kind));
            }
            Rep::List(offs) => {
                for (ox, oy) in offs {
                    let t = Transform::from_strans(self.nm(x + ox) as f64, self.nm(y + oy) as f64, angle, mirror, mag);
                    self.cells[cell].instances.push((child, t, InstanceKind::Single));
                }
            }
        }
        Ok(())
    }

    fn text(&mut self, c: &mut Cur) -> Result<()> {
        // info 0CNXYRTL: string or ref, textlayer, texttype, x, y, repetition.
        let info = c.byte()?;
        if info & 0x40 != 0 {
            if info & 0x20 != 0 {
                c.uint()?;
            } else {
                c.string()?;
            }
        }
        if info & 0x01 != 0 {
            c.uint()?;
        }
        if info & 0x02 != 0 {
            c.uint()?;
        }
        self.xy(c, info, 0x10, 0x08, 2)?;
        if info & 0x04 != 0 {
            let r = read_rep(c, &self.m.repetition)?;
            self.m.repetition = Some(r);
        }
        Ok(())
    }

    fn rectangle(&mut self, c: &mut Cur) -> Result<()> {
        // info SWHXYRDL.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x40 != 0 {
            self.m.geometry_w = Some(c.uint()? as i64);
        }
        if info & 0x20 != 0 {
            self.m.geometry_h = Some(c.uint()? as i64);
        }
        if info & 0x80 != 0 {
            self.m.geometry_h = self.m.geometry_w; // square
        }
        let w = self.m.geometry_w.ok_or_else(|| anyhow!("OASIS: rectangle with no width"))?;
        let h = self.m.geometry_h.ok_or_else(|| anyhow!("OASIS: rectangle with no height"))?;
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        self.emit(layer, &[(0, 0), (w, 0), (w, h), (0, h)], x, y, &offs)
    }

    fn polygon(&mut self, c: &mut Cur) -> Result<()> {
        // info 00PXYRDL.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x20 != 0 {
            self.m.polygon = Some(read_point_list(c, true)?);
        }
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        let pts = self.m.polygon.clone().ok_or_else(|| anyhow!("OASIS: polygon with no point list"))?;
        self.emit(layer, &pts, x, y, &offs)
    }

    fn path(&mut self, c: &mut Cur) -> Result<()> {
        // info EWPXYRDL: layer, datatype, half-width, extension scheme
        // (+ explicit extensions), point list, x, y, repetition.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x40 != 0 {
            self.m.path_hw = Some(c.uint()? as i64);
        }
        let hw = self.m.path_hw.ok_or_else(|| anyhow!("OASIS: path with no half-width"))?;
        if info & 0x80 != 0 {
            let e = c.uint()?;
            match e & 0x0c {
                0x04 => self.m.path_start = Some(0),
                0x08 => self.m.path_start = Some(hw),
                0x0c => self.m.path_start = Some(c.sint()?),
                _ => {}
            }
            match e & 0x03 {
                0x01 => self.m.path_end = Some(0),
                0x02 => self.m.path_end = Some(hw),
                0x03 => self.m.path_end = Some(c.sint()?),
                _ => {}
            }
        }
        if info & 0x20 != 0 {
            self.m.path = Some(read_point_list(c, false)?);
        }
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        let pts = self.m.path.clone().ok_or_else(|| anyhow!("OASIS: path with no point list"))?;
        let (b, e) = (self.m.path_start.unwrap_or(0), self.m.path_end.unwrap_or(0));
        let cell = self.current.ok_or_else(|| anyhow!("OASIS: path outside any cell"))?;
        for (ox, oy) in offs {
            let center: Vec<Point> = pts
                .iter()
                .map(|&(px, py)| Point::new(self.nm(x + ox + px) as i32, self.nm(y + oy + py) as i32))
                .collect();
            let (hwn, bn, en) = (self.nm(hw) as f64, self.nm(b) as f64, self.nm(e) as f64);
            if let Some(ring) = crate::path::path_to_polygon(&center, hwn, bn, en) {
                self.cells[cell].shapes.push(layer, ring);
            }
        }
        Ok(())
    }

    fn trapezoid(&mut self, c: &mut Cur, id: u64) -> Result<()> {
        // info OWHXYRDL; 23 carries delta-a and delta-b, 24 only a, 25 only b.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x40 != 0 {
            self.m.geometry_w = Some(c.uint()? as i64);
        }
        if info & 0x20 != 0 {
            self.m.geometry_h = Some(c.uint()? as i64);
        }
        let da = if id == 23 || id == 24 { c.sint()? } else { 0 };
        let db = if id == 23 || id == 25 { c.sint()? } else { 0 };
        let w = self.m.geometry_w.ok_or_else(|| anyhow!("OASIS: trapezoid with no width"))?;
        let h = self.m.geometry_h.ok_or_else(|| anyhow!("OASIS: trapezoid with no height"))?;
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        let pts = if info & 0x80 != 0 {
            // Vertical: the parallel sides are the left and right edges.
            [(0, da.max(0)), (0, h + db.min(0)), (w, h - db.max(0)), (w, -da.min(0))]
        } else {
            [(da.max(0), h), (w + db.min(0), h), (w - db.max(0), 0), (-da.min(0), 0)]
        };
        self.emit(layer, &dedup_ring(&pts), x, y, &offs)
    }

    fn ctrapezoid(&mut self, c: &mut Cur) -> Result<()> {
        // info TWHXYRDL.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x80 != 0 {
            self.m.ctrap_type = Some(c.uint()?);
        }
        if info & 0x40 != 0 {
            self.m.geometry_w = Some(c.uint()? as i64);
        }
        if info & 0x20 != 0 {
            self.m.geometry_h = Some(c.uint()? as i64);
        }
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        let t = self.m.ctrap_type.ok_or_else(|| anyhow!("OASIS: ctrapezoid with no type"))? as usize;
        let tpl = CTRAPS.get(t).ok_or_else(|| anyhow!("OASIS: invalid ctrapezoid type {t}"))?;
        let (w, h) = (self.m.geometry_w.unwrap_or(0), self.m.geometry_h.unwrap_or(0));
        let pts: Vec<(i64, i64)> = tpl.iter().map(|m| (m[0] * w + m[1] * h, m[2] * w + m[3] * h)).collect();
        // The modal width/height become the shape's bbox (KLayout; the spec's
        // derived-dimension rule for the square/triangle types).
        self.m.geometry_w = pts.iter().map(|p| p.0).max();
        self.m.geometry_h = pts.iter().map(|p| p.1).max();
        self.emit(layer, &dedup_ring(&pts), x, y, &offs)
    }

    fn circle(&mut self, c: &mut Cur) -> Result<()> {
        // info 00rXYRDL.
        let info = c.byte()?;
        let layer = self.layer_dt(c, info)?;
        if info & 0x20 != 0 {
            self.m.circle_r = Some(c.uint()? as i64);
        }
        let r = self.m.circle_r.ok_or_else(|| anyhow!("OASIS: circle with no radius"))? as f64;
        let (x, y) = self.xy(c, info, 0x10, 0x08, 0)?;
        let offs = self.rep(c, info, 0x04)?;
        let pts: Vec<(i64, i64)> = (0..CIRCLE_POINTS)
            .map(|i| {
                let a = (i as f64 + 0.5) * std::f64::consts::TAU / CIRCLE_POINTS as f64;
                ((r * a.cos()).round() as i64, (r * a.sin()).round() as i64)
            })
            .collect();
        self.emit(layer, &dedup_ring(&pts), x, y, &offs)
    }

    fn property(&mut self, c: &mut Cur) -> Result<()> {
        // info UUUUVCNS: name (ref or string) if C, values unless V.
        let info = c.byte()?;
        if info & 0x04 != 0 {
            if info & 0x02 != 0 {
                c.uint()?;
            } else {
                c.string()?;
            }
        }
        if info & 0x08 == 0 {
            let mut n = (info >> 4) as u64;
            if n == 15 {
                n = c.uint()?;
            }
            for _ in 0..n {
                let t = c.uint()?;
                match t {
                    0 | 1 | 2 | 3 => {
                        c.uint()?;
                    }
                    4 | 5 => {
                        c.uint()?;
                        c.uint()?;
                    }
                    6 => {
                        c.bytes(4)?;
                    }
                    7 => {
                        c.bytes(8)?;
                    }
                    8 => {
                        c.uint()?;
                    }
                    9 => {
                        c.sint()?;
                    }
                    10..=12 => {
                        c.string()?;
                    }
                    13..=15 => {
                        c.uint()?;
                    }
                    t => bail!("OASIS: invalid property value type {t}"),
                }
            }
        }
        Ok(())
    }

    /// Resolve cell names, merge cells that turned out to be the same, and
    /// build the database.
    fn finish(self) -> Result<CellDb> {
        let name_of = |k: &CellKey| -> String {
            match k {
                CellKey::Name(n) => String::from_utf8_lossy(n).into_owned(),
                CellKey::Ref(r) => match self.cellnames.get(r) {
                    Some(n) => String::from_utf8_lossy(n).into_owned(),
                    None => format!("CELL_{r}"),
                },
            }
        };
        let mut b = CellDbBuilder::new();
        // Intern in first-seen order (deterministic cell ids).
        let ids: Vec<usize> = self.cell_keys.iter().map(|k| b.intern(&name_of(k))).collect();
        for (i, oc) in self.cells.into_iter().enumerate() {
            let cell = b.cell_mut(ids[i]);
            if cell.shapes.is_empty() {
                cell.shapes = oc.shapes;
            } else {
                // The same cell referenced both by number and by name: merge.
                for k in 0..oc.shapes.len() {
                    cell.shapes.push(oc.shapes.layer(k), oc.shapes.points(k).iter().copied());
                }
            }
            for (child, transform, kind) in oc.instances {
                cell.instances.push(Instance { child: ids[child], transform, kind });
            }
        }
        b.build()
    }
}

/// Drop consecutive duplicate vertices (and a duplicate closing vertex) from a
/// template-built ring — a CTRAPEZOID triangle repeats a vertex.
fn dedup_ring(pts: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> = Vec::with_capacity(pts.len());
    for &p in pts {
        if v.last() != Some(&p) {
            v.push(p);
        }
    }
    while v.len() > 1 && v.first() == v.last() {
        v.pop();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn sint(v: i64, out: &mut Vec<u8>) {
        uint(((v.unsigned_abs()) << 1) | (v < 0) as u64, out);
    }

    #[test]
    fn primitives_decode() {
        let mut b = Vec::new();
        uint(0, &mut b);
        uint(127, &mut b);
        uint(128, &mut b);
        uint(300_000, &mut b);
        sint(-5, &mut b);
        sint(7, &mut b);
        let mut c = Cur { d: &b, p: 0 };
        assert_eq!(c.uint().unwrap(), 0);
        assert_eq!(c.uint().unwrap(), 127);
        assert_eq!(c.uint().unwrap(), 128);
        assert_eq!(c.uint().unwrap(), 300_000);
        assert_eq!(c.sint().unwrap(), -5);
        assert_eq!(c.sint().unwrap(), 7);
        assert!(c.at_end());
    }

    #[test]
    fn manhattan_point_list_closes_with_a_synthesized_vertex() {
        // Type 0, two deltas (+100 x, +50 y): the fourth vertex (0, 50) is implied.
        let mut b = Vec::new();
        uint(0, &mut b);
        uint(2, &mut b);
        sint(100, &mut b);
        sint(50, &mut b);
        let pts = read_point_list(&mut Cur { d: &b, p: 0 }, true).unwrap();
        assert_eq!(pts, vec![(0, 0), (100, 0), (100, 50), (0, 50)]);
    }

    /// A hand-built file: START (unit 1000), CELLNAME, a cell with a square
    /// repeated 3× along x, placed by a second cell as a 2×1 array.
    #[test]
    fn a_tiny_file_round_trips_through_the_reader() {
        let mut f = MAGIC.to_vec();
        uint(1, &mut f); // START
        uint(3, &mut f);
        f.extend_from_slice(b"1.0");
        uint(0, &mut f); // real type 0: whole number
        uint(1000, &mut f);
        uint(1, &mut f); // offset flag: tables at END
        uint(3, &mut f); // CELLNAME implicit #0
        uint(4, &mut f);
        f.extend_from_slice(b"LEAF");
        uint(3, &mut f); // CELLNAME implicit #1
        uint(3, &mut f);
        f.extend_from_slice(b"TOP");
        // CELL #0 (LEAF)
        uint(13, &mut f);
        uint(0, &mut f);
        // RECTANGLE S W X Y R D L: layer 1, dt 0, square 100, at (0,0), rep type 2 (3 × 200 in x).
        uint(20, &mut f);
        f.push(0b1101_1111);
        uint(1, &mut f);
        uint(0, &mut f);
        uint(100, &mut f);
        sint(0, &mut f);
        sint(0, &mut f);
        uint(2, &mut f);
        uint(1, &mut f); // 3 = 1 + 2
        uint(200, &mut f);
        // CELL #1 (TOP)
        uint(13, &mut f);
        uint(1, &mut f);
        // PLACEMENT C N X Y R: cell ref 0, at (1000, 0), rep type 2 (2 × 5000).
        uint(17, &mut f);
        f.push(0b1111_1000);
        uint(0, &mut f);
        sint(1000, &mut f);
        sint(0, &mut f);
        uint(2, &mut f);
        uint(0, &mut f); // 2 = 0 + 2
        uint(5000, &mut f);
        uint(2, &mut f); // END
        let db = load_oasis_db(&f).unwrap();
        let leaf = db.id_of("LEAF").unwrap();
        let top = db.id_of("TOP").unwrap();
        assert_eq!(db.top, top);
        assert_eq!(db.cell(leaf).shapes.len(), 3, "the repetition expands");
        assert_eq!(db.cell(leaf).shapes.bbox_of(2).min, Point::new(400, 0));
        match db.cell(top).instances[0].kind {
            InstanceKind::Array { cols, rows, col_step, .. } => {
                assert_eq!((cols, rows), (2, 1));
                assert_eq!(col_step, [5000.0, 0.0]);
            }
            k => panic!("expected an array placement, got {k:?}"),
        }
        assert_eq!(db.flat_shape_count(), 6);
    }
}
