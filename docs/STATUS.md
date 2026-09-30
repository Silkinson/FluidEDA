# FluidEDA — Project Status

_Last updated: 2026-09-30 · current branch: `phase5b-drc-hardening` (off `phase5-authoring-drc`)_

> The trunk line: `phase4` (integration trunk — parse worker + real editing +
> fit-view tessellation offload) → `phase5-authoring-drc` (this branch).
> `main` is untouched (owner's choice).
>
> **Ledger reshape (2026-08-21):** Phase 5 is now **authoring + DRC** (this
> branch's three slices); the backend/CAS/tile-CDN/OASIS work formerly labelled
> Phase 5 moves to **Phase 6**.

Browser-based IC layout viewer + live multiplayer. Rust everywhere, `wgpu`
(WebGPU, WebGL2 fallback), compiled to wasm. Full design in
`docs/AtomicStudio_Implementation_Plan.md`.

## Where we are

**Phase 5b in progress: DRC hardening** on `phase5b-drc-hardening` — the three
follow-ups Phase 5's own DRC measurements named.

* **SLICE 1 — booleans + merge-first, tiled DRC.** New integer boolean kernel
  (`crates/core/boolean.rs`, `i_overlay` 9, pure Rust / no_std / wasm-clean).
  DRC is rebuilt on it (`crates/core/drc/`): per layer, the live shapes near a
  tile are **unioned** (fracture slivers vanish into the real wire), then width
  and space are measured as **edge pairs on the merged boundary** — exact for
  any polygon (L arms, diagonal necks, acute tips) and space now includes
  same-polygon **notches**. A cell is cut into a density-adaptive **tile grid**
  (uniform buckets, dense ones split k×k) built in budgeted slices — no R-tree,
  no atomic block — and each tile is a pure function of Pod columns, so an edit
  re-checks only the tiles within reach of what changed. caravel_core (wasm):
  **611,680 → 61 flags**, and all 61 sit where a child instance's same-layer
  metal covers the spot (intra-cell scope, measured); full check **10.3 s →
  2.9 s** single-threaded, slowest tile 5.1 ms; **re-check after an edit
  9.6 s → 1.1 ms**; a background pass never pushes a frame past ~19 ms.
  Scheduler = `DrcDriver` (core, pure state machine: grids, per-tile results,
  generation-stamped jobs) — the seam SLICE 3's worker pool plugs into.

**Phase 5 shipped: FluidEDA authors geometry and checks it.** Three slices on
`phase5-authoring-drc`:

* **SLICE 1 — draw-new-shape.** Rect (drag) + polygon (click vertices) tools on
  an active layer, 5 nm snap, TOP-cell. Adds ship **geometry** through the CRDT
  (first documented amendment — no base to rebuild from) under stable
  `(client_id, seq)` ids: two peers drawing simultaneously both survive
  (unit + two-browser proven). Undo, incremental upload, and GDS export all
  cover adds (round-trip pixel-match 0 px).
* **SLICE 2 — vertex editing.** Handles on the selected shape (screen-space
  proximity), drag one vertex, same copy-on-write path; works on non-TOP
  masters through the transform stack. Ships the full point list
  (`SetPoints` — second amendment); concurrent same-shape edits converge
  last-writer-wins (accepted, tested). Translating a vertex-edited shape
  rewrites its geometry record so the edit is never silently lost.
* **SLICE 3 — DRC skeleton.** `crates/core/drc.rs` (pure, no new deps):
  rule table (blanket 100/100 nm placeholder), per-shape width (exact for
  rects), **exact** edge-pair spacing with R-tree candidates, integer math,
  touching/overlap = merged. **Intra-cell only — honestly scoped**; checks
  composite with the overlay (a drawn 40 nm sliver flags immediately, verified
  in-browser). Incremental at cell granularity on edit; "check visible" runs
  through an 8 ms/frame budgeted queue. caravel_core (2.5 M shapes): 10.3 s
  cold / 9.6 s warm re-check in wasm — the numbers that make DRC-in-worker the
  named follow-up.

Behind that: Phase 4 (integration + fit-view tessellation offload), Phase 2b
(chunking + streaming parser + parse worker), Phase 3 editing (overlay, hierarchy
select/move/delete, CRDT + per-user undo, GDS export). The committed golden-image
test now also guards draw + vertex-edit rendering (11 references, empty overlay
still byte-identical).

## Phase ledger

| Phase | Branch | Status | Headline result |
|---|---|---|---|
| 0 — walking skeleton | `phase0-walking-skeleton` | ✅ done | Rust+wgpu viewer + multiplayer cursors/markers; runs in browser |
| 1 — hierarchy | `phase1-hierarchy` | ✅ done | CellDb, affine transforms, per-cell R-tree, culling, GPU instancing; golden match vs KLayout |
| 1.5 — real-chip reality check | `phase1.5-real-chip` | ✅ done | Ran sky130 + caravel. Found the real bottlenecks (not what the plan assumed) |
| 2a — quick wins | `phase2a-quick-wins` | ✅ done | Fit view 12→78 fps (cull-stat off hot path); 4 GB wasm opt-in; lazy tessellation. Deleted planned LOD work |
| 2b — chunking + worker | `phase2b-chunk-worker` | ◑ STEP 1 done, STEP 2 scoped | Leaf tess 2235→22 ms, leaf GPU 141→1.1 MB, byte-identical. Exposed parse as the last slow thing |
| 2b — streaming parser | `phase2b-streaming-parser` | ✅ done | Parse heap 1.68 GB→777 MB, parse 2.4→1.2 s, leaf TTFF 2.6→1.4 s. Byte-identical to gds21 (oracle). Committed golden test |
| 2b — worker offload | `phase2b-worker` | ✅ done | Plain transferable-buffer worker (stable, no atomics). Parse off main; combined peak ~1.4 GB << 4 GB; parse freeze 1.2 s → ~0 (pannable during parse), residual ~200 ms swap frame |
| 3 — editing (skeleton) | `phase3-editing` | ✅ done | Copy-on-write overlay, select/move/delete of TOP-cell shapes, incremental re-tess (1 chunk), CRDT sync + per-user undo. Two-browser + golden verified |
| 3 — real editing | `phase3-real-editing` | ✅ done | SLICE 1: GDS export (round-trip + pixel-match). SLICE 2: non-TOP editing under the transform stack (landmine eliminated), master-edit semantics, finer chunking (edit 22→0.8 ms), two-browser non-TOP convergence |
| 4 — integrate + fit-view offload | `phase4` | ✅ done | STEP 1: merged parse-worker + real-editing onto the trunk (build + 39 core + golden green). STEP 2: fit-view tessellation offloaded to a worker (bbox proxy → stream). Longest fit frame ~1.1–1.35 s → **62–64 ms** (caravel / caravan-signoff), leaf unchanged, edit < 2 ms |
| 5 — authoring + DRC | `phase5-authoring-drc` | ✅ done | SLICE 1: rect/poly draw tools, geometry-carrying CRDT adds with stable `(client, seq)` ids — concurrent adds converge; export covers adds (0 px round-trip). SLICE 2: vertex editing via `SetPoints` (LWW on same shape), non-TOP masters. SLICE 3: intra-cell DRC (exact spacing, per-shape width), overlay-composited, 8 ms/frame budget; caravel_core 10.3 s cold / 9.6 s warm (wasm) |
| 5b — DRC hardening | `phase5b-drc-hardening` | ◑ SLICE 1 done | SLICE 1: integer booleans (`i_overlay`); merge-first edge-pair DRC (width/space/notch) on a density-adaptive tile grid. caravel_core: 611,680 → 61 flags (all 61 = child-instance context); full 10.3 s → 2.9 s wasm; edit re-check 9.6 s → 1.1 ms; slowest tile 5.1 ms |
| 6 — backend/OASIS | — | ⏳ planned | Content-addressed store, tile CDN, streaming OASIS (moved from the old "Phase 5" label — see the ledger-reshape note) |

## Current performance (caravel, 278 MB, 3.9 M polys)

| metric | now | notes |
|---|--:|---|
| leaf-zoom tessellation | 22 ms | was 2235 ms (chunking) |
| leaf GPU geometry | 1.1 MB | was 141 MB |
| main-thread parse freeze | **~0** (pannable) | was 1.2 s; parse now runs in a Web Worker (STEP 2). Residual ~200 ms design-swap frame |
| parse wall | 1.0–1.2 s | now off the main thread (in the worker) |
| combined peak heap (worker+main) | **~1.46 GB** | << 4 GB; caravan-signoff ~1.39 GB. Worker memory reclaimed on terminate |
| fit fps | 47–78 | browser WebGL2 / native Metal; chunking cost ~6% at fit |
| longest fit-view frame | **62 ms** | was ~1.1 s inline freeze (measured, headless; ~2.5 s prior estimate). STEP 2: tessellation offloaded to a worker, main draws bbox proxies then streams. No frame > 100 ms. caravan-signoff 1.35 s → 64 ms |
| fit-view TTFF | **2.1 s** | was ~3.0 s inline (first paint no longer waits for tessellation — proxies show immediately). caravan-signoff 3.3 s → 2.2 s |
| edit re-tess frame | < 2 ms | dirty chunks stay inline (finer-chunking, SLICE 2 path unchanged); offload never touches the edit path. caravel 1.8 ms, caravan-signoff 0.4 ms |
| DRC: biggest cell full check | **2.9 s** (wasm, single-threaded) | caravel_core, 2.5 M shapes, merge-first (Phase 5b); was 10.3 s cold / 9.6 s warm per-shape. Native 2.3 s. Includes the 95 ms tile-grid build (budgeted, replaces the atomic R-tree). 2923 tiles, slowest 5.1 ms |
| DRC: re-check after one edit | **1.1 ms** (wasm) | tiles within reach of the edit (1 tile for a typical shape); was a 9.6 s whole-cell re-check |
| DRC: background pass | 8.0 s wall, longest frame **19.1 ms** | "check visible" over caravel_core at leaf, 8 ms/frame budget, inline (no worker yet) |
| DRC: flags on caravel_core | **61** (blanket 100/100) | was 611,680. All 61 lie on same-layer metal of a child instance (70 nm staircase bends a via cell fills in context) — the intra-cell scope, not false geometry |
| DRC: drawn-sliver feedback | ~130 ms (harness poll) | a 40 nm-wide drawn rect flags via the edit-local tiles (in-browser verified) |
| memory ceiling | ~3.4 GB | 4 GB opt-in (real OOM ~11 M polys) |

## Decisions locked (do not relitigate without a measurement)

- **Worker (STEP 2) is a plain stable-Rust transferable-buffer worker** — shipped,
  no shared memory / atomics / nightly / COOP-COEP. Confirmed: combined peak
  ~1.4 GB << 4 GB. The 66c4472 shared-heap groundwork (MainThreadOnly, COOP/COEP)
  was dead and is removed. (phase2b-findings, "STEP 2 — worker offload (SHIPPED)")
- **Fit-view tessellation runs in a *stateless* worker; the parse worker still
  terminates.** Each job ships one chunk's `Pod` columns and returns `Pod`
  vertex/index buffers — no CellDb resident in the worker, so the ~0.8 GB
  parse-reclaim decision holds (no memory doubling). The alternative (a persistent
  worker holding the CellDb, tiny job messages) was rejected: it keeps ~250 MB+
  resident and can go stale on edits. `tessellate_columns` is proven byte-identical
  to the inline `tessellate_chunk` (unit test), so an offloaded chunk renders the
  same as an inline one — golden unchanged. (phase4 STEP 2)
- **Offload only when *many* big chunks are unresolved at once** (`> TESS_INLINE_MAX
  = 6`) — the fit-view freeze. A leaf zoom or gentle pan touching a handful of big
  chunks tessellates them inline exactly as before (~22 ms), so **leaf is unchanged**
  and no proxy flashes on a small zoom-in. Edit re-tess (dirty chunks) is always
  inline: it needs the overlay suppressions and is already < 2 ms. (phase4 STEP 2)
- **LOD-raster / tile pyramid deleted** for the M5/WebGPU target — fit view was a
  cull-stat instrumentation cost, not GPU. Contingency only for weak GPUs.
- **Memo cache: not built.** No measurement justifies it yet.
- **Chunk policy: single-owner + loose bbox.** Overlap-assignment rejected —
  duplicates geometry and breaks alpha-blended golden match.
- **`CHUNK_MIN_SHAPES=100k`, `CHUNK_TARGET_SHAPES=16k`** — swept, not guessed. Only
  die-spanning cells get chunked; dense-but-small cells don't.
- **Editing is a copy-on-write overlay; the base store is never mutated.** A move
  tombstones the base AND records a moved copy (not an in-place coord rewrite), so
  it builds the same tombstone+add path add/vertex-edit will need. (phase3)
- **CRDT edits ship the op, not geometry** — "cell:idx" -> "M dx dy"|"D"; each peer
  rebuilds the moved shape from its own base. Per-user undo = UndoManager tracks
  only the local origin; remote edits apply with no origin. (phase3)
- **AMENDMENT 1 (adds ship geometry).** A drawn shape has no base on the peer to
  rebuild from, so its CRDT record carries the full ring: key `a<client>:<seq>`,
  value `A <cell> <layer> <dt> <coords…>` in the SAME yrs map as base edits (one
  map ⇒ one undo scope covers adds for free). `(client_id, seq)` AddIds are minted
  by the creator, so concurrent adds land under distinct keys and BOTH survive;
  later moves/deletes/vertex-edits of an added shape reference the AddId and
  rewrite/tombstone the record LWW. (phase5 SLICE 1)
- **AMENDMENT 2 (vertex edits ship geometry).** `SetPoints` carries the shape's
  full new point list ("P x0 y0 …") — an arbitrary per-vertex change has no
  compact op. Concurrent vertex-edits of the SAME shape converge last-writer-wins
  (one record per shape id) — accepted. Corollary: translating a vertex-edited
  shape REWRITES its point list (a Move op would LWW the record back to
  translated-base, silently discarding the edit) — enforced in the app
  (`geom_edited`). Pure translates of pristine base shapes still ship the tiny
  "M dx dy" op — the amendments are scoped to where geometry is unavoidable.
  (phase5 SLICE 2)
- **DRC is merge-first, edge-pair, tiled (Phase 5b).** Per layer, shapes near a
  tile are unioned (integer `i_overlay`, clipped to core ⊕ reach) and width /
  space are edge pairs on the merged boundary (Euclidean, KLayout-style angle
  filter: facing edges must run opposite; adjacent edges flag only acute
  vertices). Markers are the exact too-close edge portions, clipped to the
  tile's half-open core — tiles partition the plane, so tiling never changes
  what is found (tested). Manhattan pairs use 1-D strip sweeps (H with H, V
  with V — perpendicular edges never face); only diagonals use an R-tree. The
  edge check went 6.6 s → 0.5 s on caravel_core with that split.
- **The DRC grid index replaces the R-tree for DRC.** Uniform buckets (bbox ⊕
  reach), two budgeted linear passes, deck-dependent (rebuilt on deck change),
  edit-independent (base never mutates). Dense buckets split k×k by shape
  count: caravel_core's 110 µm tiles ranged 0–35 k shapes (135 ms); split, the
  slowest is 5.1 ms (wasm).
- **DRC is intra-cell, composite-aware, budgeted.** Checks run per cell in
  cell-local space over base∖tombstones ∪ moved/vertex copies ∪ adds, using the
  existing per-cell R-tree for spacing candidates (base never mutates ⇒ the index
  survives edits). Incremental unit = the edited cell, re-queued on commit (not
  per preview frame). An 8 ms/frame budget queue spreads big cells across frames.
  Touching/overlapping same-layer shapes are merged (legal) — real-DRC semantics.
  (phase5 SLICE 3)
- **Hit-test builds only the clicked cell's index** (chunk-bbox prefilter first),
  not a background build of all 1014. TOP first-click = 50 µs on caravel. (phase3)

## Invariants (enforced)

- `crates/core` has **no** wgpu/network deps (the Web Worker seam).
- Coordinates are **i32 nm** end-to-end; cast to f32 only at upload (origin-rebased).
- Never block a frame on geometry work.
- Golden match: hier/sky130/caravel render byte-identical across changes.

## Open risks / debts

- **WebGL2 storage-buffer constraint** is still live (the `webgl` fallback). Costs
  nothing today; becomes a real fork at any GPU-compute work. Safari 26 now ships
  WebGPU by default — worth re-checking whether the fallback is still needed.
- **wgpu 22 is behind** and its `requestDevice` limits make headless Chromium
  reject WebGPU, so all browser numbers are WebGL2/ANGLE (pessimistic for fps). A
  wgpu upgrade would unblock real WebGPU measurement.
- ~~Golden PNGs are not committed~~ **RESOLVED** — `crates/render/tests/golden.rs`
  + 11 committed 512² reference PNGs (hier/sky130 fit/mid/leaf + 5 edit-overlay:
  base/moved/deleted/added/vertex), diffed with a documented tolerance in
  `cargo test`. Regenerate with `UPDATE_GOLDEN=1`.
- ~~Worker combined-peak not demonstrated end-to-end~~ **RESOLVED** — the worker
  ships (`phase2b-worker`). Measured combined peak across worker+main during load:
  caravel ~1.46 GB, caravan-signoff ~1.39 GB, both << 4 GB; worker memory reclaimed
  on `terminate()`. (phase2b-findings, "STEP 2 — worker offload (SHIPPED)")
- **Residual ~200 ms design-swap frame** on the worker load (reconstruct: two
  ~150 MB coord copies + first render), down from the ~1.2 s parse freeze — now the
  **largest single main-thread block on load** (it runs in the worker-reply handler,
  not a render frame, so it is separate from the < 100 ms fit-view frames now that
  the tessellation freeze is gone). A single-copy reconstruction (per-cell
  `subarray`, no concatenated intermediate) would get it under 100 ms — not built.
- ~~Fit-view tessellation (~2.5 s) is a main-thread freeze~~ **RESOLVED** (`phase4`
  STEP 2): the visible chunks tessellate in a stateless Web Worker; the main thread
  draws each chunk's bbox as a coarse proxy, then uploads the returned `Pod` buffers
  as they stream in (progressive refinement — the die fills in with detail over a
  couple hundred ms). Longest fit-view `update()` frame **62–64 ms** (caravel /
  caravan-signoff, headless SwiftShader) vs **~1.1–1.35 s** for the same build with
  `?tessinline=1` — a 17–21× shorter longest frame, no frame > 100 ms; leaf
  unchanged (stays inline); edit re-tess < 2 ms. Measured by
  `scripts/fit_freeze_bench.mjs`.
- **Scale ceiling:** caravel is hobby-class (half the ~10 M-poly ceiling).
  Production SoCs need out-of-core / server-side tiling — a different memory story
  to decide before committing to that market.
- ~~Phase 3: editing is TOP-cell-only~~ **RESOLVED** (`phase3-real-editing` SLICE 2):
  hit-test walks the instance tree under the transform stack (chunk-scan, no R-tree
  build — landmine *eliminated*, caravel first-click ~2.7 ms). Editing a shape edits
  the cell **master**, all placements update. caravel_core's coarse 75k-shape chunks
  are re-partitioned finer *on edit* (index-preserving, CRDT-safe) — edit re-tess
  22 ms → 0.8 ms. Editing one AREF placement vs the master, vertex edit, draw-new,
  booleans, DRC remain deferred.
- ~~GDS export of edits not built~~ **RESOLVED** (`phase3-real-editing` SLICE 1):
  `write_gds` composites base+overlay → GDSII, round-trip verified through our
  reader + gds21 + a pixel-match render test. "Save GDS" button.
- **Late-joiner edit sync** relies on a full-state broadcast on `join`; there is
  no y-sync handshake, so a peer must join before others edit (skeleton limitation,
  same as Phase 0 markers). Related operational note: the relay accumulates dead
  sockets across runs — a room reused after peers vanish can drop rebroadcasts
  (bit us in testing; restart the relay). Worth a cleanup pass with the y-sync work.
- **DRC deferrals, updated by Phase 5b SLICE 1:** merge-first width is DONE
  (the 593 k fracture flags are gone); the atomic index build is DONE (grid,
  budgeted); non-rect width is exact now (edge pairs). Still open below:
  cross-instance context (now *measured* as the only remaining source of flags
  on caravel_core), the rule table, and DRC-in-worker.
- **DRC deferrals (phase5 SLICE 3 — scoped honestly, in the code too):**
  - **Cross-instance / hierarchical spacing is NOT checked.** Checks are
    intra-cell; a shape in cell A near an instance of cell B is invisible to
    them. Flattened-context interaction regions along instance boundaries are
    the hard real-DRC problem — deferred deliberately.
  - **Width is per-shape, PRE-merge — over-flags fractured production GDS.**
    Measured: 593 k of caravel_core's 611 k flags are per-shape width on met1
    fracture slivers (P&R output arrives fractured; the merged region is wide).
    Real min-width needs boolean union first (the deferred booleans feature) —
    and non-rect polygons use a bbox min-dim approximation besides. Per-shape
    width IS correct for isolated/authored shapes — the interactive case this
    slice ships.
  - **The rule table is a blanket 100/100 nm placeholder**, checking every
    layer/datatype including markers and pins. A per-layer PDK table (with
    datatype filtering) is data work, not architecture — slot it in `DrcRules`.
  - **Big-cell latency wants the worker.** caravel_core's 9.6 s warm re-check
    never blocks a frame (8 ms budget) but takes ~20 s wall to re-verify after
    an edit. `drc.rs` is core-only (no wgpu/network) precisely so a
    DRC-in-worker offload links it unchanged — the named follow-up.
  - The one atomic block: a cell's first R-tree query builds its index
    (~0.3–0.9 s for caravel_core) inside one budget slice.

## Git hygiene

- **`main` is intentionally untouched** (owner's choice) — it's still "Initial
  commit". The real trunk is now **`phase4`**, which merges the full stacked chain
  (`phase0 → 1 → 1.5 → 2a → 2b`) plus both finished feature branches
  (`phase2b-worker`, `phase3-real-editing`). All Phase 4 work continues on `phase4`.
- `origin` is still missing several local branches (`phase2b-chunk-worker`,
  `phase2b-*`, the phase3 line, `phase4`). Push when ready to share.
- ~~`.claude/` untracked~~ **RESOLVED** — gitignored in `b619532`.

## Next step

**Phase 5 shipped** (authoring + DRC — see the ledger). FluidEDA now draws,
vertex-edits, syncs, undoes, exports, and checks — the first full author→verify
loop. The DRC skeleton's own measurements point at its follow-ups (see debts):
**booleans** (merge-first width — the single biggest correctness win on real
GDS), a **PDK rule table**, and **DRC-in-worker** (the 9.6 s big-cell re-check).

Also still queued from Phase 4, unchanged:
- **Single-copy worker reconstruction** — the last big main-thread load block
  (~200 ms → < 100 ms); per-cell `subarray`, no concatenated intermediate.
- **wgpu upgrade** — unblocks real WebGPU measurement (all browser numbers are
  WebGL2/ANGLE today) and reclaims the ~6% fit-view chunking cost.
- **Progressive render during parse** (proxy the whole design before the first
  chunk lands).

Remaining editing follow-ons (deferred, named in code): edit one AREF placement
vs. the master; drawing into an instanced master (draw is TOP-only); insert /
delete vertex (count-preserving edits only); booleans.

Then Phase 6: content-addressed store, tile CDN, streaming OASIS (the out-of-core
story the production-SoC scale ceiling needs).
