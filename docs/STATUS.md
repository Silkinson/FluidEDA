# FluidEDA — Project Status

_Last updated: 2026-07-26 · current branch: `phase3-editing`_

Browser-based IC layout viewer + live multiplayer. Rust everywhere, `wgpu`
(WebGPU, WebGL2 fallback), compiled to wasm. Full design in
`docs/AtomicStudio_Implementation_Plan.md`.

## Where we are

Phase 2b shipped (chunking + streaming parser; worker STEP 2 still deferred).
**Phase 3 editing skeleton shipped** on `phase3-editing`: select → move/delete a
TOP-cell shape via a copy-on-write overlay → incremental re-tessellation of only
the touched chunk → live CRDT sync to other browsers → per-user undo. A committed
golden-image test guards the render pipeline (now incl. move/delete goldens).

## Phase ledger

| Phase | Branch | Status | Headline result |
|---|---|---|---|
| 0 — walking skeleton | `phase0-walking-skeleton` | ✅ done | Rust+wgpu viewer + multiplayer cursors/markers; runs in browser |
| 1 — hierarchy | `phase1-hierarchy` | ✅ done | CellDb, affine transforms, per-cell R-tree, culling, GPU instancing; golden match vs KLayout |
| 1.5 — real-chip reality check | `phase1.5-real-chip` | ✅ done | Ran sky130 + caravel. Found the real bottlenecks (not what the plan assumed) |
| 2a — quick wins | `phase2a-quick-wins` | ✅ done | Fit view 12→78 fps (cull-stat off hot path); 4 GB wasm opt-in; lazy tessellation. Deleted planned LOD work |
| 2b — chunking + worker | `phase2b-chunk-worker` | ◑ STEP 1 done, STEP 2 scoped | Leaf tess 2235→22 ms, leaf GPU 141→1.1 MB, byte-identical. Exposed parse as the last slow thing |
| 2b — streaming parser | `phase2b-streaming-parser` | ✅ done | Parse heap 1.68 GB→777 MB, parse 2.4→1.2 s, leaf TTFF 2.6→1.4 s. Byte-identical to gds21 (oracle). Committed golden test |
| 2b — worker offload | — | ⏸ after parser | Now a plain transferable-buffer worker on **stable** (STEP 3 killed the shared-heap requirement). Hides remaining ~1.2 s parse |
| 3 — editing (skeleton) | `phase3-editing` | ✅ done | Copy-on-write overlay, select/move/delete of TOP-cell shapes, incremental re-tess (1 chunk), CRDT sync + per-user undo. Two-browser + golden verified |
| 4 — backend/OASIS/DRC | — | ⏳ planned | Content-addressed store, tile CDN, streaming OASIS |

## Current performance (caravel, 278 MB, 3.9 M polys)

| metric | now | notes |
|---|--:|---|
| leaf-zoom tessellation | 22 ms | was 2235 ms (chunking) |
| leaf GPU geometry | 1.1 MB | was 141 MB |
| leaf TTFF (browser) | **1.4 s** | was 2.6 s; streaming parser (STEP 3) |
| parse wall (browser) | **1.2 s** | was 2.4 s; streaming parser, 2× |
| fit fps | 47–78 | browser WebGL2 / native Metal; chunking cost ~6% at fit |
| wasm parse heap | **777 MB** | was 1.68 GB; streaming parser dropped gds21's ~1.4 GB intermediate. caravan-signoff 1.90 GB→892 MB |
| memory ceiling | ~3.4 GB | 4 GB opt-in (real OOM ~11 M polys) |

## Decisions locked (do not relitigate without a measurement)

- **Worker (STEP 2) can be plain stable Rust.** STEP 3 dropped peak parse heap to
  ~0.8 GB, so a worker that parses + copies the CellDb fits under 4 GB — no shared
  memory / atomics / nightly. (phase2b-findings, "Does STEP 2 still need a shared heap?")
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
- **Worker combined-peak claim is now measurement-backed but not yet demonstrated
  end-to-end.** STEP 3 measured the parse peak (~0.8 GB) that makes a copy-back
  worker fit; the actual two-thread peak is only proven once the worker exists.
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

Two independent threads of work are now unblocked; pick by priority:

1. **Worker offload (Phase 2b STEP 2)** — a plain stable-Rust transferable-buffer
   worker (STEP 3 removed the shared-heap requirement: ~0.8 GB parse peak, so a
   parse-and-copy worker peaks ~1.2 GB, under 4 GB). Takes the remaining ~1.2 s
   parse freeze off the main thread. No nightly / build-std / atomics.
2. **Phase 3 real editing** — un-defer the skeleton's scope cuts: editing in
   non-TOP cells (hit-test under a transform stack; finer chunking for sub-16ms
   edits on big cells), vertex editing, draw-new-shape, booleans, DRC, GDS export
   of the overlay. The overlay model + CRDT + undo carry straight over.

Deferred: progressive render during streaming parse; the ~6% fit-view chunking
cost on real WebGPU after a wgpu upgrade.
