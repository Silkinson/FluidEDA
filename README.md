# FluidEDA

A browser-based IC layout viewer with live multiplayer, built Rust-everywhere and
rendered with `wgpu` (WebGPU). This is the **Phase 0 walking skeleton**: load a GDS,
pan/zoom it smoothly, and share live cursors + markers between two browsers.

See `AtomicStudio_Implementation_Plan.md` (in the design docs) for the full roadmap.

## Architecture (Phase 0)

```
crates/core    data model + geometry (no GPU, no network) — GDS load, tessellation
crates/render  wgpu pipelines — camera, per-layer fill, overlay
crates/sync    yrs CRDT doc + awareness presence over a WebSocket (ewebsock)
crates/app     eframe app — canvas + panels, wires core → render, drives sync
server         axum per-room broadcast relay
assets/        sample.gds (598 polygons, 4 layers) embedded in the app
```

The `core` crate has no rendering or networking deps on purpose — that seam is
what lets the geometry pipeline move into a Web Worker in Phase 2 untouched.

## Prerequisites

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk
# A WebGPU-capable browser: Chrome/Edge (stable) or Safari 18+.
```

## Run the demo

Two processes. **Terminal 1 — relay server:**

```sh
cargo run -p fluideda-server        # listens on 0.0.0.0:9000
```

**Terminal 2 — web app:**

```sh
trunk serve                         # serves on http://127.0.0.1:8080
```

Trunk is preconfigured (`Trunk.toml`) with the `COOP`/`COEP` headers required for
`SharedArrayBuffer` — they're not needed until Phase 2 but are set now so nothing
changes later.

> Note: the app talks to the relay at `ws://<page-host>/ws?room=demo`. For local
> dev, either run the server behind the same origin or point `server_url()` in
> `crates/app/src/app.rs` at `ws://127.0.0.1:9000/ws?room=demo`.

Open **two** browser tabs on `http://127.0.0.1:8080`, click **Join room** in each,
and you should see: the same layout in both, independent pan/zoom, each other's
live cursors, and Shift+drag to drop a shared marker.

Native window (no browser) also works for quick iteration:

```sh
cargo run -p fluideda-app
```

## Controls

- **Drag** — pan
- **Wheel** — zoom about the cursor
- **Shift+drag** — drop a shared marker
- **Fit view** — reframe the whole layout

## Status vs. plan

Done (Phase 0 scaffold): workspace, GDS load + tessellation, wgpu fill pipeline,
pan/zoom camera, egui shell, CRDT presence + shared markers, relay server, sample data.

Next (Phase 0 finish → Phase 1): wire layer-visibility toggles through the paint
callback, y-sync handshake for late joiners, then hierarchical cell DB + `rstar`
spatial index + culling + AREF instancing for real chip files.
