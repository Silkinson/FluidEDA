//! The DRC scheduler (Phase 5b): which tiles to check, in what order, and
//! which results are still current.
//!
//! The driver is a pure state machine — no clock, no threads, no web types — so
//! the same logic drives an inline, frame-budgeted check (the app calls
//! [`DrcDriver::step`] until its budget runs out and runs each job itself) and a
//! Web Worker pool (the app posts each job and feeds replies back through
//! [`DrcDriver::accept`], possibly frames later). Per cell it holds:
//!
//! * the **grid**, built incrementally ([`DrcGridBuilder`]) — the budgeted
//!   replacement for the atomic lazy R-tree build;
//! * **results per tile**, so an edit replaces only the tiles it touched;
//! * a **pending** tile queue (edits jump to the front) and a per-tile
//!   **generation**: re-dirtying a tile bumps it, and a reply computed against an
//!   older generation (a worker job in flight when the user edited) is dropped.
//!
//! Scheduling policy: a full-cell check is only run when asked
//! ([`request_full`](DrcDriver::request_full) — "check visible"); an edit in a
//! cell nobody asked to check re-checks just the tiles near the edit. That is
//! the interactive loop: draw a sliver, see it flag, without paying for the
//! 2.5 M-shape cell around it.

use super::{cell_tiles, gather_tile, DrcGrid, DrcGridBuilder, DrcRules, TileInput, TileKey, Violation};
use crate::db::{CellDb, CellId};
use crate::edit::EditOverlay;
use crate::types::Bbox;
use std::collections::{HashMap, HashSet, VecDeque};

/// Shapes per grid-build slice (between the caller's budget checks).
pub const GRID_STEP_SHAPES: usize = 100_000;

enum GridState {
    Building(DrcGridBuilder),
    Ready(DrcGrid),
}

struct CellDrc {
    grid: GridState,
    results: HashMap<TileKey, Vec<Violation>>,
    pending: VecDeque<TileKey>,
    queued: HashSet<TileKey>,
    gen: HashMap<TileKey, u32>,
    in_flight: HashSet<TileKey>,
    /// A full-cell check was requested (all tiles, once the grid is ready).
    full: bool,
    /// A full pass has completed (so "check visible" can skip this cell).
    full_done: bool,
    /// Edit regions that arrived while the grid was still building.
    deferred: Vec<Bbox>,
    pass_active: bool,
    pass_start_ms: f64,
    pass_tiles: usize,
}

impl CellDrc {
    fn new(builder: DrcGridBuilder) -> Self {
        CellDrc {
            grid: GridState::Building(builder),
            results: HashMap::new(),
            pending: VecDeque::new(),
            queued: HashSet::new(),
            gen: HashMap::new(),
            in_flight: HashSet::new(),
            full: false,
            full_done: false,
            deferred: Vec::new(),
            pass_active: false,
            pass_start_ms: 0.0,
            pass_tiles: 0,
        }
    }

    /// (Re)schedule `tiles`: bump each one's generation (so any reply in flight
    /// for it is now stale) and queue it — at the front for an edit.
    fn schedule(&mut self, tiles: impl IntoIterator<Item = TileKey>, front: bool, now_ms: f64) {
        if !self.pass_active {
            self.pass_active = true;
            self.pass_start_ms = now_ms;
            self.pass_tiles = 0;
        }
        for t in tiles {
            let g = self.gen.entry(t).or_insert(0);
            *g = g.wrapping_add(1);
            if self.queued.insert(t) {
                if front {
                    self.pending.push_front(t);
                } else {
                    self.pending.push_back(t);
                }
            } else if front {
                if let Some(pos) = self.pending.iter().position(|&p| p == t) {
                    self.pending.remove(pos);
                }
                self.pending.push_front(t);
            }
        }
    }
}

/// One unit of DRC work: check `input` against the rules over `core`, then
/// hand the result back with [`DrcDriver::accept`] quoting `cell/tile/gen`.
pub struct TileJob {
    pub cell: CellId,
    pub tile: TileKey,
    pub gen: u32,
    pub core: Bbox,
    pub input: TileInput,
}

