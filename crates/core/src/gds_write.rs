//! GDSII writer — export a (base + edit overlay) design back to a `.gds` (Phase 3
//! SLICE 1). This is how edits made in the browser leave the browser.
//!
//! ## What it writes
//!
//! Per cell, the **composited** geometry: every base shape that is not tombstoned,
//! plus the overlay's moved copies and added shapes for that cell — the same
//! composite [`crate::cull`] draws. The hierarchy is preserved: each cell becomes
//! a GDS struct with its boundaries and its SREF/AREF instances. Nothing is
//! flattened, so a re-import reconstructs the same `CellDb`.
//!
//! ## Why hand-rolled, not gds21's writer
//!
//! We already own a hand-rolled *reader* (`gds_stream`), oracle-matched to gds21's
//! reader; a hand-rolled writer keeps the format in one place, avoids
//! materializing a whole `gds21::GdsLibrary` (the ~1.4 GB intermediate the
//! streaming parser exists to avoid), and avoids mapping our types into gds21's
//! structs. Correctness is closed the strong way instead: the round-trip test
//! re-imports the output through **both** our reader and gds21's and asserts both
//! reconstruct the composited `CellDb` (see the tests).
//!
//! ## Instances round-trip via STRANS inversion
//!
//! Every `Instance.transform` was built by [`Transform::from_strans`] at parse
//! time (a similarity: uniform mag × rotation × optional reflection, never a
//! shear), so it inverts exactly back to `(angle, reflect, mag)`:
//!   * `mag = √(a² + b²)`, `reflect = det(linear) < 0`, `angle = atan2(b, a)`.
//! `from_strans` on re-import reproduces the same matrix (to float epsilon —
//! negligible for i32-nm geometry and rendering). AREF's three-point `xy` is
//! rebuilt from the base origin `(tx, ty)` plus `cols·col_step` / `rows·row_step`.

use crate::db::{CellDb, InstanceKind};
use crate::edit::EditOverlay;
use crate::transform::Transform;
use crate::types::{Layer, Point};

// GDS record tags: the 16-bit value packs (record-type << 8 | data-type), written
// big-endian right after the u16 length. Matches assets/gen_hier.py.
const HEADER: u16 = 0x0002;
const BGNLIB: u16 = 0x0102;
const LIBNAME: u16 = 0x0206;
const UNITS: u16 = 0x0305;
const BGNSTR: u16 = 0x0502;
const STRNAME: u16 = 0x0606;
const ENDSTR: u16 = 0x0700;
const BOUNDARY: u16 = 0x0800;
const SREF: u16 = 0x0A00;
const AREF: u16 = 0x0B00;
const LAYER_REC: u16 = 0x0D02;
const DATATYPE_REC: u16 = 0x0E02;
const XY: u16 = 0x1003;
const ENDEL: u16 = 0x1100;
const SNAME: u16 = 0x1206;
const COLROW: u16 = 0x1302;
const STRANS: u16 = 0x1A01;
const MAG: u16 = 0x1B05;
const ANGLE: u16 = 0x1C05;
const ENDLIB: u16 = 0x0400;

// A valid calendar date is all the dates need to be (gds21 parses them via chrono).
const DATES: [i16; 12] = [2024, 1, 1, 0, 0, 0, 2024, 1, 1, 0, 0, 0];

