# Phase 7 — plan

_Proposed 2026-10-03 · awaiting the owner's go-ahead · branch `phase7-*` off
`phase6-backend-oasis`_

Phases 0–6 delivered the original implementation plan
(`AtomicStudio_Implementation_Plan.md`). Phase 7 takes the work its
measurements named next, plus the parts of the original plan still open:

- **auth and permissions** (original Phase 4 item, never started);
- **a randomized convergence test** (original verification item; only
  hand-written cases exist);
- **prefetch and better LOD** (original Phase 2 items, partly done);
- **the Phase 6 frontiers**: stream the hierarchy, edit streamed designs;
- **in-context DRC** (the Phase 5b frontier);
- **the fit-view GPU gap** (Phase 5c).

Conventions as before: a commit per slice with measurements in the message;
STATUS.md updated per slice; `docs/phase7-findings.md`; every performance
claim a same-session A/B; nothing pushed without the owner.

## Overview

| # | Slice | Why (measured) | Size | Needs |
|---|---|---|---|---|
| 7.1 | Quick wins: idle repaint, randomized convergence test, batched + prefetched chunk fetches | an idle tab burns a core; convergence is tested only by hand-picked cases; one HTTP request per chunk | S | — |
| 7.2 | Auth and room permissions | anyone who can reach the server can join any room and upload 16 GiB | M | — |
| 7.3 | Stream the hierarchy; colored LOD proxies | caravel's 460 k placements are 180 MB of streamed heap — the new memory floor; dense chunks draw grey | L | — |
| 7.4 | Edit streamed designs; rooms + server-side export for them | streamed designs are view-only | XL | 7.3 |
| 7.5 | In-context (hierarchical) DRC | intra-cell DRC misses cross-instance errors; caravel's pad cells leave 3,305 context-only flags | L | — |
| 7.6 | Fit-view GPU gap | caravel fit is 14% slower on WebGPU (Dawn) than WebGL (ANGLE) | S–M | (7.3's LOD) |

**Order (revised 2026-10-03): the owner moved 7.3 to the front.** It has no
dependencies. It runs as three slices: **A** placement chunks in the core
(done — see `phase7-findings.md`), **B** streaming the hierarchy (pack v3),
**C** coloured proxies. The rest follow in the original order: 7.1, 7.2,
7.4 (which needs 7.3), then 7.5 and 7.6.

The original reasoning for the rest still holds: 7.1 is small and makes
everything after it cheaper to test; 7.2 gates using the server beyond one
machine; 7.4 needs the residency machinery 7.3 builds; 7.5 and 7.6 are
independent.

---

## 7.1 Quick wins

### 7.1a Stop repainting when idle

**Problem.** `ui()` ends with an unconditional `ctx.request_repaint()`, so an
untouched tab renders every frame (one CPU core; on SwiftShader, three such
tabs starve a fourth — found in Phase 6).

**Design.**
- Repaint only while work is pending: progressive load, tessellation or
  stream fetches in flight, DRC busy, a drag or animation in progress.
- Wake the UI from the outside instead of polling: `ewebsock::connect_with_wakeup`
  calls `ctx.request_repaint()` on every socket event; the worker reply
  handlers (parse, tess, DRC) do the same.
- A slow heartbeat (`request_repaint_after(250 ms)`) as a safety net.
- `?continuous` keeps today's behavior for the fps benches (they measure
  render throughput, which needs a frame every vsync).

**Acceptance.** An idle tab: ≤ 4 frames/s (from vsync rate). Interactive fps,
load, streaming and sync timings unchanged (same-session A/B). All browser
suites pass.

### 7.1b Randomized convergence (property) test

**Problem.** Convergence is tested by specific scenarios. The CRDT carries
moves, deletes, vertex rings, adds, batches, hides and undo; their
interleavings are where bugs would hide.

**Design** (`proptest`, dev-only, in `crates/sync`):
- 3 peers × up to 200 random ops each: `set_edit` (move / delete /
  set-points) over a small id space so conflicts are frequent, `add_shape`,
  `delete_added`, `apply_batch` (incl. hide/show), undo, redo.
- Random delivery: each update to each peer in random order, some
  duplicated, some delayed to the end.
- Property: after full exchange, every peer's `edits()`, `adds()` and
  `hidden()` are identical; undo never reverts another peer's op.
- Server-in-the-loop variant: updates go through a `Room`; random snapshot
  and crash points (WAL cut at a random byte); after restore and the peers'
  reconnect push, the server doc equals every peer's.

**Acceptance.** 256 cases per run, < 30 s; shrinks to a minimal failing
sequence if anything diverges. Runs in `cargo test`.