/// What [`DrcDriver::step`] did.
pub enum DrcStep {
    /// Advanced a grid build (call again while budget remains).
    Progress,
    /// A tile to check.
    Job(TileJob),
    /// Nothing runnable now: idle, or every runnable tile is already in flight
    /// / the in-flight cap is reached (wait for replies).
    Idle,
}

/// A completed pass over one cell (panel readout / harness metric).
#[derive(Debug, Clone, Copy)]
pub struct PassStats {
    pub cell: CellId,
    /// Wall time from the pass's first scheduled tile to its last accepted one.
    pub ms: f64,
    pub tiles: usize,
    /// Violations in the cell after the pass.
    pub violations: usize,
}

/// The scheduler (see the module note).
pub struct DrcDriver {
    rules: DrcRules,
    cells: HashMap<CellId, CellDrc>,
    /// Cells with runnable work, most urgent first.
    queue: VecDeque<CellId>,
    /// Cap on jobs out at once (1 for inline; ~2× the pool size for workers).
    pub max_in_flight: usize,
    in_flight_total: usize,
    pub last_pass: Option<PassStats>,
    /// Target shapes per tile for grids built from now on.
    pub tile_target: usize,
}

impl DrcDriver {
    pub fn new(rules: DrcRules) -> Self {
        DrcDriver {
            rules,
            cells: HashMap::new(),
            queue: VecDeque::new(),
            max_in_flight: 1,
            in_flight_total: 0,
            last_pass: None,
            tile_target: super::TILE_TARGET_SHAPES,
        }
    }

    pub fn rules(&self) -> &DrcRules {
        &self.rules
    }

    /// Swap the rule deck. Grids depend on its reach and ruled layers, so every
    /// cell's state is dropped.
    pub fn set_rules(&mut self, rules: DrcRules) {
        self.rules = rules;
        self.clear();
    }

