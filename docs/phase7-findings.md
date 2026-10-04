# Phase 7: findings

**Status: in progress on `phase7`.** The owner moved 7.3 (stream the
hierarchy) to the front. It runs as three slices: **A** placement chunks in
the core, **B** streaming the hierarchy (pack v3), **C** coloured proxies.

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
