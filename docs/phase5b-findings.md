# Phase 5b — DRC hardening: findings

**Status: shipped (3 slices on `phase5b-drc-hardening`).** The three follow-ups
Phase 5's DRC skeleton measured for itself — merge-first width, a real rule
deck, and DRC off the main thread — each built and measured.

**TL;DR** (caravel_core, 2.5 M shapes; wasm unless noted):

| | Phase 5 skeleton | Phase 5b |
|---|--:|--:|
| flags (blanket 100/100 deck) | 611,680 | **0** |
| flags (sky130 deck) | — | **0** |
| full check, single-threaded | 10.3 s cold / 9.6 s warm | **2.9 s** (native 2.3 s) |
| re-check after one edit | 9.6 s (whole cell) | **1.1 ms** (1 tile) |
| largest atomic block | 0.3–0.9 s lazy R-tree build | **5.1 ms** (slowest tile) |
| background "check visible" | — | **1.7 s wall with 8 workers** vs 10.6 s inline; longest frame 15.3 ms |

The standard-cell library under the sky130 deck: **4 flags in 2 of 440 cells**,
both genuine intra-cell geometry. The whole chip (every one of caravel's 1,014
cells, intra-cell): 3,305 flags in 252 cells, concentrated in the
`sky130_fd_io` pad cells — the measured case for in-context checking.

---

## Method

* Native: `cargo run --release -p fluideda-core --example drc_bench --
  testdata/caravel/gds/caravel.gds [blanket|sky130] [--all]` — grid build,
  full check with per-tile distribution, one-edit incremental cost, flags by
  kind/layer, and (with `--all`) an every-cell sweep with a scope check: how
  many flags sit on a *child instance's* same-layer metal.
* Browser: `trunk build --release && python3 scripts/serve_bench.py 8090`, then
  `node scripts/drc_bench.mjs` — the `drc_bench` test command under both decks,
  a drawn-sliver interactive check, and a "check visible" pass through the
  worker pool vs a fresh page forced inline (`?drcinline=1`), asserting equal
  violation totals.
* Headless Chromium, WebGL2 (as all browser numbers in this repo), this
  machine's 8 spare cores for the pool.

---

## SLICE 1 — booleans + merge-first, tiled edge-pair DRC

**Booleans.** `i_overlay` 9 (integer engine, pure Rust, `no_std`, wasm-clean) —
the choice the plan named. Rings are normalised to CCW before the union so
opposite GDS windings never cancel into false holes under non-zero fill; output
rings have the interior on their left, which the edge checks rely on.

