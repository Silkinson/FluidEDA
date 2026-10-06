// Same-session A/B of two builds' in-core loads: A and B serve different
// commits (e.g. a worktree of the previous slice on :8081); the same design
// loads alternately in each, and the medians of its load metrics are compared.
// A design that fails to load (a wasm trap, a timeout) is reported as such.
//
// Run: A=http://127.0.0.1:8081 B=http://127.0.0.1:8080 DESIGN=<sha256> \
//      GPU=webgpu node scripts/load_ab.mjs
import { launch } from './launch.mjs';

const A = process.env.A || 'http://127.0.0.1:8081';
const B = process.env.B || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const RUNS = Number(process.env.RUNS || 3);
const TIMEOUT = Number(process.env.TIMEOUT || 240000);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];

async function load(browser, base) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  let trap = '';
  page.on('console', (m) => { if (/unreachable|RuntimeError|panicked|out of memory/i.test(m.text())) trap ||= m.text().slice(0, 160); });
  page.on('pageerror', (e) => { trap ||= e.message.slice(0, 160); });
  page.on('crash', () => { trap ||= 'page crashed'; });
  // The parse worker's own console (a trap there never reaches the page's).
  page.on('worker', (w) => w.on('console', (m) => {
    if (/unreachable|RuntimeError|panicked|out of memory|error/i.test(m.text())) trap ||= `worker: ${m.text().slice(0, 160)}`;
  }));
  const t0 = Date.now();
  await page.goto(`${base}/?file=/blobs/${H}`);
  try {
    await page.waitForFunction(() => {
      const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
      return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
    }, null, { timeout: TIMEOUT, polling: 200 });
  } catch (e) {
    const why = await page.evaluate(() => {
      const p = JSON.parse(window.__fluideda_perf || '{}');
      const s = JSON.parse(window.__fluideda_state || '{}');
      return `busy=${p.busy} frames=${p.frames} loading=${s.loading}`;
    }).catch(() => 'page unresponsive');
    await page.context().close();
    return { failed: trap || `no load in ${TIMEOUT / 1000} s (${why})` };
  }
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
    wall_s: (Date.now() - t0) / 1000,
    parse_ms: m.parse_ms,
    worker_mb: m.parse_heap_bytes / 1e6,
    ttff_ms: m.ttff_ms,
    geometry_ms: m.geometry_ms,
    heap_mb: p.wasm_heap / 1e6,
    gpu_mb: p.gpu_bytes / 1e6,
  };
}

const browser = await launch();
try {
  const r = { A: [], B: [] };
  for (let i = 0; i < RUNS; i++) {
    for (const k of ['A', 'B']) r[k].push(await load(browser, k === 'A' ? A : B));
  }
  for (const k of ['A', 'B']) {
    const bad = r[k].filter((x) => x.failed);
    if (bad.length) console.log(`${k}: ${bad.length} of ${RUNS} loads failed — ${bad[0].failed}`);
  }
  const ok = (k) => r[k].filter((x) => !x.failed);
  if (ok('A').length && ok('B').length) {
    for (const key of Object.keys(ok('B')[0])) {
      const a = ok('A').map((x) => x[key]), b = ok('B').map((x) => x[key]);
      console.log(`${key.padEnd(12)} ${median(a).toFixed(1).padStart(9)} → ${median(b).toFixed(1).padStart(9)}   ` +
        `(${a.map((x) => x.toFixed(0)).join('/')} vs ${b.map((x) => x.toFixed(0)).join('/')})`);
    }
  } else if (ok('B').length) {
    const b = ok('B');
    console.log(`B loads: ${JSON.stringify(b.map((x) => Object.fromEntries(Object.entries(x).map(([k, v]) => [k, Math.round(v)]))))}`);
  }
} finally {
  await browser.close();
}
