//! Phase 7.5: in-context DRC — the design checked as it is built.
//!
//! Intra-cell checking ([`super::check_cell`]) sees one cell's own shapes. A
//! shape that only meets the rules once a neighbouring cell's or a parent's
//! geometry merges with it is flagged there, and a violation between two
//! cells is never seen. Here the design is cut into world tiles, and each
//! tile is checked on everything that lies in it, gathered through the
//! hierarchy:
//! * the cull finds the placements under the tile (placement chunks prune
//!   big lists);
//! * each cell's R-tree finds its shapes in the tile's window;
//! * an edit overlay's suppressed shapes are skipped and its live copies
//!   added.
//!
//! The tile check itself is the same pure function as the intra-cell one
//! ([`super::check_tile_columns`]), so the semantics are identical; only the
//! geometry is complete.
//!
//! Tiles whose gathered content is identical, relative to the tile, are
//! checked once (the context dedup): arrays and repeated placements give
//! such tiles. Natively the tiles are checked on all cores.

use super::{check_tile_columns, merge_overlapping, DrcKind, DrcRules, TileInput, Violation};
use crate::db::{CellDb, CellId, InstanceKind};
use crate::edit::EditOverlay;
use crate::transform::{Bboxf, Transform};
use crate::types::{Bbox, Layer, Point};
use std::collections::{BTreeMap, HashMap, HashSet};

/// World tiles' side, nm. Small enough that a tile's flat geometry stays a
/// few thousand shapes in a dense core; checks are per tile.
pub const WORLD_TILE: i32 = 20_000;

/// The cores of the world tiles covering `area` (half-open, `side` nm).
pub fn world_tiles(area: &Bbox, side: i32) -> Vec<Bbox> {
    if area.min.x > area.max.x || area.min.y > area.max.y {
        return Vec::new();
    }
    let x0 = area.min.x.div_euclid(side) * side;
    let y0 = area.min.y.div_euclid(side) * side;
    let mut out = Vec::new();
    let mut y = y0;
    while y <= area.max.y {
        let mut x = x0;
        while x <= area.max.x {
            out.push(Bbox { min: Point::new(x, y), max: Point::new(x + side, y + side) });
            x += side;
        }
        y += side;
    }
    out
}

/// Every shape on a ruled layer within reach of `core`, in world nm: base
/// shapes through every placement (minus what `overlay` suppressed), plus the
/// overlay's live copies and drawn shapes.
pub fn gather_world_tile(db: &CellDb, overlay: &EditOverlay, rules: &DrcRules, core: &Bbox) -> TileInput {
    Gather::new(db, overlay, rules).tile(core)
}

/// What a check gathers from, prepared once: the overlay's live shapes are
/// placed in the world up front, so each tile's walk is the plain cull —
/// with an overlay the cull would give up its placement-chunk shortcut. An
/// overlay that hides placements still goes through the overlay-aware cull
/// (the plain one does not know what is hidden).
struct Gather<'a> {
    db: &'a CellDb,
    overlay: &'a EditOverlay,
    rules: &'a DrcRules,
    /// The overlay's live shapes at every placement of their cells, world nm.
    extra: Vec<(Layer, Vec<Point>, Bbox)>,
    /// `extra` by 50 µm grid square.
    grid: HashMap<(i32, i32), Vec<usize>>,
}

const EXTRA_CELL: i32 = 50_000;

impl<'a> Gather<'a> {
    fn new(db: &'a CellDb, overlay: &'a EditOverlay, rules: &'a DrcRules) -> Self {
        let edited: HashSet<CellId> = (0..db.cells.len()).filter(|&c| overlay.live_shapes_in(c).next().is_some()).collect();
        let places = if edited.is_empty() { HashMap::new() } else { placements(db, &edited, Some(overlay)) };
        let mut extra = Vec::new();
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for c in &edited {
            for t in places.get(c).map(Vec::as_slice).unwrap_or(&[]) {
                for s in overlay.live_shapes_in(*c) {
                    if rules.rule_for(s.layer).is_none() {
                        continue;
                    }
                    let pts: Vec<Point> = s.points.iter().map(|p| world(t, *p)).collect();
                    let mut b = Bbox::empty();
                    for p in &pts {
                        b.expand(*p);
                    }
                    for gx in b.min.x.div_euclid(EXTRA_CELL)..=b.max.x.div_euclid(EXTRA_CELL) {
                        for gy in b.min.y.div_euclid(EXTRA_CELL)..=b.max.y.div_euclid(EXTRA_CELL) {
                            grid.entry((gx, gy)).or_default().push(extra.len());
                        }
                    }
                    extra.push((s.layer, pts, b));
                }
            }
        }
        Gather { db, overlay, rules, extra, grid }
    }

