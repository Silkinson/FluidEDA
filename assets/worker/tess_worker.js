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
//   in : { type:'tess', cell, chunk, gen, coords, starts, layers, kinds }   (buffers transferred in)
//   out: { type:'tess_done', cell, chunk, gen, mesh_layers, vert_counts,
//          index_counts, verts, indices }                            (buffers transferred back)
// `cell`/`chunk`/`gen` are echoed so the main thread can key the reply and drop
// stale ones (old design or old chunk partition).
//
// Phase 6 SLICE 3 — streaming: the chunk's geometry is not on the main thread
// at all. The worker fetches the chunk's record, one byte range of the design
// pack's immutable geometry blob, and tessellates that. Phase 7.1c: one
// request carries every chunk whose record lies close to the others in the
// blob — one byte range, split here by the chunks' own offsets:
//   in : { type:'tess_fetch', gen, url, start, end, items: [{ cell, chunk, start, end }] }
//   out: one tess_done (as above) per item, + fetch_bytes and fetch_ms (the
//        request's: its bytes on the first item, 0 on the others)
// Record layout (crates/core/src/pack.rs), little-endian, 4-byte aligned:
//   [n u32][nc u32][starts (n+1) u32][layers n u32][kinds n u8, padded to 4]
//   [coords nc i32] — `kinds` (Phase 7.7): a rectangle is stored as its two
//   corners.

importScripts('worker.js');

// Kick off wasm instantiation immediately; jobs await it (early jobs just queue).
const ready = wasm_bindgen('worker_bg.wasm');

// A chunk record → the three columns, as views into the fetched buffer.
function recordColumns(buf) {
  const head = new Uint32Array(buf, 0, 2);
  const n = head[0], nc = head[1];
  const kindsAt = 4 * (3 + 2 * n);
  const coordsAt = kindsAt + 4 * Math.ceil(n / 4);
  if (buf.byteLength !== coordsAt + 4 * nc) {
    throw new Error(`chunk record: ${buf.byteLength} bytes, header says ${coordsAt + 4 * nc}`);
  }
  return {
    starts: new Uint32Array(buf, 8, n + 1),
    layers: new Uint32Array(buf, 8 + 4 * (n + 1), n),
    kinds: new Uint8Array(buf, kindsAt, n),
    coords: new Int32Array(buf, coordsAt, nc),
  };
}

async function fetchRange(url, start, end, cache) {
  const r = await fetch(url, { cache, headers: { Range: `bytes=${start}-${end - 1}` } });
  if (r.status !== 206 && r.status !== 200) throw new Error(`fetch ${url}: HTTP ${r.status}`);
  let buf = await r.arrayBuffer();
  // A server that ignores Range sends the whole blob: cut the record out.
  if (r.status === 200) buf = buf.slice(start, end);
  return buf;
}

async function fetchRecord(msg) {
  const t0 = performance.now();
  let buf = await fetchRange(msg.url, msg.start, msg.end, 'default');
  // Chrome's HTTP cache can hand back a short body when the page and this
  // worker read the same range at once (Phase 7.4: a few in 600). Read it
  // again past the cache.
  if (buf.byteLength !== msg.end - msg.start) buf = await fetchRange(msg.url, msg.start, msg.end, 'no-store');
  return { buf, fetch_ms: performance.now() - t0 };
}

self.onmessage = async (e) => {
  const msg = e.data || {};
  if (msg.type === 'tess') {
    // coords/starts/layers are transferred ArrayBuffers (moved from main).
    await job(msg, {
      coords: new Int32Array(msg.coords),
      starts: new Uint32Array(msg.starts),
      layers: new Uint32Array(msg.layers),
      kinds: new Uint8Array(msg.kinds),
    }, null);
  } else if (msg.type === 'tess_fetch') {
    let fetched;
    try {
      fetched = await fetchRecord(msg);
    } catch (err) {
      for (const it of msg.items) fail(it, msg.gen, err);
      return;
    }
    for (const [i, it] of msg.items.entries()) {
      // Each record gets a buffer of its own (typed views need it aligned).
      const buf = fetched.buf.slice(it.start - msg.start, it.end - msg.start);
      const info = { bytes: i === 0 ? fetched.buf.byteLength : 0, ms: fetched.fetch_ms };
      let cols;
      try {
        cols = recordColumns(buf);
      } catch (err) {
        fail(it, msg.gen, err);
        continue;
      }
      await job({ cell: it.cell, chunk: it.chunk, gen: msg.gen }, cols, info);
    }
  }
};

function fail(it, gen, err) {
  self.postMessage({ type: 'tess_error', cell: it.cell, chunk: it.chunk, gen, message: String((err && err.message) || err) });
}

// Tessellate one chunk's columns and send the meshes back.
async function job(msg, { coords, starts, layers, kinds }, fetched) {
  try {
    await ready;
    const r = wasm_bindgen.tessellate(coords, starts, layers, kinds); // the heavy work, off-main

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
        fetch_bytes: fetched ? fetched.bytes : 0,
        fetch_ms: fetched ? fetched.ms : 0,
      },
      [mesh_layers.buffer, vert_counts.buffer, index_counts.buffer, verts.buffer, indices.buffer]
    );
    r.free();
  } catch (err) {
    fail(msg, msg.gen, err);
  }
}
