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
//!   `[n u32][nc u32][starts (n+1) u32, 0-based][layers n u32]
//!   [kinds n u8, zero-padded to 4][coords nc i32]`, little-endian. Every
//!   column starts 4-byte aligned, so a fetched range is used in place
//!   through typed-array views. `kinds` (pack v6, Phase 7.7) is the store's
//!   rectangle column: a rectangle's coords are just its two corners.
//! * **index** — `chunks + 1` little-endian u64 byte offsets into the
//!   geometry: chunk `k` is `offsets[k]..offsets[k+1]`, with `k` numbered by
//!   [`chunk_base`].
//!
//! A record's columns are exactly what [`crate::tessellate_columns`] takes and
//! what [`crate::store::ShapeStore::chunk_columns`] yields in-core, so a
//! streamed chunk tessellates identically to a local one (tested).
//!
//! **Pack v3 streams the hierarchy too** (Phase 7.3 SLICE B). The skeleton
//! becomes a *cell table* ([`crate::serialize::cell_table_meta`]): a cell with
//! more than [`INLINE_PLACEMENTS`] placements lists only their count, and its
//! placements are two more blobs —
//!
//! * **placements** — one record per placement chunk of such a cell
//!   ([`crate::serialize::encode_instances`]), in [`inst_chunk_base`] order
//!   (inline cells' chunks are empty ranges);
//! * **placement index** — byte offsets into it, like the geometry index.
//!
//! caravel's 25 MB skeleton becomes a cell table of a few hundred KB: only
//! caravel_core's 460 k standard-cell placements (and two smaller lists) are
//! remote, fetched as the view needs them.

use crate::db::CellDb;
use anyhow::{bail, Result};
use bytemuck::cast_slice;
use std::io::{self, Write};

/// Bumped whenever the pack layout (or anything baked into it — chunking,
/// shape numbering) changes; servers key cached packs by it. 2: the skeleton
/// carries placement chunk tables (Phase 7.3). 3: big placement lists are
/// remote — a cell table plus placement records (Phase 7.3 SLICE B). 4: the
/// cell table carries each shape chunk's look — its layers' coverage
/// (SLICE C). 5: the manifest carries each cell's placement count and
/// flattened subtree size (Phase 7.4: "edit this placement only"). 6:
/// records carry a kind column, rectangles stored as two corners (7.7).
pub const PACK_VERSION: u32 = 6;

/// At most this many layers describe a chunk's look (the densest ones).
pub const LOOK_LAYERS: usize = 8;

/// What shapes `chunk.start..chunk.end` look like from far away: each layer's
/// polygon area (shoelace) over the chunk's bbox area, clamped to 1 and
/// quantized to /255, densest first, at most [`LOOK_LAYERS`]. Overlaps count
/// twice — it is an appearance estimate, not a measurement.
pub fn chunk_look(store: &crate::store::ShapeStore, chunk: &crate::chunk::Chunk) -> Vec<(crate::types::Layer, u8)> {
    let (w, h) = chunk.bbox.size();
    let bbox_area = (w * h).max(1.0);
    let mut area: Vec<(crate::types::Layer, f64)> = Vec::new();
    for i in chunk.start as usize..chunk.end as usize {
        let a = crate::boolean::signed_area2(&store.points(i)).unsigned_abs() as f64 / 2.0;
        let l = store.layer(i);
        match area.iter_mut().find(|(k, _)| *k == l) {
            Some((_, v)) => *v += a,
            None => area.push((l, a)),
        }
    }
    area.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    area.into_iter()
        .take(LOOK_LAYERS)
        .map(|(l, a)| (l, ((a / bbox_area).min(1.0) * 255.0).round() as u8))
        .filter(|&(_, c)| c > 0)
        .collect()
}

/// Fill every cell's `chunk_looks` (what a pack's cell table carries).
pub fn annotate_chunk_looks(db: &mut CellDb) {
    for cell in &mut db.cells {
        cell.chunk_looks = cell.chunks.iter().map(|ch| chunk_look(&cell.shapes, ch)).collect();
    }
}

/// A cell with at most this many placements keeps them in the cell table;
/// above it, they are fetched by placement chunk. Most cells have a handful
/// (fetching them would cost a round trip per hierarchy level for nothing);
/// the lists that matter are the huge ones.
pub const INLINE_PLACEMENTS: usize = 512;