**Merge-first.** Per layer, per tile: union every live shape (base minus
tombstones, plus the overlay's moved/vertex-edited copies and adds) whose bbox
meets the tile core ⊕ that layer's reach, clipped to that window. Fracture
slivers disappear into the wires they belong to — which is the whole fix for
the skeleton's 593 k width flags.

**Measure on the merged boundary.** Width = interior-facing edge pairs, space =
exterior-facing ones (so same-polygon **notches** are now checked too). Angle
filter (facing edges run opposite), half-plane facing test, exact intervals of
the too-close portions (not just the closest points), acute-vertex rule for
adjacent edges.

**Tiles.** A uniform grid over the cell (range = own-shape bbox ⊕ reach, so
every base shape lands in range), shapes bucketed by bbox ⊕ reach, dense
buckets split k×k by shape count (`⌈√(n/2000)⌉`). Markers are clipped to the
tile's **half-open** core, so tiles partition the plane — tested: a 1-tile and
a many-tile check of the same cell cover identical marker area, and an
incremental re-check of the tiles near an edit equals a full re-check.

Two measured optimisations:

| caravel_core, native | edge check | full check |
|---|--:|--:|
| R-tree candidate pairs | 6.6 s | 8.4 s |
| strip sweeps (H with H, V with V; R-tree for diagonals only) | 0.5 s | 2.3 s |

| tiling | tiles | slowest tile |
|---|--:|--:|
| uniform 110 µm | 1,276 | 135 ms (35 k shapes) |
| + density split | 2,923 | 3.7 ms native / 5.1 ms wasm |

The grid build is two budgeted linear passes: **46 ms native / 95 ms wasm** for
caravel_core, never atomic — it replaces the lazy R-tree whose first query was
a 0.3–0.9 s block inside one frame.

**Scheduler.** `DrcDriver` (core, no clock/threads/web types): per-cell grid
(built in slices), per-tile results, a pending queue edits jump, and a
per-tile generation so a reply computed before a re-dirty is dropped (tested
with held "in-flight" jobs). An edit in a cell nobody asked to check re-checks
only the tiles near the edit — the draw-a-sliver loop never pays for the
2.5 M-shape cell around it.

## SLICE 2 — the sky130 deck, and what production data said about semantics

`Deck::Sky130`: width/space for 22 drawing layers with rule ids from the
manual; drawing datatypes only (a pin, label, boundary or marker purpose is not
geometry to check); auto-selected when li1+met1+met2 drawing are present.
`dnwell` is omitted (its 6.3 µm space would widen every tile's reach).

Running it over data known to be signoff-clean exposed two semantics bugs —
the useful part of this slice:

1. **Shielding.** 272 poly "spacing" flags in the standard cells were pairs of
   edges each on the other's exterior half-plane but whose connection ran
   *through the polygon's own interior* (two stems joined by a band; the gap
   lands on the band's reflex corners). Fix: probe from the middle of each
   violating portion to its nearest point on the other edge; if that lands on a
   vertex and enters the wrong side of the region's corner wedge, the pair is
   shielded (KLayout's default mode). Standard cells: **286 → 5 flags**.
2. **Projection width.** `sky130_fd_sc_hd__dfrtp_4` — a shipping cell — has a
   li1 jog whose reflex corners are 88.6 nm apart *diagonally* against li.1's
   170 nm; caravel_core's P&R staircase bends are the same shape at 99 nm vs
   m1.1's 140 nm. Signoff passes both, so width between **parallel** edges is
   now projection-measured (they must overlap in projection); space and
   non-parallel pairs stay Euclidean. Accepted cost, tested and documented: a
   corner-overlap "diagonal neck" is not a width flag.

Before the projection change, every one of the 61 flags left on caravel_core
lay on same-layer metal of a child instance (a via cell filling the step) — so
the flags were never evidence of a real defect either way.

| sky130 deck | flags |
|---|--:|
| caravel_core | **0** |
| standard-cell library (440 cells) | 4 in 2 cells — `probe_p_8` has 25 nm-wide met3 shapes; `dfrtp_2` a 75 nm li1 gap at its rail end |
| whole caravel, every cell, intra-cell | 3,305 in 252 of 1,014 cells (3.6 s native) |

The whole-chip residue is dominated by `sky130_fd_io` pad cells: isolated 45°
corner-fill triangles (down to 15 × 15 nm) and short stubs whose partner metal
lives in another cell of the hierarchy. Intra-cell checking cannot see that;
**in-context (flattened-window) checking** is the named next step — the tile
architecture is ready for it (a tile's gather becomes a hierarchical window
query), and SLICE 3's pool is what makes its extra work affordable.

## SLICE 3 — DRC in a worker pool

A pool of persistent, stateless workers (`assets/worker/drc_worker.js` over the
same core-only wasm as the parse/tessellation workers; one per spare core,
≤ 8). Each tile job ships its Pod columns (transferred) + the packed deck;
`check_tile_columns` is the same pure function the inline path runs, so pool
and inline results are identical (the harness asserts equal totals). The main
thread only gathers (~0.1 ms/tile) and posts, within the 8 ms budget; replies
feed the driver next frame. An **epoch** (bumped on every driver clear) guards
against a reply from before a design/deck change whose `(cell, tile, gen)`
happens to collide with the new state.

| "check visible" at leaf on caravel (caravel_core + visible cells) | wall | longest frame |
|---|--:|--:|
| inline, 8 ms/frame budget | 10.6 s | 17.2 ms |
| pool, 8 workers | **1.7 s** | **15.3 ms** |

`DRC_JOBS_PER_WORKER = 8` keeps workers fed between once-per-frame refills
while bounding how many already-posted jobs an edit's tiles (which jump the
driver's queue) wait behind.

---

## Open / next

* **In-context DRC** (the measured remainder: pad-cell corner fills and stubs).
* Rules beyond width/space: wide-metal spacing, end-of-line, enclosure, density.
* The per-tile gather re-reads a split tile's parent bucket once per sub-tile
  (268 ms native total on caravel_core) — cacheable if it ever matters.
