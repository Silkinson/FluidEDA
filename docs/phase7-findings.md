# Phase 7: findings

**Status: in progress on `phase7`.** The owner moved 7.3 (stream the
hierarchy) to the front. **7.3 is done** in three slices: **A** placement
chunks in the core, **B** streaming the hierarchy (pack v3), **C** coloured
proxies (pack v4). **7.4 is done** (edit streamed designs): **A** picking and editing, **B**
rooms and "edit this placement only", **C** server-side export. The owner
then asked to finish all of Phase 7: **7.1a** (idle repaint), **7.7**
(compact rectangles) and **7.8** (compact placements) are done.

## 7.3 SLICE A — placement chunks

**Problem.** Shapes have been chunked spatially since Phase 2b, but a cell's
*placements* were one flat list. caravel_core places 460 k standard cells
directly, and the cull visited every one of them every frame, mostly just to
find it off-screen. That is also the list an out-of-core client would have to
download before its first frame.

**Design.**
- Cells with more than 4,096 placements get a placement grid, aiming for about
  2,048 placements per chunk (cap 128 × 128). Like shape chunks, it is a
  stable counting sort into grid squares by footprint centre, so each chunk is
  a contiguous run of the cell's instances. An AREF counts as one placement
  with its whole array's footprint.
- Shapes and placements now share one grid-sort helper (`chunk::grid_runs`).
  Shape order is byte-identical, so shape ids are unchanged.
- Each cell carries `inst_chunks` (runs + parent-frame footprints). The
  serialized skeleton carries them too (format v2, range-checked on read).
- The cull tests each chunk's footprint and skips an off-screen chunk
  whole. While an edit has grown a cell past its stored bbox, the chunk
  footprints may be too small, so the cull falls back to walking every
  placement — exactly the old behaviour.
- **Placement numbering changes** for re-sorted cells, so `SHAPE_ID_SCHEME`
  is now 3. A room bound under scheme 2 refuses scheme-3 clients, which is
  the intended safety behaviour. `PACK_VERSION` is 2 (the skeleton format
  changed); servers rebuild packs on demand.

**Correctness.**
- `placement_chunks_cull_exactly_like_walking_every_placement`: a 6,000-placement
  lattice plus an AREF, at four views × three zooms, gives identical draws and
  proxies as multisets, and never more nodes.
- `cull_bench` asserts identical selections on caravel at every view.
- KLayout oracle on caravel: 1,014 cells, 3,925,013 polygons, 462,523
  placements identical (placements compared as multisets).
- Golden images unchanged; all 98 core tests (including the parser
  cross-checks and round-trips that compare placements in order) pass,
  because the sort is stable and idempotent: a written-then-re-read file
  keeps its order.
- In the browser, streamed caravel is still pixel-identical to in-core at
  the four busy views (0 of 688,000 pixels), and the two-browser sync test
  passes.

**Measured** (same binary, chunked vs every cell collapsed to one chunk):

| caravel cull | native, walk all | native, chunked | browser (wasm, WebGPU), walk all | browser, chunked |
|---|--:|--:|--:|--:|
| fit | 12.6 ms | 12.7 ms | 17.3 ms/frame | 17.7 ms/frame |
| mid (400 µm views) | 4.8 ms/view | **0.3 ms** | 10.0 ms/frame | **0.73 ms/frame** (14×) |
| leaf (50 µm views) | 4.5 ms/view | **0.06 ms** | 9.7 ms/frame | **0.37 ms/frame** (26×) |

caravel gets 561 placement chunks in 3 cells. Native: a 50 µm scan of the
whole die (1,218 views) drops from 5.5 s to 77 ms, and nodes visited from
396 M to 3.7 M. In the browser (`?instwalk` is the same-build A/B switch)
this is ~9–10 ms of main-thread time saved on every mid and leaf frame.

**What it shows next.** The fit view still spends ~17.5 ms per frame in the
cull in wasm: 553,551 placements each become a proxy box. Slice B's density
rule (one proxy per dense placement chunk) removes that for streamed
designs; applying it in-core is the obvious first step of 7.6.

## 7.3 SLICE B — streaming the hierarchy (pack v3)

**Problem.** A streamed design still downloaded every cell's full placement
list before its first frame: caravel's 25 MB skeleton (460 k placements) became
180 MB of browser heap. A design with tens of millions of placements could not
stream at all.

**Design.**
- **Pack v3.** The skeleton becomes a *cell table* (format v3): a cell with
  more than 512 placements lists only their count and its placement chunk
  table. Its placements are one record per placement chunk in a *placements*
  blob, with a byte-offset *placement index*. Small cells stay inline, so the
  hierarchy costs no round trips except where it is big.
- **Remote cells.** `Cell::remote` holds the placement chunks that have
  arrived. The cull walks a remote cell a chunk at a time:
  - a chunk whose placements would average under `min_pixel` across draws as
    one proxy, resident or not — so a zoomed-out view's work is bounded by
    the screen, not the placement count;
  - a visible chunk that has not arrived draws as a proxy and is reported in
    `placements_wanted`;
  - a resident one is walked like in-core placements and reported in
    `placements_used`.
