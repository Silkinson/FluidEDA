# FluidEDA — Project Status

_Last updated: 2026-07-28 · current branch: `phase2b-worker`_

Browser-based IC layout viewer + live multiplayer. Rust everywhere, `wgpu`
(WebGPU, WebGL2 fallback), compiled to wasm. Full design in
`docs/AtomicStudio_Implementation_Plan.md`.

## Where we are

Phase 2b fully shipped: chunking, streaming parser, **and the worker offload
(STEP 2)** — parse now runs in a plain transferable-buffer Web Worker off the main
thread (`phase2b-worker`). Phase 3 editing skeleton shipped on `phase3-editing`
(copy-on-write overlay, select/move/delete, incremental re-tess, CRDT sync +
per-user undo). A committed golden-image test guards the render pipeline.

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
| 4 — backend/OASIS/DRC | — | ⏳ planned | Content-addressed store, tile CDN, streaming OASIS |

## Current performance (caravel, 278 MB, 3.9 M polys)

| metric | now | notes |
|---|--:|---|
| leaf-zoom tessellation | 22 ms | was 2235 ms (chunking) |
| leaf GPU geometry | 1.1 MB | was 141 MB |
| main-thread parse freeze | **~0** (pannable) | was 1.2 s; parse now runs in a Web Worker (STEP 2). Residual ~200 ms design-swap frame |
| parse wall | 1.0–1.2 s | now off the main thread (in the worker) |
| combined peak heap (worker+main) | **~1.46 GB** | << 4 GB; caravan-signoff ~1.39 GB. Worker memory reclaimed on terminate |
| fit fps | 47–78 | browser WebGL2 / native Metal; chunking cost ~6% at fit |
| fit-view tessellation | ~2.5 s freeze | still main-thread (parse worker is parse-only) |
| memory ceiling | ~3.4 GB | 4 GB opt-in (real OOM ~11 M polys) |

## Decisions locked (do not relitigate without a measurement)

- **Worker (STEP 2) is a plain stable-Rust transferable-buffer worker** — shipped,
  no shared memory / atomics / nightly / COOP-COEP. Confirmed: combined peak
  ~1.4 GB << 4 GB. The 66c4472 shared-heap groundwork (MainThreadOnly, COOP/COEP)
  was dead and is removed. (phase2b-findings, "STEP 2 — worker offload (SHIPPED)")
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
  + 6 committed 512² reference PNGs, diffed with a documented tolerance in
  `cargo test`. Regenerate with `UPDATE_GOLDEN=1`.
- ~~Worker combined-peak not demonstrated end-to-end~~ **RESOLVED** — the worker
  ships (`phase2b-worker`). Measured combined peak across worker+main during load:
  caravel ~1.46 GB, caravan-signoff ~1.39 GB, both << 4 GB; worker memory reclaimed
  on `terminate()`. (phase2b-findings, "STEP 2 — worker offload (SHIPPED)")
- **Residual ~200 ms design-swap frame** on the worker load (reconstruct: two
  ~150 MB coord copies + first render), down from the ~1.2 s parse freeze. A
  single-copy reconstruction (per-cell `subarray`, no concatenated intermediate)
  would get it under 100 ms — noted, not built.
- **Fit-view tessellation (~2.5 s) is still a main-thread freeze** — the parse
  worker is parse-only. Needs chunk-tessellation-in-worker or a fit LOD proxy.
- **Scale ceiling:** caravel is hobby-class (half the ~10 M-poly ceiling).
  Production SoCs need out-of-core / server-side tiling — a different memory story
  to decide before committing to that market.
- **Phase 3 scope cuts to restore** (named in code): editing is TOP-cell-only
  (dodges hit-testing under a transform stack + the edit-one-AREF-instance-vs-master
  question); move+delete only (no vertex edit, booleans, draw-new); single
  selection. When per-cell editing lands, `caravel_core`'s coarse 75k-shape chunks
  need a finer grid or per-shape mesh diff for sub-16ms edits (TOP is fine at 80 µs).
- **GDS export of edits not built** — needs flattening base+overlay. The overlay
  carries the owning CellId per entry so this isn't designed out.
- **Late-joiner edit sync** relies on a full-state broadcast on `join`; there is
  no y-sync handshake, so a peer must join before others edit (skeleton limitation,
  same as Phase 0 markers).

## Git hygiene (action needed)

- **Nothing is merged to `main`** — it's still "Initial commit". All phases are a
  stacked branch chain (`phase0 → 1 → 1.5 → 2a → 2b`). `origin` is missing
  `phase2b-chunk-worker`. Recommend: merge the chain into `main` so it becomes the
  real trunk, then branch future work from `main`.
- `.claude/` shows untracked; add to `.gitignore`.

## Next step

**Phase 3 real editing** (`phase3-real-editing`, next) — un-defer the skeleton's
scope cuts, in two slices: (1) GDS export (flatten base+overlay, save), (2) non-TOP
editing (hit-test under a transform stack, AREF/master semantics, finer chunking
for sub-16 ms edits on big cells). The overlay model + CRDT + undo carry over.

Smaller follow-ups, deferred: single-copy worker reconstruction (swap frame <100 ms);
chunk-tessellation in the worker or a fit LOD proxy (the fit-view ~2.5 s freeze);
progressive render during parse; ~6% fit-view chunking cost on real WebGPU after a
wgpu upgrade.
