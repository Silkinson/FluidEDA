# FluidEDA — Project Status

_Last updated: 2026-09-30 · current branch: `phase6-backend-oasis` (off `phase5d-editing`)_

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

**Phase 6 in progress: formats, backend, out-of-core** on `phase6-backend-oasis`.

* **SLICE 1 — OASIS in and out, and paths.** A streaming OASIS (SEMI P39)
  reader (`crates/core/oasis.rs`: every element kind incl. all 26 CTRAPEZOID
  templates, all 12 repetition types, modal variables, deflate CBLOCKs,
  forward cell references) and writer (`oasis_write.rs`: RECTANGLE / g-delta
  POLYGON, modal layers, lattice repetitions, per-cell CBLOCKs). Both GDS
  readers now convert PATHs to outlines — 480 in caravel and 854 in the
  standard cells were silently missing (sky130 goldens re-baselined). Verified
  against **KLayout as an independent oracle** (`scripts/oracle/`): on caravel,
  our GDS read, our read of KLayout's OASIS, and KLayout's read of *our* OASIS
  all agree cell-for-cell — 1,014 cells, 3,925,013 polygons, 462,523
  placements. caravel: 291 MB GDS → 16.6 MB our OASIS (KLayout: 10.6 MB); in
  the browser caravel.oas loads faster than caravel.gds (geometry in 0.88 s vs
  1.05 s). "Open" takes .oas, "Export OASIS" writes it.

**Phase 5d shipped: the editing follow-ons** on `phase5d-editing` — every
deferred editing item named in code since Phase 3.

* **SLICE 1 — insert / delete vertex.** Double-click a handle to delete it, an
  outline to insert one (snapped); rings stay GDS-closed, degenerate results
  are refused. Vertex edits already ship whole rings, so no new CRDT kind.
* **SLICE 2 — boolean edit ops + multi-selection.** Shift-click builds a set;
  Union / Subtract / Intersect / Xor over same-cell same-layer shapes via the
  Phase 5b kernel; holes are keyholed into single rings (a GDS boundary has
  none). `Session::apply_batch` makes a boolean — or a multi-delete — one
  transaction, one undo step.
* **SLICE 3 — draw into an instanced master.** The draw tools author into a
  chosen cell at a chosen placement (pointer → placement-inverse → the cell's
  own grid); the shape appears at every placement. `pick_composite` picks what
  is *drawn* — tombstones skipped, moved copies where they are, drawn shapes in
  any cell — so a moved shape is now selectable at its new position.
* **SLICE 4 — edit one placement only.** "Edit this placement only" hides the
  clicked placement at the deepest hop whose parent is placed exactly once
  (a caravel standard cell → flattened into caravel_core: 52 shapes, 0.01 ms)
  and copies its composite subtree into the parent as ordinary shapes, capped
  at 50 k. Hidden placements are a tiny CRDT record; cull/pick skip them;
  export splits an AREF around hidden elements (re-import through our reader
  AND gds21 flattens to exactly what was drawn).

All of it converges in the two-browser test (insert/delete vertex, union + one
undo, draw into LEAF + pick it at its placement, flatten + one undo).

**Phase 5c shipped: load performance + the rendering stack** on
`phase5c-load-perf` (write-up: `docs/phase5c-findings.md`) — the Phase 4
performance queue.

* **SLICE 1 — single-copy, budgeted design swap.** The parse worker fetches
  the file itself; each cell's columns are copied once, straight from the
  transferred buffers (no concatenated intermediate: main heap 434 → 254 MB);
  the rebuild is budgeted across frames.
* **SLICE 2 — progressive load, skeleton first.** The 24.6 MB metadata blob is
  parsed in slices into a skeleton design that installs at once (proxies), then
  geometry fills in, on-screen cells first. Longest load frame 76–176 → ~50 ms;
  fit-view first frame 2.7 → 1.7 s.
* **SLICE 3 — wgpu 30 + egui/eframe 0.36.** Real WebGPU in the browser, first
  measured: sky130 fit 22 → 60 fps (vs WebGL2-on-Metal); caravel fit 51 → 44
  (GPU-bound, Dawn trails ANGLE ~14%).

