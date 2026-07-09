# Atomic-Studio-style Layout Viewer — Implementation Plan

**Stack:** Rust everywhere, rendering via `wgpu` (WebGPU backend), compiled to `wasm32-unknown-unknown`.
**First demo target:** read-only GDS viewer with smooth pan/zoom **plus** two browsers sharing live cursors and markers.
**Guiding principle:** get one thin end-to-end slice working across *every* axis first (render + wasm + multiplayer), then deepen. Do not build any layer to "full" before the whole loop runs.

---

## 0. Tech decisions (locked)

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (single workspace) | User preference; one language across core, renderer, server |
| Rendering | `wgpu` → WebGPU | Native + web from the same code; unified-memory-friendly on Apple Silicon |
| Web harness | `eframe` (egui + winit + wgpu) | Fastest path to a web canvas + side panels; supports custom `wgpu` paint callbacks |
| GDSII parse | `gds21` | Mature Rust GDSII reader/writer |
| Tessellation | `lyon` | Polygon/path → triangle buffers |
| Math | `glam` | 2D transforms, camera |
| Spatial index | `rstar` | R-tree (Phase 1+) |
| CRDT + presence | `yrs` + `y-sync` + yrs *awareness* | Don't hand-roll multiplayer; awareness gives cursors/viewport for free |
| WebSocket (client) | `ewebsock` | Works on native and wasm |
| Server | `axum` + `tokio` + `tokio-tungstenite` | Rust room relay for `y-sync` + awareness |
| Booleans (later) | `i_overlay` or `geo` | Edit ops, Phase 3 |
| Build/serve | `trunk` | wasm bundling + dev server with COOP/COEP headers |

**Two hard requirements to set up on day one, or things break later:**
- Serve with `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp` (needed for `SharedArrayBuffer`/threads in Phase 2). Configure in `Trunk.toml` now.
- Target a WebGPU-capable browser (Chrome/Edge stable, Safari 18+). On the demo Mac this is fine.

---

## 1. Repository layout

```
atomic-studio/
├─ Cargo.toml                # workspace
├─ crates/
│  ├─ core/                  # data model + geometry (no rendering, no I/O of GPU)
│  │   ├─ db.rs              # cell database (flat in P0, hierarchical in P1)
│  │   ├─ gds.rs            # gds21 → internal model
│  │   ├─ tess.rs           # lyon tessellation → CPU vertex/index buffers
│  │   ├─ index.rs          # rstar spatial index (P1)
│  │   └─ types.rs          # int-nm coords, Layer, Shape, Transform
│  ├─ render/               # wgpu pipelines, camera, layer compositing
│  │   ├─ camera.rs         # pan/zoom, origin rebasing (P1)
│  │   ├─ pipeline.rs       # fill pipeline, per-layer color
│  │   └─ overlay.rs        # remote cursors, markers, selection
│  ├─ sync/                 # yrs document + awareness + ewebsock transport
│  └─ app/                  # eframe entry: panels + wgpu callback + input
├─ server/                  # axum room relay (y-sync + awareness broadcast)
├─ assets/sample.gds        # small test cell shipped with the app
└─ Trunk.toml               # COOP/COEP headers, wasm build
```

Keep `core` free of `wgpu` and of networking — it takes shapes in, emits CPU buffers out. That seam is what lets the worker split (Phase 2) happen without a rewrite.

---

## 2. Phase 0 — Walking skeleton (the demo). Target ≈ 8–12 working days

**Definition of done:** open two browser tabs on the same room → both show `sample.gds` with correct layer colors → each can pan/zoom independently → each sees the other's live cursor → either can drop a rectangle marker that appears in both instantly.

**Scope cuts (intentional, restored later):** single-threaded, no worker; flatten hierarchy to a polygon list; render all geometry into static buffers once (no tiling, no LOD, no culling); GDS only; markers/cursors only (no shape editing yet); one hard-coded room.

### Day-by-day