- **Residency (app).** Placement chunks are fetched on the main thread by byte
  range (a record is ~100 KB and decodes in well under a millisecond), at most
  16 in flight. Arrivals are slotted into their cells, and the least recently
  walked chunks are evicted over 64 MB (`?placement_budget=`).
- The skeleton format bump (v3: a per-cell inline/remote flag) is internal;
  shape and placement numbering are unchanged from SLICE A.
- In-core placement sorting now permutes in place (a 50 M-placement list is
  5 GB; copying it would double the parse peak).

**Correctness.**
- Core test: a 20 k-placement cell, streamed, culls exactly like in-core once
  its wanted chunks arrive; zoomed out it draws one proxy per chunk and
  visits under a tenth of the nodes.
- `stream_test`: eight views now, four of them on the densest placement
  chunks of the remote lists (caravel_core, housekeeping, RAM128,
  caravel_clocking): **0 pixels** differ from in-core at all eight.
- Under a 1 MB placement budget, cycling the remote views evicts 20 placement
  chunks; the first view re-fetches its placements and matches pixel for
  pixel.
- The shape-chunk residency test and the two-browser sync test still pass.

**Measured** (`outofcore_bench.mjs`, WebGPU, headless; packs cached):

| design | cell table | streamed first frame | streamed main heap (fit · leaf) | fit cull | in-core |
|---|--:|--:|--:|--:|---|
| caravel (460 k placements) | **430 KB** (was a 25 MB skeleton) | 87 ms | **16 MB** · 16 MB (was 180 MB) | 2.7 ms (in-core 17.5) | 1.7 s first frame, 309 MB + 1.1 GB worker |
| flat_11M | 18 KB | 81 ms | 6 MB · 6 MB | 0.13 ms | 3.9 s, 534 MB + 2.2 GB worker |
| flat_32M | 25 KB | 77 ms | 6 MB · 8 MB | 0.15 ms | fails (allocation panic) |
| **place_50M** (50 M placements) | 394 KB | **92 ms** | **8 MB · 9 MB** | 0.45 ms | **fails** (wasm abort after 4 s) |

- **place_50M** (`scripts/gen_placements.py 50`): a 1.3 GB GDS whose size is
  its placements — a TOP with 50 M standard-cell placements in rows, 350 M
  shapes flattened. Server pack build: 39 s (parse 18 s), peak RSS 5.0 GB;
  2.65 GB of placement records in 16,384 chunks. At fit the browser draws
  16,384 placement-chunk proxies and fetches nothing; a leaf view fetches 4
  placement chunks and 4 shape chunks.
- caravel at fit: 32,053 proxies (in-core: 553,551) and 5 placement chunks
  fetched.
- Fixed in the bench: it reported the embedded sample's metrics as a
  successful in-core load when the worker died with a bare wasm trap
  (`unreachable`). It now treats traps as failures and requires the big
  file's metrics.

## 7.3 SLICE C — coloured proxies (pack v4)

**Problem.** A streamed chunk too dense to fetch draws as a neutral slate
box. Where the real geometry is sparse that box is far darker than what a
full load shows. flat_11M at fit: 71% of pixels visibly different from
in-core.

**What did not work.** The plan said "the dominant layer's colour". Measured,
it made flat_11M *worse*: mean absolute difference 22.6 → 30.2. A full load of
five interleaved layers averages to a blend, and any single layer's full
colour is further from that blend than grey is.

**Design.**
- The pack records each shape chunk's *look*: its layers' polygon area over
  the chunk's bbox area (≤ 8 layers, /255). The cell table carries it
  (format v4, `PACK_VERSION` 4).
- The app turns a look into one colour with the renderer's palette
  (`composite_rgba`): each visible layer goes over the previous in draw
  order, its palette alpha scaled by its coverage. A stack of src-over draws
  is itself one src-over, so the proxy shows what the geometry averages to.
  Channels are quantized to 5 bits, so a view's proxies fall into few colour
  groups (draw calls). Colours are recomputed when a layer is shown or
  hidden.
- The renderer draws pending proxies grouped by colour (neutral first, then
  in colour order, so blending is reproducible), with a bind group per
  colour made on first use.
- `?greyproxies` is the same-build A/B switch.

**Measured** (`proxy_fidelity.mjs`; pixels "visibly off" = any channel more
than 24/255 from in-core; WebGPU, headless):

| view | grey proxies | coloured proxies |
|---|--:|--:|
| flat_11M at fit (729 dense chunks) | 70.8% off · mean diff 22.6 | **17.9% off · mean diff 13.0** |
| caravel at fit (192 dense shape chunks) | 0.0% · 1.0 | 0.0% · **0.5** |
| both at mid | 0.0% · 0.0 | 0.0% · 0.0 |

The plan's target was to halve the visibly-off share at fit. flat_11M
drops to a quarter; caravel was already indistinguishable, and its mean
difference halves. A full load of flat_11M at fit is per-pixel noise
(sub-pixel squares either hit a pixel or miss), so no uniform box can reach
0 there. Mean difference is the better fidelity measure for such views.

