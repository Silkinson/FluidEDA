# Phase 1.5 — Real-chip reality check: findings

**Question this phase answered:** Phase 1 benchmarked beautifully on `hier.gds`
(980 bytes, 3 cell defs, ~30 distinct polygons, 10M shapes by pure AREF
replication). That proved instancing + culling but nothing about parse
throughput, tessellation cost, memory, or cell-count scaling on *real* files.
Before building any of Phase 2 (worker offload / tiling / memo cache) we measured
where the current pipeline actually breaks.

**TL;DR — where it breaks first:** **wasm memory.** A 278 MB chip parses to a
**1.68 GB** wasm heap; a 345 MB chip reaches **1.90 GB** — essentially the ~2 GB
practical wasm32 ceiling. The heap high-water is driven by the **parse step**
(gds21 materializes the entire library in memory before we convert it), not by
tessellation output (210 MB) or the spatial index. The **second** wall is
time-to-first-frame: parse + eager tessellation run synchronously on the main
thread and freeze the tab for **~3.8 s** on caravel. Culling itself is *not* a
bottleneck when zoomed in (µs–ms), but at **fit view** a real chip shows 10M+
shapes because large flat top-level cells sit above the pixel threshold, so
fit-view is render/cull-bound (~12–22 fps).

Recommendation (justified below): **build the streaming parser first**, then move
parse+tessellation to a Web Worker. Tiling/LOD for fit-view fps is third. The
memo cache is not yet justified by any measurement.

