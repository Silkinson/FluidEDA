//! OASIS writer — Phase 6. Exports base ⊕ edit overlay with the same composite
//! semantics as [`crate::write_gds`] (tombstones out; moved copies, drawn and
//! flattened shapes in; hidden placements out, arrays split around them).
//!
//! Encoding choices, all plain OASIS 1.0 that any reader accepts:
//! * one CELLNAME table up front, cells referenced by number;
//! * axis-aligned rectangles as RECTANGLE (square flag when w = h), every other
//!   polygon as POLYGON with a g-delta point list;
//! * layer / datatype written only when they change (modal);
//! * arrays as a repetition on one PLACEMENT (type 1 for axis-aligned lattices,
//!   type 8 otherwise), 90°-multiple unmagnified placements in the compact
//!   PLACEMENT form, anything else with explicit magnification and angle;
//! * each cell's records deflated into a CBLOCK.
//!
//! Units: the database is nm, so `unit` = 1000 grid steps per µm.

use crate::db::{CellDb, InstanceKind};
use crate::edit::EditOverlay;
use crate::oasis::MAGIC;
use crate::transform::Transform;
use crate::types::{Layer, Point};

/// Export the composite design as OASIS bytes.
pub fn write_oasis(db: &CellDb, overlay: &EditOverlay) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    // START: version "1.0", unit 1000 (real type 0 = whole number), offset
    // flag 0 with six zero table entries ("tables may be anywhere").
    uint(1, &mut out);
    string(b"1.0", &mut out);
    uint(0, &mut out);
    uint(1000, &mut out);
    uint(0, &mut out);
    for _ in 0..12 {
        uint(0, &mut out);
    }
    // CELLNAME table, implicit numbering = cell id.
    for cell in &db.cells {
        uint(3, &mut out);
        string(cell.name.as_bytes(), &mut out);
    }
    for (id, cell) in db.cells.iter().enumerate() {
        uint(13, &mut out); // CELL by reference number
        uint(id as u64, &mut out);
        let mut body = Vec::new();
        let mut modal: Option<Layer> = None;
        for i in 0..cell.shapes.len() {
            if !overlay.deleted.contains(&(id, i as u32)) {
                shape(cell.shapes.layer(i), &cell.shapes.points(i), &mut modal, &mut body);
            }
        }
        for s in overlay.live_shapes_in(id) {
            shape(s.layer, &s.points, &mut modal, &mut body);
        }
        for (ii, inst) in cell.instances.iter().enumerate() {
            match inst.kind() {
                InstanceKind::Single => {
                    if !overlay.is_hidden((id, ii as u32, 0, 0)) {
                        placement(inst.child, &inst.transform(), None, &mut body);
                    }
                }
                InstanceKind::Array { cols, rows, col_step, row_step } => {
                    for (c0, r0, nc, nr) in visible_blocks(overlay, id, ii as u32, cols, rows) {
                        let t = inst.placement(c0, r0);
                        placement(inst.child, &t, Some((nc, nr, col_step, row_step)), &mut body);
                    }
                }
            }
        }
        cblock(&body, &mut out);
    }
    // END: exactly 256 bytes — record id, padding string, validation scheme 0.
    out.push(2);
    let pad = 252usize;
    uint(pad as u64, &mut out);
    out.extend(std::iter::repeat(0u8).take(pad));
    uint(0, &mut out);
    out
}

/// The visible elements of an array as `(first col, first row, cols, rows)`
/// blocks: the whole array if nothing is hidden; otherwise full rows merged
/// into blocks and each partly hidden row split into runs (as the GDS writer).
fn visible_blocks(o: &EditOverlay, parent: usize, ii: u32, cols: i32, rows: i32) -> Vec<(i32, i32, i32, i32)> {
    let hidden = |c: i32, r: i32| o.is_hidden((parent, ii, c, r));
    if !o.hides_in(parent) || !(0..rows).any(|r| (0..cols).any(|c| hidden(c, r))) {
        return vec![(0, 0, cols, rows)];
    }
    let mut out = Vec::new();
    let mut r = 0;
    while r < rows {
        if (0..cols).any(|c| hidden(c, r)) {
            let mut c = 0;
            while c < cols {
                if hidden(c, r) {
                    c += 1;
                    continue;
                }
                let c0 = c;
                while c < cols && !hidden(c, r) {
                    c += 1;
                }
                out.push((c0, r, c - c0, 1));
            }
            r += 1;
        } else {
            let r0 = r;
            while r < rows && !(0..cols).any(|c| hidden(c, r)) {
                r += 1;
            }
            out.push((0, r0, cols, r - r0));
        }
    }
    out
}

