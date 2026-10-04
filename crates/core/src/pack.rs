//! Design packs (Phase 6 SLICE 3): a parsed design laid out for streaming.
//!
//! In-core loading ships a design's whole geometry to the browser before any
//! of it is used, so the browser's memory caps the design size. A pack splits
//! a parsed design into three immutable blobs that the server stores by
//! content hash:
//!
//! * **skeleton** — the [`crate::serialize`] metadata blob: every cell's
//!   hierarchy, chunk table and bboxes, no geometry. The client installs it
//!   as it installs a worker-parsed skeleton (Phase 5c), and every chunk draws
//!   as a bbox proxy until its geometry arrives.
//! * **geometry** — one self-contained record per chunk, cells in order and
//!   each cell's chunks in order, each fetchable with one HTTP byte range:
//!   `[n u32][nc u32][starts (n+1) u32, 0-based][layers n u32][coords nc i32]`,
//!   little-endian. Every field is 4-byte aligned, so a fetched range is used
//!   in place through typed-array views.
//! * **index** — `chunks + 1` little-endian u64 byte offsets into the
//!   geometry: chunk `k` is `offsets[k]..offsets[k+1]`, with `k` numbered by
//!   [`chunk_base`].
//!
//! A record's columns are exactly what [`crate::tessellate_columns`] takes and
//! what [`crate::store::ShapeStore::chunk_columns`] yields in-core, so a
//! streamed chunk tessellates identically to a local one (tested).

use crate::db::CellDb;
use anyhow::{bail, Result};
use bytemuck::cast_slice;
use std::io::{self, Write};

/// Bumped whenever the pack layout (or anything baked into it — chunking,
/// shape numbering) changes; servers key cached packs by it. 2: the skeleton
/// carries placement chunk tables (Phase 7.3).
pub const PACK_VERSION: u32 = 2;

/// The first global chunk number of each cell, cells in order, followed by
/// the total: cell `c`'s chunk `i` is chunk `base[c] + i` of the pack.
pub fn chunk_base(db: &CellDb) -> Vec<u32> {
    let mut base = Vec::with_capacity(db.cells.len() + 1);
    let mut n = 0u32;
    for cell in &db.cells {
        base.push(n);
        n += cell.chunks.len() as u32;
    }
    base.push(n);
    base
}

/// Write every chunk's record to `out`, in [`chunk_base`] order. Returns the
/// index: `chunks + 1` byte offsets. Streams — nothing but one record is
/// buffered, so a pack of any size is written in bounded memory.
pub fn write_geometry(db: &CellDb, out: &mut impl Write) -> io::Result<Vec<u64>> {
    #[cfg(target_endian = "big")]
    compile_error!("pack records are little-endian; they are written by casting native columns");
    let mut offsets = Vec::with_capacity(db.cells.iter().map(|c| c.chunks.len()).sum::<usize>() + 1);
    offsets.push(0u64);
    let mut pos = 0u64;
    let mut rec: Vec<u8> = Vec::new();
    for cell in &db.cells {
        for ch in &cell.chunks {
            let (coords, starts, layers) = cell.shapes.chunk_columns(ch.start, ch.end);
            rec.clear();
            rec.extend_from_slice(&(layers.len() as u32).to_le_bytes());
            rec.extend_from_slice(&(coords.len() as u32).to_le_bytes());
            rec.extend_from_slice(cast_slice(&starts));
            rec.extend_from_slice(cast_slice(layers));
            rec.extend_from_slice(cast_slice(coords));
            out.write_all(&rec)?;
            pos += rec.len() as u64;
            offsets.push(pos);
        }
    }
    Ok(offsets)
}

/// The index blob for `offsets`.
pub fn index_bytes(offsets: &[u64]) -> Vec<u8> {
    offsets.iter().flat_map(|o| o.to_le_bytes()).collect()
}

/// Offsets from an index blob.
pub fn parse_index(bytes: &[u8]) -> Result<Vec<u64>> {
    if !bytes.len().is_multiple_of(8) || bytes.is_empty() {
        bail!("pack index: {} bytes is not a whole number of offsets", bytes.len());
    }
    let offsets: Vec<u64> = bytes.chunks_exact(8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).collect();
    if offsets[0] != 0 || offsets.windows(2).any(|w| w[1] < w[0]) {
        bail!("pack index: offsets must start at 0 and never decrease");
    }
    Ok(offsets)
}