> **UPDATE — see the [After Phase 2a](#after-phase-2a-cheap-high-leverage-fixes)
> section below.** Three cheap fixes changed this conclusion: a 4 GB wasm opt-in
> (+3 bytes) doubled the memory headroom, removing a per-frame cull-stat query
> took caravel fit from 12 → 78 fps (native), and lazy tessellation cut sky130
> TTFF 98 → 24 ms. **Fit view is no longer slow (LOD work deleted); the streaming
> parser is now "important, not urgent"; worker offload of the ~2.4 s TTFF freeze
> is the new #1 Phase 2b priority.**

---

## Method

**Files** (fetched by `scripts/fetch_testdata.sh`, not committed):

| file | size | what it stresses |
|---|---|---|
| `hier.gds` (embedded) | 980 B | Phase 1 baseline (AREF replication) |
| `sky130_fd_sc_hd_merged.gds` | 4.2 MB | **cell-count / tessellation** — 435 distinct std cells merged into one file (via `merge_sky130.py`), wrapped in a 21×21 grid TOP |
| `caravel.gds` | 278 MB | **parse throughput / memory** — efabless Caravel full SoC |
| `caravan-signoff.gds` | 345 MB | break-point probe (largest on hand) |

sky130 has 435 *real* standard cells (not thousands — that's the whole
`sky130_fd_sc_hd` library); still a representative distinct-cell-def workload.

**Native** (`examples/bench.rs`, `examples/shot.rs`, `--file` arg) on Apple M5 /
Metal: parse, index (total + p99 + max per cell), eager tessellation, peak RSS
(`getrusage`), cull time + visible counts, and GPU frame time.

**Browser** (`scripts/browser_bench.mjs` + `serve_bench.py`) in Playwright's
headless Chromium: the release wasm app loads each file via `?file=` autoload and
publishes wasm-measured metrics (`window.__fluideda_metrics`) — parse, tess,
index, TTFF, and **wasm linear-memory size** — plus a live fps sample.

**Browser caveats (do not over-read the fps):**
- This Chromium's WebGPU rejects wgpu 22's `requestDevice` limits, so the harness
  forces wgpu's **WebGL2** backend (ANGLE-Metal) — which is the workspace's real
  Safari fallback path anyway. CPU/memory/TTFF are backend-neutral and
  representative; **fps over WebGL2 is a floor, not the WebGPU number** (native
  has the GPU fps).
- fps is rAF-sampled and **caps at ~120** (display/vsync); "120" means "≥120,
  not the bottleneck."
- The browser's `index_ms` reads *lower* than native because the first cull
  builds the visible cells' R-trees lazily before the metric is taken; treat the
  **native** index numbers as authoritative.

---

## Measurements

### Native (Apple M5, Metal)

| metric | hier | sky130 | caravel |
|---|--:|--:|--:|
| file size | 980 B | 4.2 MB | 278 MB |
| parse time | 63 µs | 16 ms | **1.17 s** (248 MB/s) |
| cell defs | 3 | 440 | 1014 |
| distinct polys | 30 | 48,784 | 3.92 M |
| flattened shapes | 10.0 M | 48,946 | 37.08 M |
| **peak RSS** | ~small | 37.6 MB | **1.56 GB** |
| eager tessellate (all defs) | ~0 | 23 ms | **1.35 s** |
| GPU geo (vtx+idx) | tiny | 4.0 MB | 210 MB |
| index build total | 13 µs | 4.5 ms | 708 ms |
| index build, p99 / **max cell** | — | 26 µs / 28 µs | 2.6 ms / **465 ms** |
| cull @ fit | 161 µs | 256 µs | **74 ms** |
| render fps @ fit | ~5200 | 108 | **12** |
| render fps @ mid / leaf | ~21000 | ~9700 | 295 / 289 |
| visible @ fit / of total | 0 / 0% | 48,946 / 100% | 10.9 M / 29% |
| visible @ leaf | ~50 | 36 | 7 |

### Browser (headless Chromium, wasm32, WebGL2/ANGLE-Metal)

| metric | hier | sky130 | caravel | caravan-signoff |
|---|--:|--:|--:|--:|
| file size | 980 B | 4.2 MB | 278 MB | 345 MB |
| parse | ~0 | 20 ms | 1.23 s | 1.37 s |
| tessellate | ~0 | 35 ms | 1.71 s | 2.11 s |
| **TTFF** | 12 ms | 101 ms | **3.8 s** | **4.5 s** |
| **wasm heap** | 4 MB | 63 MB | **1.68 GB** | **1.90 GB** |
| fps @ fit | 120\* | **40** | **22** | — |
| fps @ mid / leaf | 120\* | 120\* / 120\* | 120\* / 120\* | — |

\* vsync/rAF-capped.

---

## Hypotheses

**H1 — eager tessellation stalls the main thread and blows TTFF. → CONFIRMED.**
The app tessellates *every* cell def at load. caravel: eager tess **1.35 s**
native / **1.71 s** wasm, and with parse the tab is frozen for **TTFF 3.8 s**
(4.5 s for caravan-signoff). sky130's 440 defs already cost 23–35 ms; a
full-corner PDK with thousands of defs scales this linearly. Much of it is wasted
— most cell defs are never on-screen.

**H2 — parse of a multi-hundred-MB GDS is slow and/or busts wasm memory.**
**Split verdict: speed is fine, MEMORY is the wall.** Parse runs at ~230–250
MB/s (caravel 1.2 s) — not a bottleneck. But memory is: caravel's wasm heap is
**1.68 GB**, caravan-signoff's **1.90 GB** — at the ~2 GB practical wasm32
ceiling. The driver is the parse high-water: `gds21` builds the *entire*
`GdsLibrary` (3.9 M boundary elements, each an enum + `Vec<GdsPoint>` + optional
fields) in memory before we convert to `CellDb`, and wasm linear memory never
shrinks back, so the peak becomes the resident size. **This is the first hard
wall — the next size class up OOMs.**

**H3 — tessellated vertex buffers exceed GPU/wasm memory. → NOT the first
breaker.** caravel's full eager GPU geometry is **210 MB** (271 MB for
caravan-signoff) — comfortably within budget and dwarfed by the 1.4–1.9 GB parse
footprint. Tessellation output is not what runs us out of memory; the parsed
in-memory model is.

**H4 — cull stays negligible even on a real hierarchy. → CONFIRMED zoomed in,
FALSE at fit.** Zoomed to a cell, cull is 3–4 ms native (caravel) / trivial, and
visible shapes are 7–50 of 37 M — culling is *done* and Phase 2 should not touch
the zoomed-in path. But at **fit view** a real chip exposes large flat top-level
cells (power grid / fill / global routing) that project *above* the pixel
threshold, so their R-tree query returns nearly all their shapes: **10.9 M
visible (29%)**, cull **74 ms**, render **12 fps**. Hierarchy culling
structurally can't help a single giant near-flat cell — that needs LOD/tiling.

---

## Where it breaks first (ranked)

