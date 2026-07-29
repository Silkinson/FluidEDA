# Phase 2b — Spatial chunking + worker offload + streaming parser: findings

**Status: STEP 1 (chunking) and STEP 3 (streaming parser) shipped and measured.
STEP 2 (worker) is NOT implemented** — its toolchain groundwork is done and
proven, but the STEP 3 result (below) means it no longer needs that groundwork.
See [STEP 2](#step-2--worker-offload-not-implemented) and
[STEP 3](#step-3--streaming-gdsii-parser).

**TL;DR.** Two levers, measured end to end:

- **Chunking** collapsed leaf-zoom tessellation on caravel **2235 ms → 22 ms**
  (100×) and leaf GPU geometry **141 MB → 1.1 MB**, byte-identical output. It did
  *not* fix time-to-first-frame, because after it, TTFF was **92% parse**.
- **The streaming parser** then took the parse itself: caravel's wasm parse heap
  **1.68 GB → 777 MB**, parse wall **2.4 s → 1.2 s**, leaf TTFF **2.60 s → 1.38 s**
  (caravan-signoff: **1.90 GB → 892 MB**, TTFF ~4.5 s → 1.75 s). Byte-identical to
  gds21, proven cell-for-cell on hier/sky130/caravel.

The headline consequence is architectural: peak parse heap is now ~0.8–0.9 GB, so
**a worker that parses and copies the CellDb back fits under 4 GB with margin — the
worker (STEP 2) no longer needs shared memory / atomics / nightly at all** and can
ship as a plain transferable-buffer worker on stable. See
[the STEP 2 answer](#does-step-2-still-need-a-shared-heap).

---

## Method

Same harnesses and files as Phase 1.5 (`crates/core/examples/bench.rs`,
`crates/render/examples/shot.rs`, `scripts/browser_bench.mjs`). Two changes worth
knowing about before reading any number:

**Every "before" number here was re-measured on this machine, not copied from the
Phase 1.5/2a doc.** The doc's browser baseline (caravel TTFF 2.4 s, parse 1.23 s)
does not reproduce here — this machine/Chromium measures the *same pre-chunking
commit* at TTFF 4.55 s / parse 2.15 s. The old numbers are not wrong so much as
not comparable; all before/after pairs below come from the same session on the
same machine, which is the only way the deltas mean anything.

**`shot.rs` now builds a fresh renderer per viewport.** It used to share one
across `fit → mid → leaf`, so the first view uploaded everything and every later
view reported ~0 ms tessellation and the whole file's GPU bytes. That made the
exact metric Phase 2b targets unmeasurable. Each viewport now measures what
opening the file *at that zoom* actually costs.

---

## STEP 0 (prerequisite) — the golden check didn't work

The invariant is "hier.gds + sky130 must render identically to the Phase 1 golden
PNGs at every checkpoint". That could not be checked, for two reasons.

**The renders were nondeterministic.** `cull_inner` materialised its `grouped`
HashMap straight into the draw list, so draw order was per-process random; the
renderer draws in that order and the pipeline alpha-blends, so overlapping cells
composited differently run to run. Two runs of *unmodified* code differed by 118
pixels (0.01%, max channel delta 28) on sky130 at fit. Sorting the groups by cell
id fixed it. No policy change — cross-cell paint order was already arbitrary; this
only pins it.

**There were no golden PNGs.** `target/shots/` is generated and gitignored, and
nothing was committed. So "the Phase 1 golden PNGs" were re-created from the
now-deterministic pre-chunking code and every checkpoint diffed against those.

Worth fixing properly: the goldens still aren't committed, so this only protects
within a session. A committed PNG + a `cargo test` that diffs it would make the
invariant real.

---

## STEP 1 — spatial chunking

### The shape of the problem

Per-cell shape counts on caravel (1014 defs, 3.92 M distinct polygons) — the
measurement that drove every decision here:

| cell | shapes | % | bbox |
|---|--:|--:|---|
| `caravel_core` | 2,504,272 | 63.8% | 3194 × 4819 µm |
| `housekeeping` | 363,765 | 9.3% | 410 × 551 µm |
| `RAM128` | 312,019 | 8.0% | 406 × 438 µm |
| `manual_power_connections` | 112,672 | 2.9% | 3194 × 4819 µm |
| `sky130_fd_io__*` (5 cells) | 55–77 k each | 8.4% | ~75 × 200 µm |

One cell holds 64% of the file and spans the entire die. That is the whole
problem: hierarchy culling cannot help a single die-spanning definition, so a 5 µm
window tessellated all 2.5 M of its shapes.

### Chunk-assignment policy: single-owner + loose bbox

Each shape belongs to **exactly one** chunk — the one containing its bbox centre.
Each chunk stores the **union bbox of the shapes it owns**, which may extend past
its nominal grid rect, and selection tests that union (a "loose grid").

**Why not assign a border-spanning shape to every chunk it overlaps:** it
duplicates geometry. The pipeline alpha-blends, so a shape tessellated into two
visible chunks blends with itself and renders a different colour than the same
file renders today — it breaks the golden invariant outright. Single ownership
also keeps total tessellation work exactly equal to the unchunked path (every
shape tessellated once) and keeps a chunk a contiguous range rather than a gather.

**What it costs**, measured rather than assumed — `chunk_stats` reports each
chunk's loose bbox as a multiple of its ideal grid square:

| cell | chunks | mean shapes | worst loose bbox |
|---|--:|--:|--:|
| `caravel_core` | 71 | 35,271 | **80.7× grid square** |
| `housekeeping` | 25 | 14,551 | 25.0× |
| `RAM128` | 25 | 12,481 | 25.0× |
| `manual_power_connections` | 8 | 14,084 | 8.8× |

The inflation is real and large: a die-spanning power strap drags its owner
chunk's bbox across the die, so that chunk is selected from any viewport and its
shapes always tessellate. It is bounded — one chunk's worth of work (~35 k
shapes), not the cell's 2.5 M — and the leaf numbers below show it does not
matter in practice. Two other things this table shows: `caravel_core`'s grid is
**sparse** (71 non-empty of 169 squares, so its geometry is clustered, and mean
occupancy runs 2× the target), and a smarter policy (quadtree, or promoting long
shapes to a coarse level) has measurable headroom if it ever matters.

### Thresholds are measured, not guessed

Chunking is **only ever a cost at fit view** — every chunk of a visible cell is on
screen, so each is an extra draw call buying no culling — and only a win when a
viewport sees a fraction of a cell. Both constants were swept on caravel (full
tables in the doc comments on `CHUNK_MIN_SHAPES` / `CHUNK_TARGET_SHAPES`):

| threshold | cells chunked | fit frame | leaf tess | leaf GPU |
|---:|--:|--:|--:|--:|
| (none) | 0 | 18.8 ms | 1469 ms | 141 MB |
| 50 k | 9 | 22.3 ms | 11.9 ms | 1.1 MB |
| **100 k** | **4** | **20.0 ms** | **12.0 ms** | **1.1 MB** |
| 200 k | 3 | 21.9 ms | 72.9 ms | 7.1 MB |
| 500 k | 1 | 19.1 ms | 73.5 ms | 7.1 MB |

The 50 k → 100 k step is the interesting one, and the reason is structural. It
excludes the five `sky130_fd_io__*` pad cells, which are shape-*dense* (55–77 k)
but spatially *tiny* (75 × 200 µm). Hierarchy culling already handles a small
cell — it is one bbox test — so chunking them bought nothing and cost ~50 draw
calls at fit. **What defeats hierarchy culling is a cell that spans the die, not
one with many shapes.** 100 k keeps both of caravel's die-spanning cells and drops
the pads. Going coarser (200 k+) drops `manual_power_connections` and leaf
tessellation jumps 6×.

Settled on **`CHUNK_MIN_SHAPES = 100_000`, `CHUNK_TARGET_SHAPES = 16_000`**.
Accepted cost: a leaf view centred inside a 55–77 k pad cell tessellates it whole
(~45 ms), a one-off well inside a frame budget.

### Results — caravel, native (M5 / Metal, 1000×1000, fresh renderer per view)

| viewport | tess before | after | GPU before | after | frame before | after |
|---|--:|--:|--:|--:|--:|--:|
| fit | 2200 ms | 2292 ms | 207.1 MB | 207.1 MB | 18.8 ms (53 fps) | 20.4 ms (50 fps) |
| mid | 1472 ms | **12 ms** | 140.8 MB | **1.1 MB** | 5.9 ms | 5.9 ms |
| leaf | 1469 ms | **12 ms** | 140.8 MB | **1.1 MB** | 5.7 ms | 5.7 ms |

### Results — browser (headless Chromium, WebGL2/ANGLE-Metal, M5)

| caravel | TTFF before | after | parse before | after | tess before | after | heap | fps before | after |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| fit | 5444 ms | 5154 ms | 2148 | 2423 | 3121 ms | 2488 ms | 1680 MB | 52 | 47 |
| mid | 4563 ms | **2641 ms** | 2164 | 2424 | 2240 ms | **22 ms** | 1680 MB | 98 | 100 |
| leaf | 4549 ms | **2602 ms** | 2147 | 2387 | 2235 ms | **22 ms** | 1680 MB | 100 | 101 |

sky130 is untouched, as intended — **0 cells chunked**, TTFF 44 → 46 ms at leaf,
heap 63 → 64 MB. hier is unchanged (~18 ms). Native peak RSS 1.7 → 1.6 GB;
sky130's native RSS is 32.6 MB (Phase 1.5 recorded 37.6 MB pre-store).

### Honest costs

- **Fit view regressed ~6%**: native 53 → 50 fps, browser 52 → 47. Chunking cannot
  help at fit (every chunk is on screen) and costs draw calls: 152 → 277 plans.
  This is inherent, not a bug, and the sweep above already minimises it.
- **Parse pays +275 ms in wasm** (2148 → 2423 ms) for the chunk sort/reorder.
  Native pays only +60 ms (1.47 → 1.53 s).
- **The wasm heap did not move at all** (1680 MB before and after). The columnar
  store saved ~100 MB of *native* RSS, but the wasm high-water is set by gds21's
  whole-library parse, which chunking does not touch. See the streaming-parser
  section — this is the single most important number in this document.

### Two bugs found by measuring rather than assuming

Both were invisible natively and would have shipped:

1. **Chunking ran while the whole gds21 library was still alive.** `build_db(&lib)`
   finalised the DB — including the reorder's transient coordinate copy — before
   `lib` was dropped, stacking the copy on the parse high-water. Peak RSS went
   *up*, 1.56 → 1.8 GB. The library is now dropped before finalisation.
2. **Chunking re-derived every shape's bbox three times** (grid key, chunk union,
   then `own_bbox`), walking a 2.5 M-shape cell's coordinates repeatedly. Native
   cost: +0.2 s, easy to miss. **Wasm cost: +1.3 s — enough to cancel the entire
   tessellation win.** Caching per-shape bboxes once and folding `own_bbox` from
   the chunk table fixed it. Without the browser measurement, STEP 1 would have
   been a wash on the platform that matters.

### Renderer: draw order is load-bearing

Splitting a cell into chunks naturally draws chunk-major (chunk 0's every layer,
then chunk 1's) where it previously drew layer-major (all of layer 0, then layer
1). That re-composites every cross-layer overlap and **changed 4.7% of caravel's
pixels at fit**. The renderer now sorts draws by `(cell, layer, chunk)`, restoring
the original layer sweep; reordering chunks *within* a layer is safe because a
layer has one colour and alpha, and `src-over` of identical colours commutes.

**Verified: hier, sky130 and caravel all render byte-identical to pre-chunking at
fit/mid/leaf.**

---

## STEP 2 — worker offload (SHIPPED)

**Status: shipped as a plain transferable-buffer worker on stable Rust** (branch
`phase2b-worker`). Parse runs in a Web Worker — a separate `fluideda-core`-only
wasm module (174 KB, no wgpu/egui) with its own linear memory. It receives the
file bytes (transferred in), runs the streaming parser + chunking, serializes the
`CellDb` to Pod columns, and transfers those back as `ArrayBuffer`s; the main
thread rebuilds the `CellDb` and proceeds exactly as before (cull → lazy tess →
draw). No shared heap, no atomics, no `build-std`, no COOP/COEP — STEP 3's
~0.8 GB parse peak is what made a parse-and-copy worker fit under 4 GB.

### Results (browser, headless Chromium, WebGL2/ANGLE, leaf zoom)

Leaf/mid is where parse was the whole freeze (chunking took tessellation to
22 ms). Measured with `scripts/worker_test.mjs`:

| metric | caravel (278 MB) | caravan-signoff (329 MB) |
|---|--:|--:|
| parsed in worker | ✅ | ✅ |
| parse wall (in worker, off main) | ~1.05 s | ~1.23 s |
| main-thread reconstruct | ~62 ms | ~72 ms |
| **worker heap (parse peak)** | ~1.03 GB | ~0.89 GB |
| main heap (after reconstruct) | ~0.45 GB | ~0.50 GB |
| **combined peak** | **~1.46 GB** | **~1.39 GB** |
| longest main-thread frame during load | ~200 ms | ~260 ms |

**Closes the "combined-peak not demonstrated" debt.** Peak across worker+main is
~1.4–1.5 GB, well under 4 GB (the worker's parse peak of ~1 GB plus the main
thread's ~0.5 GB reconstructed copy, both alive at the instant before the worker
is terminated). The worker's memory is *reclaimed*: each load spawns a one-shot
worker that is `terminate()`d after the transfer, freeing its whole wasm instance
(wasm memory never shrinks in place, so reclamation = terminate).

**The parse freeze is gone.** During the ~1 s parse the main thread runs at ~16 ms
frames — the canvas stays pannable while caravel parses, which was the point.

**The residual is a ~200 ms design-swap frame**, not a parse freeze — down from
the old ~1.2 s parse block. It is the moment the reconstructed design installs:
two ~150 MB coordinate copies (JS→wasm when reading the transferred typed array,
then the per-cell split inside `deserialize`), plus meta parse, plus the first
cull+tessellate. The worker's summary (layers/bbox/counts) is computed worker-side
so the main thread does *not* rescan 3.9 M shapes (that alone was 80 ms), and the
packed layer column is cast (not unpacked) on the main thread. Getting the swap
under 100 ms would need a **single-copy reconstruction** — build each cell's `Vec`
directly from a per-cell `subarray` of the transferred column, eliminating the
concatenated intermediate — noted as a follow-up, not built here. The renderer is
now *reset* rather than rebuilt on install, so a second file-open doesn't recompile
the shader pipeline.

> ⚠️ **Fit view is a separate, larger freeze the parse worker does NOT address.**
> At fit, the giant flat cells tessellate on the main thread (~2.5 s) — measured
> here as a ~7 s swap frame at fit. That is tessellation, not parse; offloading it
> would mean chunk-tessellation in the worker (a bigger protocol) or a fit-view LOD
> proxy. Out of scope for STEP 2, which is parse-only.

### Optional follow-up: progressive render (feasible, not built)

The streaming parser finalizes cells in file order (on ENDSTR), so the worker
*could* emit each completed cell's columns as it parses and the main thread draw
the design filling in, instead of one reconstruct at the end. The obstacle is the
one flagged before: **GDSII allows forward references** — an SREF/AREF can name a
child whose definition comes later — so a cell is only safe to draw once every
cell in its instance subtree is finalized. In practice tools write children-first
(caravel has 0 forward references), so the common case works with a "defer a cell
whose descendants aren't all done" guard. Whether it is worth it is questionable
now that parse is off the main thread and the canvas is already responsive during
it; the win would be showing geometry sooner (lower time-to-*useful*-frame), not
removing a freeze. Deferred.

### What STEP 1 changed about the requirement

The Phase 2a doc justified the worker as "parse 1.2 s + tessellating the few giant
flat cells 1.1 s". Chunking removed the second half at every zoom except fit. What
is left:

| caravel, browser | parse | tess | TTFF |
|---|--:|--:|--:|
| leaf / mid | 2.4 s | **0.02 s** | 2.6 s |
| fit | 2.4 s | 2.5 s | 5.2 s |

So the answer to the question the brief asked — *"if STEP 1 alone gets leaf-zoom
TTFF acceptable and the freeze only matters at fit, say so"* — is: **half of it.**
Leaf-zoom *tessellation* is solved (22 ms; STEP 2 has nothing to offer it). But
leaf TTFF is still 2.6 s, and it is **92% parse**. Parse is not a fit-view problem;
it is on the critical path at every zoom. So STEP 2 narrows to **parse everywhere,
plus tessellation at fit only** — it does not need chunk-tessellation offload for
mid/leaf at all, which simplifies the protocol considerably.

### Memory model, and why it forces the design

> **Superseded by STEP 3 — read [Does STEP 2 still need a shared
> heap?](#does-step-2-still-need-a-shared-heap).** This subsection reasoned from a
> 1.68 GB parse heap and concluded option 1 (worker parses + copies) was worse than
> the baseline, forcing the shared-heap design. The streaming parser dropped the
> parse heap to ~777 MB, which makes option 1 fit under 4 GB with ~2.8 GB to spare.
> **Option 1 is now the recommended design and needs no shared memory.** The
> analysis below is kept for the record of why the shared heap looked necessary
> before parse was fixed.

The brief's trap is real and worth restating with the measured numbers. caravel's
wasm heap is 1680 MB, of which the columnar store is 196 MB — **gds21's
intermediate library is ~1.4 GB of it**. Three options:

1. **Worker parses, postMessages the CellDb to main.** Rejected. The store lives
   inside wasm linear memory, so "transfer" means copying out to an ArrayBuffer
   and back in. Worse, wasm memory never shrinks: the worker's heap stays ~1.7 GB
   reserved until terminated, so the pair peaks at ~2.1 GB — **worse than the
   1.68 GB single-thread baseline**, exactly as the brief warns.
2. **Two wasm modules sharing one memory.** Unsound: each module has its own
   allocator state, and two allocators over one heap corrupt it.
3. **One module, shared memory, two threads.** The only option that holds peak at
   the baseline. Parse runs on a worker thread, allocates the `CellDb` in the
   shared heap, and moving it to the main thread is a pointer move — no copy. This
   is why STEP 1's store had to be `Pod` columns with no per-shape `Vec`.

**The peak-heap proof the brief asks for cannot be given yet** — nothing runs in a
worker. The claim option 3 *would* hold peak at 1680 MB is a design argument, not
a measurement, and should be treated as unproven until the worker exists.

### Groundwork done and proven

Option 3 needs `-C target-feature=+atomics` so the module's memory is a
SharedArrayBuffer. Two blockers found and cleared:

1. **`getrandom` 0.1.16 cannot compile for wasm with atomics** — wasm-bindgen
   changes `JsStatic` handling under threads, so its `MODULE.require("crypto")`
   stops resolving. It arrived via an *unused* `y-sync` dependency that also
   dragged in a duplicate `yrs 0.17`. `fluideda-sync` declared y-sync but never
   referenced it; removing it deletes the whole chain.
2. **wgpu 22's wasm handles are `!Send + !Sync` under atomics** — they wrap JS
   objects, which cannot cross a worker boundary, so wgpu drops its
   `fragile-send-sync-non-atomic-wasm` shim. egui's `type_map::concurrent::TypeMap`
   requires `Send + Sync` unconditionally. **That single bound was the only
   remaining blocker in the entire dependency tree** — 4 errors, all from one
   `callback_resources.insert` call site. A `MainThreadOnly` marker resolves it,
   sound because eframe paints only on the main thread and the worker will link
   `fluideda-core` alone.

With both cleared, the app **links to a shared-memory wasm module** — verified by
reading the emitted memory section: `flags=0x03` (shared + declared max):

```bash
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals \
  -C link-arg=--max-memory=4294967296 -C link-arg=--shared-memory" \
  cargo +nightly build --release -p fluideda-app \
  --target wasm32-unknown-unknown -Z build-std=std,panic_abort
```

That was STEP 2's largest unknown, and it is now answered: **the geometry pipeline
(`fluideda-core`) and the whole app both compile and link under atomics.** The
default stable/trunk build is untouched — no toolchain pin, no `.cargo/config.toml`
change — so this costs nothing until the worker is turned on.

### What remains

- Trunk integration: nightly + `build-std` + atomics flags, and wasm-bindgen glue
  emitted with shared memory. Likely a `rust-toolchain.toml` and a
  `[unstable] build-std` entry (which only engages when `--target` is passed, so
  native builds stay on precompiled std).
- Thread spawn. `std::thread::spawn` does not work on wasm; `wasm-bindgen-rayon`'s
  `init_thread_pool` handles worker creation and module/memory sharing. Then parse
  is `rayon::spawn(move || parse(bytes))` with the `CellDb` sent back over a
  channel — a pointer move in the shared heap. `CellDb` looks `Send` already
  (`OnceCell<ShapeIndex>` is `Send` when `T: Send`); worth confirming.
- Keep the `CellDb` in an `Arc` so the worker can tessellate chunks while the main
  thread culls.
- Non-worker fallback for native and non-crossorigin-isolated contexts.
- The measurements the brief asks for: longest main-thread frame during load, and
  **peak heap across the worker+main pair** — the number that proves or refutes
  the no-double-buffer claim above.

Stopping here rather than landing a half-working worker was deliberate: STEP 1 is
complete and verified, and the remaining integration is a substantial piece of
work with its own risk (Trunk + build-std + wasm-bindgen-rayon + eframe init
order) that deserves its own commit and its own measurements.

### Does STEP 2 still need a shared heap?

**No — STEP 3 removed the reason.** The whole shared-heap / atomics / build-std
design existed to avoid doubling a 1.68 GB heap when a worker parses and the main
thread reads. With the streaming parser, peak parse heap is **~777 MB (caravel) /
~892 MB (caravan-signoff)**. A worker that parses and then *copies* the CellDb
back over `postMessage` now peaks at roughly:

```
  worker heap during parse            ~800 MB
  + serialised store in a transfer buf ~200 MB   (transient, in the worker)
  + main-thread copy of the store      ~200 MB
  ------------------------------------------------
  ~1.2 GB combined across both threads — vs a 4 GB ceiling
```

That fits with ~2.8 GB of headroom. So STEP 2 can be a **plain transferable-buffer
worker on stable Rust**: no `+atomics`, no `build-std`, no nightly, no
`SharedArrayBuffer`, no `wasm-bindgen-rayon`. The columnar store is already `Pod`
bytes (Phase 2b STEP 1), so "serialise" is `bytemuck::cast_slice` into an
`ArrayBuffer` and "deserialise" is a borrow — the copy is a memcpy, not a
traversal. The step-2-groundwork commit's shared-memory machinery (the
`MainThreadOnly` marker, the atomics build) is no longer on the critical path; it
still links and costs nothing, so it stays for now, but the worker doesn't need
it. This is the single most useful thing STEP 3 did for STEP 2, and it was the
explicit reason the brief sequenced the parser first.

---

## WebGL2 vs WebGPU

**Not observed — and this is a gap, not a finding.** The harness still forces
wgpu's WebGL2 backend because this Chromium's WebGPU rejects wgpu 22's
`requestDevice` limits (unchanged since Phase 1.5). So every browser number here
is WebGL2/ANGLE-Metal, and no WebGPU-vs-WebGL2 comparison was possible.

What the data does suggest: chunking's fit-view cost is **draw-call bound**, and
WebGL2 has higher per-draw overhead than WebGPU. Fit regressed 10% on WebGL2
(52 → 47 fps) versus 6% natively on Metal (53 → 50). So WebGL2 is the pessimistic
case for chunking, and the numbers above are a floor. Worth re-checking on WebGPU
before treating the fit regression as settled — a wgpu upgrade (22 is well behind)
would likely unblock headless WebGPU and is a prerequisite for that.

---

## STEP 3 — streaming GDSII parser

Replaces "gds21 parses the whole `GdsLibrary`, then we convert" with a hand-rolled
record reader (`crates/core/src/gds_stream.rs`) that emits directly into the
columnar `CellDb` and drops each raw record as it goes. GDSII framing is trivial
(a flat sequence of length-prefixed records), so this is a small, plain-stable-Rust
module in `crates/core`, no new deps.

### Cell-id parity, and why it's load-bearing

The reader makes two passes over the bytes: pass 1 interns every struct name in
file order, pass 2 builds shapes + instances. The split is not for convenience —
it reproduces gds21's exact cell-id assignment. The renderer draws **cell-major**
and the pipeline alpha-blends, so the id given to each cell decides the image
wherever cells on different layers overlap. Matching gds21's "intern all struct
names first, in file order, then fill" is what keeps the golden image
byte-identical across the parser swap. (Measured: caravel has 0 forward
references, so even single-pass would match it — but two-pass matches *any* file,
and the oracle test holds it to that.)

### Correctness proof — gds21 kept as an oracle

gds21 is not deleted; it's retained as `load_gds_db_gds21` and an oracle test
parses a file both ways and asserts identical cells / shapes (layer + every point)
/ instances (child + transform + array params). hier and sky130 run in
`cargo test`; caravel was checked once by hand:

| file | cells | shapes | instances | result |
|---|--:|--:|--:|---|
| hier | 3 | 30 | — | identical |
| sky130 | 440 | 48,784 | — | identical |
| caravel | 1014 | 3,924,533 | 462,523 | **identical** |

The golden-image test (GATE 0) stays green at 0 px — the swap is invisible.

Quirks handled (the set gds21 handled): AREF 3-point XY + COLROW, STRANS
reflect/rotate/mag decoded from 8-byte GDS reals, the repeated boundary closing
point kept verbatim, negative i32 coords. Read: BOUNDARY/SREF/AREF. Consumed but
skipped (as before): PATH/TEXT/NODE/BOX. Malformed/truncated records return an
error rather than panic.

### Results

Headline is **peak wasm heap during parse** — the transient high-water. wasm
linear memory never shrinks, so a sample taken immediately after parse (the new
`parse_heap_bytes` metric, before any tessellation) *is* the high-water.

Browser (headless Chromium, WebGL2/ANGLE-Metal, M5):

| file | size | parse wall | wasm parse heap | leaf TTFF |
|---|--:|--:|--:|--:|
| caravel | 278 MB | 2387 → **1180 ms** | 1.68 GB → **777 MB** | 2.60 → **1.38 s** |
| caravan-signoff | 345 MB | ~2100 → **1400 ms** | 1.90 GB → **892 MB** | ~4.5 → **1.75 s** |
| sky130 | 4.2 MB | 40 → 16 ms | tiny (27 MB total) | ~unchanged |
| hier | 980 B | ~0 | tiny | ~unchanged |

Native (M5), caravel: parse **1.53 s → 549 ms** (530 MB/s), peak RSS **1.6 GB →
765 MB**; caravan-signoff parse 675 ms, parse-peak RSS 878 MB.

### Did wall-clock improve, or only memory?

**Both — and the speedup is measured, not assumed.** caravel parse dropped 2× in
the browser (2387 → 1180 ms) and 2.8× native (1.53 s → 549 ms). The cause is the
same as the memory win: gds21 allocated an enum + a `Vec<GdsPoint>` + optional
fields per boundary (3.9 M of them), and the streaming reader allocates none of
that — each boundary's points go straight from the byte slice into the columnar
store. Fewer allocations is fewer cycles as well as fewer bytes.

### Is progressive render (draw completed cells during parse) worth a follow-up?

**Feasible, with one real obstacle; not built here (out of scope).** Cells finalise
in file order (on ENDSTR), so the reader could hand each completed cell to the
renderer as it lands and paint the design as it streams in. The obstacle the brief
flagged is real: **GDSII permits forward references** — an SREF/AREF can name a
child whose BGNSTR comes later — so an instance may point at a not-yet-parsed
(empty) cell, and drawing its parent early would show a hole. It is safe to draw a
cell only once every cell in its instance subtree is finalised. Most tools
(including all of caravel) write children-first, so in practice the top cell is
last and everything it references already exists by the time it's drawn — meaning
the *useful* case (show the chip filling in) works for the common file, and the
guard is "defer a cell whose descendants aren't all done yet." Whether it's worth
it now is questionable given parse is already ~1.2 s and off the critical path once
STEP 2 lands; it's a UX nicety, not a bottleneck fix. Recommend deferring until
after the worker.

---

## Reproduce

```bash
# native (add --example cells for the per-cell shape distribution)
cargo run --release -p fluideda-core   --example bench -- testdata/caravel/gds/caravel.gds
cargo run --release -p fluideda-render --example shot  -- testdata/caravel/gds/caravel.gds

# browser (parse_heap_bytes in the metrics is the parse high-water)
trunk build --release
python3 scripts/serve_bench.py 8090 &
npm i playwright && npx playwright install chromium
node scripts/browser_bench.mjs

# golden image test (committed references; 0 px on the capture machine)
cargo test -p fluideda-render --test golden
#   regenerate after an intended visual change:
#   UPDATE_GOLDEN=1 cargo test -p fluideda-render --test golden

# streaming parser == gds21 oracle (byte-identical CellDb)
cargo test -p fluideda-core oracle

# STEP 2 groundwork: prove the shared-memory build still links
rustup toolchain install nightly --component rust-src
rustup target add wasm32-unknown-unknown --toolchain nightly
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals \
  -C link-arg=--max-memory=4294967296 -C link-arg=--shared-memory" \
  cargo +nightly build --release -p fluideda-app \
  --target wasm32-unknown-unknown -Z build-std=std,panic_abort
```
