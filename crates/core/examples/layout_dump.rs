//! Dump a layout as FluidEDA's readers see it, in the oracle's JSON-lines form
//! (scripts/oracle/): one line per cell, its polygons `[layer, datatype, [[x, y], …]]`
//! and placements `[child, x, y, rot, mirror, mag, na, nb, ax, ay, bx, by]`.
//! Compared against KLayout's reading by scripts/oracle/compare.py.
//!
//! Run: cargo run --release -p fluideda-core --example layout_dump -- <file.gds|.oas> <out.json>

use fluideda_core::{InstanceKind, Transform};
use std::fmt::Write;

fn main() {
    let src = std::env::args().nth(1).expect("usage: layout_dump <in> <out.json>");
    let dst = std::env::args().nth(2).expect("usage: layout_dump <in> <out.json>");
    let bytes = std::fs::read(&src).expect("read");
    let db = fluideda_core::load_layout_db(&bytes).expect("parse");
    use std::io::Write as _;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&dst).expect("create"));
    for cell in db.cells.iter() {
        let mut s = String::new();
        write!(s, "{{\"cell\":{:?},\"polygons\":[", cell.name).unwrap();
        for i in 0..cell.shapes.len() {
            if i > 0 {
                s.push(',');
            }
            let l = cell.shapes.layer(i);
            write!(s, "[{},{},[", l.layer, l.datatype).unwrap();
            for (k, p) in cell.shapes.points(i).iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                write!(s, "[{},{}]", p.x, p.y).unwrap();
            }
            s.push_str("]]");
        }
        s.push_str("],\"instances\":[");
        for (k, inst) in cell.instances.iter().enumerate() {
            if k > 0 {
                s.push(',');
            }
            let (rot, mirror, mag) = strans(&inst.transform());
            let (na, nb, a, b) = match inst.kind() {
                InstanceKind::Single => (1, 1, [0.0, 0.0], [0.0, 0.0]),
                InstanceKind::Array { cols, rows, col_step, row_step } => (cols, rows, col_step, row_step),
            };
            write!(
                s,
                "[{:?},{},{},{},{},{},{},{},{},{},{},{}]",
                db.cell(inst.child).name,
                inst.transform().tx.round() as i64,
                inst.transform().ty.round() as i64,
                rot,
                mirror,
                mag,
                na,
                nb,
                a[0].round() as i64,
                a[1].round() as i64,
                b[0].round() as i64,
                b[1].round() as i64
            )
            .unwrap();
        }
        s.push_str("]}\n");
        out.write_all(s.as_bytes()).expect("write");
    }
    println!("dumped {} cells to {dst}", db.cells.len());
}

/// (rotation °, mirrored, magnification) of a `from_strans` transform, in
/// KLayout's convention (rotation normalized to [0, 360)).
fn strans(t: &Transform) -> (f64, bool, f64) {
    let mag = (t.a * t.a + t.b * t.b).sqrt();
    let mirror = t.a * t.d - t.b * t.c < 0.0;
    let mut rot = t.b.atan2(t.a).to_degrees();
    if rot < 0.0 {
        rot += 360.0;
    }
    ((rot * 1000.0).round() / 1000.0, mirror, (mag * 1000.0).round() / 1000.0)
}
