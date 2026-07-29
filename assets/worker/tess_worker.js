// FluidEDA tessellation worker driver (Phase 4 STEP 2).
//
// Runs the same core-only wasm module the parse worker uses (worker.js +
// worker_bg.wasm, from scripts/build_worker.sh), but for a different job: it
// tessellates chunks off the main thread so fit-view no longer freezes on the
// giant flat cells. Unlike the parse worker this one is PERSISTENT — it is not
// terminated after a job; it serves tessellation jobs for the life of the design
// (the main thread respawns it on a new load / chunk re-partition).
//
// Protocol — one message per chunk:
//   in : { type:'tess', cell, chunk, gen, coords, starts, layers }   (buffers transferred in)
//   out: { type:'tess_done', cell, chunk, gen, mesh_layers, vert_counts,
//          index_counts, verts, indices }                            (buffers transferred back)
// `cell`/`chunk`/`gen` are echoed so the main thread can key the reply and drop
// stale ones (old design or old chunk partition).

importScripts('worker.js');

// Kick off wasm instantiation immediately; jobs await it (early jobs just queue).
const ready = wasm_bindgen('worker_bg.wasm');

self.onmessage = async (e) => {
  const msg = e.data || {};
  if (msg.type !== 'tess') return;
  try {
    await ready;
    // coords/starts/layers are transferred ArrayBuffers (moved from main).
    const coords = new Int32Array(msg.coords);
    const starts = new Uint32Array(msg.starts);
    const layers = new Uint32Array(msg.layers);
    const r = wasm_bindgen.tessellate(coords, starts, layers); // the heavy work, off-main

    const mesh_layers = r.mesh_layers;
    const vert_counts = r.vert_counts;
    const index_counts = r.index_counts;
    const verts = r.verts;
    const indices = r.indices;
    self.postMessage(
      {
        type: 'tess_done',
        cell: msg.cell,
        chunk: msg.chunk,
        gen: msg.gen,
        mesh_layers: mesh_layers.buffer,
        vert_counts: vert_counts.buffer,
        index_counts: index_counts.buffer,
        verts: verts.buffer,
        indices: indices.buffer,
      },
      [mesh_layers.buffer, vert_counts.buffer, index_counts.buffer, verts.buffer, indices.buffer]
    );
    r.free();
  } catch (err) {
    self.postMessage({
      type: 'tess_error',
      cell: msg.cell,
      chunk: msg.chunk,
      gen: msg.gen,
      message: String((err && err.message) || err),
    });
  }
};