### 7.1c Batched and prefetched chunk fetches

**Problem.** Streaming issues one HTTP request per chunk and fetches nothing
ahead of the view. Free on localhost; costly at real network latency.

**Design.**
- Coalesce: chunks requested in the same frame whose records are adjacent
  in the geometry blob go out as one range request; the worker splits the
  buffer by the index offsets.
- Prefetch ring: when on-screen work is idle, fetch the chunks one screen
  around the view that pass the density rule at the current zoom — lowest
  priority, cancelled (`AbortController`) when the view moves on.
- A latency harness: CDP `Network.emulateNetworkConditions` (e.g. 80 ms RTT).

**Acceptance** (caravel pad-ring pan, 80 ms RTT): requests per view ≤ 1/5 of
today's; median time-to-filled view ≤ 1.5 × RTT + tessellation; ≥ 80% of
one-screen pan steps show no proxies. Pixel identity unchanged.

---

## 7.2 Auth and room permissions

**Problem.** No authentication: any client that reaches the server can join
any room, read its state, and upload blobs (≤ 16 GiB). Fine on 127.0.0.1,
unusable beyond it.

**Design — capability links** (self-contained; no identity provider):
- Roles per room: **owner**, **editor**, **viewer**.
- Tokens: `room | role | expiry | HMAC(server secret)`, base64url; the
  secret lives in the store (`store/secret`, created on first start).
  Revocation: rotate the room's key — old links stop working.
- Enforcement:
  - WebSocket join: token in the query (browsers cannot set WS headers);
    missing / forged / expired → refused (close 4401). **Viewers'** document
    updates are dropped server-side (presence still relayed) — read-only by
    construction, not by UI.
  - `GET /rooms/<r>`, `GET /packs/<d>`: a valid token for the room / for a
    room bound to that design.
  - `PUT /blobs`: editor or owner token (or a server upload key); per-token
    size quota.
  - `GET /blobs/<h>`: a 256-bit hash is itself an unguessable capability;
    hashes are only revealed through authorized endpoints.
- Default: auth **on** when bound to a non-loopback address, **off** on
  127.0.0.1 (dev, tests).
- Client: links carry `&t=<token>`; the app sends it on the socket and as
  `Authorization: Bearer` on HTTP. The owner gets a "Share" control (copy
  editor / viewer link); a viewer's edit tools are disabled.
- TLS stays with a reverse proxy (documented), not in the server.

**Acceptance.** Server tests: each endpoint × {none, forged, expired,
viewer, editor, owner}; a viewer's update never reaches the doc or the WAL.
Browser: a viewer follows an editor's edits live and cannot make any.
Existing suites pass with auth off.

**Owner decision:** capability links (proposed) vs accounts with OAuth
(GitHub/Google) — the latter needs an identity provider and more scope.

---

## 7.3 Stream the hierarchy (and colored LOD)

**Problem.** Streaming loads the whole skeleton first: every cell's full
instance list. caravel: 460 k placements, a 25 MB blob, 180 MB of streamed
heap. A production SoC with tens of millions of placements would not fit —
the same wall in-core geometry hit, one level up.

**Design — pack v2.**
- **Cell table** (small, per cell — not per placement): name, bbox, shape
  chunk table, and its *instance chunks*.