    /// Forget everything (new design, deck change, DRC switched off). Replies
    /// still in flight will find no cell state and be dropped.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.queue.clear();
        self.in_flight_total = 0;
        self.last_pass = None;
    }

    fn ensure(&mut self, db: &CellDb, cell: CellId) -> &mut CellDrc {
        let (rules, target) = (&self.rules, self.tile_target);
        self.cells
            .entry(cell)
            .or_insert_with(|| CellDrc::new(DrcGridBuilder::new(db.cell(cell), rules.max_reach(), target)))
    }

    fn enqueue(&mut self, cell: CellId, front: bool) {
        if let Some(pos) = self.queue.iter().position(|&c| c == cell) {
            if !front {
                return;
            }
            self.queue.remove(pos);
        }
        if front {
            self.queue.push_front(cell);
        } else {
            self.queue.push_back(cell);
        }
    }

    /// Check all of `cell` (no-op if a full check is done or under way).
    pub fn request_full(&mut self, db: &CellDb, overlay: &EditOverlay, cell: CellId, now_ms: f64) {
        let rules = self.rules.clone();
        let st = self.ensure(db, cell);
        if st.full || st.full_done {
            return;
        }
        st.full = true;
        if let GridState::Ready(g) = &st.grid {
            let tiles = cell_tiles(g, cell, overlay, &rules);
            st.schedule(tiles, false, now_ms);
        }
        self.enqueue(cell, false);
    }

    /// Geometry inside `region` (cell-local nm) of `cell` changed: re-check the
    /// tiles near it, ahead of any background work.
    pub fn invalidate(&mut self, db: &CellDb, cell: CellId, region: Bbox, now_ms: f64) {
        let st = self.ensure(db, cell);
        match &st.grid {
            GridState::Ready(g) => {
                let tiles = g.tiles_near(&region);
                st.schedule(tiles, true, now_ms);
            }
            GridState::Building(_) => st.deferred.push(region),
        }
        self.enqueue(cell, true);
    }

    /// Advance: build a grid slice, or produce the next tile job.
    pub fn step(&mut self, db: &CellDb, overlay: &EditOverlay, now_ms: f64) -> DrcStep {
        loop {
            let Some(&cid) = self.queue.front() else { return DrcStep::Idle };
            let rules = &self.rules;
            let Some(st) = self.cells.get_mut(&cid) else {
                self.queue.pop_front();
                continue;
            };
            let cell = db.cell(cid);
            if let GridState::Building(b) = &mut st.grid {
                let checked = |l| rules.rule_for(l).is_some();
                let done = b.step(cell, GRID_STEP_SHAPES, &checked);
                if done {
                    let GridState::Building(b) = std::mem::replace(&mut st.grid, GridState::Ready(placeholder()))
                    else {
                        unreachable!()
                    };
                    let g = b.finish();
                    let mut tiles: Vec<TileKey> = Vec::new();
                    for r in std::mem::take(&mut st.deferred) {
                        tiles.extend(g.tiles_near(&r));
                    }
                    st.schedule(tiles, true, now_ms);
                    if st.full {
                        let all = cell_tiles(&g, cid, overlay, rules);
                        st.schedule(all, false, now_ms);
                    }
                    st.grid = GridState::Ready(g);
                }
                return DrcStep::Progress;
            }
            let GridState::Ready(g) = &st.grid else { unreachable!() };
            if self.in_flight_total >= self.max_in_flight {
                return DrcStep::Idle;
            }
            // Next pending tile that is not already out (a re-dirtied tile whose
            // old job is still in flight waits for that reply to clear).
            let mut picked = None;
            for _ in 0..st.pending.len() {
                let t = st.pending.pop_front().unwrap();
                if st.in_flight.contains(&t) {
                    st.pending.push_back(t);
                    continue;
                }
                picked = Some(t);
                break;
            }
            match picked {
                Some(t) => {
                    st.queued.remove(&t);
                    st.in_flight.insert(t);
                    self.in_flight_total += 1;
                    let gen = st.gen.get(&t).copied().unwrap_or(0);
                    let input = gather_tile(cell, cid, overlay, g, rules, t);
                    return DrcStep::Job(TileJob { cell: cid, tile: t, gen, core: g.tile_core(t), input });
                }
                None => {
                    // Nothing dispatchable here: done, or waiting on replies.
                    // Either way the cell leaves the run queue; `accept` closes
                    // the pass when its last reply lands.
                    self.queue.pop_front();
                    if st.pending.is_empty() && st.in_flight.is_empty() {
                        Self::finish_pass(&mut self.last_pass, cid, st, now_ms);
                    } else if !st.pending.is_empty() {
                        // Blocked behind in-flight tiles — retry after the rest.
                        self.queue.push_back(cid);
                        return DrcStep::Idle;
                    }
                }
            }
        }
    }

    fn finish_pass(last: &mut Option<PassStats>, cid: CellId, st: &mut CellDrc, now_ms: f64) {
        if !st.pass_active {
            return;
        }
        st.pass_active = false;
        if st.full {
            st.full_done = true;
        }
        *last = Some(PassStats {
            cell: cid,
            ms: now_ms - st.pass_start_ms,
            tiles: st.pass_tiles,
            violations: st.results.values().map(|v| v.len()).sum(),
        });
    }

    /// A job's result. Stale replies (the tile was re-dirtied since dispatch, or
    /// the state was cleared) are dropped; the in-flight slot is freed either way.
    pub fn accept(&mut self, cell: CellId, tile: TileKey, gen: u32, violations: Vec<Violation>, now_ms: f64) {
        let Some(st) = self.cells.get_mut(&cell) else { return };
        if st.in_flight.remove(&tile) {
            self.in_flight_total = self.in_flight_total.saturating_sub(1);
        }
        if st.gen.get(&tile) != Some(&gen) {
            return;
        }
        st.pass_tiles += 1;
        if violations.is_empty() {
            st.results.remove(&tile);
        } else {
            st.results.insert(tile, violations);
        }
        if st.pending.is_empty() && st.in_flight.is_empty() {
            Self::finish_pass(&mut self.last_pass, cell, st, now_ms);
        }
    }

    /// The executor went away with jobs out (worker pool torn down): put every
    /// in-flight tile back in the queue under a new generation.
    pub fn requeue_in_flight(&mut self, now_ms: f64) {
        let mut cells = Vec::new();
        for (&cid, st) in self.cells.iter_mut() {
            let out: Vec<TileKey> = st.in_flight.drain().collect();
            if !out.is_empty() {
                st.schedule(out, true, now_ms);
                cells.push(cid);
            }
        }
        self.in_flight_total = 0;
        for c in cells {
            self.enqueue(c, true);
        }
    }

    /// Every current violation, with its cell.
    pub fn violations(&self) -> impl Iterator<Item = (CellId, &Violation)> + '_ {
        self.cells.iter().flat_map(|(&c, st)| st.results.values().flatten().map(move |v| (c, v)))
    }

    pub fn violation_count(&self) -> usize {
        self.cells.values().map(|st| st.results.values().map(|v| v.len()).sum::<usize>()).sum()
    }

    /// Cells with DRC state (checked, or being checked).
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Tiles queued or in flight, across all cells.
    pub fn pending_tiles(&self) -> usize {
        self.cells.values().map(|st| st.pending.len() + st.in_flight.len()).sum()
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight_total
    }

    /// Anything left to do (grid builds, queued or in-flight tiles)?
    pub fn is_busy(&self) -> bool {
        !self.queue.is_empty() || self.in_flight_total > 0
    }

    /// A grid build in progress: `(cell, fraction done)`.
    pub fn grid_progress(&self, db: &CellDb) -> Option<(CellId, f64)> {
        self.cells.iter().find_map(|(&c, st)| match &st.grid {
            GridState::Building(b) => Some((c, b.progress(db.cell(c)))),
            GridState::Ready(_) => None,
        })
    }

    /// Has a full pass over `cell` completed (and nothing is pending)?
    pub fn is_fully_checked(&self, cell: CellId) -> bool {
        self.cells.get(&cell).is_some_and(|st| st.full_done && st.pending.is_empty() && st.in_flight.is_empty())
    }
}