/// One shape: RECTANGLE if it is an axis-aligned rectangle, else POLYGON.
fn shape(layer: Layer, pts: &[Point], modal: &mut Option<Layer>, out: &mut Vec<u8>) {
    let ring = crate::boolean::open_ring(pts);
    if ring.len() < 3 {
        return;
    }
    let ld_bits = if *modal == Some(layer) { 0 } else { 0x03 };
    if let Some((x, y, w, h)) = as_rect(ring) {
        // info SWHXYRDL
        let square = w == h;
        let info = if square { 0x80 | 0x40 } else { 0x40 | 0x20 } | 0x10 | 0x08 | ld_bits;
        uint(20, out);
        out.push(info);
        layer_dt(layer, ld_bits, out);
        uint(w as u64, out);
        if !square {
            uint(h as u64, out);
        }
        sint(x as i64, out);
        sint(y as i64, out);
    } else {
        // info 00PXYRDL, point list type 4 (g-deltas) relative to vertex 0.
        uint(21, out);
        out.push(0x20 | 0x10 | 0x08 | ld_bits);
        layer_dt(layer, ld_bits, out);
        uint(4, out);
        uint((ring.len() - 1) as u64, out);
        for w in ring.windows(2) {
            gdelta((w[1].x - w[0].x) as i64, (w[1].y - w[0].y) as i64, out);
        }
        sint(ring[0].x as i64, out);
        sint(ring[0].y as i64, out);
    }
    *modal = Some(layer);
}

fn layer_dt(layer: Layer, bits: u8, out: &mut Vec<u8>) {
    if bits != 0 {
        uint(layer.layer as u64, out);
        uint(layer.datatype as u64, out);
    }
}

