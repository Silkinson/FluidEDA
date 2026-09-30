# Phase 5c — load performance + the rendering stack: findings

**Status: shipped (3 slices on `phase5c-load-perf`).** The Phase 4 performance
queue: the design-swap block, progressive load, and the wgpu upgrade.

**TL;DR** (caravel 278 MB, headless Chromium, same-session before/after):

| | before | after |
|---|--:|--:|
| longest main-thread frame during load (leaf) | 76–176 ms | **~50 ms** |
| main-thread heap after load | 434 MB | **231–254 MB** |
| combined peak heap (worker + main) | 1.46 GB | **1.26–1.28 GB** |
| first frame of the new design (fit) | 2.7 s | **1.7 s** (skeleton proxies) |
| real WebGPU in the browser | no (wgpu 22 rejected) | **yes** (wgpu 30) |

## Method

`node scripts/worker_test.mjs` (load: longest rAF gap, heaps, TTFF /
`geometry_ms`, reconstruction frames), `node scripts/fit_freeze_bench.mjs`
(fit-view freeze), `CASE=… GPU=… node scripts/browser_bench.mjs` (fps). GPU
modes live in `scripts/launch.mjs`: default = Chromium's headless WebGL2 on
SwiftShader; `webgl-metal` = WebGL2 on ANGLE-Metal; `webgpu` = WebGPU on Metal.

The machine was intermittently loaded by macOS background services (Spotlight,
`amsaccountsd` at ~100% CPU each) during this phase, so absolute browser numbers
drift run to run; every before/after pair here was measured back to back in one
session, 3–5 runs each.

## SLICE 1 — single-copy, budgeted design swap + worker-side fetch

Three separate stalls, found by tracing (`performance.now()` marks) rather than
assumed:

1. **The fetch body.** For `?file=` loads the main thread materialised the
   278 MB response as an `ArrayBuffer` before transferring it (~70 ms). The
   parse worker now fetches the file itself.
2. **Two copies of every column.** JS buffer → one concatenated wasm `Vec` →
   per-cell `Vec`s. Now each cell's slice is copied once, straight from the
   transferred `ArrayBuffer` (`ColumnSource`), and the concatenated
   intermediate — and its 180 MB of main heap — is gone.
3. **One task.** The rebuild is budgeted (≤ 6 ms/frame, 16 MB coordinate
   slices, so even caravel_core's 125 MB column spans frames) and the design
   installs on a frame of its own.

## SLICE 2 — progressive render during load (skeleton first)

The metadata blob is bigger than it looks — 24.6 MB for caravel (~460 k
instance records, mostly caravel_core's standard cells) — and parsing it whole
was a 14 ms block. `SkeletonParse` parses it in slices into a skeleton
`CellDb` (hierarchy, chunk tables, bboxes, empty stores) that installs at once:
every chunk renders as a bbox proxy through the renderer's existing
pending-chunk path, so the new design's outline appears before any geometry is
copied. `ProgressiveLoad` then fills cells in, **cells on screen first**, and
`Cell::set_shapes` slots each store into the live design (dropping any cached
index). Picking, DRC and save wait for the geometry.

What remains in the longest load frame (~33 ms update + render) is the first
leaf view's inline tessellation of caravel_core — the Phase 4 leaf policy, not
load work — plus one ~20 ms slice that allocates caravel_core's 125 MB column
(`memory.grow`).

## SLICE 3 — wgpu 30 + egui/eframe 0.36

Mechanical API port (render pipeline descriptors, texel-copy/poll APIs, egui's
`App::ui` and in-`Ui` panels, dropped-file handles, `StrokeKind`); the golden
images pass unchanged. `build_worker.sh` now resolves the wasm-bindgen CLI from
`Cargo.lock` (wgpu 30 needs ≥ 0.2.127).

**Real WebGPU**, first measurement (Metal, 120 Hz cap, 3 runs each):

| view | draws / frame | WebGL2 (ANGLE-Metal) | WebGPU (Dawn-Metal) |
|---|--:|--:|--:|
| sky130 fit | 5,144 | 22 fps | **60 fps** |
| caravel fit | 1,822 | 51 fps | 44 fps |
| caravel mid / leaf | — | 93 / 94 | 93 / 94 |

The draw counts refute the obvious hypothesis: the view with *more* draws is
where WebGPU wins (its per-draw CPU cost is lower than ANGLE's). caravel's fit
is GPU-bound (millions of triangles), and there Dawn-on-Metal trails
ANGLE-on-Metal by ~14% — the renderer's next measured question.
