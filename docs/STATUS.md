# FluidEDA — Project Status

_Last updated: 2026-07-13 · current branch: `phase2b-chunk-worker` @ `b14dacf`_

Browser-based IC layout viewer + live multiplayer. Rust everywhere, `wgpu`
(WebGPU, WebGL2 fallback), compiled to wasm. Full design in
`docs/AtomicStudio_Implementation_Plan.md`.

## Where we are

Phase 2b **STEP 1 shipped** (spatial chunking + columnar store). Phase 2b STEP 2
(worker offload) is **scoped and de-risked but not built** — and measurement in
STEP 1 **reordered the remaining work**: the streaming parser now comes before the
worker.

## Phase ledger

| Phase | Branch | Status | Headline result |
|---|---|---|---|
| 0 — walking skeleton | `phase0-walking-skeleton` | ✅ done | Rust+wgpu viewer + multiplayer cursors/markers; runs in browser |
| 1 — hierarchy | `phase1-hierarchy` | ✅ done | CellDb, affine transforms, per-cell R-tree, culling, GPU instancing; golden match vs KLayout |
| 1.5 — real-chip reality check | `phase1.5-real-chip` | ✅ done | Ran sky130 + caravel. Found the real bottlenecks (not what the plan assumed) |
| 2a — quick wins | `phase2a-quick-wins` | ✅ done | Fit view 12→78 fps (cull-stat off hot path); 4 GB wasm opt-in; lazy tessellation. Deleted planned LOD work |
| 2b — chunking + worker | `phase2b-chunk-worker` | ◑ STEP 1 done, STEP 2 scoped | Leaf tess 2235→22 ms, leaf GPU 141→1.1 MB, byte-identical. Exposed parse as the last slow thing |
| 2b — streaming parser | — | ⏭ next | The only work that removes the 2.4 s freeze AND the 1.4 GB heap |
| 2b — worker offload | — | ⏸ after parser | Hides remaining freeze; needs nightly + build-std + atomics (groundwork proven) |
| 3 — editing + incremental | — | ⏳ planned | Select/move/draw, CRDT shapes, undo. Hit-test will revive lazy R-tree builds |
| 4 — backend/OASIS/DRC | — | ⏳ planned | Content-addressed store, tile CDN, streaming OASIS |

## Current performance (caravel, 278 MB, 3.9 M polys)

| metric | now | notes |
|---|--:|---|
| leaf-zoom tessellation | 22 ms | was 2235 ms (chunking) |
| leaf GPU geometry | 1.1 MB | was 141 MB |
| leaf TTFF (browser) | 2.6 s | **92% is parse** — the target of the streaming parser |
| fit fps | 47–78 | browser WebGL2 / native Metal; chunking cost ~6% at fit |
| wasm heap | 1.68 GB | ~1.4 GB is gds21's intermediate library; columnar store is only 196 MB |
| memory ceiling | ~3.4 GB | 4 GB opt-in (real OOM ~11 M polys) |

## Decisions locked (do not relitigate without a measurement)

- **Streaming parser before worker.** Parser removes the 2.4 s freeze *and* the
  1.4 GB heap; worker only hides the freeze. (phase2b-findings)
- **LOD-raster / tile pyramid deleted** for the M5/WebGPU target — fit view was a
  cull-stat instrumentation cost, not GPU. Contingency only for weak GPUs.
- **Memo cache: not built.** No measurement justifies it yet.
- **Chunk policy: single-owner + loose bbox.** Overlap-assignment rejected —
  duplicates geometry and breaks alpha-blended golden match.
- **`CHUNK_MIN_SHAPES=100k`, `CHUNK_TARGET_SHAPES=16k`** — swept, not guessed. Only
  die-spanning cells get chunked; dense-but-small cells don't.

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
- **Golden PNGs are not committed** (gitignored) — the invariant only holds within
  a session. Needs a committed baseline + `cargo test` diff.
- **Worker peak-heap claim is unproven** — the "shared heap holds peak at 1.68 GB"
  argument is design, not measurement, until the worker exists.
- **Scale ceiling:** caravel is hobby-class (half the ~10 M-poly ceiling).
  Production SoCs need out-of-core / server-side tiling — a different memory story
  to decide before committing to that market.

## Git hygiene (action needed)

- **Nothing is merged to `main`** — it's still "Initial commit". All phases are a
  stacked branch chain (`phase0 → 1 → 1.5 → 2a → 2b`). `origin` is missing
  `phase2b-chunk-worker`. Recommend: merge the chain into `main` so it becomes the
  real trunk, then branch future work from `main`.
- `.claude/` shows untracked; add to `.gitignore`.

## Next step

Streaming GDS parser in `crates/core`: incremental reader that emits the columnar
store element-by-element and drops each raw record, collapsing the ~1.4 GB parse
high-water and removing parse from the TTFF critical path. Groundwork already in
place: `gds.rs` collects into the columnar store and drops the library before
finalizing, so an incremental reader slots into `collect()` without touching the
chunker, culler, or renderer.