/// `(x, y, w, h)` if the open ring is an axis-aligned rectangle.
fn as_rect(r: &[Point]) -> Option<(i32, i32, i32, i32)> {
    if r.len() != 4 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in r {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    let corners = [Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)];
    let axis_edges = (0..4).all(|i| {
        let (a, b) = (r[i], r[(i + 1) % 4]);
        (a.x == b.x) != (a.y == b.y)
    });
    (axis_edges && corners.iter().all(|c| r.contains(c)) && x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// One placement (and, with `array`, its lattice as a repetition).
fn placement(child: usize, t: &Transform, array: Option<(i32, i32, [f64; 2], [f64; 2])>, out: &mut Vec<u8>) {
    // Canonical parts: the same bytes on every platform (Phase 7.4).
    let (angle, mirror, mag) = t.strans_parts();
    let quarter = (angle / 90.0).round();
    let compact = (mag - 1.0).abs() < 1e-9 && (angle - quarter * 90.0).abs() < 1e-9;
    let rep = array.filter(|&(c, r, _, _)| c * r > 1);
    let r_bit = if rep.is_some() { 0x08 } else { 0 };
    // info CNXYR…F with C (cell present) and N (by reference number).
    let base = 0x80 | 0x40 | 0x20 | 0x10 | r_bit | mirror as u8;
    if compact {
        uint(17, out);
        out.push(base | (((quarter as i64).rem_euclid(4) as u8) << 1));
        uint(child as u64, out);
    } else {
        uint(18, out);
        out.push(base | 0x04 | 0x02);
        uint(child as u64, out);
        real(mag, out);
        real(angle, out);
    }
    sint(t.tx.round() as i64, out);
    sint(t.ty.round() as i64, out);
    if let Some((cols, rows, cs, rs)) = rep {
        let a = (cs[0].round() as i64, cs[1].round() as i64);
        let b = (rs[0].round() as i64, rs[1].round() as i64);
        let (n, m) = (cols.max(1) as u64, rows.max(1) as u64);
        if a.1 == 0 && b.0 == 0 && a.0 > 0 && b.1 > 0 && n >= 2 && m >= 2 {
            uint(1, out); // rectangular, axis-aligned
            uint(n - 2, out);
            uint(m - 2, out);
            uint(a.0 as u64, out);
            uint(b.1 as u64, out);
        } else if m == 1 && a.1 == 0 && a.0 > 0 {
            uint(2, out); // one row, x steps
            uint(n - 2, out);
            uint(a.0 as u64, out);
        } else if n == 1 && b.0 == 0 && b.1 > 0 {
            uint(3, out); // one column, y steps
            uint(m - 2, out);
            uint(b.1 as u64, out);
        } else if m == 1 {
            uint(9, out); // 1-D, arbitrary step
            uint(n - 2, out);
            gdelta(a.0, a.1, out);
        } else if n == 1 {
            uint(9, out);
            uint(m - 2, out);
            gdelta(b.0, b.1, out);
        } else {
            uint(8, out); // 2-D, arbitrary steps
            uint(n - 2, out);
            uint(m - 2, out);
            gdelta(a.0, a.1, out);
            gdelta(b.0, b.1, out);
        }
    }
}

/// Wrap `body` (a cell's records) in a deflate CBLOCK.
fn cblock(body: &[u8], out: &mut Vec<u8>) {
    if body.is_empty() {
        return;
    }
    let comp = miniz_oxide::deflate::compress_to_vec(body, 6);
    uint(34, out);
    uint(0, out); // DEFLATE
    uint(body.len() as u64, out);
    uint(comp.len() as u64, out);
    out.extend_from_slice(&comp);
}

// --- primitive encoders ---------------------------------------------------------

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
    uint((v.unsigned_abs() << 1) | (v < 0) as u64, out);
}

fn string(s: &[u8], out: &mut Vec<u8>) {
    uint(s.len() as u64, out);
    out.extend_from_slice(s);
}

/// A real: whole numbers exactly (types 0/1), anything else as an f64 (type 7).
fn real(v: f64, out: &mut Vec<u8>) {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        uint(if v < 0.0 { 1 } else { 0 }, out);
        uint(v.abs() as u64, out);
    } else {
        uint(7, out);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// A g-delta: the compact octangular form when possible, else explicit (x, y).
fn gdelta(dx: i64, dy: i64, out: &mut Vec<u8>) {
    let oct = match (dx.signum(), dy.signum()) {
        (1, 0) => Some((0, dx)),
        (0, 1) => Some((1, dy)),
        (-1, 0) => Some((2, -dx)),
        (0, -1) => Some((3, -dy)),
        (1, 1) if dx == dy => Some((4, dx)),
        (-1, 1) if -dx == dy => Some((5, dy)),
        (-1, -1) if dx == dy => Some((6, -dx)),
        (1, -1) if dx == -dy => Some((7, dx)),
        _ => None,
    };
    match oct {
        Some((d, m)) => uint(((m as u64) << 4) | ((d as u64) << 1), out),
        None => {
            uint((dx.unsigned_abs() << 2) | (((dx < 0) as u64) << 1) | 1, out);
            sint(dy, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical per-cell content: normalized polygons + placements, by name.
    fn canon(db: &CellDb) -> Vec<(String, Vec<(Layer, Vec<(i32, i32)>)>, Vec<String>)> {
        let mut out: Vec<_> = db
            .cells
            .iter()
            .map(|c| {
                let mut polys: Vec<(Layer, Vec<(i32, i32)>)> = (0..c.shapes.len())
                    .map(|i| {
                        let mut r: Vec<(i32, i32)> =
                            crate::boolean::open_ring(&c.shapes.points(i)).iter().map(|p| (p.x, p.y)).collect();
                        let k = (0..r.len()).min_by_key(|&k| r[k]).unwrap();
                        r.rotate_left(k);
                        (c.shapes.layer(i), r)
                    })
                    .collect();
                polys.sort();
                // Arrays canonicalized (a 1-D lattice may come back on the other
                // axis — the same placements).
                let mut insts: Vec<String> = c
                    .instances
                    .iter()
                    .map(|i| {
                        let kind = match i.kind() {
                            InstanceKind::Single => String::from("single"),
                            InstanceKind::Array { cols, rows, col_step, row_step } => {
                                let dim = |n: i32, s: [f64; 2]| if n > 1 { (n, [s[0] as i64, s[1] as i64]) } else { (1, [0, 0]) };
                                let mut d = [dim(cols, col_step), dim(rows, row_step)];
                                d.sort();
                                format!("{d:?}")
                            }
                        };
                        format!("{} {:?} {kind}", db.cell(i.child).name, i.transform())
                    })
                    .collect();
                insts.sort();
                (c.name.clone(), polys, insts)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
    const FIXTURE: &[u8] = include_bytes!("../../../assets/edit_fixture.gds");

    #[test]
    fn our_oasis_reads_back_to_the_same_design() {
        for (label, bytes) in [("hier", HIER), ("fixture", FIXTURE)] {
            let db = crate::gds::load_gds_db(bytes).unwrap();
            let oas = write_oasis(&db, &EditOverlay::new());
            let back = crate::oasis::load_oasis_db(&oas).unwrap();
            assert_eq!(canon(&back), canon(&db), "{label}: OASIS round trip changed the design");
            assert!(oas.len() < bytes.len(), "{label}: {} B OASIS vs {} B GDS", oas.len(), bytes.len());
        }
    }

    #[test]
    fn primitive_encoders_round_trip_through_the_reader() {
        // g-deltas of every form, via a POLYGON the reader decodes.
        let tri = [Point::new(0, 0), Point::new(7, 7), Point::new(-3, 11), Point::new(0, 0)];
        let mut body = Vec::new();
        shape(Layer::new(5, 6), &tri, &mut None, &mut body);
        let mut f = MAGIC.to_vec();
        for v in [1u64] {
            uint(v, &mut f);
        }
        string(b"1.0", &mut f);
        uint(0, &mut f);
        uint(1000, &mut f);
        uint(1, &mut f);
        uint(14, &mut f); // CELL by name
        string(b"T", &mut f);
        f.extend_from_slice(&body);
        uint(2, &mut f);
        let db = crate::oasis::load_oasis_db(&f).unwrap();
        let c = db.cell(db.id_of("T").unwrap());
        assert_eq!(c.shapes.layer(0), Layer::new(5, 6));
        assert_eq!(crate::boolean::open_ring(&c.shapes.points(0)), &tri[..3]);
    }
}
