// Phase 6 SLICE 3: in-core vs out-of-core (streamed) loading, per design.
//
// Both modes read the design from the server's blob store, in fresh browser
// contexts, one after the other:
//   in-core   `?file=/blobs/<sha256>` — the worker fetches and parses the whole
//             file; every cell's geometry is copied to the main thread.
//   streamed  `?stream=<sha256>` — the server's pack: skeleton + chunk index,
//             then only the chunks on screen, fetched by byte range.
// Reported: first frame, full load (in-core), heaps (worker parse peak +
// main), and for streaming the bytes fetched at fit and at a leaf view.
//
// Prereqs: server (:9000) holding each design blob (PUT /blobs/<sha256>),
// trunk serve (:8080). Designs: DESIGNS="name:sha256,..." (default: the
// Phase 6 findings set, if their blobs are stored).
// Run: GPU=webgpu node scripts/outofcore_bench.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const DESIGNS = (process.env.DESIGNS ||
  'caravel:54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7,' +
  'flat_11M:d61d884a598dba5355d7d469642986748adda24703656be996e973e56f081c0d,' +
  'flat_32M:d3164d433559f79d2989fbe2fedb22d2febbe6f35d5baea6350d28e41a1a37cb')
  .split(',').map((d) => d.split(':'));
const TIMEOUT = 240_000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const mb = (b) => `${(b / 1e6).toFixed(0)} MB`;

async function inCore(browser, hash) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  let error = null;
  page.on('console', (m) => { if (/panicked|load failed|parse failed/i.test(m.text())) error ??= m.text().split('\n')[0].slice(0, 120); });
  page.on('crash', () => { error ??= 'page crashed'; });
  const t0 = Date.now();
  await page.goto(`${BASE}/?file=/blobs/${hash}`);
  let m = null;
  while (Date.now() - t0 < TIMEOUT && !error) {
    m = await page.evaluate(() => (window.__fluideda_metrics ? JSON.parse(window.__fluideda_metrics) : null)).catch(() => null);
    if (m && m.file_bytes > 1e6 && m.geometry_ms > 0) break;
    await sleep(200);
  }
  await page.context().close();
  if (error || !(m && m.geometry_ms > 0)) return `in-core: FAILED (${error || 'timeout'}) after ${((Date.now() - t0) / 1000).toFixed(0)} s`;
  return `in-core: first frame ${m.ttff_ms.toFixed(0)} ms, all geometry ${m.geometry_ms.toFixed(0)} ms; ` +
    `worker parse peak ${mb(m.parse_heap_bytes)}, main heap ${mb(m.wasm_heap_bytes)}`;
}

async function settle(page) {
  let last = '', stable = 0;
  for (let i = 0; i < 600; i++) {
    const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf));
    const sig = `${p.uploaded}/${p.in_flight}/${p.stream_fetched}`;
    if (p.in_flight === 0 && sig === last) { if (++stable >= 5) return p; } else stable = 0;
    last = sig;
    await sleep(100);
  }
  throw new Error('did not settle');
}

async function streamed(browser, hash) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  const t0 = Date.now();
  await page.goto(`${BASE}/?stream=${hash}`);
  await page.waitForFunction(() => JSON.parse(window.__fluideda_perf || '{}').streaming, null, { timeout: TIMEOUT, polling: 50 });
  const fit = await settle(page);
  const m = await page.evaluate(() => JSON.parse(window.__fluideda_metrics || '{}'));
  // A leaf view: 5 µm at the design's centre (the ?zoom=leaf preset).
  await page.evaluate(() => { window.__fluideda_cmd = 'zoom leaf'; });
  await page.waitForFunction(() => !window.__fluideda_cmd);
  const leaf = await settle(page);
  await page.context().close();
  return `streamed: first frame ${m.ttff_ms.toFixed(0)} ms (skeleton ${mb(fit.stream_skeleton_bytes)}, pack fetch ${fit.stream_pack_ms.toFixed(0)} ms); ` +
    `fit: ${fit.stream_fetched} chunks ${mb(fit.stream_fetched_bytes)} fetched, ${fit.stream_dense} dense; ` +
    `leaf: ${leaf.stream_fetched - fit.stream_fetched} more (${mb(leaf.stream_fetched_bytes - fit.stream_fetched_bytes)}); ` +
    `of ${mb(fit.stream_geometry_bytes)} geometry; main heap ${mb(leaf.wasm_heap)}, GPU ${mb(leaf.gpu_bytes)}`;
}

const browser = await launch();
try {
  for (const [name, hash] of DESIGNS) {
    console.log(`${name}:`);
    console.log(`  ${await streamed(browser, hash)}`);
    console.log(`  ${await inCore(browser, hash)}`);
  }
} finally {
  await browser.close();
}
