// Phase 7.7 A/B in the browser: compact rectangles (default) against every
// shape a ring (`?norects`), same build, same session — caravel loaded
// in-core: the parse worker's peak, the main heap once loaded, load timings,
// and the fit view's tessellation. Alternating runs, medians.
//
// Prereqs: server (:9000) holding caravel's blob, trunk serve (:8080).
// Run: GPU=webgpu node scripts/rect_ab.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const RUNS = Number(process.env.RUNS || 3);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];

async function run(browser, flag) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${BASE}/?file=/blobs/${H}${flag ? `&${flag}` : ''}`);
  await page.waitForFunction(() => {
    const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
    return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
  }, null, { timeout: 240000, polling: 100 });
  // The fit view's chunks all tessellated and uploaded.
  let last = '', stable = 0;
  for (let i = 0; i < 300 && stable < 5; i++) {
    const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
    const sig = `${p.uploaded}/${p.in_flight}`;
    stable = !p.in_flight && sig === last ? stable + 1 : 0;
    last = sig;
    await sleep(200);
  }
  const [m, p] = await page.evaluate(() => [JSON.parse(window.__fluideda_metrics), JSON.parse(window.__fluideda_perf)]);
  await page.context().close();
  return {
    parse_ms: m.parse_ms,
    worker_mb: m.parse_heap_bytes / 1e6,
    ttff_ms: m.ttff_ms,
    geometry_ms: m.geometry_ms,
    heap_mb: p.wasm_heap / 1e6,
    tess_ms: p.inline_tess_ms,
    worker_chunks: p.worker_chunks,
    uploaded: p.uploaded,
    gpu_mb: p.gpu_bytes / 1e6,
  };
}

const browser = await launch();
try {
  const r = { norects: [], default: [] };
  for (let i = 0; i < RUNS; i++) {
    for (const mode of ['norects', 'default']) {
      r[mode].push(await run(browser, mode === 'norects' ? 'norects' : ''));
    }
  }
  const keys = Object.keys(r.default[0]);
  for (const k of keys) {
    const a = r.norects.map((x) => x[k]), b = r.default.map((x) => x[k]);
    console.log(`${k.padEnd(14)} ${median(a).toFixed(1).padStart(9)} → ${median(b).toFixed(1).padStart(9)}   ` +
      `(${a.map((x) => x.toFixed(0)).join('/')} vs ${b.map((x) => x.toFixed(0)).join('/')})`);
  }
} finally {
  await browser.close();
}