/// One chunk record's columns: `(coords, starts, packed layers)`.
pub fn decode_record(rec: &[u8]) -> Result<(Vec<i32>, Vec<u32>, Vec<u32>)> {
    let word = |i: usize| -> Result<u32> {
        match rec.get(i * 4..i * 4 + 4) {
            Some(b) => Ok(u32::from_le_bytes(b.try_into().unwrap())),
            None => bail!("pack record truncated"),
        }
    };
    let n = word(0)? as usize;
    let nc = word(1)? as usize;
    let words = 2 + (n + 1) + n + nc;
    if rec.len() != words * 4 {
        bail!("pack record: {} bytes, header says {}", rec.len(), words * 4);
    }
    let starts: Vec<u32> = (0..=n).map(|i| word(2 + i)).collect::<Result<_>>()?;
    let layers: Vec<u32> = (0..n).map(|i| word(3 + n + i)).collect::<Result<_>>()?;
    let coords: Vec<i32> = (0..nc).map(|i| word(3 + 2 * n + i).map(|w| w as i32)).collect::<Result<_>>()?;
    if starts.last().copied().unwrap_or(0) as usize * 2 != nc {
        bail!("pack record: starts end at {} points, coords hold {}", starts.last().unwrap_or(&0), nc / 2);
    }
    Ok((coords, starts, layers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(bytes: &[u8], label: &str) {
        let db = crate::load_layout_db(bytes).unwrap();
        let mut geometry = Vec::new();
        let offsets = write_geometry(&db, &mut geometry).unwrap();
        let base = chunk_base(&db);
        assert_eq!(offsets.len() as u32, base[db.cells.len()] + 1, "{label}: one offset per chunk + 1");
        assert_eq!(*offsets.last().unwrap(), geometry.len() as u64);
        assert_eq!(parse_index(&index_bytes(&offsets)).unwrap(), offsets);
        let mut shapes = 0;
        for (c, cell) in db.cells.iter().enumerate() {
            for (i, ch) in cell.chunks.iter().enumerate() {
                let k = (base[c] + i as u32) as usize;
                let rec = &geometry[offsets[k] as usize..offsets[k + 1] as usize];
                let (coords, starts, layers) = decode_record(rec).unwrap();
                let (c0, s0, l0) = cell.shapes.chunk_columns(ch.start, ch.end);
                assert_eq!((coords.as_slice(), &starts, layers.as_slice()), (c0, &s0, l0), "{label}: cell {c} chunk {i}");
                // …so it tessellates exactly like the local chunk.
                let streamed = crate::tessellate_columns(&coords, &starts, &layers);
                let local = crate::tessellate_chunk(&cell.shapes, ch);
                assert_eq!(streamed.len(), local.len(), "{label}: cell {c} chunk {i} meshes");
                for (a, b) in streamed.iter().zip(&local) {
                    assert_eq!((a.layer, &a.indices), (b.layer, &b.indices), "{label}: cell {c} chunk {i}");
                    assert_eq!(a.vertices.len(), b.vertices.len());
                }
                shapes += layers.len();
            }
        }
        assert_eq!(shapes, db.cells.iter().map(|c| c.shapes.len()).sum::<usize>(), "{label}: every shape, once");
    }

    #[test]
    fn records_hold_exactly_each_chunks_columns() {
        check(include_bytes!("../../../assets/hier.gds"), "hier");
        check(include_bytes!("../../../assets/edit_fixture.gds"), "fixture");
        if let Ok(b) = std::fs::read("../../testdata/sky130_fd_sc_hd_merged.gds") {
            check(&b, "sky130");
        }
    }

    #[test]
    fn corrupt_records_and_indices_are_errors() {
        assert!(decode_record(&[1, 0, 0, 0]).is_err());
        assert!(decode_record(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]).is_err());
        assert!(parse_index(&[]).is_err());
        assert!(parse_index(&index_bytes(&[0, 10, 5])).is_err());
        assert!(parse_index(&index_bytes(&[3, 10])).is_err());
    }
}
