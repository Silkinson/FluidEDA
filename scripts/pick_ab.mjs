// Finer streamed picks: the same 128 picks on streamed caravel (the stream
// edit test's eight views, 16 points each), with whole chunks fetched
// (`?wholepicks`, before) and with the server's answers (after), in one
// build. Reports what the picks fetched (bytes, requests) and how long
// they took, and checks both modes pick the same shapes.
//
//   GPU=webgpu node scripts/pick_ab.mjs   (server with FLUIDEDA_LATENCY_MS for a network)
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const VIEWS = [
  [3_150_000, 5_000_000, 30_000], [1_950_000, 5_000_000, 30_000], [150_000, 2_000_000, 30_000], [3_450_000, 1_700_000, 30_000],
  [1_907_290, 1_027_860, 30_000], [3_146_240, 675_220, 30_000], [653_950, 766_300, 30_000], [1_804_110, 441_100, 30_000],
];
const GRID = 4;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
const state = (p) => p.evaluate(() => JSON.parse(window.__fluideda_state || '{}'));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];

function points([cx, cy, span]) {
  const out = [];
  for (let i = 0; i < GRID; i++) {
    for (let j = 0; j < GRID; j++) {
      const f = (k) => (k / (GRID - 1) - 0.5) * span * 0.5;
      out.push([Math.round(cx + f(i)), Math.round(cy + f(j))]);
    }
  }
  return out;
}

async function run(browser, query) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${BASE}/?stream=${H}${query}`);
  await page.waitForFunction(() => JSON.parse(window.__fluideda_perf || '{}').streaming, null, { timeout: 180000, polling: 100 });
  await sleep(1000);
  const p0 = await perf(page);
  const keys = [], ms = [];
  for (const [x, y] of VIEWS.flatMap(points)) {
    const t = Date.now();
    await page.evaluate((c) => { window.__fluideda_cmd = c; }, `pick ${x} ${y}`);
    await page.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
    await page.waitForFunction(() => !JSON.parse(window.__fluideda_perf || '{}').pick_pending, null, { timeout: 30000, polling: 5 });
    ms.push(Date.now() - t);
    // The state is published early in a frame: two more frames, and it
    // holds the pick.
    const f0 = (await perf(page)).frames;
    await page.waitForFunction((f0) => JSON.parse(window.__fluideda_perf || '{}').frames >= f0 + 2, f0, { timeout: 10000, polling: 5 });
    const s = await state(page);
    keys.push(s.selected < 0 ? '-' : `${s.selected_cell}:${s.selected}:${s.sel_points}`);
  }
  const p1 = await perf(page);
  await page.context().close();
  const d = (k) => (p1[k] ?? 0) - (p0[k] ?? 0);
  return {
    keys,
    ms: median(ms),
    max: Math.max(...ms),
    bytes: d('s_fetched_bytes') + d('a_fetched_bytes') + d('p_fetched_bytes'),
    requests: d('s_fetched') + d('a_requests') + d('p_fetched'),
    chunks: d('s_fetched'),
    answers: d('a_fetched'),
  };
}

const browser = await launch();
try {
  const a = await run(browser, '&wholepicks');
  const b = await run(browser, '');
  const same = a.keys.filter((k, i) => k === b.keys[i]).length;
  const hits = b.keys.filter((k) => k !== '-').length;
  const line = (r) => `${(r.bytes / 1e6).toFixed(2)} MB in ${r.requests} requests (${r.chunks} chunks, ${r.answers} answers), ` +
    `pick ${r.ms} ms median (max ${r.max})`;
  console.log(`${a.keys.length} picks (${hits} hit a shape), ${same} the same in both modes`);
  console.log(`  whole chunks: ${line(a)}`);
  console.log(`  answers:      ${line(b)}`);
  process.exit(same === a.keys.length ? 0 : 1);
} finally {
  await browser.close();
}