    fn tile(&self, core: &Bbox) -> TileInput {
        let reach = self.rules.max_reach();
        let win = Bboxf::new(
            (core.min.x - reach) as f64,
            (core.min.y - reach) as f64,
            (core.max.x + reach) as f64,
            (core.max.y + reach) as f64,
        );
        // A huge scale: no LOD proxies, nothing dense — every shape is seen.
        let hides = !self.overlay.hidden.is_empty();
        let vs = if hides {
            crate::cull::cull_with_overlay(self.db, &win, 1e3, 0.0, self.overlay)
        } else {
            crate::cull::cull(self.db, &win, 1e3, 0.0)
        };
        let mut input = TileInput::default();
        let mut seen: HashSet<(CellId, [u64; 6])> = HashSet::new();
        for d in &vs.draws {
            let cell = self.db.cell(d.cell);
            let suppressed = self.overlay.suppressed_in(d.cell);
            for t in &d.transforms {
                // A cell's chunks share its placements: one query per placement.
                if !seen.insert((d.cell, tf_key(t))) {
                    continue;
                }
                for i in cell.index().query(&t.inverse().apply_bboxf(&win)) {
                    if suppressed.is_some_and(|s| s.contains(&i)) {
                        continue;
                    }
                    let layer = cell.shapes.layer(i as usize);
                    if self.rules.rule_for(layer).is_none() {
                        continue;
                    }
                    let pts: Vec<Point> = cell.shapes.points(i as usize).iter().map(|p| world(t, *p)).collect();
                    input.push(layer, &pts);
                }
            }
        }
        if !self.extra.is_empty() {
            let wb = Bbox { min: Point::new(core.min.x - reach, core.min.y - reach), max: Point::new(core.max.x + reach, core.max.y + reach) };
            let mut taken = HashSet::new();
            for gx in wb.min.x.div_euclid(EXTRA_CELL)..=wb.max.x.div_euclid(EXTRA_CELL) {
                for gy in wb.min.y.div_euclid(EXTRA_CELL)..=wb.max.y.div_euclid(EXTRA_CELL) {
                    for &k in self.grid.get(&(gx, gy)).map(Vec::as_slice).unwrap_or(&[]) {
                        let (layer, pts, b) = &self.extra[k];
                        if super::intersects(b, &wb) && taken.insert(k) {
                            input.push(*layer, pts);
                        }
                    }
                }
            }
        }
        input
    }
}

fn tf_key(t: &Transform) -> [u64; 6] {
    [t.a.to_bits(), t.b.to_bits(), t.c.to_bits(), t.d.to_bits(), t.tx.to_bits(), t.ty.to_bits()]
}

fn world(t: &Transform, p: Point) -> Point {
    let (x, y) = t.apply_point(p);
    Point::new(x.round() as i32, y.round() as i32)
}

/// What an in-context check found and what it cost.
#[derive(Debug, Default)]
pub struct WorldReport {
    /// In world nm, markers merged across tile edges (one per connected
    /// region of a rule's violation).
    pub violations: Vec<Violation>,
    pub tiles: usize,
    /// Tiles with any ruled geometry.
    pub nonempty: usize,
    /// Tiles whose content repeated one checked before (checked once).
    pub shared: usize,
    /// Shapes gathered over all tiles.
    pub shapes: usize,
}

/// Check the tiles `cores` in context, on `threads` threads (1 on the web).
pub fn check_world_tiles(db: &CellDb, overlay: &EditOverlay, rules: &DrcRules, cores: &[Bbox], threads: usize) -> WorldReport {
    check_tiles(&Gather::new(db, overlay, rules), rules, cores, threads)
}

