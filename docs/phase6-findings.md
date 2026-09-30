# Phase 6 — formats, backend, out-of-core: findings

**Status: shipped (3 slices on `phase6-backend-oasis`)** — SLICE 1: OASIS +
paths; SLICE 2: persistent rooms on a content-addressed store; SLICE 3:
out-of-core streaming.

**TL;DR**

| | before | after |
|---|---|---|
| OASIS | not readable | read + written; KLayout-verified on caravel (3,925,013 polygons identical both ways) |
| GDS PATH elements | silently dropped (1,334 on caravel + std cells) | converted to outlines |
| a peer joining after edits were made | saw nothing done before it connected | receives the whole room state first; converges |
| server restart | the room was forgotten | crash: WAL replay; graceful: snapshot |
| a peer whose design differs | applied shape-id edits to the wrong geometry | refused before any state is exchanged |
| edits made before joining (web) | lost — pushed while the socket was still connecting | merged when the socket opens |
| getting the design to a collaborator | they need the file | `?room=R` fetches it by hash from an immutable blob store |
| caravel room link → design + edits, late joiner | — | **1.41 s** (first frame 998 ms; WebGPU, headless) |
| a 2 GB, 32 M-polygon design in the browser | fails (parse worker panics: wasm32 allocation limit) | **opens streamed**: first frame 45 ms, main heap 8 MB |
| caravel, first frame | 873 ms (in-core) | **79 ms** streamed (pixel-identical wherever chunks are fetched) |

## Method

Native: `cargo test -p fluideda-server` (unit + `tests/rooms.rs`: a real
server on an ephemeral port, real `fluideda_sync::Session`s over WebSockets,
a real store directory; a server is "crashed" by dropping its runtime) and
`cargo run --release -p fluideda-server --example room_scale`. Browser:
`node scripts/edit_sync_test.mjs` (checks 21–22), `node scripts/persist_test.mjs`
(runs its own server, SIGKILLs and restarts it), `node scripts/hash_bench.mjs`
(`?nohash` A/B), `GPU=webgpu node scripts/stream_test.mjs` (streamed vs
in-core pixels; residency) and `GPU=webgpu node scripts/outofcore_bench.mjs`. Headless Chromium; GPU mode per `scripts/launch.mjs`. The
machine carried ~1.7 cores of other apps' load throughout, so every A/B
alternates its two variants in one session.

## SLICE 1 — OASIS in and out, GDS paths

- **Reader** (`crates/core/oasis.rs`): SEMI P39 varints and reals, every
  element kind (RECTANGLE, POLYGON, PATH, TRAPEZOID, all 26 CTRAPEZOID
  templates, CIRCLE as a 64-gon), every point-list and repetition type, modal
  variables, deflate CBLOCKs (`miniz_oxide`), cells referenced by number before
  their CELLNAME. Regular placement repetitions become AREFs; shape repetitions
  expand.
- **Writer** (`oasis_write.rs`): the GDS export's composite semantics; RECTANGLE
  or g-delta POLYGON, modal layers, lattice repetitions, a CBLOCK per cell.
- **Paths** (`path.rs`): both GDS readers converted nothing — 480 PATHs in
  caravel and 854 in the sky130 standard cells were missing. Now outlines
  (miter joins; flush / half-width / explicit extensions). The sky130 goldens
  were re-baselined. This changed shape numbering in cells with paths, which is
  why rooms now record a shape-id scheme (`SHAPE_ID_SCHEME` = 2, below).
- **KLayout as an oracle** (`scripts/oracle/`): normalized per-cell digests. On
  caravel all three agree cell for cell — 1,014 cells, 3,925,013 polygons,
  462,523 placements: our GDS read vs KLayout's; our read of KLayout's OASIS vs
  KLayout's; KLayout's read of *our* OASIS vs our GDS read.
