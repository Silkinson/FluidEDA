//! Design-rule checks — Phase 5b: **merge-first, tiled, edge-pair** DRC.
//!
//! Phase 5 SLICE 3 shipped a per-shape skeleton (bbox width, shape-pair
//! spacing). Its own measurement named the fix: on caravel_core, 593 k of its
//! 611 k flags were per-shape width on P&R **fracture slivers** — abutting
//! pieces of one wide wire, each "too narrow" alone. Real DRC measures the
//! *merged* layer. This module now does:
//!
//! 1. **merge** — per layer, union every live shape near a tile (base minus
//!    tombstones, plus moved/vertex-edited copies and drawn shapes) with the
//!    integer boolean kernel ([`crate::boolean`]), clipped to the tile's window;
//! 2. **measure** — width and space as edge pairs on the merged boundary
//!    ([`edges`]): exact for any polygon (an L's thin arm, a diagonal neck, an
//!    acute tip), and space now includes **notches** within one polygon;
//! 3. **tile** — the cell is cut into a grid ([`grid`]); each tile is an
//!    independent pure function of Pod columns ([`check_tile_columns`]), so the
//!    same code runs inline under a frame budget or in a Web Worker pool, and an
//!    edit re-checks only the tiles within `reach` of what changed.
//!
//! ## Scope (still honest)
//!
//! Checks remain **intra-cell**: a cell is checked in its own frame, composited
//! with the edit overlay; a shape in cell A near an *instance* of cell B is not
//! seen (hierarchical / cross-instance context is the deferred hard problem).
//! Rules are min width + min space per layer; wide-metal, end-of-line and
//! enclosure rules are out of scope.
//!
//! No wgpu, no network — core stays the Web Worker seam.

pub mod driver;
pub mod edges;
pub mod grid;

pub use driver::{DrcDriver, DrcStep, PassStats, TileJob};
pub use grid::{DrcGrid, DrcGridBuilder, TileKey, TILE_TARGET_SHAPES};

use crate::boolean::union_clipped;
use crate::db::{Cell, CellId};
use crate::edit::EditOverlay;
use crate::types::{Bbox, Layer, Point};
use edges::{check_region, BoxF, Facing};
use std::collections::{BTreeMap, HashMap};

/// Slack (nm) added to a rule's distance to get its reach, so geometry at
/// exactly the rule distance from a tile core is inside the window.
const REACH_MARGIN: i32 = 2;

/// Rules for one layer, in nm. `0` disables that check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerRule {
    pub min_width: i32,
    pub min_spacing: i32,
}

impl LayerRule {
    /// How far a tile must look past its core for this layer.
    pub fn reach(&self) -> i32 {
        self.min_width.max(self.min_spacing).max(0) + REACH_MARGIN
    }
}

/// The rule table: per-layer entries plus an optional blanket default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrcRules {
    /// Applied to any layer without a `per_layer` entry (None ⇒ unchecked).
    pub default_rule: Option<LayerRule>,
    pub per_layer: HashMap<Layer, LayerRule>,
}

impl DrcRules {
    /// The blanket deck: 100 nm width / 100 nm space on every layer. A
    /// placeholder, not a PDK — it makes a carelessly drawn shape flag at once.
    pub fn default_table() -> Self {
        DrcRules { default_rule: Some(LayerRule { min_width: 100, min_spacing: 100 }), per_layer: HashMap::new() }
    }

    pub fn rule_for(&self, layer: Layer) -> Option<LayerRule> {
        self.per_layer.get(&layer).copied().or(self.default_rule)
    }

    /// The largest reach of any rule — the grid's bucketing halo.
    pub fn max_reach(&self) -> i32 {
        self.per_layer.values().chain(self.default_rule.iter()).map(|r| r.reach()).max().unwrap_or(0)
    }

    /// Flatten to `i32`s for a worker job: `[has_default, w, s, n, (layer, w, s)×n]`
    /// with each layer packed `layer | datatype << 16`.
    pub fn pack(&self) -> Vec<i32> {
        let mut v = Vec::with_capacity(4 + 3 * self.per_layer.len());
        match self.default_rule {
            Some(r) => v.extend([1, r.min_width, r.min_spacing]),
            None => v.extend([0, 0, 0]),
        }
        let mut layers: Vec<_> = self.per_layer.iter().collect();
        layers.sort_by_key(|(l, _)| **l);
        v.push(layers.len() as i32);
        for (l, r) in layers {
            v.extend([pack_layer(*l) as i32, r.min_width, r.min_spacing]);
        }
        v
    }