/// Serialize `db` composited with `overlay` to GDSII bytes.
pub fn write_gds(db: &CellDb, overlay: &EditOverlay) -> Vec<u8> {
    let mut w = Writer::new();

    w.record(HEADER, &i16_bytes(&[600]));
    w.record(BGNLIB, &i16_bytes(&DATES));
    w.record(LIBNAME, b"FLUIDEDA.DB");
    // 1 user unit = 1e-3 m, 1 DB unit = 1e-9 m (nm) — matches how the reader treats
    // coordinates (it ignores UNITS and keeps DB units as nm, so any consistent
    // pair round-trips; this is the conventional one).
    let mut units = gds_real(1e-3).to_vec();
    units.extend_from_slice(&gds_real(1e-9));
    w.record(UNITS, &units);

    for (id, cell) in db.cells.iter().enumerate() {
        w.record(BGNSTR, &i16_bytes(&DATES));
        w.record(STRNAME, cell.name.as_bytes());

        // Composited boundaries: base minus tombstones, then moved + added.
        for i in 0..cell.shapes.len() {
            if overlay.deleted.contains(&(id, i as u32)) {
                continue;
            }
            w.boundary(cell.shapes.layer(i), cell.shapes.points(i));
        }
        for (&(c, i), shape) in &overlay.edited {
            if c == id && overlay.deleted.contains(&(c, i)) {
                // A move tombstones the base and records the moved copy here.
                w.boundary(shape.layer, &shape.points);
            }
        }
        for a in &overlay.added {
            if a.cell == id {
                w.boundary(a.shape.layer, &a.shape.points);
            }
        }

        // Instances. Phase 5d: a placement hidden by "edit this placement
        // only" is left out (its geometry was flattened into this cell's added
        // shapes, written above); an AREF with hidden elements is split into
        // sub-arrays covering exactly the visible ones.
        for (ii, inst) in cell.instances.iter().enumerate() {
            let child = &db.cells[inst.child].name;
            let strans = invert_strans(&inst.transform);
            match inst.kind {
                InstanceKind::Single => {
                    if overlay.is_hidden((id, ii as u32, 0, 0)) {
                        continue;
                    }
                    w.sref(child, strans, inst.transform.tx, inst.transform.ty);
                }
                InstanceKind::Array { cols, rows, col_step, row_step } => {
                    let hidden = |c: i32, r: i32| overlay.is_hidden((id, ii as u32, c, r));
                    let any_hidden = overlay.hides_in(id) && (0..rows).any(|r| (0..cols).any(|c| hidden(c, r)));
                    if !any_hidden {
                        w.aref(child, strans, (inst.transform.tx, inst.transform.ty), cols, rows, col_step, row_step);
                        continue;
                    }
                    let origin = |c: i32, r: i32| {
                        let t = inst.placement(c, r);
                        (t.tx, t.ty)
                    };
                    // Full rows (no hidden element) merge into row blocks; a row
                    // with hidden elements becomes one 1-row array per visible run.
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
                                w.aref(child, strans, origin(c0, r), c - c0, 1, col_step, row_step);
                            }
                            r += 1;
                        } else {
                            let r0 = r;
                            while r < rows && !(0..cols).any(|c| hidden(c, r)) {
                                r += 1;
                            }
                            w.aref(child, strans, origin(0, r0), cols, r - r0, col_step, row_step);
                        }
                    }
                }
            }
        }

        w.record(ENDSTR, &[]);
    }

    w.record(ENDLIB, &[]);
    w.into_vec()
}

/// Recover `(angle_deg, reflected, mag)` from a `from_strans`-built transform.
fn invert_strans(t: &Transform) -> (f64, bool, f64) {
    let mag = (t.a * t.a + t.b * t.b).sqrt();
    let det = t.a * t.d - t.b * t.c;
    let reflected = det < 0.0;
    let angle = t.b.atan2(t.a).to_degrees();
    (angle, reflected, mag)
}

fn round(v: f64) -> i32 {
    v.round() as i32
}

struct Writer(Vec<u8>);
impl Writer {
    fn new() -> Self {
        Writer(Vec::new())
    }

    /// One SREF of `child` at `(x, y)` with `strans` = (angle°, reflected, mag).
    fn sref(&mut self, child: &str, strans: (f64, bool, f64), x: f64, y: f64) {
        self.record(SREF, &[]);
        self.record(SNAME, child.as_bytes());
        self.strans(strans.0, strans.1, strans.2);
        self.record(XY, &i32_bytes(&[round(x), round(y)]));
        self.record(ENDEL, &[]);
    }