/// A throwaway grid used only while swapping a `Building` state for `Ready`.
fn placeholder() -> DrcGrid {
    DrcGrid::empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Cell, CellDbBuilder};
    use crate::drc::{check_tile, LayerRule, DrcKind};
    use crate::types::{Layer, Point};

    const L1: Layer = Layer { layer: 1, datatype: 0 };

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Vec<Point> {
        vec![Point::new(x, y), Point::new(x + w, y), Point::new(x + w, y + h), Point::new(x, y + h), Point::new(x, y)]
    }

    /// A one-cell db: a 20×20 lattice of clean wires plus a few violations.
    fn db() -> CellDb {
        let mut b = CellDbBuilder::new();
        let id = b.intern("TOP");
        let c = b.cell_mut(id);
        for row in 0..20 {
            for col in 0..20 {
                c.shapes.push(L1, rect(col * 3000, row * 3000, 2500, 400));
                if (row + col) % 9 == 0 {
                    c.shapes.push(L1, rect(col * 3000, row * 3000 + 1000, 2500, 60));
                }
            }
        }
        b.build().unwrap()
    }

    fn rules() -> DrcRules {
        DrcRules { default_rule: Some(LayerRule { min_width: 100, min_spacing: 100 }), per_layer: Default::default() }
    }

    /// Drive to completion inline, optionally holding back the first `hold`
    /// jobs (simulating replies still in flight).
    fn run(d: &mut DrcDriver, db: &CellDb, o: &EditOverlay, held: &mut Vec<TileJob>, hold: usize) {
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 100_000, "driver did not converge");
            match d.step(db, o, 0.0) {
                DrcStep::Progress => {}
                DrcStep::Job(j) => {
                    if held.len() < hold {
                        held.push(j);
                    } else {
                        let v = check_tile(j.core, d.rules(), &j.input);
                        d.accept(j.cell, j.tile, j.gen, v, 1.0);
                    }
                }
                DrcStep::Idle => {
                    if d.in_flight() == 0 || held.len() >= hold {
                        break;
                    }
                }
            }
        }
    }

    fn cell0(db: &CellDb) -> &Cell {
        db.cell(0)
    }

    /// Flagged lattice sites (marker centre → 3 µm lattice cell). Tiling may
    /// split one violation into several markers; the sites must match.
    fn sites<'a>(v: impl Iterator<Item = &'a Violation>) -> std::collections::BTreeSet<(i32, i32)> {
        v.map(|x| ((x.bbox.min.x + x.bbox.max.x) / 2 / 3000, (x.bbox.min.y + x.bbox.max.y) / 2 / 3000)).collect()
    }

    #[test]
    fn full_request_matches_check_cell() {
        let db = db();
        let o = EditOverlay::new();
        let mut d = DrcDriver::new(rules());
        d.tile_target = 60; // force many tiles
        d.max_in_flight = 1;
        d.request_full(&db, &o, 0, 0.0);
        run(&mut d, &db, &o, &mut Vec::new(), 0);
        assert!(d.is_fully_checked(0));
        let want = crate::drc::check_cell(cell0(&db), 0, &o, &rules());
        assert!(want.iter().all(|v| v.kind == DrcKind::Width));
        assert_eq!(sites(d.violations().map(|(_, v)| v)), sites(want.iter()), "same flagged lattice sites");
        assert_eq!(sites(want.iter()).len(), (0..400).filter(|k| (k / 20 + k % 20) % 9 == 0).count());
        assert!(d.last_pass.is_some());
    }

    #[test]
    fn edit_while_in_flight_drops_the_stale_reply() {
        let db = db();
        let mut d = DrcDriver::new(rules());
        d.tile_target = 60;
        d.max_in_flight = 8;
        let clean = EditOverlay::new();
        d.request_full(&db, &clean, 0, 0.0);
        // Dispatch jobs and hold them (as if a worker pool had them).
        let mut held = Vec::new();
        run(&mut d, &db, &clean, &mut held, 8);
        assert_eq!(held.len(), 8);

        // The user draws a sliver inside the first held tile's core.
        let t = held[0].core;
        let sliver = rect(t.min.x + 10, t.min.y + 10, 30, 800);
        let mut o = EditOverlay::new();
        o.add((1, 0), 0, crate::edit::Shape { layer: L1, points: sliver.clone() });
        let mut bb = Bbox::empty();
        for p in &sliver {
            bb.expand(*p);
        }
        d.invalidate(&db, 0, bb, 2.0);

        // Now the held (pre-edit) replies arrive: the re-dirtied tile's is stale.
        for j in held.drain(..) {
            let v = check_tile(j.core, d.rules(), &j.input);
            d.accept(j.cell, j.tile, j.gen, v, 3.0);
        }
        run(&mut d, &db, &o, &mut Vec::new(), 0);
        assert!(!d.is_busy());
        let want = crate::drc::check_cell(cell0(&db), 0, &o, &rules());
        assert_eq!(
            sites(d.violations().map(|(_, v)| v)),
            sites(want.iter()),
            "the sliver is flagged; nothing stale remains"
        );
    }

    #[test]
    fn edit_in_an_unchecked_cell_checks_only_nearby_tiles() {
        let db = db();
        let mut d = DrcDriver::new(rules());
        d.tile_target = 60;
        let mut o = EditOverlay::new();
        let sliver = rect(100, 100, 30, 500);
        o.add((1, 0), 0, crate::edit::Shape { layer: L1, points: sliver.clone() });
        let mut bb = Bbox::empty();
        for p in &sliver {
            bb.expand(*p);
        }
        d.invalidate(&db, 0, bb, 0.0);
        run(&mut d, &db, &o, &mut Vec::new(), 0);
        let pass = d.last_pass.expect("pass recorded");
        assert!(pass.tiles <= 4, "only the tiles near the edit ran ({})", pass.tiles);
        assert!(d.violations().any(|(_, v)| v.bbox.min.x <= 130 && v.bbox.max.x >= 100), "sliver flagged");
        assert!(!d.is_fully_checked(0));
    }
}
