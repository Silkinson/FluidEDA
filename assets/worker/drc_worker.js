// FluidEDA DRC worker driver (Phase 5b SLICE 3).
//
// Runs the same core-only wasm module as the parse / tessellation workers
// (worker.js + worker_bg.wasm, from scripts/build_worker.sh), for merge-first
// DRC tiles. The app keeps a POOL of these (one per spare core) and feeds each
// one tile jobs round-robin; like the tessellation worker it is stateless and
// persistent — every job carries everything it needs.
//
// Protocol — one message per tile:
//   in : { type:'drc', epoch, cell, tx, ty, sx, sy, gen, core, rules, coords, starts, layers }
//        (core/rules: Int32Array; coords/starts/layers buffers transferred in)
//   out: { type:'drc_done', epoch, cell, tx, ty, sx, sy, gen, viol }   (viol buffer transferred back)
//        { type:'drc_error', …same keys…, message }
// The keys are echoed so the main thread can match the reply to its tile and
// drop it if the tile was re-dirtied (gen) or the design/deck changed (epoch).

importScripts('worker.js');

const ready = wasm_bindgen('worker_bg.wasm');

self.onmessage = async (e) => {
  const m = e.data || {};
  if (m.type !== 'drc') return;
  const keys = { epoch: m.epoch, cell: m.cell, tx: m.tx, ty: m.ty, sx: m.sx, sy: m.sy, gen: m.gen };
  try {
    await ready;
    const viol = wasm_bindgen.drc_tile(
      m.core,
      m.rules,
      new Int32Array(m.coords),
      new Uint32Array(m.starts),
      new Uint32Array(m.layers)
    );
    self.postMessage({ type: 'drc_done', ...keys, viol: viol.buffer }, [viol.buffer]);
  } catch (err) {
    self.postMessage({ type: 'drc_error', ...keys, message: String((err && err.message) || err) });
  }
};
