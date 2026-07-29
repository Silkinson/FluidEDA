# FluidEDA — Project Status

_Last updated: 2026-07-29 · current branch: `phase4` (integration trunk)_

> `phase4` is the integration trunk. It merges the two finished feature branches
> that had sat independently on top of `phase3-editing` — `phase2b-worker` (parse
> worker) and `phase3-real-editing` (hierarchy pick/edit + GDS export) — into one
> line. Both branched from `b619532`; they are semantically orthogonal (worker owns
> the load path, editing owns pick/edit state), so the merge keeps both in full.
> `main` is untouched (owner's choice); all Phase 4 work continues here.

Browser-based IC layout viewer + live multiplayer. Rust everywhere, `wgpu`
(WebGPU, WebGL2 fallback), compiled to wasm. Full design in
`docs/AtomicStudio_Implementation_Plan.md`.

## Where we are

**Phase 4 is the integration trunk.** STEP 1 merged the two finished feature
branches — `phase2b-worker` (off-main-thread parse) and `phase3-real-editing`
(hierarchy pick/edit + GDS export) — into one line. STEP 2 took the **last load
wart** off the main thread: fit-view used to tessellate the giant flat cells inline
(~1.1 s freeze measured, ~2.5 s prior estimate); it now offloads chunk tessellation
to a Web Worker, drawing a coarse bbox proxy per chunk until real geometry streams
back (progressive refinement). Longest fit-view frame is now **62–64 ms** (caravel /
caravan-signoff), no frame > 100 ms.

Behind that: Phase 2b (chunking + streaming parser + parse worker) and Phase 3
editing (copy-on-write overlay, hierarchy select/move/delete, incremental re-tess,
CRDT sync + per-user undo, GDS export) all shipped. A committed golden-image test
guards the render pipeline across all of it.

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
| 5 — backend/OASIS/DRC | — | ⏳ planned | Content-addressed store, tile CDN, streaming OASIS |

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
  + 9 committed 512² reference PNGs (hier/sky130 fit/mid/leaf + 3 edit-overlay),
  diffed with a documented tolerance in `cargo test`. Regenerate with `UPDATE_GOLDEN=1`.
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
  same as Phase 0 markers).

## Git hygiene

- **`main` is intentionally untouched** (owner's choice) — it's still "Initial
  commit". The real trunk is now **`phase4`**, which merges the full stacked chain
  (`phase0 → 1 → 1.5 → 2a → 2b`) plus both finished feature branches
  (`phase2b-worker`, `phase3-real-editing`). All Phase 4 work continues on `phase4`.
- `origin` is still missing several local branches (`phase2b-chunk-worker`,
  `phase2b-*`, the phase3 line, `phase4`). Push when ready to share.
- ~~`.claude/` untracked~~ **RESOLVED** — gitignored in `b619532`.

## Next step

**Phase 4 STEP 2 shipped** (fit-view tessellation offload — see the ledger + debts).
The load path is now off the main thread end to end: parse in a worker, fit-view
tessellation in a worker, progressive bbox-proxy refinement. The largest single
main-thread block on load is the ~200 ms reconstruct (design-swap) frame.

Nearest wins, in rough order:
- **Single-copy worker reconstruction** — the last big main-thread load block
  (~200 ms → < 100 ms); per-cell `subarray`, no concatenated intermediate.
- **wgpu upgrade** — unblocks real WebGPU measurement (all browser numbers are
  WebGL2/ANGLE today) and reclaims the ~6% fit-view chunking cost.
- **Progressive render during parse** (proxy the whole design before the first
  chunk lands).

Remaining Phase 3 editing follow-ons (deferred, named in code): edit one AREF
placement vs. the master; vertex editing; draw-new-shape; booleans; DRC.

Then Phase 5: content-addressed store, tile CDN, streaming OASIS (the out-of-core
story the production-SoC scale ceiling needs).