- **Instance chunks**: a cell's placements, spatially partitioned like shape
  chunks (cells with > N placements get a grid — caravel_core's 460 k);
  each a fetchable record with its bbox.
- The cull descends into an instance chunk only when it is resident; an
  unresolved one draws as its bbox proxy and is requested — the shape
  chunks' mechanism, applied to the hierarchy. Same density/LOD rule, so
  placements resolved per frame are bounded by the screen.
- Instance chunks join the same LRU residency budget.
- One code path: in-core designs are "fully resident" (every instance chunk
  loaded), so cull, pick and export are unchanged for them.
- **Colored proxies**: each shape chunk's dominant layer (by area) goes in
  the pack; a dense chunk's proxy draws in that layer's color instead of
  neutral grey — the original plan's "density raster" LOD, cheaply.
- `PACK_VERSION` 2; servers rebuild packs on the bump.

**Acceptance.**
- caravel streamed: main heap 180 MB → **≤ 40 MB** at fit; first frame
  ≤ 100 ms; still **0 pixels** different from in-core at the four busy views.
- A generated hierarchical design with **50 M placements** opens streamed
  (main heap ≤ 100 MB); in-core it fails.
- Fit view: pixels differing from in-core measured before/after colored
  proxies — target: halved.

---

## 7.4 Edit streamed designs

**Problem.** Every editing feature reads base geometry: picking, move (the
moved copy), vertex edit (the ring), delete (suppression), booleans,
flatten, export, DRC. Streamed cells hold none, so streamed designs are
view-only and cannot join rooms.

**Design.**
- **Chunk-level residency in the `ShapeStore`**: a store whose chunks are
  each resident or not; shape lookups go through the chunk (binary search
  on the chunk table). Callers handle "not resident" explicitly.
- **Pick on demand**: a click fetches the chunks under the cursor, then
  completes the pick.
- **Overlay materialized per chunk**: a CRDT edit whose base chunk is not
  resident waits; when the chunk arrives, its edits are applied. Deletes
  reach the tessellation worker as a suppression list for that chunk.
- **Rooms for streamed designs** (the reason above), with 7.2's permissions.
- **Server-side export** (`POST /rooms/<r>/export?format=gds|oas` → a blob):
  the server holds the design and the room doc, so a design too big for the
  browser can still be exported with its edits.
- DRC on streamed designs runs server-side (with 7.5).

**Acceptance.**
- The full two-peer edit scenario (today's `edit_sync_test` 1–22) with one
  peer in-core and one streamed: both converge, pixel-identical.
- Server export ≡ in-core export of the same composite (normalized digest
  via the KLayout oracle).
- Streamed memory stays bounded while editing (heap within the 7.3 targets
  plus the resident chunks).

---

## 7.5 In-context (hierarchical) DRC

**Problem.** DRC is intra-cell: a shape in cell A near an instance of cell B
is never checked against it. caravel's whole-chip sweep leaves 3,305 flags,
mostly pad-cell geometry that is legal once merged in context — and real
cross-cell violations would go unreported.

**Design.**
- **Interaction regions**: for each cell, sibling instances (and parent
  shapes) within rule reach of each other; only the overlap ⊕ reach window
  is flattened.
- **Context dedup**: the same pair of cells at the same relative transform
  is checked once (memoized by (cell A, cell B, relative transform)) — what
  makes hierarchical DRC tractable on arrays of standard cells.
- The flattened window feeds the existing `check_tile_columns` unchanged —
  in-browser via the worker pool for the visible window, and natively on
  the server for whole chips and for streamed designs.
- Violations are reported in the parent's frame, deduplicated against
  intra-cell results.
- **Oracle**: KLayout's DRC engine (sky130 width/space for the deck's
  layers) on caravel; violation sets compared after normalization —
  extending `scripts/oracle/`.

**Acceptance.** caravel: the 3,305 context-only flags resolve to the genuine
set; agreement with KLayout on width/space within the documented semantics
(shielding, projection width), every difference explained. Whole-chip
in-context sweep, native: target < 15 s. An edit re-checks only its
interaction regions (incremental, as now).

---

## 7.6 Fit-view GPU gap

**Problem.** caravel's fit view is GPU-bound and 14% slower on Dawn-Metal
(WebGPU) than ANGLE-Metal (WebGL2): 44 vs 51 fps. In-core it draws millions
of sub-pixel triangles at fit.

**Design.** Profile first (draw calls, triangles, blend, MSAA, instance
buffer layout on both backends). Leading hypothesis: sub-pixel overdraw.
The streaming density rule applies in-core too: at fit, a chunk too dense
for its footprint draws as a (7.3-colored) proxy instead of its triangles.
Then Dawn-specific tuning where the profile points.

**Acceptance.** caravel fit on WebGPU ≥ 60 fps (and ≥ ANGLE), or a profile
that explains the residue. Leaf / mid views and goldens unchanged.

---

## Housekeeping (owner's calls)

- **Push / merge.** Nothing is pushed; the stacked chain ends at
  `phase6-backend-oasis`. Proposal: merge the chain into the `phase4` trunk
  (or a new `trunk`) and push it before 7.2 opens the server to others.
- **CI.** GitHub Actions: `cargo test` (incl. the property tests), the wasm
  build, and the browser suites in headless Chromium (SwiftShader, ≤ 2 live
  pages — or after 7.1a, no limit).

## Open decisions for the owner

1. Confirm the order (7.1 → 7.6) or reorder / drop slices.
2. Auth model: capability links (proposed) or accounts / OAuth.
3. Server-side compute (export, whole-chip DRC on the server): acceptable?
4. Push and merge before 7.2? CI?

## Verification, every slice

Golden images unchanged unless a slice intends otherwise; all browser suites
(sync, persistence, streaming) green; same-session A/B for every number;
STATUS.md + findings updated; one commit per slice.