    /// One AREF (a 1×1 array is written as an SREF). Three-point xy: p0 =
    /// origin, p1 = p0 + cols·col_step, p2 = p0 + rows·row_step (see the module
    /// note).
    #[allow(clippy::too_many_arguments)]
    fn aref(
        &mut self,
        child: &str,
        strans: (f64, bool, f64),
        (p0x, p0y): (f64, f64),
        cols: i32,
        rows: i32,
        col_step: [f64; 2],
        row_step: [f64; 2],
    ) {
        if cols == 1 && rows == 1 {
            self.sref(child, strans, p0x, p0y);
            return;
        }
        self.record(AREF, &[]);
        self.record(SNAME, child.as_bytes());
        self.strans(strans.0, strans.1, strans.2);
        self.record(COLROW, &i16_bytes(&[cols as i16, rows as i16]));
        let p1x = p0x + cols as f64 * col_step[0];
        let p1y = p0y + cols as f64 * col_step[1];
        let p2x = p0x + rows as f64 * row_step[0];
        let p2y = p0y + rows as f64 * row_step[1];
        self.record(XY, &i32_bytes(&[round(p0x), round(p0y), round(p1x), round(p1y), round(p2x), round(p2y)]));
        self.record(ENDEL, &[]);
    }

    /// One GDS record: u16 length (incl. this 4-byte header, BE), u16 tag (BE),
    /// then payload (padded to even length).
    fn record(&mut self, tag: u16, payload: &[u8]) {
        let padded = payload.len() + (payload.len() & 1);
        let len = (padded + 4) as u16;
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(&tag.to_be_bytes());
        self.0.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            self.0.push(0);
        }
    }

    fn boundary(&mut self, layer: Layer, points: &[Point]) {
        if points.len() < 3 {
            return;
        }
        self.record(BOUNDARY, &[]);
        self.record(LAYER_REC, &i16_bytes(&[layer.layer as i16]));
        self.record(DATATYPE_REC, &i16_bytes(&[layer.datatype as i16]));
        let mut xy = Vec::with_capacity(points.len() * 8);
        for p in points {
            xy.extend_from_slice(&p.x.to_be_bytes());
            xy.extend_from_slice(&p.y.to_be_bytes());
        }
        self.record(XY, &xy);
        self.record(ENDEL, &[]);
    }

    /// Emit a STRANS (+ MAG/ANGLE when non-default). Written whenever the
    /// placement is reflected, rotated, or scaled, so re-import rebuilds the exact
    /// transform.
    fn strans(&mut self, angle: f64, reflect: bool, mag: f64) {
        if !reflect && angle == 0.0 && mag == 1.0 {
            return; // identity placement — STRANS optional, omit it
        }
        let flags: u16 = if reflect { 0x8000 } else { 0x0000 };
        self.record(STRANS, &flags.to_be_bytes());
        if mag != 1.0 {
            self.record(MAG, &gds_real(mag));
        }
        if angle != 0.0 {
            self.record(ANGLE, &gds_real(angle));
        }
    }

    fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

fn i16_bytes(vals: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 2);
    for &x in vals {
        v.extend_from_slice(&x.to_be_bytes());
    }
    v
}

fn i32_bytes(vals: &[i32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 4);
    for &x in vals {
        v.extend_from_slice(&x.to_be_bytes());
    }
    v
}

