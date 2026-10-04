# Phase 7: findings

**Status: in progress on `phase7`.** The owner moved 7.3 (stream the
hierarchy) to the front. It runs as three slices: **A** placement chunks in
the core (done), **B** streaming the hierarchy, pack v3 (done), **C**
coloured proxies.

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