1. **Memory (hard crash).** ~2 GB wasm ceiling reached at a ~345–365 MB file, or
   sooner if a second file is opened (wasm memory doesn't shrink between loads).
   Failure mode: OOM, the file cannot be opened at all. Root cause: whole-library
   parse high-water, not tessellation.
2. **TTFF (frozen tab).** ~3.8 s synchronous main-thread freeze on caravel
   (parse 1.2 s + eager tess 1.7 s). Recoverable but unacceptable UX; scales with
   file size and cell-def count.
3. **Fit-view fps (jank).** 12–22 fps at fit on caravel because giant flat cells
   defeat hierarchy culling. Fine (≥60, vsync-capped) at every zoomed viewport.

Everything else measured is healthy: parse *speed* (~250 MB/s), zoomed cull
(µs–ms), index build (p99 26 µs sky130; one 465 ms outlier cell on caravel),
GPU geometry size (210 MB).

---

## Recommendation for Phase 2 — build order

1. **Streaming GDS parser (first).** It is the only fix that changes whether a
   file *loads at all*. Replace "parse the whole `GdsLibrary`, then convert" with
   an incremental reader that emits our compact `CellDb` shapes element-by-element
   and drops each raw record immediately, so the 1.4–1.9 GB parse high-water
   collapses toward the ~230–450 MB the converted `CellDb` + geometry actually
   need. Numbers that justify it: **1.68 GB heap / 278 MB file (~6× blow-up),
   1.90 GB at 345 MB, ceiling ~2 GB.** Consider a compact columnar/arena shape
   store (drop per-`Polygon` `Vec<Point>` overhead: ~24 B/shape × 3.9 M ≈ 90 MB)
   as part of the same work.

2. **Worker offload of parse + tessellation (second).** Moves the **3.8 s** TTFF
   freeze off the main thread (COOP/COEP headers are already set for this) and
   pairs with **lazy, on-demand tessellation** — tessellate a cell def the first
   time it's actually drawn instead of all 1014 up front (caravel eager tess
   1.7 s → near-zero for the handful on screen). Note: a worker alone does *not*
   raise the memory ceiling, which is why the parser comes first.

3. **Tile pyramid / LOD raster for coarse zoom (third).** Fixes the 12–22 fps
   fit-view case (H4) by rendering dense flat cells as a pre-rasterized proxy
   instead of 10.9 M live shapes. Only matters at fit/overview; the zoomed path
   is already fast.

**Not yet justified:** the **memo cache**. Tessellation is a one-time 1.7 s cost,
not a repeated one; there's no measurement showing re-tessellation churn. Revisit
after lazy tessellation + editing (Phase 3) create real cache pressure.

If the near-term target files stay ≤ ~250 MB and single-file, the pipeline
already *works* (slow TTFF, jank at fit, but no crash) — in that case Phase 2
could reasonably prioritize worker offload (UX) over the parser (ceiling). The
parser wins the moment files exceed ~300 MB or multiple files are opened.

---

# After Phase 2a (cheap high-leverage fixes)

Three cheap changes were applied and re-measured (branch `phase2a-quick-wins`).
They **reorder** the Phase 2 plan: the fit-view LOD work is deleted, and the
streaming parser drops from "urgent" to "important."

### STEP 1 — 4 GB wasm memory opt-in (`-C link-arg=--max-memory=4294967296`)
The "~2 GB ceiling" was V8's default, not a wasm32 limit. Declaring a 4 GB
maximum lets the engine grow past 2 GB. **It works, for +3 bytes of wasm and no
perf change:**

| flat probe file | distinct polys | wasm heap | result |
|---|--:|--:|---|
| flat_5M.gds (305 MB) | 5.0 M | 2.35 GB | OK (was: crash < 2 GB) |
| flat_8M.gds (488 MB) | 8.0 M | 2.71 GB | OK |
| flat_11M.gds (671 MB) | 11.0 M | ~3.7 GB | **trap (OOM at the 4 GB max)** |

New effective ceiling ≈ 8–10 M distinct polys (~2.7–3.4 GB steady; the parse
transient hits the 4 GB wall). Real chips sit at ~2× headroom: caravel 3.9 M
polys / 1.68 GB, caravan-signoff 4.8 M / 1.90 GB.

### STEP 2 — take the visible-shape count off the cull hot path
`cull.rs` queried every visible cell's R-tree every frame purely to fill a stat
the renderer never reads. Made it opt-in (`cull_counted`, a debug toggle). This
was the fit-view fix.

### STEP 3 — lazy tessellation
Tessellate + upload a cell def the first time it's drawn, not all defs at load.

### Before / after (the numbers that matter)