fn check_tiles(gather: &Gather<'_>, rules: &DrcRules, cores: &[Bbox], threads: usize) -> WorldReport {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    // Content key → the tile's markers relative to its core's origin.
    let memo: Mutex<HashMap<(u64, u64), Vec<Violation>>> = Mutex::new(HashMap::new());
    let next = AtomicUsize::new(0);
    let stats = Mutex::new((0usize, 0usize, 0usize)); // nonempty, shared, shapes
    let found: Mutex<Vec<Violation>> = Mutex::new(Vec::new());
    let work = || {
        let mut mine = Vec::new();
        let (mut nonempty, mut shared, mut shapes) = (0, 0, 0);
        loop {
            let k = next.fetch_add(1, Ordering::Relaxed);
            let Some(core) = cores.get(k) else { break };
            let input = gather.tile(core);
            if input.is_empty() {
                continue;
            }
            nonempty += 1;
            shapes += input.len();
            let key = content_key(&input, core.min);
            let cached = memo.lock().unwrap().get(&key).cloned();
            let rel = match cached {
                Some(v) => {
                    shared += 1;
                    v
                }
                None => {
                    let local = Bbox { min: Point::new(0, 0), max: Point::new(core.max.x - core.min.x, core.max.y - core.min.y) };
                    let coords: Vec<i32> = input
                        .coords
                        .chunks_exact(2)
                        .flat_map(|p| [p[0] - core.min.x, p[1] - core.min.y])
                        .collect();
                    let v = check_tile_columns(local, rules, &coords, &input.starts, &input.layers);
                    memo.lock().unwrap().insert(key, v.clone());
                    v
                }
            };
            mine.extend(rel.into_iter().map(|v| Violation { bbox: shift(&v.bbox, core.min), ..v }));
        }
        let mut s = stats.lock().unwrap();
        s.0 += nonempty;
        s.1 += shared;
        s.2 += shapes;
        found.lock().unwrap().extend(mine);
    };
    let threads = threads.max(1);
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::scope(|s| {
        for _ in 1..threads {
            s.spawn(work);
        }
        work();
    });
    #[cfg(target_arch = "wasm32")]
    {
        let _ = threads;
        work();
    }
    let (nonempty, shared, shapes) = *stats.lock().unwrap();
    WorldReport { violations: merge_markers(found.into_inner().unwrap()), tiles: cores.len(), nonempty, shared, shapes }
}

/// The whole design in context, on `threads` threads: its extent, grown by
/// whatever edits reach past it.
pub fn check_world(db: &CellDb, overlay: &EditOverlay, rules: &DrcRules, threads: usize) -> WorldReport {
    let gather = Gather::new(db, overlay, rules);
    let w = db.world_bbox();
    let mut area = Bbox {
        min: Point::new(w.min_x.floor() as i32, w.min_y.floor() as i32),
        max: Point::new(w.max_x.ceil() as i32, w.max_y.ceil() as i32),
    };
    for (_, _, b) in &gather.extra {
        area.expand(b.min);
        area.expand(b.max);
    }
    check_tiles(&gather, rules, &world_tiles(&area, WORLD_TILE), threads)
}