/// The first global placement-chunk number of each cell, cells in order,
/// followed by the total (the placement index's numbering).
pub fn inst_chunk_base(db: &CellDb) -> Vec<u32> {
    let mut base = Vec::with_capacity(db.cells.len() + 1);
    let mut n = 0u32;
    for cell in &db.cells {
        base.push(n);
        n += cell.inst_chunks.len() as u32;
    }
    base.push(n);
    base
}

/// Write the placement records of every cell with more than `inline_max`
/// placements (other cells' chunks are empty ranges), in
/// [`inst_chunk_base`] order. Returns the index (`chunks + 1` offsets).
pub fn write_placements(db: &CellDb, inline_max: usize, out: &mut impl Write) -> io::Result<Vec<u64>> {
    let mut offsets = vec![0u64];
    let mut pos = 0u64;
    for cell in &db.cells {
        let remote = cell.instances.len() > inline_max;
        for ch in &cell.inst_chunks {
            if remote {
                let rec = crate::serialize::encode_instances(&cell.instances[ch.start as usize..ch.end as usize]);
                out.write_all(&rec)?;
                pos += rec.len() as u64;
            }
            offsets.push(pos);
        }
    }
    Ok(offsets)
}

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
            let (coords, starts, layers, kinds) = cell.shapes.chunk_columns(ch.start, ch.end);
            rec.clear();
            rec.extend_from_slice(&(layers.len() as u32).to_le_bytes());
            rec.extend_from_slice(&(coords.len() as u32).to_le_bytes());
            rec.extend_from_slice(cast_slice(&starts));
            rec.extend_from_slice(cast_slice(layers));
            rec.extend_from_slice(kinds);
            rec.resize(rec.len().next_multiple_of(4), 0);
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

/// A chunk record's columns: `(coords, starts, packed layers, kinds)`.
pub type RecordColumns = (Vec<i32>, Vec<u32>, Vec<u32>, Vec<u8>);

/// One chunk record's columns.
pub fn decode_record(rec: &[u8]) -> Result<RecordColumns> {
    let word = |i: usize| -> Result<u32> {
        match rec.get(i * 4..i * 4 + 4) {
            Some(b) => Ok(u32::from_le_bytes(b.try_into().unwrap())),
            None => bail!("pack record truncated"),
        }
    };
    let n = word(0)? as usize;
    let nc = word(1)? as usize;
    // Words before the kinds; the kinds take n bytes, padded to whole words.
    let head = 2 + (n + 1) + n;
    let kind_words = n.div_ceil(4);
    let words = head + kind_words + nc;
    if rec.len() != words * 4 {
        bail!("pack record: {} bytes, header says {}", rec.len(), words * 4);
    }
    let starts: Vec<u32> = (0..=n).map(|i| word(2 + i)).collect::<Result<_>>()?;
    let layers: Vec<u32> = (0..n).map(|i| word(3 + n + i)).collect::<Result<_>>()?;
    let kinds = rec[head * 4..head * 4 + n].to_vec();
    let at = head + kind_words;
    let coords: Vec<i32> = (0..nc).map(|i| word(at + i).map(|w| w as i32)).collect::<Result<_>>()?;
    if starts.last().copied().unwrap_or(0) as usize * 2 != nc {
        bail!("pack record: starts end at {} points, coords hold {}", starts.last().unwrap_or(&0), nc / 2);
    }
    // A rectangle is exactly two stored points; a ring at least one.
    for i in 0..n {
        let len = starts[i + 1].checked_sub(starts[i]);
        let ok = match kinds[i] {
            crate::store::KIND_RING => len.is_some(),
            1..=16 => len == Some(2),
            _ => false,
        };
        if !ok {
            bail!("pack record: shape {i} has kind {} and {:?} points", kinds[i], len);
        }
    }
    Ok((coords, starts, layers, kinds))
}