**Phase 5b shipped: DRC hardening** on `phase5b-drc-hardening` — the three
follow-ups Phase 5's own DRC measurements named, each built and measured (full
write-up: `docs/phase5b-findings.md`).

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
* **SLICE 2 — a real rule deck + signoff measurement semantics.** `Deck::Sky130`
  (`drc/decks.rs`): width/space for 22 drawing layers with the manual's rule
  ids (`m1.1`, `li.3`, …), **drawing datatypes only** (pins, labels, markers
  unchecked), auto-selected when a design carries the sky130 back-end stack;
  a panel picker switches decks. Validating it against production data fixed
  two measurement semantics: **shielding** (a pair whose connection lands on a
  vertex and runs through the region's own corner is not a violation — the
  standard-cell library went 286 → 4 flags) and **projection width** for
  parallel edges (a jog whose reflex corners are only *diagonally* closer than
  min width is legal: the production cell `sky130_fd_sc_hd__dfrtp_4` has an
  88.6 nm one against li.1's 170 nm). Result (sky130 deck): **caravel_core 0
  flags**, the 440-cell standard-cell library 4 flags in 2 cells (genuine
  intra-cell geometry). Whole-chip intra-cell sweep: 3,305 flags in 252 of
  1,014 cells, concentrated in `sky130_fd_io` pad cells whose 45° corner-fill
  triangles and stubs only merge in hierarchical context — the measured case
  for in-context (flattened-window) checking.
* **SLICE 3 — DRC in a worker pool.** Persistent stateless workers
  (`assets/worker/drc_worker.js`, one per spare core, ≤ 8) run the same pure
  `check_tile_columns` the inline path runs; the main thread only gathers and
  posts within the frame budget; replies are epoch-guarded against design/deck
  changes. "Check visible" on caravel: **1.7 s wall with 8 workers vs 10.6 s
  inline**, identical violation totals, longest frame 15.3 ms.

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
| 5b — DRC hardening | `phase5b-drc-hardening` | ✅ done | SLICE 1: integer booleans (`i_overlay`); merge-first edge-pair DRC (width/space/notch) on a density-adaptive tile grid; full 10.3 s → 2.9 s wasm; edit re-check 9.6 s → 1.1 ms; slowest tile 5.1 ms. SLICE 2: sky130 deck (22 drawing layers, named rules, datatype-filtered, auto-detected) + shielding + projection width. caravel_core **611,680 → 0 flags**; stdcell library 4 (genuine). SLICE 3: worker pool — check-visible 10.6 s → **1.7 s** (8 workers), identical results |
| 5c — load perf + stack | `phase5c-load-perf` | ✅ done | SLICE 1: worker-side fetch, single-copy budgeted rebuild (main heap 434 → 254 MB). SLICE 2: skeleton-first progressive load (longest load frame 76–176 → ~50 ms; fit first frame 2.7 → 1.7 s). SLICE 3: wgpu 30 / egui 0.36 — real WebGPU: sky130 fit 22 → 60 fps, caravel fit 51 → 44 |
| 5d — editing follow-ons | `phase5d-editing` | ✅ done | SLICE 1: insert/delete vertex. SLICE 2: booleans (union/subtract/intersect/xor, keyholed holes) + multi-select, one-transaction batches. SLICE 3: draw into an instanced master + overlay-aware composite pick. SLICE 4: "edit this placement only" (flatten at the unique-parent hop; hidden placements in the CRDT, cull, pick and export — AREFs split around them) |
| 6 — backend/OASIS | `phase6-backend-oasis` | ◑ SLICE 1 done | SLICE 1: OASIS reader + writer (KLayout-verified on caravel: 3,925,013 polygons identical both ways), GDS paths → outlines (1,334 missing shapes restored). Next: content-addressed store + persistent rooms, out-of-core chunk streaming |

## Current performance (caravel, 278 MB, 3.9 M polys)

| metric | now | notes |
|---|--:|---|
| leaf-zoom tessellation | 22 ms | was 2235 ms (chunking) |
| leaf GPU geometry | 1.1 MB | was 141 MB |
| main-thread parse freeze | **~0** (pannable) | was 1.2 s; parse runs in a Web Worker (STEP 2), which now also *fetches* the file (Phase 5c). The design arrives progressively: skeleton parsed in slices, installed as proxies, geometry filled in visible-first — ≤ 6 ms/frame, ~8 frames. Longest load frame at leaf **50 ms** (was 76–176 ms, same session, headless; what remains is the leaf view's inline tessellation) |
| parse wall | 1.0–1.2 s | now off the main thread (in the worker) |
| combined peak heap (worker+main) | **~1.28 GB** | was 1.46 GB: single-copy reconstruction drops the concatenated wasm-side columns (main heap 434 → 254 MB). Worker memory reclaimed on terminate |
| fit fps | 47–78 | browser WebGL2 / native Metal; chunking cost ~6% at fit |
| fit fps, **real WebGPU** (Metal) | sky130 **60** / caravel 44 | Phase 5c (wgpu 30): first real WebGPU numbers — WebGL2-on-Metal for the same views is 22 / 51 (3 runs each, headless, 120 Hz cap). sky130's fit (5,144 draws/frame, CPU-bound) is 2.7× faster: WebGPU's per-draw cost is lower than ANGLE's. caravel's fit (1,822 draws over millions of triangles, GPU-bound) is 14% slower on Dawn-Metal than ANGLE-Metal. mid/leaf identical (93–94 fps) |
| longest fit-view frame | **62 ms** | was ~1.1 s inline freeze (measured, headless; ~2.5 s prior estimate). STEP 2: tessellation offloaded to a worker, main draws bbox proxies then streams. No frame > 100 ms. caravan-signoff 1.35 s → 64 ms |
| fit-view TTFF | **1.7 s** (first frame of the new design) | Phase 5c: the skeleton (hierarchy + chunk boxes) installs before any geometry, so first paint no longer waits for the column copy either; all geometry in ~150 ms later (`geometry_ms`). Same session before/after: caravel 2.7 → 1.7 s, caravan-signoff 3.0 → 2.0 s; longest fit frame 112 → 84 ms / 119 → 82 ms (machine under load; absolute numbers above this table's older 62 ms were measured on a quiet machine) |
| edit re-tess frame | < 2 ms | dirty chunks stay inline (finer-chunking, SLICE 2 path unchanged); offload never touches the edit path. caravel 1.8 ms, caravan-signoff 0.4 ms |
| DRC: biggest cell full check | **2.9 s** (wasm, single-threaded) | caravel_core, 2.5 M shapes, merge-first (Phase 5b); was 10.3 s cold / 9.6 s warm per-shape. Native 2.3 s. Includes the 95 ms tile-grid build (budgeted, replaces the atomic R-tree). 2923 tiles, slowest 5.1 ms |
| DRC: re-check after one edit | **1.1 ms** (wasm) | tiles within reach of the edit (1 tile for a typical shape); was a 9.6 s whole-cell re-check |
| DRC: background pass | **1.7 s** wall (8 workers), longest frame **15.3 ms** | "check visible" at leaf on caravel (caravel_core + visible cells); inline fallback (`?drcinline=1`) 10.6 s / 17.2 ms, same totals |
| DRC: flags on caravel_core | **0** (sky130 and blanket) | was 611,680 (per-shape, blanket). SLICE 1's merge-first left 61 staircase-bend diagonals; SLICE 2's projection width (sky130 signoff semantics) clears them |
| DRC: whole caravel, every cell (sky130) | 3,305 flags / 252 of 1,014 cells, 3.6 s native | intra-cell sweep (`drc_bench … sky130 --all`); dominated by `sky130_fd_io` pad cells (45° corner-fill triangles and stubs that merge only in hierarchical context). Stdcell library: 4 flags / 2 of 440 cells |
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
- **One placement is edited by flattening it (Phase 5d).** Not cell variants:
  the clicked placement is hidden (`PlacementKey = (parent, inst, col, row)`,
  a tiny LWW record) and its composite subtree becomes the parent's ordinary
  added shapes, so every edit tool, DRC and export already handle it. The hop
  is the deepest with a parent placed exactly once — hiding an element inside
  a multiply-placed cell would change every placement of that cell. Capped at
  `FLATTEN_LIMIT` = 50 k shapes.
- **Multi-shape edits are one CRDT transaction (Phase 5d).**
  `Session::apply_batch` — a boolean, a multi-delete or a flatten is one undo
  step and one broadcast.
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
- **Width between parallel edges is projection-measured; everything else
  Euclidean; pairs are shielded (Phase 5b SLICE 2).** Measured against
  production sky130 data, not chosen: Euclidean width flagged a jog in the
  shipping cell `dfrtp_4` (88.6 nm diagonal vs li.1 170 nm) and every P&R
  staircase bend in caravel_core; unshielded space flagged 272 corner-across-
  interior poly configurations in the standard cells. The accepted cost: a
  corner-overlap "diagonal neck" is not a width flag (tested, documented).
- **KLayout is the format oracle (Phase 6).** Readers/writers are verified by
  comparing FluidEDA's reading of a file with KLayout's reading of the same
  file — normalized polygons (vertex order/collinear points), canonicalized
  arrays (an AREF's axes may be swapped for the same placements), per-cell
  digests so a 2.5 M-polygon cell streams (`scripts/oracle/`). Not fixtures:
  the real chips, both directions.
- **Rule decks are data, drawing datatypes only.** `Deck::Sky130` (22 layers,
  basic width/space, rule ids from the manual; `dnwell` omitted — its 6.3 µm
  space would widen every tile's reach); `Deck::Blanket` (100/100 everywhere).
  Auto-detected per design (li1+met1+met2 drawing present ⇒ sky130).
- **The DRC grid index replaces the R-tree for DRC.** Uniform buckets (bbox ⊕
  reach), two budgeted linear passes, deck-dependent (rebuilt on deck change),
  edit-independent (base never mutates). Dense buckets split k×k by shape
  count: caravel_core's 110 µm tiles ranged 0–35 k shapes (135 ms); split, the
  slowest is 5.1 ms (wasm).
- **DRC runs in a stateless worker pool, fed by the pure driver (Phase 5b
  SLICE 3).** One persistent worker per spare core (≤ 8), each job = one tile's
  Pod columns + the packed deck, ≤ 8 jobs queued per worker. Consistent with
  the Phase 4 tessellation decision: no CellDb resident in any worker (8 × a
  big cell would not fit), the base never mutates so there is nothing to keep
  in sync, and results are byte-for-byte the inline function's. Replies carry
  the driver's epoch (bumped on clear) *and* the tile generation.
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
- ~~wgpu 22 is behind~~ **RESOLVED** (`phase5c-load-perf` SLICE 3): wgpu 30 +
  egui/eframe 0.36. The app now runs on real WebGPU wherever the browser offers
  it (eframe prefers it; WebGL2 stays the fallback) — in headless Chromium with
  `--enable-unsafe-webgpu --use-angle=metal` that is the Apple GPU (`GPU=webgpu`
  in every harness; `scripts/launch.mjs`). The golden images pass unchanged.
  The measured split (fit fps row above) is the next renderer question: the
  GPU-bound caravel fit is ~14% slower on Dawn than on ANGLE.
- ~~Golden PNGs are not committed~~ **RESOLVED** — `crates/render/tests/golden.rs`
  + 11 committed 512² reference PNGs (hier/sky130 fit/mid/leaf + 5 edit-overlay:
  base/moved/deleted/added/vertex), diffed with a documented tolerance in
  `cargo test`. Regenerate with `UPDATE_GOLDEN=1`.
- ~~Worker combined-peak not demonstrated end-to-end~~ **RESOLVED** — the worker
  ships (`phase2b-worker`). Measured combined peak across worker+main during load:
  caravel ~1.46 GB, caravan-signoff ~1.39 GB, both << 4 GB; worker memory reclaimed
  on `terminate()`. (phase2b-findings, "STEP 2 — worker offload (SHIPPED)")
- ~~No progressive render during load~~ **RESOLVED** (`phase5c-load-perf`
  SLICE 2): the worker's metadata blob (caravel: 24.6 MB, ~460 k instance
  records) is parsed in budgeted slices into a *skeleton* `CellDb` — hierarchy,
  chunk tables, bboxes, empty shape stores — installed on a frame of its own
  (every chunk draws as a bbox proxy), then each cell's geometry is copied in
  (`ProgressiveLoad`, cells on screen first) and slotted into the live design
  (`Cell::set_shapes`). Picking, DRC and save wait for the geometry. TTFF now
  means *first frame of the new design*; `geometry_ms` is when every cell's
  shapes are in.
- ~~Residual ~200 ms design-swap frame~~ **RESOLVED** (`phase5c-load-perf`
  SLICE 1): the worker fetches the file itself (the main thread materialising a
  278 MB fetch body was a ~70 ms stall of its own), the reply only queues a
  budgeted `Reconstruction` (core `Deserializer`: one copy per cell column,
  straight from the transferred JS buffers — no concatenated intermediate — a
  big cell's coords in 16 MB slices), and the finished design installs on a
  frame of its own. Longest load frame 76–176 → 42–58 ms; what remains is the
  first leaf view's inline tessellation (~22 ms, the Phase 4 policy) and one
  ~20 ms slice that allocates caravel_core's 125 MB coord column. Cost: TTFF
  +~150 ms (the copy spans ~7 frames).
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
  cross-instance context (measured: on the whole chip it is what remains —
  pad-cell corner fills and stubs). The rule table is DONE (SLICE 2: sky130
  subset; wide-metal / EOL / enclosure / density rules are not modeled);
  DRC-in-worker is DONE (SLICE 3).
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

**Phase 5b shipped** — FluidEDA's DRC now merges before it measures, carries a
real (sky130) deck, re-checks an edit in ~1 ms, and fans a full check out over a
worker pool. Its own measurements name what is next for DRC: **in-context
(flattened-window) checking** — the whole-chip residue is pad-cell geometry that
only merges across the hierarchy.

The Phase 4 performance queue is done (`phase5c-load-perf`: single-copy
budgeted design swap, progressive skeleton-first load, wgpu 30 with real
WebGPU). The renderer's next measured question: why the GPU-bound caravel fit is
~14% slower on Dawn-Metal than on ANGLE-Metal.

The editing follow-ons are done (Phase 5d). Known edges, documented in code: a
shape drawn past a cell's original extent grows that cell's cull/pick bbox but
not its ancestors' precomputed ones; picking a flattened placement's copy
needs a fresh click after the flatten (the app re-picks at the same point).

Then Phase 6: content-addressed store, tile CDN, streaming OASIS (the out-of-core
story the production-SoC scale ceiling needs).