    /// Inverse of [`pack`](Self::pack). Malformed input yields an empty deck.
    pub fn unpack(v: &[i32]) -> Self {
        let mut rules = DrcRules::default();
        if v.len() < 4 {
            return rules;
        }
        if v[0] != 0 {
            rules.default_rule = Some(LayerRule { min_width: v[1], min_spacing: v[2] });
        }
        let n = v[3].max(0) as usize;
        for k in 0..n {
            let Some(e) = v.get(4 + 3 * k..7 + 3 * k) else { break };
            rules.per_layer.insert(unpack_layer(e[0] as u32), LayerRule { min_width: e[1], min_spacing: e[2] });
        }
        rules
    }
}

/// `layer | datatype << 16` — the packing every worker column uses.
pub fn pack_layer(l: Layer) -> u32 {
    l.layer as u32 | ((l.datatype as u32) << 16)
}

pub fn unpack_layer(v: u32) -> Layer {
    Layer::new((v & 0xffff) as u16, (v >> 16) as u16)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DrcKind {
    Width,
    Spacing,
}

/// One violation marker, in the checked cell's local nm: the region of the
/// merged boundary that is too close to itself, clipped to one tile's core.
#[derive(Debug, Clone)]
pub struct Violation {
    pub kind: DrcKind,
    pub layer: Layer,
    pub bbox: Bbox,
}

/// One tile's shapes as Pod columns — the exact shape a worker job ships
/// (`starts` holds `len + 1` point offsets, as in [`crate::ShapeStore`]).
#[derive(Debug, Clone, Default)]
pub struct TileInput {
    pub coords: Vec<i32>,
    pub starts: Vec<u32>,
    pub layers: Vec<u32>,
}

impl TileInput {
    fn push(&mut self, layer: Layer, pts: &[Point]) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        for p in pts {
            self.coords.push(p.x);
            self.coords.push(p.y);
        }
        self.starts.push((self.coords.len() / 2) as u32);
        self.layers.push(pack_layer(layer));
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

fn ring_bbox(pts: &[Point]) -> Bbox {
    let mut b = Bbox::empty();
    for p in pts {
        b.expand(*p);
    }
    b
}

fn inflate(b: &Bbox, r: i32) -> Bbox {
    Bbox {
        min: Point::new(b.min.x.saturating_sub(r), b.min.y.saturating_sub(r)),
        max: Point::new(b.max.x.saturating_add(r), b.max.y.saturating_add(r)),
    }
}

fn intersects(a: &Bbox, b: &Bbox) -> bool {
    a.min.x <= b.max.x && b.min.x <= a.max.x && a.min.y <= b.max.y && b.min.y <= a.max.y
}

/// Collect tile `t`'s input: every live shape of `cell` on a ruled layer whose
/// bbox meets that layer's window (core ⊕ reach) — base shapes from the grid
/// bucket minus tombstones, plus the overlay's live copies and adds.
pub fn gather_tile(
    cell: &Cell,
    cell_id: CellId,
    overlay: &EditOverlay,
    grid: &DrcGrid,
    rules: &DrcRules,
    t: TileKey,
) -> TileInput {
    let core = grid.tile_core(t);
    let mut input = TileInput::default();
    let suppressed = overlay.suppressed_in(cell_id);
    let mut windows: HashMap<Layer, Option<Bbox>> = HashMap::new();
    let mut window = |layer: Layer| -> Option<Bbox> {
        *windows.entry(layer).or_insert_with(|| rules.rule_for(layer).map(|r| inflate(&core, r.reach())))
    };
    for &i in grid.bucket(t) {
        if suppressed.is_some_and(|s| s.contains(&i)) {
            continue; // deleted or moved away — its live copy comes from the overlay
        }
        let layer = cell.shapes.layer(i as usize);
        let Some(win) = window(layer) else { continue };
        let pts = cell.shapes.points(i as usize);
        if intersects(&ring_bbox(pts), &win) {
            input.push(layer, pts);
        }
    }
    for s in overlay.live_shapes_in(cell_id) {
        let Some(win) = window(s.layer) else { continue };
        if intersects(&ring_bbox(&s.points), &win) {
            input.push(s.layer, &s.points);
        }
    }
    input
}

/// Check one tile from its gathered input. See [`check_tile_columns`].
pub fn check_tile(core: Bbox, rules: &DrcRules, input: &TileInput) -> Vec<Violation> {
    check_tile_columns(core, rules, &input.coords, &input.starts, &input.layers)
}

/// Check one tile — the pure worker-seam function. `core` is the tile's
/// half-open core `[min, max)`; the columns hold every live shape within reach.
/// Per ruled layer: union ∩ window, then width (interior-facing pairs) and
/// space (exterior-facing pairs) on the merged edges; markers are clipped to
/// the core and overlapping ones merged. Output is sorted (deterministic).
pub fn check_tile_columns(core: Bbox, rules: &DrcRules, coords: &[i32], starts: &[u32], layers: &[u32]) -> Vec<Violation> {
    let n = layers.len();
    if n == 0 || starts.len() != n + 1 {
        return Vec::new();
    }
    let pts_of = |i: usize| -> &[Point] {
        let (s, e) = (starts[i] as usize * 2, starts[i + 1] as usize * 2);
        bytemuck::cast_slice(&coords[s..e.min(coords.len())])
    };
    let mut by_layer: BTreeMap<Layer, Vec<usize>> = BTreeMap::new();
    for (i, &l) in layers.iter().enumerate() {
        by_layer.entry(unpack_layer(l)).or_default().push(i);
    }

    let mut out = Vec::new();
    for (layer, idx) in by_layer {
        let Some(rule) = rules.rule_for(layer) else { continue };
        let win = inflate(&core, rule.reach());
        let region = union_clipped(idx.iter().map(|&i| pts_of(i)), Some(win), core.min);
        if region.is_empty() {
            continue;
        }
        for (kind, d, facing) in
            [(DrcKind::Width, rule.min_width, Facing::Interior), (DrcKind::Spacing, rule.min_spacing, Facing::Exterior)]
        {
            let mut raw: Vec<BoxF> = Vec::new();
            check_region(&region, d, facing, Some(&win), &mut raw);
            let clipped: Vec<Bbox> = raw.iter().filter_map(|m| clip_to_core(m, &core)).collect();
            for bbox in merge_overlapping(clipped) {
                out.push(Violation { kind, layer, bbox });
            }
        }
    }
    out.sort_by_key(|v| (v.kind, v.layer, v.bbox.min.x, v.bbox.min.y, v.bbox.max.x, v.bbox.max.y));
    out
}

/// Clip a marker to the half-open core `[min, max)`, rounding outward to nm.
/// `None` if the marker has no point in the core (it belongs to a neighbour).
fn clip_to_core(m: &BoxF, core: &Bbox) -> Option<Bbox> {
    let (x0, y0, x1, y1) = (core.min.x as f64, core.min.y as f64, core.max.x as f64, core.max.y as f64);
    if !(m[2] >= x0 && m[0] < x1 && m[3] >= y0 && m[1] < y1) {
        return None;
    }
    let lo = |v: f64| v.floor() as i32;
    let hi = |v: f64| v.ceil() as i32;
    Some(Bbox {
        min: Point::new(lo(m[0].max(x0)), lo(m[1].max(y0))),
        max: Point::new(hi(m[2].min(x1)), hi(m[3].min(y1))),
    })
}

/// Merge boxes that overlap or touch into their union bboxes (one marker per
/// connected cluster — e.g. the two edge pairs of one corner-to-corner gap).
fn merge_overlapping(mut boxes: Vec<Bbox>) -> Vec<Bbox> {
    if boxes.len() < 2 {
        return boxes;
    }
    boxes.sort_by_key(|b| b.min.x);
    let n = boxes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for i in 0..n {
        for j in i + 1..n {
            if boxes[j].min.x > boxes[i].max.x {
                break; // sorted by min.x: nothing further can overlap i
            }
            if intersects(&boxes[i], &boxes[j]) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a] = b;
                }
            }
        }
    }
    let mut groups: HashMap<usize, Bbox> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let g = groups.entry(r).or_insert_with(Bbox::empty);
        g.expand(boxes[i].min);
        g.expand(boxes[i].max);
    }
    groups.into_values().collect()
}