Correctness: coloured or grey, proxies exist only where chunks are not
drawn, so the eight pixel-identity views are unchanged (0 pixels), and the
golden images too. Core tests cover coverage (a lattice reads ≈5% per
layer) and the look's round trip through the cell table; render tests cover
the composite (a full single layer is that layer, a hidden layer adds
nothing, sparse coverage is translucent).

## 7.4 SLICE A — picking and editing a streamed design

**Problem.** A streamed design keeps no shapes in main memory: the
tessellation worker fetches a chunk's record, draws it and drops it. Picking,
selection, moves, vertex edits and redrawing an edited chunk all read base
geometry, so streamed designs were view-only.

**Design.**
- **Shapes by chunk.** Every cell of a streamed design reads its shapes
  through per-chunk stores (`Cell::remote_shapes`). The accessors
  (`shape_points`, `shape_layer`, `chunk_store`, `resident_instances`) serve
  in-core and streamed cells alike, and say when something is not in memory.
- **Picks fetch what they need.** `pick_composite_needs` also returns the
  shape and placement chunks whose footprints hold the point but are not in
  memory. The app fetches them on the main thread by byte range (≤ 8 shape
  and 16 placement fetches in flight) and picks again each frame until
  nothing is missing: a placement chunk can reveal deeper cells' chunks. A
  pick waiting for chunks pauses eviction (see below).