/// One chunk record as a shape store of its own (its shape `j` is the
/// chunk's shape `start + j`): what a streamed client keeps in memory to
/// pick and edit (Phase 7.4).
pub fn decode_store(rec: &[u8]) -> Result<crate::store::ShapeStore> {
    let (coords, starts, layers, kinds) = decode_record(rec)?;
    let layers = bytemuck::cast_slice::<u32, crate::types::Layer>(&layers).to_vec();
    Ok(crate::store::ShapeStore::from_columns(coords, starts, layers, kinds))
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
                let (coords, starts, layers, kinds) = decode_record(rec).unwrap();
                let (c0, s0, l0, k0) = cell.shapes.chunk_columns(ch.start, ch.end);
                assert_eq!(
                    (coords.as_slice(), &starts, layers.as_slice(), kinds.as_slice()),
                    (c0, &s0, l0, k0),
                    "{label}: cell {c} chunk {i}"
                );
                // …so it tessellates exactly like the local chunk.
                let streamed = crate::tessellate_columns(&coords, &starts, &layers, &kinds);
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

    /// Pack v3: the cell table installs with the big placement lists
    /// remote; once their records arrive, the cull selects exactly what it
    /// does in-core at any view where no chunk is dense.
    #[test]
    fn a_streamed_hierarchy_culls_like_the_in_core_one_once_resident() {
        use crate::db::{CellDbBuilder, Instance, InstanceKind};
        use crate::transform::{Bboxf, Transform};
        use crate::types::{Layer, Point};
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let core = b.intern("CORE");
        let leaf = b.intern("LEAF");
        b.cell_mut(leaf).shapes.push(Layer::new(1, 0), [Point::new(0, 0), Point::new(400, 0), Point::new(400, 900), Point::new(0, 900)]);
        b.cell_mut(top).shapes.push(Layer::new(2, 0), [Point::new(0, 0), Point::new(9, 0), Point::new(9, 9), Point::new(0, 9)]);
        for i in 0..20_000i32 {
            b.cell_mut(core).instances.push(Instance {
                child: leaf,
                transform: Transform::translate(((i % 200) * 500) as f64, ((i / 200) * 1_000) as f64),
                kind: InstanceKind::Single,
            });
        }
        for k in 0..3 {
            b.cell_mut(top).instances.push(Instance {
                child: core,
                transform: Transform::translate((k * 120_000) as f64, 0.0),
                kind: InstanceKind::Single,
            });
        }
        let db = b.build().unwrap();
        assert!(db.cell(core).inst_chunks.len() > 1);

        // The pack's two placement artefacts, then the client's view of them.
        let table = crate::serialize::cell_table_meta(&db, INLINE_PLACEMENTS);
        let mut placements = Vec::new();
        let offsets = write_placements(&db, INLINE_PLACEMENTS, &mut placements).unwrap();
        assert!(table.len() < crate::skeleton_meta(&db).len() / 20, "the cell table drops the big list");
        let (mut streamed, _) = crate::ProgressiveLoad::new(&table).unwrap();
        assert!(streamed.cell(core).remote.is_some() && streamed.cell(top).remote.is_none());
        assert!(streamed.cell(core).instances.is_empty());

        let view = Bboxf { min_x: 125_000.0, min_y: 20_000.0, max_x: 160_000.0, max_y: 40_000.0 };
        let before = crate::cull(&streamed, &view, 0.05, 4.0);
        assert!(!before.placements_wanted.is_empty(), "visible remote chunks are asked for");
        // Deliver every wanted chunk from the placement records.
        let base = inst_chunk_base(&db);
        for &(c, ci) in &before.placements_wanted {
            let k = (base[c] + ci) as usize;
            let rec = &placements[offsets[k] as usize..offsets[k + 1] as usize];
            let insts = crate::serialize::decode_instances(rec, db.cells.len()).unwrap();
            streamed.cells[c].remote.as_mut().unwrap().chunks[ci as usize] = Some(insts);
        }
        let after = crate::cull(&streamed, &view, 0.05, 4.0);
        assert!(after.placements_wanted.is_empty());
        let local = crate::cull(&db, &view, 0.05, 4.0);
        let key = |v: &crate::VisibleSet| {
            let mut d: Vec<(usize, u32, usize)> = v.draws.iter().map(|d| (d.cell, d.chunk, d.transforms.len())).collect();
            d.sort();
            (d, v.proxies.len())
        };
        assert_eq!(key(&after), key(&local), "resident ⇒ same selection as in-core");

        // Zoomed out, the big list draws as one proxy per chunk, unfetched.
        let fit = Bboxf { min_x: -1e5, min_y: -1e5, max_x: 5e5, max_y: 3e5 };
        let far = crate::cull(&streamed, &fit, 0.0005, 4.0);
        assert!(far.stats.dense_inst_chunks > 0 && far.placements_wanted.is_empty());
        assert!(far.stats.nodes_visited < crate::cull(&db, &fit, 0.0005, 4.0).stats.nodes_visited / 10);
    }

    #[test]
    fn chunk_looks_measure_coverage_and_travel_in_the_cell_table() {
        // flat_N-style lattice: 100 nm squares at a 200 nm pitch, layers 0..4
        // in turn — each layer covers ~5% of the chunk, the five 25%.
        use crate::db::CellDbBuilder;
        use crate::types::{Layer, Point};
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        for i in 0..10_000i32 {
            let (x, y) = ((i % 100) * 200, (i / 100) * 200);
            b.cell_mut(top).shapes.push(
                Layer::new((i % 5) as u16, 0),
                [Point::new(x, y), Point::new(x + 100, y), Point::new(x + 100, y + 100), Point::new(x, y + 100)],
            );
        }
        let mut db = b.build().unwrap();
        annotate_chunk_looks(&mut db);
        let look = &db.cell(top).chunk_looks[0];
        assert_eq!(look.len(), 5);
        for &(_, c) in look {
            assert!((12..=14).contains(&c), "≈5% of 255, got {c}");
        }
        let (back, _) = crate::ProgressiveLoad::new(&crate::serialize::cell_table_meta(&db, INLINE_PLACEMENTS)).unwrap();
        for (a, b) in db.cells.iter().zip(&back.cells) {
            assert_eq!(a.chunk_looks, b.chunk_looks);
        }
        // An unannotated design carries none.
        let plain = crate::load_layout_db(include_bytes!("../../../assets/hier.gds")).unwrap();
        let (back, _) = crate::ProgressiveLoad::new(&crate::skeleton_meta(&plain)).unwrap();
        assert!(back.cells.iter().all(|c| c.chunk_looks.is_empty()));
    }

    /// Phase 7.4: a streamed design picks what in-core picks, once it has
    /// fetched exactly the chunks the pick reports missing — repeatedly, as
    /// fetched placements reveal more of the hierarchy under the point.
    #[test]
    fn a_streamed_pick_fetches_what_it_needs_then_matches_in_core() {
        use crate::db::{CellDbBuilder, Instance, InstanceKind};
        use crate::edit::EditOverlay;
        use crate::pick::ChunkNeed;
        use crate::transform::Transform;
        use crate::types::{Layer, Point};
        // A CORE with 20 k placements of a LEAF (remote in the pack), a big
        // flat MESH (several shape chunks) and a TOP placing both.
        let rect = |x: i32, y: i32, w: i32, h: i32| [Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h)];
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let core = b.intern("CORE");
        let leaf = b.intern("LEAF");
        let mesh = b.intern("MESH");
        b.cell_mut(leaf).shapes.push(Layer::new(1, 0), rect(0, 0, 400, 900));
        b.cell_mut(leaf).shapes.push(Layer::new(2, 0), rect(100, 100, 100, 500));
        for i in 0..150_000i32 {
            b.cell_mut(mesh).shapes.push(Layer::new(3, 0), rect((i % 400) * 300, (i / 400) * 300, 200, 200));
        }
        for i in 0..20_000i32 {
            b.cell_mut(core).instances.push(Instance {
                child: leaf,
                transform: Transform::translate(((i % 200) * 500) as f64, ((i / 200) * 1_000) as f64),
                kind: InstanceKind::Single,
            });
        }
        for (child, x) in [(core, 0.0), (mesh, 200_000.0)] {
            b.cell_mut(top).instances.push(Instance { child, transform: Transform::translate(x, 0.0), kind: InstanceKind::Single });
        }
        let db = b.build().unwrap();
        assert!(db.cell(mesh).chunks.len() > 1, "MESH must have several shape chunks");

        // The pack, and a client holding only its cell table.
        let mut geometry = Vec::new();
        let offsets = write_geometry(&db, &mut geometry).unwrap();
        let mut placements = Vec::new();
        let p_offsets = write_placements(&db, INLINE_PLACEMENTS, &mut placements).unwrap();
        let (base, p_base) = (chunk_base(&db), inst_chunk_base(&db));
        let (mut streamed, _) = crate::ProgressiveLoad::new(&crate::serialize::cell_table_meta(&db, INLINE_PLACEMENTS)).unwrap();
        for cell in &mut streamed.cells {
            cell.make_remote_shapes();
        }

        let none = EditOverlay::new();
        let points = [(250, 450), (9_850, 7_150), (200_000 + 1_000, 1_000), (200_000 + 60_250, 30_250), (-5, -5), (50_150, 99_100)];
        for (x, y) in points {
            let at = Point::new(x, y);
            let mut rounds = 0;
            let hit = loop {
                let (hit, needs) = crate::pick_composite_needs(&streamed, &none, at);
                if needs.is_empty() {
                    break hit;
                }
                rounds += 1;
                assert!(rounds < 10, "needs must converge");
                for need in needs {
                    match need {
                        ChunkNeed::Shapes(c, ci) => {
                            let k = (base[c] + ci) as usize;
                            let store = decode_store(&geometry[offsets[k] as usize..offsets[k + 1] as usize]).unwrap();
                            streamed.cells[c].remote_shapes.as_mut().unwrap().chunks[ci as usize] = Some(store);
                        }
                        ChunkNeed::Placements(c, ci) => {
                            let k = (p_base[c] + ci) as usize;
                            let rec = &placements[p_offsets[k] as usize..p_offsets[k + 1] as usize];
                            let insts = crate::serialize::decode_instances(rec, db.cells.len()).unwrap();
                            streamed.cells[c].remote.as_mut().unwrap().chunks[ci as usize] = Some(insts);
                        }
                    }
                }
            };
            let local = crate::pick_composite(&db, &none, at);
            assert_eq!(hit.as_ref().map(|h| (h.target, h.path.clone())), local.as_ref().map(|h| (h.target, h.path.clone())), "pick at {at:?}");
        }
        // Only what the picks touched is resident: far from all of MESH.
        let resident = streamed.cell(mesh).remote_shapes.as_ref().unwrap().chunks.iter().filter(|c| c.is_some()).count();
        assert!(resident < db.cell(mesh).chunks.len());

        // "Edit this placement only" on remote placements of CORE: once its
        // needs are fetched, a streamed flatten copies exactly what in-core
        // copies. Evict CORE's placements first, so the placement itself is
        // the first need.
        for c in streamed.cells[core].remote.as_mut().unwrap().chunks.iter_mut() {
            *c = None;
        }
        let sizes = crate::subtree_shapes(&db);
        assert_eq!((sizes[leaf], sizes[core]), (2, 40_000));
        for ii in [0u32, 7_777, 19_999] {
            let key = (core, ii, 0, 0);
            let mut rounds = 0;
            loop {
                let needs = crate::flatten_needs(&streamed, key);
                if needs.is_empty() {
                    break;
                }
                rounds += 1;
                assert!(rounds < 5, "flatten needs must converge");
                for need in needs {
                    match need {
                        ChunkNeed::Shapes(c, ci) => {
                            let k = (base[c] + ci) as usize;
                            let store = decode_store(&geometry[offsets[k] as usize..offsets[k + 1] as usize]).unwrap();
                            streamed.cells[c].remote_shapes.as_mut().unwrap().chunks[ci as usize] = Some(store);
                        }
                        ChunkNeed::Placements(c, ci) => {
                            let k = (p_base[c] + ci) as usize;
                            let rec = &placements[p_offsets[k] as usize..p_offsets[k + 1] as usize];
                            let insts = crate::serialize::decode_instances(rec, db.cells.len()).unwrap();
                            streamed.cells[c].remote.as_mut().unwrap().chunks[ci as usize] = Some(insts);
                        }
                    }
                }
            }
            let want = crate::flatten_placement(&db, &none, key, crate::FLATTEN_LIMIT).unwrap();
            let got = crate::flatten_placement(&streamed, &none, key, crate::FLATTEN_LIMIT).unwrap();
            assert_eq!(got, want, "flatten of {key:?}");
            assert_eq!(got.len(), 2);
        }
    }

    #[test]
    fn placement_records_round_trip_and_reject_garbage() {
        use crate::db::{Instance, InstanceKind};
        use crate::transform::Transform;
        let insts = vec![
            Instance { child: 2, transform: Transform::translate(5.0, -7.0), kind: InstanceKind::Single },
            Instance {
                child: 0,
                transform: Transform::translate(1.0, 2.0),
                kind: InstanceKind::Array { cols: 3, rows: 4, col_step: [10.0, 0.0], row_step: [0.0, 20.0] },
            },
        ];
        let rec = crate::serialize::encode_instances(&insts);
        let back = crate::serialize::decode_instances(&rec, 3).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!((back[0].child, back[0].transform), (2, insts[0].transform));
        assert!(matches!(back[1].kind, InstanceKind::Array { cols: 3, rows: 4, .. }));
        assert!(crate::serialize::decode_instances(&rec, 2).is_err(), "child out of range");
        assert!(crate::serialize::decode_instances(&rec[..rec.len() - 1], 3).is_err(), "truncated");
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