/// Two independent 64-bit hashes of a tile's content relative to `origin`.
fn content_key(input: &TileInput, origin: Point) -> (u64, u64) {
    let (mut a, mut b) = (0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64);
    let mut mix = |v: u32| {
        a = (a ^ v as u64).wrapping_mul(0x0000_0100_0000_01b3);
        b = (b ^ (v as u64).rotate_left(17)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    };
    for (k, &l) in input.layers.iter().enumerate() {
        mix(l);
        let (s, e) = (input.starts[k] as usize * 2, input.starts[k + 1] as usize * 2);
        mix((e - s) as u32);
        for p in input.coords[s..e].chunks_exact(2) {
            mix((p[0] - origin.x) as u32);
            mix((p[1] - origin.y) as u32);
        }
    }
    (a, b)
}

fn shift(b: &Bbox, by: Point) -> Bbox {
    Bbox { min: Point::new(b.min.x + by.x, b.min.y + by.y), max: Point::new(b.max.x + by.x, b.max.y + by.y) }
}

/// Merge markers of one rule and layer that overlap or touch (a violation cut
/// by tile edges), sorted.
pub fn merge_markers(found: Vec<Violation>) -> Vec<Violation> {
    let mut groups: BTreeMap<(DrcKind, Layer), Vec<Bbox>> = BTreeMap::new();
    for v in found {
        groups.entry((v.kind, v.layer)).or_default().push(v.bbox);
    }
    let mut out = Vec::new();
    for ((kind, layer), boxes) in groups {
        out.extend(merge_overlapping(boxes).into_iter().map(|bbox| Violation { kind, layer, bbox }));
    }
    out.sort_by_key(|v| (v.kind, v.layer, v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y));
    out
}

/// Every placement of the cells in `wanted`, as world transforms (the top
/// cell's frame; arrays expanded).
pub fn world_placements(db: &CellDb, wanted: &HashSet<CellId>) -> HashMap<CellId, Vec<Transform>> {
    placements(db, wanted, None)
}

/// [`world_placements`], skipping what `overlay` hides ("edit this placement
/// only").
fn placements(db: &CellDb, wanted: &HashSet<CellId>, overlay: Option<&EditOverlay>) -> HashMap<CellId, Vec<Transform>> {
    let hidden = |key| overlay.is_some_and(|o| o.is_hidden(key));
    // Only descend into cells whose subtree holds a wanted cell.
    fn holds(db: &CellDb, c: CellId, wanted: &HashSet<CellId>, memo: &mut Vec<Option<bool>>) -> bool {
        if let Some(h) = memo[c] {
            return h;
        }
        let h = wanted.contains(&c) || db.cell(c).instances.iter().any(|i| holds(db, i.child, wanted, memo));
        memo[c] = Some(h);
        h
    }
    let mut memo = vec![None; db.cells.len()];
    let holds: Vec<bool> = (0..db.cells.len()).map(|c| holds(db, c, wanted, &mut memo)).collect();
    let mut out: HashMap<CellId, Vec<Transform>> = HashMap::new();
    let mut stack = vec![(db.top, Transform::IDENTITY)];
    while let Some((c, t)) = stack.pop() {
        if wanted.contains(&c) {
            out.entry(c).or_default().push(t);
        }
        for (ii, inst) in db.cell(c).instances.iter().enumerate() {
            if !holds[inst.child] {
                continue;
            }
            match inst.kind() {
                InstanceKind::Single => {
                    if !hidden((c, ii as u32, 0, 0)) {
                        stack.push((inst.child, t.then(&inst.transform())));
                    }
                }
                InstanceKind::Array { cols, rows, .. } => {
                    for r in 0..rows {
                        for col in 0..cols {
                            if !hidden((c, ii as u32, col, r)) {
                                stack.push((inst.child, t.then(&inst.placement(col, r))));
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Intra-cell flags against the design checked in context (Phase 7.5).
#[derive(Debug, Default)]
pub struct FlagReview {
    /// Intra-cell flags, counted per placement in the world.
    pub placed: usize,
    /// …of which an in-context violation of the same rule overlaps: genuine.
    pub genuine: usize,
    /// …of which nothing overlaps: the cell's own geometry is fine once its
    /// context merges in (or the flag is the cell's view of a neighbour).
    pub context_only: usize,
    /// In-context violations no intra-cell flag overlaps: found only in
    /// context (between cells).
    pub only_in_context: usize,
}

/// Sort intra-cell flags (`flags[c]`, cell `c`'s frame) into genuine and
/// context-only against the in-context violations `world` (world frame).
pub fn review_flags(db: &CellDb, flags: &HashMap<CellId, Vec<Violation>>, world: &[Violation]) -> FlagReview {
    let wanted: HashSet<CellId> = flags.iter().filter(|(_, v)| !v.is_empty()).map(|(&c, _)| c).collect();
    let places = world_placements(db, &wanted);
    // A coarse grid over the in-context markers, per rule and layer.
    const CELL: i32 = 50_000;
    let mut grid: HashMap<(DrcKind, Layer, i32, i32), Vec<usize>> = HashMap::new();
    for (k, v) in world.iter().enumerate() {
        for gx in v.bbox.min.x.div_euclid(CELL)..=v.bbox.max.x.div_euclid(CELL) {
            for gy in v.bbox.min.y.div_euclid(CELL)..=v.bbox.max.y.div_euclid(CELL) {
                grid.entry((v.kind, v.layer, gx, gy)).or_default().push(k);
            }
        }
    }
    let mut matched = vec![false; world.len()];
    let mut review = FlagReview::default();
    for (c, vs) in flags {
        for t in places.get(c).map(Vec::as_slice).unwrap_or(&[]) {
            for v in vs {
                let b = t.apply_bbox(&v.bbox);
                let wb = Bbox {
                    min: Point::new(b.min_x.floor() as i32 - 1, b.min_y.floor() as i32 - 1),
                    max: Point::new(b.max_x.ceil() as i32 + 1, b.max_y.ceil() as i32 + 1),
                };
                review.placed += 1;
                let mut hit = false;
                for gx in wb.min.x.div_euclid(CELL)..=wb.max.x.div_euclid(CELL) {
                    for gy in wb.min.y.div_euclid(CELL)..=wb.max.y.div_euclid(CELL) {
                        for &k in grid.get(&(v.kind, v.layer, gx, gy)).map(Vec::as_slice).unwrap_or(&[]) {
                            if super::intersects(&world[k].bbox, &wb) {
                                matched[k] = true;
                                hit = true;
                            }
                        }
                    }
                }
                if hit {
                    review.genuine += 1;
                } else {
                    review.context_only += 1;
                }
            }
        }
    }
    review.only_in_context = matched.iter().filter(|m| !**m).count();
    review
}

/// Every shape of a ruled layer, flattened into one cell (tests: the
/// reference an in-context check must equal).
pub fn flatten_ruled(db: &CellDb, rules: &DrcRules) -> crate::db::Cell {
    let all: HashSet<CellId> = (0..db.cells.len()).collect();
    let places = world_placements(db, &all);
    let mut shapes = crate::store::ShapeStore::new();
    for (c, ts) in &places {
        let cell = db.cell(*c);
        for t in ts {
            for i in 0..cell.shapes.len() {
                let layer = cell.shapes.layer(i);
                if rules.rule_for(layer).is_some() {
                    let pts: Vec<Point> = cell.shapes.points(i).iter().map(|p| world(t, *p)).collect();
                    shapes.push(layer, pts);
                }
            }
        }
    }
    let chunks = crate::chunk::chunk_store(&mut shapes);
    crate::db::Cell::from_parts("FLAT".into(), shapes, Vec::new(), chunks, Vec::new())
}

#[cfg(test)]
mod tests_support {
    pub fn design_and_overlay(seed: u64) -> (crate::db::CellDb, crate::edit::EditOverlay) {
        let db = super::tests::random_design(seed);
        let o = super::tests::random_overlay(&db, seed);
        (db, o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CellDbBuilder, Instance};
    use crate::drc::{check_cell, LayerRule};

    /// A small deterministic generator.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next() % (hi - lo) as u64) as i32
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h)]
    }

    /// Leaves with shapes that touch their edges (so neighbours merge with
    /// them), a mid cell placing them abutted, overlapped, rotated and
    /// mirrored with its own wires over them, and a top placing mids, an
    /// array of leaves and its own shapes.
    pub(super) fn random_design(seed: u64) -> CellDb {
        let mut r = Rng(seed | 1);
        let mut b = CellDbBuilder::new();
        let top = b.intern("TOP");
        let mid = b.intern("MID");
        let leaves: Vec<CellId> = (0..3).map(|k| b.intern(&format!("LEAF{k}"))).collect();
        let layers = [Layer::new(1, 0), Layer::new(2, 0)];
        for &leaf in &leaves {
            for _ in 0..r.range(3, 7) {
                let l = layers[r.range(0, 2) as usize];
                let (x, y) = (r.range(0, 900), r.range(0, 900));
                b.cell_mut(leaf).shapes.push(l, rect(x, y, r.range(40, 400).min(1000 - x), r.range(40, 400).min(1000 - y)));
            }
        }
        for k in 0..12 {
            let leaf = leaves[r.range(0, 3) as usize];
            // Abutted in a row (x = 1000 k), some nudged to overlap or gap.
            let (x, y) = (1000 * k + r.range(-60, 60), r.range(-40, 40));
            b.cell_mut(mid).instances.push(Instance::from_manhattan(leaf, x, y, r.range(0, 8) as u8, InstanceKind::Single));
        }
        for _ in 0..8 {
            let l = layers[r.range(0, 2) as usize];
            b.cell_mut(mid).shapes.push(l, rect(r.range(-500, 12_000), r.range(-500, 1500), r.range(60, 3000), r.range(60, 200)));
        }
        for k in 0..3 {
            b.cell_mut(top).instances.push(Instance::from_manhattan(mid, 0, 1500 * k + r.range(-50, 50), r.range(0, 8) as u8, InstanceKind::Single));
        }
        let step = [1000.0 + r.range(-50, 50) as f64, 0.0];
        b.cell_mut(top).instances.push(Instance::new(
            leaves[0],
            Transform::from_strans(0.0, 5000.0, 0.0, false, 1.0),
            InstanceKind::Array { cols: 4, rows: 2, col_step: step, row_step: [0.0, 1050.0] },
        ));
        for _ in 0..6 {
            let l = layers[r.range(0, 2) as usize];
            b.cell_mut(top).shapes.push(l, rect(r.range(-1000, 13_000), r.range(-1000, 7000), r.range(60, 2000), r.range(60, 120)));
        }
        b.build().unwrap()
    }

    /// `db` with `overlay`'s edits applied: each cell's shapes are its live
    /// base shapes plus the overlay's copies and drawn shapes; hidden
    /// placements are dropped (arrays are expanded so one element can go).
    fn applied(db: &CellDb, overlay: &EditOverlay) -> CellDb {
        let mut b = CellDbBuilder::new();
        let ids: Vec<CellId> = db.cells.iter().map(|c| b.intern(&c.name)).collect();
        for (c, cell) in db.cells.iter().enumerate() {
            let sup = overlay.suppressed_in(c);
            for i in 0..cell.shapes.len() {
                if !sup.is_some_and(|s| s.contains(&(i as u32))) {
                    b.cell_mut(ids[c]).shapes.push(cell.shapes.layer(i), cell.shapes.points(i).to_vec());
                }
            }
            for s in overlay.live_shapes_in(c) {
                b.cell_mut(ids[c]).shapes.push(s.layer, s.points.clone());
            }
            for (ii, inst) in cell.instances.iter().enumerate() {
                match inst.kind() {
                    InstanceKind::Single => {
                        if !overlay.is_hidden((c, ii as u32, 0, 0)) {
                            b.cell_mut(ids[c]).instances.push(Instance::new(ids[inst.child], inst.transform(), InstanceKind::Single));
                        }
                    }
                    InstanceKind::Array { cols, rows, .. } => {
                        for r in 0..rows {
                            for col in 0..cols {
                                if !overlay.is_hidden((c, ii as u32, col, r)) {
                                    b.cell_mut(ids[c]).instances.push(Instance::new(ids[inst.child], inst.placement(col, r), InstanceKind::Single));
                                }
                            }
                        }
                    }
                }
            }
        }
        b.build().unwrap()
    }

    /// Random edits: deletes, moves, drawn shapes, and (half the time) a
    /// hidden array element.
    pub(super) fn random_overlay(db: &CellDb, seed: u64) -> EditOverlay {
        let mut r = Rng(seed.wrapping_mul(7919) | 1);
        let mut o = EditOverlay::new();
        let layers = [Layer::new(1, 0), Layer::new(2, 0)];
        for c in 0..db.cells.len() {
            let n = db.cell(c).shapes.len();
            for _ in 0..2 {
                if n == 0 {
                    break;
                }
                let i = r.range(0, n as i32) as u32;
                if r.range(0, 2) == 0 {
                    o.delete((c, i));
                } else {
                    let (dx, dy) = (r.range(-150, 150), r.range(-150, 150));
                    let pts = db.cell(c).shapes.points(i as usize).iter().map(|p| Point::new(p.x + dx, p.y + dy)).collect();
                    o.move_shape((c, i), crate::edit::Shape { layer: db.cell(c).shapes.layer(i as usize), points: pts });
                }
            }
            let l = layers[r.range(0, 2) as usize];
            let (x, y) = (r.range(-500, 10_000), r.range(-500, 4000));
            o.add((1, c as u32), c, crate::edit::Shape { layer: l, points: rect(x, y, r.range(30, 600), r.range(30, 300)) });
        }
        if seed.is_multiple_of(2) {
            let top = db.top;
            if let Some(ii) = db.cell(top).instances.iter().position(|i| matches!(i.kind(), InstanceKind::Array { .. })) {
                o.hide_placement((top, ii as u32, 1, 0));
            }
        }
        o
    }

    #[test]
    fn in_context_with_edits_equals_the_edited_design_flattened() {
        let mut rules = DrcRules::default();
        rules.per_layer.insert(Layer::new(1, 0), LayerRule { min_width: 80, min_spacing: 120 });
        rules.per_layer.insert(Layer::new(2, 0), LayerRule { min_width: 100, min_spacing: 90 });
        let key = |v: &Violation| (v.kind, v.layer, v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y);
        for seed in 1..=12u64 {
            let db = random_design(seed);
            let overlay = random_overlay(&db, seed);
            let flat = flatten_ruled(&applied(&db, &overlay), &rules);
            let want = merge_markers(check_cell(&flat, 0, &EditOverlay::new(), &rules));
            let got = check_world(&db, &overlay, &rules, 3);
            assert_eq!(got.violations.iter().map(key).collect::<Vec<_>>(), want.iter().map(key).collect::<Vec<_>>(), "seed {seed}");
        }
    }

    #[test]
    fn in_context_equals_the_flattened_design() {
        let mut rules = DrcRules::default();
        rules.per_layer.insert(Layer::new(1, 0), LayerRule { min_width: 80, min_spacing: 120 });
        rules.per_layer.insert(Layer::new(2, 0), LayerRule { min_width: 100, min_spacing: 90 });
        let none = EditOverlay::new();
        let mut differing_from_intra = 0;
        for seed in 1..=12u64 {
            let db = random_design(seed);
            let flat = flatten_ruled(&db, &rules);
            let want = merge_markers(check_cell(&flat, 0, &none, &rules));
            for threads in [1, 4] {
                let got = check_world(&db, &none, &rules, threads);
                let key = |v: &Violation| (v.kind, v.layer, v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y);
                assert_eq!(
                    got.violations.iter().map(key).collect::<Vec<_>>(),
                    want.iter().map(key).collect::<Vec<_>>(),
                    "seed {seed}, {threads} threads"
                );
            }
            // Intra-cell checking sees something else on these designs.
            let mut flags: HashMap<CellId, Vec<Violation>> = HashMap::new();
            for c in 0..db.cells.len() {
                flags.insert(c, check_cell(db.cell(c), c, &none, &rules));
            }
            let review = review_flags(&db, &flags, &want);
            differing_from_intra += review.context_only + review.only_in_context;
        }
        assert!(differing_from_intra > 0, "the designs must exercise context");
    }
}

#[cfg(test)]
mod cull_agreement {
    /// The overlay-aware cull, over everything, emits each live shape once
    /// per visible placement of its cell — what this module places itself
    /// (a window query may miss a shape drawn far past its cell's extent:
    /// ancestors' bboxes do not grow, a documented limit of the cull).
    #[test]
    fn overlay_list_matches_live_placements() {
        use super::tests_support::*;
        for seed in 1..=12u64 {
            let (db, overlay) = design_and_overlay(seed);
            let w = crate::transform::Bboxf::new(-1e8, -1e8, 1e8, 1e8);
            let vs = crate::cull::cull_with_overlay(&db, &w, 1e3, 0.0, &overlay);
            let edited: std::collections::HashSet<_> = (0..db.cells.len()).filter(|&c| overlay.live_shapes_in(c).next().is_some()).collect();
            let places = super::placements(&db, &edited, Some(&overlay));
            let want: usize = edited.iter().map(|c| places.get(c).map_or(0, |v| v.len()) * overlay.live_shapes_in(*c).count()).sum();
            assert_eq!(vs.overlay.len(), want, "seed {seed}");
        }
    }
}