- caravel: 291 MB GDS → 16.6 MB OASIS with our writer (KLayout's: 10.6 MB).
  Full load on WebGPU: caravel.oas 0.80 s vs caravel.gds 0.93–0.96 s (this
  slice's `hash_bench` runs; SLICE 1 measured 0.88 vs 1.05 s).

## SLICE 2 — the server keeps the room

Phase 0's relay forwarded opaque bytes and remembered nothing: a peer saw only
updates sent while it was connected, and a restart emptied every room. STATUS
carried it as a debt ("a peer must join before others edit").

### Design

**One authoritative document per room** (`server/src/room.rs`). The wire
format is unchanged (byte 0: `0x01` a yrs update, `0x02` presence). The server
now decodes every document update — dropping any that do not decode — applies
it to the room's `yrs::Doc`, appends it to the room's WAL, then relays it.
A joining peer's first message is the whole room state
(`encode_state_as_update_v1` from an empty state vector). It is encoded under
the same lock the subscription is taken under, so no update can fall between
the two. A peer that falls more than the broadcast buffer behind is resynced
with the whole state instead of dropped.

**Rooms are bound to a design.** Every CRDT edit is keyed by base-shape id,
which is only meaningful for one file parsed one way. The join URL names the
design: `design=<sha256 of the file bytes>&ids=<SHAPE_ID_SCHEME>&name=<file>`.
The first peer with a design binds an unbound room: the server writes `design`,
`ids` and `design_name` into the document's `meta` map, where every peer can
read them. A peer with a different design is refused **before any state is
exchanged**: a JSON text frame, then close code 4409. So the room never merges
its edits and it never applies the room's. If a client update rewrites `meta`,
the server writes the binding back; its write is causally later, so it wins.
The app turns a refusal into a button, "Open the room's design".

**Content-addressed storage** (`server/src/store.rs`): immutable blobs named by
their SHA-256, plus a small per-room log.

```text
blobs/<sha256>      designs and room snapshots — never change, so cache forever
rooms/<room>/head   current snapshot (fsynced temp file + rename)
rooms/<room>/log    "<seq> <unix ms> <sha256> <bytes>" per snapshot: the history
rooms/<room>/wal    [u32 len][update] per update since the head
```

- `GET|HEAD /blobs/<h>`: `Cache-Control: public, max-age=31536000, immutable`,
  `ETag` = the hash (a revalidation is a 304), single byte ranges.
- `PUT /blobs/<h>`: streamed to disk while hashing; refused unless the bytes
  hash to the name.
- `GET /rooms/<name>`: binding, head, versions and peers; a lookup never
  creates a room.
- Room names are 1–64 of `[A-Za-z0-9_-]`, because they become directory names.

**Persistence = snapshot + WAL.**

- Every update is appended to the room's WAL before it is relayed. The WAL is
  not fsynced: it survives a process crash (the kernel holds it), not a power
  cut.
- When a room goes quiet (2 s idle), or after 30 s dirty, or once the WAL
  passes 1 MiB, its state is written as a snapshot blob and becomes the new
  head. The WAL records it covers are then trimmed.
- Restore is the head snapshot plus WAL replay. Updates are idempotent, so
  replaying records the snapshot already holds is harmless; that is what makes
  every crash point between the steps safe.
- The version log is the room's history at "editing burst" granularity, and
  every version is itself a fetchable blob.

**The client** (`crates/sync`, `crates/app`):

- The parse worker hashes the file.
- `?room=R` joins R once the design is in. Without `?file=`, it first asks
  `GET /rooms/R` and loads the room's design from `/blobs/<hash>`.
- After a join is accepted, the app `HEAD`s its design's blob and `PUT`s it if
  missing. It reads the bytes again from their source: a local `File` handle is
  disk-backed and costs nothing to keep; a URL is fetched again. So the main
  thread never holds the bytes.
- Local files now go to the parse worker as `File` handles too, instead of
  being read into wasm memory on the main thread first.
- A dropped connection reconnects with backoff (0.5 s doubling to 8 s), not
  after a refusal.

### A latent bug the new server exposed

`Session::join` pushed the client's whole state immediately after
`ewebsock::connect`. On the web that send happens while the socket is still
CONNECTING, and ewebsock logs "Failed to send" and drops it. So **edits made
before joining never reached anyone** on the web. It went unseen because every
test joined before editing. The session now pushes its whole state when the
socket *opens*, and sends nothing before. That also covers every edit made
while a dropped connection was down. Native test:
`edits_made_before_joining_reach_the_room_when_the_socket_opens`; browser:
persist_test's offline edit merges after the restart.

### Hashing the design costs the load nothing — the second time

| caravel.gds (291 MB), hash vs `?nohash`, medians | GPU mode, runs | first frame | all geometry |
|---|---|--:|--:|
| hash *before* the parse, overlapping it | SwiftShader, 4 alternating | 1143 vs 1014 ms (**+129**) | frame-paced (below) |
| hash *after* the parse reply | SwiftShader, 4 alternating | 1002 vs 1023 ms | frame-paced (below) |
| hash *after* the parse reply | WebGPU, 6 alternating | 873 vs 875 ms | 929 vs 956 ms |

The first version started `crypto.subtle.digest` before the wasm parse so the
two would overlap. The digest itself was fully hidden (the worker then waited
0.1 ms for it). But WebCrypto copies its input synchronously, and copying 291
MB put ~130 ms on the path to the first frame. The worker now replies with the
parse first, then hashes: 116 ms off the critical path (2.5 GB/s), delivered in
a second message, after which the worker closes. Nothing needs the hash until a
join, and a pending join waits for it. caravel.oas (10.6 MB): 5 ms either way,
no measurable difference.

Why the SwiftShader rows have no geometry number: under headless SwiftShader the
progressive load needs only 7 frames, but each takes ~1.2 s to present
(main-thread work ≤ 60 ms per frame). So `geometry_ms` there is ~8.8 s of frame
pacing, not load work. On WebGPU the same load finishes in ~1.0 s, matching
SLICE 1. Load comparisons belong on `GPU=webgpu`.

### Room scale (`examples/room_scale.rs`, native release)

Rooms of N edits in the sync crate's formats (70 % moves, 20 % deletes, 10 %
drawn rectangles), one transaction each as clients send them:

| edits | room state | encode (under the room lock) | write snapshot (fsyncs) | restore from snapshot | restore from WAL only |
|--:|--:|--:|--:|--:|--:|
| 1,000 | 32 KB | < 0.1 ms | 28 ms | 0.8 ms | 1.4 ms |
| 10,000 | 326 KB | 0.3 ms | 16 ms | 3.3 ms | 6.7 ms |
| 100,000 | 3.4 MB | 2.3 ms | 25 ms | 23 ms | 48 ms |

About 33 bytes of state per edit. Rooms hold edits, not geometry, so they stay
small. The snapshot's cost is fsync-bound (macOS). The first version held the
room lock across it, stalling every update in the room for 16–28 ms. Now the
lock covers only the encode and, afterwards, a trim of the WAL records the
snapshot covers; updates that land while the blob is written stay in the WAL
and keep the room dirty. Narrowing the lock opened one gap, which
persist_test caught: `GET /rooms` could list a version whose head was not
yet set. The lookup now waits out a snapshot in progress. Also fixed on the
way: a crash that tore the WAL's
last record left later appends stranded behind the tear. Replay stopped there
and would have lost them. Restore now cuts the tear.

### End to end

- **persist_test** (browser, the server run by the script):
  - edits live only in the WAL (312 B, no snapshot yet);
  - the server is SIGKILLed; the page notices ("connection closed") and makes
    an offline edit;
  - the server restarts and replays 6 WAL records; the page reconnects by
    itself (first attempt, **475–584 ms** after the restart) and its offline
    edit merges;
  - a late joiner matches exactly;
  - once quiet: snapshot v1 (285 B), immutable blob, 304 on revalidation, WAL
    truncated;
  - after SIGTERM and a restart, the snapshot alone restores the room.
- **edit_sync_test 21–22**:
  - a late joiner opening only `?room=` loads the fixture from `/blobs` and
    matches A on every edit count (moves, deletes, adds, vertex records,
    hidden placements, adds in masters), then edits like any peer;
  - a peer with the embedded hier.gds is refused ("room holds another design:
    edit_fixture.gds"), changes nothing, then opens the room's design from the
    store and converges.
- **caravel** (WebGPU): a 291 MB `PUT` through Trunk's proxy takes 1.12 s and
  the `GET` back 0.14 s (hash identical; a 1,000-byte range is a 206). A room
  link opened cold loads caravel from the store and the room's edits in
  **1.41 s** (first frame 998 ms, all geometry 1148 ms, hash 180 ms).

### Harness finding: three rendering pages starve SwiftShader

The first late-joiner run timed out with the third page's app never starting:
its HTML and wasm arrived in 20 ms, then nothing — not even the GPU adapter
log. With two other pages rendering every frame on Chromium's software GL, the
third page's WebGL context creation starved. Reproduced standalone:
- two peers open, third page stalls;
- close one peer, the third starts in 157 ms;
- on the real GPU (`webgl-metal`), 118 ms with three open.

The browser tests now keep at most two live pages. The underlying product
behaviour is worth a line in the debts: **the app requests a repaint every
frame even when idle**, so an open, untouched tab keeps a core busy.

### Limits (recorded in STATUS debts)

- **No auth.** Anyone who can reach the server can join any room and `PUT`
  blobs up to 16 GiB. It binds 127.0.0.1 by default (it was 0.0.0.0);
  `FLUIDEDA_ADDR` widens it.
- **No blob GC.** Every version stays, and rooms stay in memory once touched.
- **The join exchanges whole states both ways.** There is no state-vector
  handshake (y-sync step 1/2). Fine at ~33 B/edit; the first thing to change if
  rooms grow to MBs.
- **A design is its exact file.** The same layout exported as OASIS is another
  room's design: its shape numbering can differ, so this is correct, not a
  shortcut.

## SLICE 3 — out of core

In-core loading moves a design's whole geometry into the browser before any
of it is used, so the browser's memory caps the design size. On wasm32 that
cap is hard: a 4 GB address space and 2 GB per allocation. Phase 2a measured
it at ~11 M flat polygons; the streaming parser has since raised it (flat_11M
now loads, with a 2.2 GB worker peak). A 2 GB, 32 M-polygon flat design still
cannot load: the parse worker panics with `capacity overflow` after 5 s.

### Design

**Packs** (`crates/core/src/pack.rs`). A parsed design becomes three blobs in
the SLICE 2 store:

- the **skeleton** — the existing metadata blob: hierarchy, chunk tables and
  bboxes, no geometry;
- the **geometry** — one self-contained record per chunk:
  `[n][nc][starts][layers][coords]`, 4-byte aligned, so a fetched range is used
  in place through typed-array views;
- an **index** of byte offsets.

A record holds exactly the columns `ShapeStore::chunk_columns` gives in-core,
so a streamed chunk tessellates identically (tested per chunk on hier, the
fixture and the sky130 library).

**The server builds them** (`server/src/packs.rs`). `GET /packs/<design>`
parses the stored design natively on first request (no wasm ceiling), writes
the three blobs — geometry streamed through a hashing writer, never whole in
memory — and caches a manifest. Everything a client then fetches is an
immutable blob, so a CDN can cache it, byte ranges included.

**The client streams** (`?stream=<sha256>`):

- It fetches the manifest, skeleton and index, and installs the skeleton
  through the Phase 5c path unchanged. The first frame shows every chunk as a
  proxy.
- Each frame, for each chunk on screen it asks the tessellation worker to
  fetch that chunk's byte range and tessellate it. The geometry never touches
  the main thread.
- **Density rule:** a chunk is fetched only once its screen footprint gives
  each of its shapes at least one pixel. Denser chunks stay proxies. So
  resident geometry is bounded by the *screen* — about one shape per pixel —
  whatever the design's size.
- **Residency:** over a GPU budget (384 MB; `?stream_budget=`), the least
  recently drawn chunks are dropped. What is on screen is never dropped. A
  revisited view is fetched again, usually from the HTTP cache (immutable).
- Failed fetches are reported back and retried after 2 s.
- A streamed design is **view-only**. Picking, editing, DRC, export and rooms
  are all off: they need base geometry, which the cells do not hold.

### Measurements (`outofcore_bench.mjs`, WebGPU, headless; blobs served by the local server)

| design | GDS | in-core: first frame · all geometry · worker parse peak · main heap | streamed: first frame · fetched at fit · at leaf · main heap |
|---|--:|---|---|
| caravel (3.9 M polygons, 460 k placements) | 291 MB | 873 ms · 994 ms · 1,081 MB · 313 MB | **79 ms** · 2 MB (31 chunks; 235 dense) · +0 · 180 MB |
| flat_11M | 671 MB | 2,342 ms · 2,452 ms · 2,220 MB · 534 MB | **51 ms** · 0 (729 dense) · +1 MB · **6 MB** |
| flat_32M | 2,048 MB | **fails** (worker panic after 5 s) | **45 ms** · 0 (1,024 dense) · +6 MB · **8 MB** |

Pack builds (server, native, once per design): caravel 1.19 s (parse 366 ms)
→ skeleton 24.6 MB, geometry 189 MB, 1,135 chunks; flat_11M 2.79 s, 729
chunks; flat_32M 9.2 s (parse 3.3 s) → geometry 1.54 GB, 1,024 chunks, server
peak RSS 5.4 GB. Uploading flat_32M: 7.7 s (`curl -T`; `--data-binary`
buffers the file and gives up at 2 GB).

**Correctness** (`stream_test.mjs`). caravel streamed vs in-core at the four
busiest 30 µm windows of a scan (pad-ring IO cells, 364–766 placements each):
**0 pixels differ** of 688,000, every time. The die's middle is caravel's
empty user-project area; the first test views landed there and proved
nothing, hence the scan. At fit, the dense chunks draw as neutral proxies:
the overall picture matches in-core, but the pad ring's fine structure shows
as boxes.

**Residency.** Panning 25 views around caravel's pad ring under a 16 MB
budget: 1,260 chunks fetched, 948 evicted. At no step did the GPU exceed the
budget while holding an off-screen chunk. The peak, 45.9 MB, is one 120 µm
view's own geometry. A view revisited after eviction was fetched again and
matched pixel for pixel.

**Latency.** First visits to busy views fill in within 53–186 ms streamed
(in-core: 7–379 ms; its first view tessellates inline). The longest
main-thread frame across the run was 9.8 ms streamed vs 15.9 ms in-core. One
tessellation worker keeps up. A pool (the DRC pool's pattern) is the lever if
views get busier.

### What this settles, and what it names next

- **The size ceiling is gone for viewing.** Browser memory now scales with
  the screen and the hierarchy, not the geometry. flat_32M runs in an 8 MB
  main heap, and nothing fetched at fit depends on design size.
- **The skeleton is the new floor.** caravel's 460 k placements are a 25 MB
  blob and dominate its 180 MB streamed heap high-water. A production SoC with
  tens of millions of placements needs the hierarchy streamed too: per-cell
  instance lists fetched as the cull descends.
- **Proxies could carry color.** A per-chunk layer summary in the index would
  let a dense chunk draw in its dominant layer's color rather than neutral.
- **Editing out of core** needs chunk-level residency in the `ShapeStore` (and
  in picking and the overlay) — the step after view-only.
- **One request per chunk** is fine on localhost. Over a real network,
  coalescing adjacent chunks' ranges (and HTTP/2 multiplexing through a CDN)
  is the obvious next step.