- **Edits wait for their base.** A move or vertex edit whose chunk is not in
  memory (a peer's edit, say) is left out of the overlay until that chunk
  arrives, then reconciled. Chunks holding edited shapes or the selection are
  pinned; the rest are evicted least-recently-needed over 128 MB
  (`?shape_budget=`).
- **Edited chunks redraw in place.** In-core, a big edited chunk is split
  into finer chunks. A streamed design cannot do that, because the pack's
  byte offsets are per chunk. Instead the drawn chunk is re-tessellated from
  its copy in memory, minus its suppressed shapes, and swapped in (no proxy
  flash). This happens only when the chunk's suppressed set changes, so a
  drag re-tessellates once. A chunk the worker draws whole while it has
  suppressed shapes is redrawn filtered. (The plan had suppression lists sent
  to the worker. The chunk has to be in memory anyway, for the moved copy
  and the overlay, so the main thread does it.)
- **No double downloads.** A chunk a pick or edit brought into memory is
  drawn from that copy (posted to the worker), not fetched again.
- Still view-only: "edit this placement only" (needs every cell's placement
  count: SLICE B), rooms (SLICE B), DRC, and export (server-side: SLICE C).

**Found by the browser test.**
- **Chrome returns short bodies for concurrent range reads.** When the page
  and a worker read the same byte range of one blob at once, Chrome's HTTP
  cache sometimes returns a short body. A probe hit 2 and 6 short reads in
  599. With `cache: 'no-store'` there were 0. Node against the server and
  against the trunk proxy also gave 0 in 4,800. A short read is now detected
  and read again past the cache, in the worker and on the main thread.
- **Picks livelocked under a tight budget.** A pick's first chunk was evicted
  while its second was in flight, so the pick only ended at its 15 s timeout.
  Eviction now pauses while a pick is waiting.
- **Pre-existing (Phase 3): edits vanished after a view change.** The
  overlay (moved copies, drawn shapes) is culled to the view, but it was
  re-uploaded only on an edit. An edit made at one view did not show after
  zooming elsewhere, until the next edit. This happened in-core too. The
  overlay is now re-uploaded whenever the visible part differs.

**Correctness.**
- Core test: on a design with a remote 20 k-placement cell and a
  multi-chunk flat cell, a streamed pick that fetches exactly what it
  reports missing (repeatedly, as placements reveal cells) picks what
  in-core picks, with only part of the flat cell fetched.
- `stream_edit_test` (caravel, WebGPU) compares a streamed page against an
  in-core page throughout. The 8-view `stream_test` (0 px) and the two-peer
  `edit_sync_test` still pass.

**Measured** (`stream_edit_test.mjs`, caravel, WebGPU, headless):

| check | result |
|---|---|
| 128 picks over the 8 stream_test views | **128 of 128** pick the same shape (cell, index, vertex count) in-core and streamed |
| streamed pick latency | median **46 ms**, max 139 ms (fetching 143 shape chunks, 73.9 MB, and 7 placement chunks in all) |
| delete + vertex move + nudge on caravel_core | streamed = in-core, **0 of 688,000 pixels**; the edits changed 11,507 |
| undo ×3 | both back to the original, 0 px |
| 1 MB shape budget, 128 picks after the edits | 2,127 chunks evicted, 4 kept (5.2 MB, the edited ones); edited view still 0 px off |
| main heap | streamed 128 MB with 74 MB of shapes held; 79 MB under the 1 MB budget; in-core 361 MB |

The heap is a high-water mark: wasm memory never shrinks, and a pick
waiting for chunks holds everything fetched for it.

## 7.4 SLICE B — rooms and "edit this placement only"

**Rooms.** A streamed design now joins rooms like an in-core one. A peer's
edit fetches the chunks it touches (SLICE A), so a streamed peer converges
like any other.
- `?stream=<sha256>&room=R` joins with the streamed design.
- `?room=R&streamed` opens the room's own design out of core.

**Edit this placement only** needed two things a streamed client lacks:
- **Which hop to flatten.** It is the deepest hop whose parent is placed
  exactly once in the whole design, and counting placements needs all of
  them. Pack v5: the server's manifest carries every cell's placement count
  and its flattened subtree size (`subtree_shapes`, which equals the
  loader's flat count).
- **The subtree's geometry.** `flatten_needs` lists the placement's own
  chunk first (its child is unknown until then), then the subtree's shape
  and placement chunks. The flatten runs once they are all in memory. A
  subtree over the 50 k-shape limit is refused at once, from the manifest,
  before anything is fetched. Eviction pauses while a flatten waits, as for
  a pick.

**Correctness.**
- Core: a streamed flatten of placements in a remote 20 k-placement list,
  once its needs are fetched, copies exactly what in-core copies. Subtree
  sizes equal the design's flat count on two fixtures. The server pack test
  checks the manifest's counts.
- `edit_sync_test` gained **20b**: after the whole scenario (moves, deletes,
  adds, vertex edits, booleans, a shape drawn into a master, flatten and its
  undo), A and B draw the same pixels.
  - With `STREAMED=1`, B and the late joiner C open the fixture out of core
    while A stays in-core. All **33 checks pass**, and A and B differ in
    **0 of 619,200 pixels**.
  - In-core, all 32 pass.
- `stream_edit_test` on caravel: "edit this placement only" on a standard
  cell inside caravel_core, whose placements are remote. Streamed copies the
  same **53 shapes** into caravel_core as in-core, in **31 ms** including
  the fetches, and the views are identical.

## 7.4 SLICE C — server-side export

**Problem.** A streamed design is never wholly in the browser, so "Save
GDS" and "Export OASIS" could not write it.

**Design.**
- `POST /export/<sha256>?format=gds|oas&ids=<scheme>`, with the client's
  document state (a yrs v1 update) as the body. The server:
  - parses the stored design in full;
  - builds the overlay with `Session::overlay`, which moved from the app
    into the sync crate, so client and server share one builder;
  - writes the file with the same writers as an in-core save;
  - puts the output in the blob store.
- The client then downloads `/blobs/<hash>` directly, so a big export never
  passes through the page's memory.
- `POST /rooms/<name>/export` does the same with a room's bound design and
  state. The plan named only this route. The client-state route also covers
  a streamed design edited offline, with no room.
- Refusals:
  - 404: unknown design, or a room that is unknown or not bound.
  - 400: an unknown format, or a state that does not decode.
  - 409: another shape-id scheme. Its edits would land on other shapes.
- One export runs at a time, because each parses a whole design.

**Found: the same edits did not always write the same file.** Moved copies
were written in hash-map order, which differs from one map to the next
(Rust's per-instance random hashing). Two clients with identical edits
could save files with the same geometry in a different record order. The
first server test passed by luck and failed on the next run. Both writers
now emit moved copies in base-id order (`EditOverlay::live_shapes_in`), so
the same edits give the same bytes. A core test builds overlays in opposite
orders and gets identical GDS and OASIS.

**Correctness.**
- Server tests: an export with every kind of edit is byte-for-byte the
  in-core write (GDS and OASIS), and re-reads as exactly the composite. The
  edits are moves in TOP and in a master, a delete, a vertex edit, shapes
  drawn into TOP and into a master, and a hidden placement. A room's export
  is the same file. All the refusals are covered.
- `edit_sync_test` gained **20c**: after the whole scenario, both peers
  "Save GDS", and the downloads must be the same file. With `STREAMED=1`,
  B's file is written by the server: same SHA-256 (128 ms round trip). The
  STREAMED run passes all 34 checks; the in-core run passes all 33.
- **On caravel** (`stream_edit_test`): after the three edits, the in-core
  page saves the file itself and the streamed page saves through the
  server. Both downloads are the **same 276.3 MB file**.
  - **KLayout as the oracle**: KLayout's reading of the server's export
    matches ours on all 1,014 cells, 3,925,012 polygons (the original
    3,925,013 minus the delete) and 462,523 placements.
- **A second nondeterminism, found on caravel**: the first comparison
  failed in **582 ANGLE records**, all −90° ± 1 ulp. The writers recovered
  angles with `atan2`, and wasm's math library and the native one disagree
  in the last bit. `Transform::strans_parts` now gives both writers one
  canonical form: the angle in [0°, 360°), exact for multiples of 90°,
  other angles and magnifications rounded. The bytes now match. A unit test
  feeds it ulp noise of both signs around each quadrant. (The reader's
  sin/cos keeps its ulp noise in the matrices; drawing and picking are
  unaffected, as the pixel and pick checks show.)

**Measured** (caravel, WebGPU, headless):

| save the edited design | in-core page | streamed page (server writes) |
|---|--:|--:|
| wall time to the download | 1.15 s | 3.7 s (parse 291 MB GDS + write on the server) |
| page heap afterwards | **860 MB** (from ~380: the 276 MB file is built in wasm) | **128 MB** (the file never enters the page) |

## 7.4 — acceptance

- **Two-peer scenario, one peer streamed, pixel-identical: met.**
  `edit_sync_test` STREAMED=1 passes all 34 checks: the original 22 steps
  plus 20b (0 of 619,200 pixels differ) and 20c (same file). B and the late
  joiner C stream; A is in-core.
- **Server export ≡ in-core export: met, byte for byte.** This is stronger
  than the plan's normalized digest. It holds on the fixture and on edited
  caravel (276 MB), and the KLayout oracle agrees.
- **Memory bounded while editing: met.** Main memory holds the shape chunks
  picks and edits fetched, up to a budget (128 MB by default). Edited and
  selected chunks are pinned.
  - caravel after 128 picks and the edits: 128 MB heap with 74 MB of shapes
    held (in-core: 360–380 MB, 860 MB after a save).
  - Under a 1 MB budget: 2,126 chunks evicted, 4 pinned (5.2 MB), heap
    78 MB.
- Not in 7.4, as the plan said:
  - DRC on streamed designs moves to the server with 7.5.
  - Rooms, streamed or not, stay open to anyone until 7.2.
- Follow-ups it measured:
  - A pick fetches whole chunks under the point: 74 MB for 128 caravel
    picks. A server-side pick would send only the answer.
  - An edited chunk of a streamed cell cannot be split finer. A shape in a
    100 k-shape single-chunk cell re-tessellates that whole chunk once per
    suppression change (in-core splits it to 3 k).

## 7.1a — stop repainting when idle

**Problem.** The app asked for a repaint at the end of every frame, so an
untouched tab drew at the display rate and kept the main thread busy.
Measured in headless Chromium at 120 Hz: the embedded sample drew 120
frames/s (22% main thread), caravel in-core 30 frames/s with the main thread
at 100% (each frame redoes the 17.5 ms fit-view cull).

**Design.**
- Draw every frame only while work is in progress or for 0.5 s after
  anything happened. Work in progress means a load, tessellation or chunk
  fetches in flight, chunks left by the per-frame limits, a pick or flatten
  waiting, an edit's redraw, DRC, or a socket connecting. Anything that
  happened means input, a harness command, or a result arriving. Otherwise a
  250 ms heartbeat (4 frames/s) covers retry timers and reconnect backoff.
- Async results wake the UI themselves: `webio::wake()` runs at every
  completion (worker replies, fetches, uploads, exports, room lookups). The
  socket uses ewebsock's `connect_with_wakeup`.
- The harness is unchanged. `window.__fluideda_cmd` became a property whose
  setter wakes the app, so a test command never waits for a heartbeat.
- `?continuous` restores drawing every frame (the fps benchmarks need it). It
  is also the same-build A/B switch. The perf JSON now names what keeps the
  app busy (`busy`).

**Measured** (`idle_bench.mjs`, WebGPU, headless, same build: `?continuous` →
default):

| | `?continuous` | default |
|---|--:|--:|
| idle, embedded sample | 120 frames/s, main thread 22% | **4.2 frames/s, 2.4%** |
| idle, caravel in-core (fit) | 29.6 frames/s, 100% | **3.8 frames/s, 17%** |
| idle, caravel streamed | 120 frames/s, 44.5% | **4.2 frames/s, 5.1%** |
| frames/s while panning (sample · in-core · streamed) | 120.8 · 31.6 · 120.2 | 120.5 · 31.6 · 120.2 |
| caravel first frame, in-core · streamed (median of 3) | 1,626 · 75 ms | 1,630 · 75 ms |
| a peer's edit showing in an idle page (median of 5) | 19 ms | 20 ms |

caravel in-core still spends 17% while idle: each heartbeat frame redoes the
fit-view cull. The fit-view slice (7.6) cuts that cost. Every browser suite
passes in the default mode: the stream test, the streamed edit test, the sync
test (in-core and STREAMED), and the persistence test. In the persistence
test, a dropped peer reconnects on its own 490 ms after a server restart,
driven by the heartbeat.

## 7.7 — compact rectangles

**Problem.** 99.8% of caravel's 3,925,012 polygons are axis-aligned
rectangles, each stored as a closed 5-point ring: 40 B of coordinates plus
8 B per shape. The shape stores were 209 MB of caravel in-core, the pack's
geometry 189 MB, and every rectangle went through lyon's general polygon
tessellator.

**Design.**
- A rectangle is stored as its two extreme corners (16 B) plus a kind byte.
  The kind records how the file drew it: start corner, winding, and whether
  the first point repeats (16 variants). `ShapeStore::points` returns a
  `Ring` that derefs to exactly the original points. A ring that would not
  re-expand bit for bit stays a ring, so exports, vertex numbering and picks
  are unchanged.
- The kind column travels with the others: in the worker-to-main transfer
  (serialize v5), in pack records (v6: kinds after layers, padded to 4
  bytes), and in the tessellation worker's columns.
- Tessellation turns a rectangle into two triangles directly, without lyon.
  It uses the same four corners, and whichever diagonal splits them, each
  pixel inside is covered once. The 4 golden images and every pixel-identity
  check are unchanged.
- `?norects` keeps every shape a ring: the same-build A/B switch (the parse
  worker gets it too).
- The GPU side is unchanged: a rectangle is still 4 vertices and 6 indices
  there. Drawing rectangles as instances would need storage buffers in the
  vertex stage (WebGPU only); left for later.

**Measured** (same build, rings → compact):

| | rings | compact |
|---|--:|--:|
| caravel shape stores (native) | 209.4 MB | **104.1 MB** |
| tessellating every chunk (native) | 3,051 ms | **88 ms** (same 7,895,663 triangles) |
| parse (native) | 661 ms | 664 ms |
| GDS export (native) | — | **byte-identical**; every ring identical |
| parse worker peak (browser, in-core caravel) | 1,080 MB | **597 MB** |
| all geometry in | 2,376 ms | **1,785 ms** |
| first frame | 2,031 ms | **1,723 ms** |
| main heap after load (high-water) | 361 MB | **305 MB** |
| fit-view tessellation on the main thread | 131 ms | **14 ms** |
| caravel pack geometry (v5 → v6) | 188.8 MB | **98.7 MB** (pack build 2.5 → 1.8 s) |
| streamed: 128 picks fetch · heap after (vs 7.4) | 73.9 MB · 128 MB | **38.6 MB · 83 MB** |

The browser rows are medians of 3 alternating runs (`rect_ab.mjs`, WebGPU);
parse time in the browser was 1,160 vs 1,224 ms with overlapping runs. The
native rows come from `rect_bench`. Correctness:
- 107 core tests. New: every rectangle variant round-trips, and
  near-rectangles stay rings.
- 17 server tests, and the goldens unchanged.
- `stream_test`: 0 px at 8 views.
- `stream_edit_test`: 128/128 picks, edits, flatten, and the in-core save =
  the server export.
- `edit_sync_test`: 33 checks in-core and 34 streamed.

## 7.8 — compact placements

**Problem.** All 462,523 of caravel's placements are plain 90° rotations at
scale 1 with integer origins. Each still carried a full `f64` transform and
the array fields: about 104 bytes in memory (48 MB for caravel; 5.2 GB for
the 50 M-placement design, which could never load in-core), and 53 bytes
encoded (+40 for an array) in the worker transfer and in pack records.

**Design.**
- `Instance` stores a placement whose transform is exactly what
  `Transform::from_strans` builds for an integer origin, a multiple of 90°
  and unit magnification as `x, y` and one of 8 orientations. It re-expands
  through a table built by that same function, so `transform()` gives back
  the very bits the reader produced (checked bit for bit; a transform that
  differs even in a `-0.0` stays general). Other transforms and array
  lattices are boxed. 20 bytes on wasm32 (a compile-time check), 32 on
  64-bit hosts.
- The fields became accessors (`transform()`, `kind()`; `Instance::new`).
  Placements are no longer `Copy`, so the cull borrows them, and the
  in-place sort became swap-based.
- Encoding (serialize v6, PACK_VERSION 7): `[child varint][flags]`, then a
  plain placement's origin as zigzag-varint deltas from the previous plain
  placement of the list. Lists are spatially sorted, so the deltas are
  short. Other transforms are written in full (6 f64), an array adds its
  lattice.

**Measured.**

| | before (7.7) | compact (7.8) |
|---|--:|--:|
| caravel placements in memory (wasm32) | 48.1 MB | **9.3 MB** (all 462,523 compact) |
| place_50M placements in memory (wasm32) | 5,200 MB | **1,000 MB** |
| caravel placements encoded | 24.5 MB | **3.1 MB** (6.8 B each) |
| place_50M placements encoded | 2,650 MB | **251 MB** (5.0 B each) |
| caravel pack: placements · cell table | 24.2 MB · 467 KB | **3.1 MB · 195 KB** |
| place_50M pack: placements · build | 2.65 GB · 39 s | **251 MB · 6.1 s** |
| in-core caravel main heap (browser, median of 5) | 305 MB | **266 MB** |
| in-core caravel parse worker peak | 597 MB | **548 MB** |
| parse · first frame · all geometry in | 634 · 919 · 926 ms | 624 · 906 · 913 ms |

- The memory and encoding rows come from `place_bench` (native, exact;
  every transform round-trips bit for bit).
- The browser rows are a same-session A/B between the two builds
  (`load_ab.mjs`): the 7.7 commit is served from a worktree on :8081, and
  loads alternate between the two.
- place_50M in-core still fails in both builds, with different errors. 7.7
  hits a wasm trap; 7.8 panics with "capacity overflow" instead. That error
  fits the in-core cull, which builds one proxy per sub-pixel placement at
  fit: 50 M boxes overflow wasm32's 2 GB array limit. 7.6 adds the in-core
  density rule that removes this; whether place_50M then loads is checked
  there.

Correctness:
- 109 core tests. New: every orientation is compact and exact, the general
  fallbacks hold, and mixed lists encode and decode bit for bit.
- 17 server tests and the goldens are unchanged.
- The browser suites pass: the stream test (its placement-eviction check now
  uses a 0.25 MB budget; 1 MB no longer forces evictions), the streamed edit
  test, and the sync test, in-core and STREAMED.

## 7.6 — the fit view

**Problem.** Zoomed out, an in-core design drew everything it had. At
caravel's fit, the cull produced 391,850 proxies (one per sub-pixel
placement) and 2,160 chunk draws. Every chunk with geometry on screen was
tessellated and uploaded, so 202 MB sat on the GPU, nearly all of it
triangles smaller than a pixel. The fit view ran at 76 frames/s on WebGPU
and 66 on WebGL, against 120 at mid and leaf. Phase 5c measured Dawn 14%
behind ANGLE there. Each idle heartbeat frame redid the 8 ms cull (13.6% of
the main thread). On the 50 M-placement design, the per-placement proxies
were fatal: 50 M boxes overflow wasm32's 2 GB array limit, so the in-core
load panicked ("capacity overflow").

Streamed designs never had this problem. Since Phase 6 they fetch a chunk
only once each of its shapes gets a pixel, and since 7.3 a dense placement
chunk is one proxy.

**Design.** Streaming's density rule now runs in the cull, for every design.
- Shape chunks: each draw carries `dense`. It is set when the chunk's
  footprint, at its placement's scale, gives its shapes fewer than
  `DENSE_PX_PER_SHAPE` (1) pixels each. A dense chunk draws as its proxy,
  in its look's colour, even when its mesh is already loaded from a closer
  zoom. The app neither tessellates nor uploads it. Streaming now reads the
  same flag instead of keeping its own copy of the rule.
- In-core, only chunks of at least `DENSE_MIN_SHAPES` (1,024) shapes are
  ruled dense. The rule exists to remove big chunks' thousands of sub-pixel
  triangles. A small chunk, such as a standard cell's few dozen shapes,
  costs next to nothing and keeps its exact look. Without this floor the
  sky130 fit golden moved 4.18%; with it, every golden is unchanged. A
  streamed cell keeps the rule at every size, because there it decides what
  is fetched.
- Placement chunks: a chunk whose placements would average under the LOD
  size (`min_pixel`, 4 px) is one proxy box, as a streamed cell's has been
  since 7.3. At caravel's fit (native `cull_bench`, 1,280 px), 129 of its
  561 placement chunks become one box each. The cull emits 86,094 draws and
  proxies where the full walk emits 585,515.
- The parse worker and the native loader now annotate chunk looks, as the
  pack does, so in-core proxies get the same colours as streamed ones.
- The cull equivalence test now covers two cases, and asserts that both
  occur:
  - where no placement chunk is dense, the output is exactly the
    per-placement walk's;
  - where one is, every draw is also one the full walk makes.

  `cull_bench` follows the same rule.
- `?webgl` keeps wgpu on WebGL even where the browser offers WebGPU. Headless
  Chromium now offers WebGPU by default, so without the flag a "WebGL" run
  was really WebGPU. `UNCAP=1` in the harness lifts the display-rate cap.

**Measured.** Same-session A/B, in-core caravel: the 7.8 commit served from a
worktree on :8081 against this build (`fit_fps.mjs` with `?continuous`,
`fit_ab.mjs`, `load_ab.mjs`):

| | before (7.8) | dense rule (7.6) |
|---|--:|--:|
| fit, frames/s: WebGPU · WebGL (display cap 120) | 76.4 · 65.8 | **120.1 · 120.1** |
| fit, frames/s uncapped: WebGPU · WebGL | 73.0 · 76.0 | **742 · 227** |
| fit: cull per frame | 8.4 ms | **1.4 ms** |
| fit: draws · proxies | 2,160 · 391,850 | **704 · 32,053** |
| fit: chunks uploaded · GPU memory | 222 · 202 MB | **84 · 14 MB** |
| fit: main thread while idle | 13.6% | **3.8%** |
| mid uncapped, frames/s: WebGPU · WebGL | 3,657 · 998 | 3,643 · 1,001 (unchanged) |
| place_50M in-core | fails ("capacity overflow") | **opens**: first frame 10.0 s, heap 1,258 MB |

Mid and leaf at the display cap are unchanged too (120 frames/s, 29 draws,
1 MB GPU on both backends).

The fit view is no longer GPU-bound. WebGPU now leads WebGL by 3.3× there
(742 vs 227 uncapped), where Phase 5c had it 14% behind. Before this slice
the uncapped gap had already narrowed to 4% (73 vs 76).

Fidelity at fit: 14.2% of pixels differ from the 7.8 view, but only 0.01%
by more than 24/255 in any channel (mean absolute difference 0.21). At mid
the views are identical. A dense chunk's coloured proxy averages to what
its sub-pixel triangles blended to.

place_50M now loads in-core, but its parse worker peaks at 3,986 MB, just
under wasm32's 4 GB. The worker holds the 1.3 GB file and the whole
database at once; 7.9 removes those peaks.

Correctness:
- 109 core tests (the cull equivalence and pack tests cover the new
  semantics), 5 render tests with the goldens unchanged, 17 server tests.
- The browser suites pass on this build: the stream test (11 checks), the
  streamed edit test (11), and the sync test, in-core (33) and STREAMED (34).

## 7.9 — no permanent peaks

**Problem.** wasm memory only grows, so whatever a page allocates at its peak
stays allocated for the life of the page. Two operations put whole files in
wasm memory:
- **Saving in-core.** The writer built the file in a `Vec`, which was then
  copied into a Blob. Saving caravel as GDS grew the page's heap from 122
  to 689 MB for good. place_50M could not be saved at all: no file arrived
  in 15 minutes, and its 1.3 GB file does not fit beside a 1.26 GB heap.
- **Parsing in the worker.** The file was copied into wasm memory before
  parsing, so the worker's peak carried it: 278 MB for caravel and 1.3 GB
  for place_50M. place_50M's worker peaked at 3,986 MB, just under wasm32's
  4 GB.

The server had the same pattern on a smaller scale. An export read the whole
design and built the whole output before storing it, and a pack build read
the whole design.

**Design.**
- **Writers stream.** `write_gds_to` and `write_oasis_to` write into any
  `io::Write`; `write_gds` and `write_oasis` wrap them.
  - GDS goes out in 1 MiB pieces.
  - OASIS writes a cell's records as CBLOCKs of about 1 MiB each, so a big
    cell spans several. Modal state carries across CBLOCKs, since their
    contents read as if written in place. Our reader and KLayout both read
    such files.
- **The page saves straight into the download.** `webio::save_with` hands
  the writer a `BlobParts` sink: every write becomes a JS buffer of its own,
  and the Blob is assembled from those parts. Nothing of the file stays in
  wasm memory.
- **The GDS reader reads through a `ByteSource`.** A slice is read in place,
  as before. Anything else is read through a 1 MiB window copied out a range
  at a time.
  - The parse worker passes its JS `Uint8Array` (`JsBytes`), so the file
    never enters wasm memory. OASIS (compressed and small) is still read
    whole.
  - The server reads stored blobs from disk the same way (`BlobFile`), for
    exports and pack builds. An export is written to a temp file and hashed
    on the way.
- **The reader is generic over its window.** Reading a slice therefore
  compiles to the same code as before. The first version went through a
  `dyn` source and parsed caravel 25% slower natively (316 → 396 ms).
  Making it generic and forcing `next()` inline brought it to 300 ms.

**Measured.** Same-session A/B: the 7.6 commit served from a worktree on
:8081 against this build (`save_ab.mjs`, `load_ab.mjs`, WebGPU). The server
rows come from `export_peak` under `/usr/bin/time -l`.

| | before (7.6) | streamed (7.9) |
|---|--:|--:|
| caravel in-core: heap after load → after "Save GDS" | 122 → 689 MB | **122 → 122 MB** |
| caravel in-core: heap after "Save OASIS" too | 689 MB | **122 MB** (126 MB more with one CBLOCK per cell) |
| caravel load: parse worker peak (wasm) | 548 MB | **364 MB** |
| caravel load: parse · first frame · all geometry | 723 · 976 · 1,000 ms | **662 · 909 · 932 ms** |
| place_50M load: parse worker peak (wasm) | 3,986 MB | **2,687 MB** |
| place_50M load: parse · first frame | 7.5 · 10.6 s | **6.7 · 9.2 s** |
| place_50M in-core "Save GDS" (heap 1,258 MB) | no file in 15 min | **6.6 s, heap unchanged** |
| server export, caravel GDS: peak RSS · time | 799 MB · 1.87 s | **249 MB · 1.74 s** |
| server export, caravel OASIS: peak RSS · time | 592 MB · 1.61 s | **251 MB · 1.54 s** |
| server export, place_50M GDS: peak RSS · time | 4,148 MB · 11.2 s | **2,816 MB · 10.2 s** |
| native caravel parse (`bench`) | 317 ms | **300 ms** |

The worker peak counts wasm memory. The file itself still sits in a JS
buffer until the worker exits, but outside the 4 GB wasm limit.

The written files:
- GDS is byte for byte what 7.6 wrote (caravel `be1b67f3…`, place_50M
  `88bb0a08…`), from the page and from the server alike.
- OASIS changes, because big cells now span several CBLOCKs: caravel
  16,506,858 → 16,515,607 bytes (+0.05%). KLayout reads it exactly like the
  GDS: 1,014 cells, 3,925,013 polygons and 462,523 placements identical
  (`scripts/oracle`). The in-core page's OASIS save is the same file as
  the server's export (`c9ef35b0…`).

Correctness:
- 111 core tests. New: reading through a 64-byte window builds exactly what
  reading in place does, and a cell over several CBLOCKs (300,000 shapes on
  changing layers, 20,000 placements in all 8 orientations) reads back the
  same.
- 17 server tests (export and packs now read blobs through a window).
- The browser suites pass: stream 11, streamed edit 11 (including the
  check that the in-core save equals the server's export byte for byte),
  sync 33, sync STREAMED 34.

The machine went into system sleep about every 16 minutes during these
runs, despite keep-awake: about 15 minutes asleep, then a short wake. Three
measurements that crossed a sleep are left out of the table: one load's
wall time (1,051 s), one OASIS save (962 s), and one streamed edit run, which
timed out waiting for a frame and passed when re-run.