1. **Scaffold + blank canvas.** Cargo workspace; `eframe` web app rendering a solid-color `wgpu` render pass via a paint callback; `trunk serve` with COOP/COEP. *Check: colored canvas in browser.*
2. **Load GDS.** `gds21` parse `assets/sample.gds` → `Vec<(Layer, Polygon)>` in `core`, coords as `i32` nm. *Check: log shape/layer counts.*
3. **Tessellate.** `lyon` fills each polygon → one interleaved vertex buffer + index buffer per layer, uploaded to `wgpu`. *Check: buffer sizes sane.*
4. **Draw layers.** Fill pipeline with a camera uniform (2D ortho) and a per-layer color uniform; draw each layer. *Check: recognizable layout on screen, right colors.*
5. **Pan/zoom.** winit pointer/scroll events → update camera (drag = translate, wheel = scale about cursor). *Check: smooth navigation; matches KLayout visually side by side.*
6. **egui shell.** Layer panel (toggle visibility/color), fps counter, room/name field. *Check: toggling a layer hides it.*
7. **Wire sync transport.** `sync` crate: `yrs::Doc` + awareness; `ewebsock` to server URL; encode/relay `y-sync` messages. Stand up the `axum` relay that rebroadcasts to a room. *Check: two clients connect, server logs joins.*
8. **Presence cursors.** Publish local cursor in **world coordinates** via awareness; render remote cursors + names in `overlay.rs`. *Check: move mouse in tab A, cursor moves in tab B regardless of B's zoom.*
9. **Shared markers.** A `yrs` array of `{x, y, w, h, color}`; add-rectangle tool writes to it; render markers from the shared doc. *Check: marker drawn in A appears in B.*
10. **Polish + deploy.** Reconnect handling, cursor colors per user, package wasm, host static + server. *Check: full DoD script passes on two machines.*

*(Days 11–12 are buffer for wgpu/WebGPU or COOP/COEP friction — budget for it.)*

**Phase 0 risks / gotchas**
- `SharedArrayBuffer`/threads not needed yet — keep P0 single-threaded so you dodge cross-origin-isolation debugging until Phase 2 (but set the headers now).
- WebGPU adapter request is async on web — handle the "no WebGPU" path with a clear message.
- Send cursors in **world space**, not screen space, or they won't line up across users at different zooms.
- Don't tessellate every frame — tessellate once, redraw buffers. Pan/zoom only changes the camera uniform.

---

## 3. Phase 1 — Real files: hierarchy, indexing, culling. ≈ 2–3 weeks

Goal: open a large hierarchical GDS without flattening, still 60fps.

- Hierarchical cell DB: cells hold shapes + `SREF`/`AREF` instances with transforms.
- `rstar` R-tree **per cell definition**, built lazily; query in cell-local space by inverse-transforming the viewport.
- Viewport hierarchy walk with a transform stack; cull by cell bbox; stop descending below a pixel threshold.
- **Instanced draw** for `AREF` arrays (one cell buffer, per-instance transforms in a storage buffer).
- Origin rebasing in `camera.rs` so `f32` stays precise at deep zoom on million-nm chips.
- Milestone: open a real open-PDK block or a large generated array at 60fps.

## 4. Phase 2 — Tiling, LOD, worker offload, progressive. ≈ 3–4 weeks

Goal: smooth on very large layouts; never block a frame.

- Move `core` into a Web Worker as a second wasm module (needs the COOP/COEP headers from Day 1); communicate via `postMessage` + `SharedArrayBuffer` ring; `wasm-bindgen-rayon` for the worker pool.
- Tile pyramid + LRU GPU cache; render via `OffscreenCanvas` in a worker if desired.
- LOD proxies (bbox / density raster) for tiny cells.
- Progressive refinement: draw best-available tile immediately, swap refined tile on delivery.
- Prefetch ring + cancellation; memo cache keyed by content hash.
- Milestone: pan across a huge layout coarse-to-fine with no stalls.

## 5. Phase 3 — Editing + collaborative incremental. ≈ 3–4 weeks

Goal: presence graduates to shared editing.

- Geometry kernel booleans (`i_overlay`/`geo`); select / move / draw shape tools.
- Shapes live in the `yrs` doc → edits are automatically conflict-free and multiplayer.
- Incremental tracker: an edit marks only affected spatial-index nodes, memo entries, and tiles dirty; rebuild on next frame.
- Undo/redo via `yrs` `UndoManager` (scoped per user).
- Milestone: two users edit the same cell concurrently, conflict-free, with incremental redraw.

## 6. Phase 4 — Hardening / backend / interop. Ongoing

- Content-addressed version store + blob persistence; tile edge cache/CDN.
- OASIS import/export; streaming loader.
- Incremental DRC on edited regions.
- Auth, rooms/permissions, reconnection/state-resync.

---

## 7. Verification (every phase)

- **Golden-image diff** against a KLayout screenshot of the same file/viewport (the demo's own credibility test).
- Shape/layer count assertions after parse.
- On-screen fps overlay; frame-time budget alarms.
- Two-client sync test script (cursor + marker + later edit convergence).
- Phase 3+: property test that concurrent random edit streams converge to identical docs.

## 8. Fastest-path checklist (do these first)

1. Cargo workspace + `eframe` blank `wgpu` canvas on web via `trunk`, with COOP/COEP headers.
2. `gds21` → flat polygon list → `lyon` → one buffer/layer → draw.
3. Camera pan/zoom.
4. `yrs` awareness over `ewebsock` + `axum` relay → remote cursors.
5. Shared marker array.

That five-item loop **is** the demo. Everything in Phases 1–4 deepens it without changing its shape.