/// 8-byte GDSII real: sign, 7-bit excess-64 base-16 exponent, 56-bit mantissa.
/// Value = ±(mantissa / 2^56) · 16^(exp-64). Used for UNITS/MAG/ANGLE. Mirrors the
/// encoder in assets/gen_hier.py (the format the reader already accepts).
fn gds_real(x: f64) -> [u8; 8] {
    if x == 0.0 {
        return [0; 8];
    }
    let sign: u8 = if x < 0.0 { 0x80 } else { 0 };
    let mut x = x.abs();
    let mut exp: i32 = 64;
    while x >= 1.0 {
        x /= 16.0;
        exp += 1;
    }
    while x < 1.0 / 16.0 {
        x *= 16.0;
        exp -= 1;
    }
    let mut mant = (x * (1u64 << 56) as f64 + 0.5) as u64;
    if mant >= (1u64 << 56) {
        // rounding carried past the top nibble
        mant >>= 4;
        exp += 1;
    }
    let mut out = [0u8; 8];
    out[0] = sign | (exp as u8 & 0x7f);
    let mb = mant.to_be_bytes(); // 8 bytes; mantissa is 56 bits = low 7 bytes
    out[1..8].copy_from_slice(&mb[1..8]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CellDb;
    use crate::edit::EditOverlay;
    use crate::types::Point;

    const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
    const FIXTURE: &[u8] = include_bytes!("../../../assets/edit_fixture.gds");

    /// Build the CellDb that `write_gds` *should* produce on re-import: the base
    /// with the overlay applied to its shape stores. Compares against a fresh
    /// export→reimport.
    fn composited_shapes(db: &CellDb, overlay: &EditOverlay, cell: usize) -> Vec<(Layer, Vec<Point>)> {
        let mut out = Vec::new();
        let c = &db.cells[cell];
        for i in 0..c.shapes.len() {
            if !overlay.deleted.contains(&(cell, i as u32)) {
                out.push((c.shapes.layer(i), c.shapes.points(i).to_vec()));
            }
        }
        for (&(cc, i), s) in &overlay.edited {
            if cc == cell && overlay.deleted.contains(&(cc, i)) {
                out.push((s.layer, s.points.clone()));
            }
        }
        for a in &overlay.added {
            if a.cell == cell {
                out.push((a.shape.layer, a.shape.points.clone()));
            }
        }
        out.sort_by_key(|(l, p)| (l.layer, l.datatype, p.first().map(|q| (q.x, q.y))));
        out
    }

    /// Assert an export re-imports (through a `reader`) to the composited design.
    fn assert_round_trip(bytes: &[u8], reader: fn(&[u8]) -> anyhow::Result<CellDb>, label: &str) {
        let db = crate::gds::load_gds_db(bytes).unwrap();
        // Representative edits: move shape 0 of TOP, delete shape 1, and ADD a
        // brand-new rectangle (Phase 5 SLICE 1 — export must carry drawn shapes).
        let top = db.top;
        let mut overlay = EditOverlay::new();
        if db.cells[top].shapes.len() >= 2 {
            let pts0 = db.cells[top].shapes.points(0).to_vec();
            let l0 = db.cells[top].shapes.layer(0);
            overlay.move_shape((top, 0), crate::edit::translate(l0, &pts0, 4000, -2000));
            overlay.delete((top, 1));
        }
        overlay.add(
            (99, 0),
            top,
            crate::edit::Shape {
                layer: Layer::new(3, 0),
                points: vec![
                    Point::new(100, 100),
                    Point::new(600, 100),
                    Point::new(600, 400),
                    Point::new(100, 400),
                    Point::new(100, 100), // closed ring, as the draw tools emit
                ],
            },
        );

        let out = write_gds(&db, &overlay);
        let re = reader(&out).unwrap_or_else(|e| panic!("{label}: re-import failed: {e}"));

        assert_eq!(re.cells.len(), db.cells.len(), "{label}: cell count");
        for id in 0..db.cells.len() {
            assert_eq!(re.cells[id].name, db.cells[id].name, "{label}: cell {id} name");
            // Shapes: compare as sorted (layer, points) sets — chunk order may
            // differ but the geometry must match the composite.
            let mut got: Vec<(Layer, Vec<Point>)> = (0..re.cells[id].shapes.len())
                .map(|i| (re.cells[id].shapes.layer(i), re.cells[id].shapes.points(i).to_vec()))
                .collect();
            got.sort_by_key(|(l, p)| (l.layer, l.datatype, p.first().map(|q| (q.x, q.y))));
            let want = composited_shapes(&db, &overlay, id);
            assert_eq!(got, want, "{label}: cell '{}' composited shapes", db.cells[id].name);

            // Instances: same children + transforms within float epsilon.
            assert_eq!(
                re.cells[id].instances.len(),
                db.cells[id].instances.len(),
                "{label}: '{}' instance count",
                db.cells[id].name
            );
            for (a, b) in re.cells[id].instances.iter().zip(&db.cells[id].instances) {
                assert_eq!(a.child, b.child, "{label}: inst child");
                let (ta, tb) = (a.transform, b.transform);
                for (x, y) in [(ta.a, tb.a), (ta.b, tb.b), (ta.c, tb.c), (ta.d, tb.d), (ta.tx, tb.tx), (ta.ty, tb.ty)] {
                    assert!((x - y).abs() < 1e-6, "{label}: inst transform {x} vs {y}");
                }
            }
        }
    }

    #[test]
    fn round_trip_hier_via_our_reader() {
        assert_round_trip(HIER, crate::gds::load_gds_db, "hier/ours");
    }

    #[test]
    fn round_trip_hier_via_gds21() {
        // Independent validation: gds21's reader must also accept our output.
        assert_round_trip(HIER, crate::gds::load_gds_db_gds21, "hier/gds21");
    }

    #[test]
    fn round_trip_fixture_via_our_reader() {
        assert_round_trip(FIXTURE, crate::gds::load_gds_db, "fixture/ours");
    }

    #[test]
    fn round_trip_fixture_via_gds21() {
        assert_round_trip(FIXTURE, crate::gds::load_gds_db_gds21, "fixture/gds21");
    }

    /// Phase 5d: hidden placements are left out of the export — an AREF with
    /// hidden elements is split into sub-arrays around them — and the flattened
    /// copies travel as the parent's shapes. Re-imported by BOTH readers, the
    /// file flattens to exactly what was drawn.
    #[test]
    fn hidden_placements_export_as_split_arrays() {
        use crate::db::{CellDbBuilder, Instance};
        use crate::transform::Transform;
        let rect = |x: i32, y: i32, w: i32, h: i32| {
            vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h), Point::new(x, y)]
        };
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let leaf = b.intern("LEAF");
        b.cell_mut(leaf).shapes.push(Layer::new(1, 0), rect(0, 0, 100, 100));
        b.cell_mut(top).shapes.push(Layer::new(2, 0), rect(-400, -400, 50, 50));
        b.cell_mut(top).instances.push(Instance {
            child: leaf,
            transform: Transform::translate(0.0, 0.0),
            kind: InstanceKind::Array { cols: 4, rows: 3, col_step: [500.0, 0.0], row_step: [0.0, 700.0] },
        });
        b.cell_mut(top).instances.push(Instance {
            child: leaf,
            transform: Transform::translate(-2000.0, 0.0),
            kind: InstanceKind::Single,
        });
        let db = b.build().unwrap();
        // Flatten two elements of the array (one mid-row, one corner) and the SREF.
        let mut o = EditOverlay::new();
        for key in [(top, 0, 1, 1), (top, 0, 3, 2), (top, 1, 0, 0)] {
            for (k, s) in crate::flatten::flatten_placement(&db, &o.clone(), key, 1000).unwrap().into_iter().enumerate() {
                o.add((key.1 as u64, (key.2 * 10 + key.3 + k as i32) as u32), top, s);
            }
            o.hide_placement(key);
        }
        let want = {
            let mut v: Vec<(Layer, Vec<(i32, i32)>)> = crate::flatten::composite_flat(&db, &o)
                .into_iter()
                .map(|s| (s.layer, s.points.iter().map(|p| (p.x, p.y)).collect()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(want.len(), 1 + 12 + 1, "nothing lost or doubled by the flatten");
        let bytes = write_gds(&db, &o);
        for (label, reader) in [
            ("ours", crate::gds::load_gds_db as fn(&[u8]) -> anyhow::Result<CellDb>),
            ("gds21", crate::gds::load_gds_db_gds21),
        ] {
            let re = reader(&bytes).unwrap();
            let mut got: Vec<(Layer, Vec<(i32, i32)>)> = re
                .flatten()
                .polygons
                .into_iter()
                .map(|p| (p.layer, p.points.iter().map(|q| (q.x, q.y)).collect()))
                .collect();
            got.sort();
            assert_eq!(got, want, "{label}: re-imported design differs from what was drawn");
        }
    }

    #[test]
    fn empty_overlay_preserves_geometry() {
        // Exporting with no edits and re-importing must preserve every shape.
        let db = crate::gds::load_gds_db(FIXTURE).unwrap();
        let out = write_gds(&db, &EditOverlay::new());
        let re = crate::gds::load_gds_db(&out).unwrap();
        for id in 0..db.cells.len() {
            assert_eq!(re.cells[id].shapes.len(), db.cells[id].shapes.len(), "cell {id} shape count");
        }
    }
}
