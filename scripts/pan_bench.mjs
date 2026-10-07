// Phase 7.1c: panning a streamed design at network latency, two builds side
// by side (A = before, B = after). Run the server with FLUIDEDA_LATENCY_MS
// (e.g. 80): every blob read then waits that long, standing in for a round
// trip. Each build opens caravel streamed on the pad ring and pans one screen
// at a time along it. Per step:
//   * requests — range requests the step sent (geometry and placements,
//     prefetching included);
//   * filled — ms from the pan until every on-screen chunk is drawn;
//   * proxies — whether the frame after the pan still showed proxies where
//     geometry belongs.
// Between steps the page rests until it is idle (up to DWELL ms), as a
// user's pause would. A build without the 7.1c counters (`requests`,
// `unfilled`) sends one request per chunk and has no prefetch, so its
// requests are its fetches and "drawn" is "nothing in flight".
//
//   A=http://127.0.0.1:8081 B=http://127.0.0.1:8080 GPU=webgpu node scripts/pan_bench.mjs
//   (STEPS, SPAN nm, DWELL ms, RTT — the server's latency, for the report;
//   QA/QB — extra query for A/B, e.g. QA='&noprefetch' with A = B)
import { launch } from './launch.mjs';

const A = process.env.A || 'http://127.0.0.1:8081';
const B = process.env.B || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const STEPS = Number(process.env.STEPS || 20);
const SPAN = Number(process.env.SPAN || 120_000);
const DWELL = Number(process.env.DWELL || 3000);
const RTT = Number(process.env.RTT || 80);
const QA = process.env.QA || '', QB = process.env.QB || '';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));

// Along the bottom of caravel's pad ring, a screen at a time.
const Y = 200_000, X0 = 300_000;

const requests = (q) => (q.requests ?? q.stream_fetched) + q.p_fetched;
const bytes = (q) => q.stream_fetched_bytes + (q.p_fetched_bytes ?? 0);
// Every on-screen chunk drawn, and no placement list the view needs still
// missing. (Placement fetches in flight may be the prefetch's, for the next
// view; `p_wanted` counts only what this view needs.)
const drawn = (q) => (q.unfilled ?? q.in_flight) === 0 && q.p_wanted === 0;

async function frames(p, n) {
  const f0 = (await perf(p)).frames;
  await p.waitForFunction(([f0, n]) => JSON.parse(window.__fluideda_perf || '{}').frames >= f0 + n, [f0, n], {
    timeout: 20000,
    polling: 5,
  });
}

async function idle(p) {
  const t0 = Date.now();
  let q = await perf(p);
  while (Date.now() - t0 < DWELL && (q.busy || q.in_flight || q.p_in_flight)) {
    await sleep(25);
    q = await perf(p);
  }
  return q;
}

async function run(browser, base, query) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${base}/?stream=${H}${query}`);
  await page.waitForFunction(() => JSON.parse(window.__fluideda_perf || '{}').streaming, null, { timeout: 180000, polling: 100 });
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, `view ${X0} ${Y} ${SPAN}`);
  await frames(page, 2);
  while (!drawn(await perf(page))) await sleep(25);
  let q = await idle(page);
  const steps = [];
  for (let i = 1; i <= STEPS; i++) {
    const before = q;
    const t0 = Date.now();
    await page.evaluate((c) => { window.__fluideda_cmd = c; }, `view ${X0 + i * SPAN} ${Y} ${SPAN}`);
    await frames(page, 2); // the pan's frame has culled, fetched and published
    const first = await perf(page);
    let now = first;
    while (!drawn(now)) {
      await sleep(5);
      now = await perf(page);
    }
    const filled = Date.now() - t0;
    q = await idle(page);
    const step = { requests: requests(q) - requests(before), bytes: bytes(q) - bytes(before), filled, proxies: !drawn(first) };
    if (process.env.VERBOSE) {
      console.log(`    ${base} step ${i}: ${JSON.stringify(step)} unfilled ${first.unfilled ?? first.in_flight}, ` +
        `prefetched ${(q.prefetched ?? 0) - (before.prefetched ?? 0)}, gpu ${(q.gpu_bytes / 1e6).toFixed(0)} MB`);
    }
    steps.push(step);
  }
  await page.context().close();
  return steps;
}

const browser = await launch();
try {
  const r = { A: await run(browser, A, QA), B: await run(browser, B, QB) };
  const line = (k) => {
    const s = r[k];
    const req = s.reduce((a, x) => a + x.requests, 0) / s.length;
    const mb = s.reduce((a, x) => a + x.bytes, 0) / s.length / 1e6;
    const clear = s.filter((x) => !x.proxies).length / s.length;
    return `${req.toFixed(1)} requests and ${mb.toFixed(2)} MB per step, filled in ${median(s.map((x) => x.filled))} ms (median; ` +
      `max ${Math.max(...s.map((x) => x.filled))}), ${(100 * clear).toFixed(0)}% of steps with no proxies`;
  };
  console.log(`${STEPS} one-screen pans along the pad ring at ${RTT} ms latency:`);
  console.log(`  A: ${line('A')}`);
  console.log(`  B: ${line('B')}`);
} finally {
  await browser.close();
}