/// The tiles a full check of `cell` must visit: every tile with base shapes in
/// reach, plus every tile near a live overlay shape (a drawn shape may sit
/// beyond the base extent). Sorted, deduplicated.
pub fn cell_tiles(grid: &DrcGrid, cell_id: CellId, overlay: &EditOverlay, rules: &DrcRules) -> Vec<TileKey> {
    let mut tiles = grid.nonempty_tiles();
    for s in overlay.live_shapes_in(cell_id) {
        if rules.rule_for(s.layer).is_some() {
            tiles.extend(grid.tiles_near(&ring_bbox(&s.points)));
        }
    }
    tiles.sort_unstable();
    tiles.dedup();
    tiles
}

/// Build the grid for `cell` under `rules` (convenience for tests / benches).
pub fn build_grid(cell: &Cell, rules: &DrcRules) -> DrcGrid {
    DrcGrid::build(cell, rules.max_reach(), &|l| rules.rule_for(l).is_some())
}

/// Convenience: check the whole cell in one call (tests, native bench).
pub fn check_cell(cell: &Cell, cell_id: CellId, overlay: &EditOverlay, rules: &DrcRules) -> Vec<Violation> {
    let grid = build_grid(cell, rules);
    let mut out = Vec::new();
    for t in cell_tiles(&grid, cell_id, overlay, rules) {
        let input = gather_tile(cell, cell_id, overlay, &grid, rules, t);
        out.extend(check_tile(grid.tile_core(t), rules, &input));
    }
    out
}

#[cfg(test)]
mod tests;