| metric | before 2a | after 2a | note |
|---|--:|--:|---|
| **caravel cull @ fit** (native) | 53.6 ms | **6.8 ms** | STEP 2; ~8× |
| **caravel render @ fit** (native) | 12 fps | **78 fps** | STEP 2 |
| caravel fit fps (browser WebGL2\*) | 22 | 102 | corroborates |
| **caravel TTFF** (browser) | 3.8 s | **2.4 s** (mid/leaf) | STEP 2 dropped the 0.7 s forced index build; STEP 3 dropped small-cell tess |
| caravel GPU geo @ zoom | 210 MB | 141 MB | STEP 3 (only visible defs) |
| sky130 TTFF (browser) | 98 ms | **24 ms** (mid/leaf) | STEP 3 |
| sky130 GPU geo @ zoom | 4 MB | ~0 MB | STEP 3 |
| sky130 render @ fit (native) | 108 fps | 98 fps | unchanged (render-bound) |
| memory ceiling | ~2 GB | **~3.4 GB** | STEP 1 |

\* browser fps is WebGL2/ANGLE and rAF-capped (~120); native Metal is
authoritative. The 22→102 jump confirms a real per-frame stall was removed.

### Answers to the STEP 4 questions

**Is fit view still slow? — NO.** The fit-view cost was the cull-path
instrumentation (STEP 2), not GPU triangle count. caravel fit went 12 → 78 fps
(native) / 22 → 102 (browser); sky130 fit is 98 fps native (its 38 fps in the
bench is the headless WebGL2 software rasterizer, not a real GPU). **The planned
density-proxy / LOD-raster work is not justified and is deleted** for the current
M5/WebGPU target. Revisit only if a much weaker GPU becomes a target — caravel
fit does draw ~7.9 M triangles, fine on an M5 (12.8 ms) but a candidate for LOD
on low-end hardware. It is not a Phase 2 priority.

**Where does memory stand? — comfortable, ~2× headroom.** The 4 GB opt-in moved
the ceiling from ~2 GB to ~3.4 GB for a 3-byte change; real chips are at
1.7–1.9 GB. **The streaming parser is no longer the top priority** — it's
important (it would halve the parse footprint and push the ceiling further, and
it speeds parse) but not urgent.

### Revised Phase 2b priority

1. **Worker offload of parse + tessellation.** The #1 remaining issue is the
   **2.4 s main-thread freeze** on caravel (parse 1.2 s + tessellating the few
   giant flat top-level cells 1.1 s — those cells are visible at every zoom, so
   lazy tessellation can't skip them). Moving parse+tess into the Web Worker
   (COOP/COEP already set) unfreezes the tab and pairs with the lazy
   tessellation already in place.
2. **Streaming parser.** Important, not urgent (STEP 1 bought the headroom).
   Fold it into the worker: parse incrementally into the compact `CellDb`,
   dropping raw records as you go, to cut the 1.4–1.9 GB parse high-water and
   push the ceiling past 4 GB-equivalent.
3. ~~Tile pyramid / LOD raster for fit view~~ — **deleted** (STEP 2 fixed fit).
   Keep only as a contingency for low-end GPU targets.
4. **Memo cache** — still not justified by any measurement.

---

## Reproduce

```bash
# 1. fetch real test data (large; gitignored)
scripts/fetch_testdata.sh

# 2. native measurements
cargo run --release -p fluideda-core   --example bench -- testdata/sky130_fd_sc_hd_merged.gds
cargo run --release -p fluideda-core   --example bench -- testdata/caravel/gds/caravel.gds
cargo run --release -p fluideda-render --example shot  -- testdata/caravel/gds/caravel.gds

# 3. browser measurements (release wasm + headless Chromium)
trunk build --release
python3 scripts/serve_bench.py 8090 &
node scripts/browser_bench.mjs         # needs `npm i playwright && npx playwright install chromium`

# interactively: trunk serve, then open
#   http://localhost:8080/?file=/testdata/caravel/gds/caravel.gds
#   append &zoom=leaf to open zoomed; &stats=1 to enable the slow visible count
# (or drag a .gds onto the canvas / use the Open button)

# 4. Phase 2a memory-ceiling probe (the 4 GB opt-in is in .cargo/config.toml)
python3 scripts/gen_flat.py 5     # -> testdata/flat_5M.gds (and 8, 11, ...)
#   load flat_*.gds in the browser and watch wasm_heap_bytes climb past 2 GB
```
