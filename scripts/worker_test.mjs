// Phase 2b STEP 2 verification: off-main-thread parse.
//
// Loads a GDS via the worker path and checks:
//   1. it parsed in the worker (worker_used=true) and rendered (no crash);
//   2. the main thread stayed responsive during load — the longest single
//      main-thread frame (max requestAnimationFrame gap) is reported, target
//      < ~150 ms (one reconstruction frame; the old sync parse froze ~1.2 s);
//   3. combined peak heap (worker + main) is reported, well under 4 GB.
//
// Prereq: dist built (trunk build --release) and served with testdata:
//   python3 scripts/serve_bench.py 8090 &
// Run: node scripts/worker_test.mjs [/testdata/....gds]
import { launch } from './launch.mjs';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;
const FILE = process.argv[2] || '/testdata/caravel/gds/caravel.gds';
// Zoom for the load view. `leaf`/`mid` tessellate almost nothing (Phase 2b
// chunking), so at those zooms PARSE was the main-thread freeze — exactly what
// the worker offloads. `fit` also tessellates the giant flat cells on the main
// thread (~2.5 s), which the worker does NOT address (parse-only); measure there
// to see that separately.
const ZOOM = process.env.ZOOM || 'leaf';
const TIMEOUT = 240_000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  const browser = await launch();
  const ctx = await browser.newContext();
  const page = await ctx.newPage();

  // Track the largest gap between animation frames — a long gap == a frozen
  // main thread. Installed before any app code runs.
  await page.addInitScript(() => {
    window.__maxFrameGap = 0;
    window.__frames = 0;
    window.__spikes = [];
    window.__t0 = performance.now();
    let last = performance.now();
    const tick = (t) => {
      const gap = t - last;
      last = t;
      window.__frames++;
      if (gap > window.__maxFrameGap) window.__maxFrameGap = gap;
      if (gap > 60) window.__spikes.push({ at: Math.round(t - window.__t0), gap: Math.round(gap) });
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });

  page.on('console', (m) => {
    const t = m.text();
    if (/error|panic|FLUIDEDA_METRICS/i.test(t)) console.log('  [page]', t.slice(0, 200));
  });

  const url = `${BASE}/?file=${encodeURIComponent(FILE)}&zoom=${ZOOM}`;
  console.log(`loading ${url}`);
  await page.goto(url);

  // Let the app boot (wasm compile + embedded hier render is a one-time cost,
  // present with or without the worker), then reset the frame-gap monitor so it
  // measures ONLY the target file's load (fetch + worker parse + reconstruct).
  await page.waitForFunction(() => window.__fluideda_metrics != null, { timeout: 30000 }).catch(() => {});
  await page.evaluate(() => { window.__maxFrameGap = 0; window.__frames = 0; window.__spikes = []; window.__t0 = performance.now(); });
  console.log('app booted; measuring frame gaps during file load…');

  // Wait for the loaded file's metrics (name != embedded hier).
  const fileName = FILE.split('/').pop();
  const t0 = Date.now();
  let metrics = null;
  for (;;) {
    metrics = await page.evaluate(() => {
      try { return JSON.parse(window.__fluideda_metrics || 'null'); } catch { return null; }
    });
    if (metrics && metrics.name && metrics.name.includes(fileName.replace('.gds', ''))) break;
    if (Date.now() - t0 > TIMEOUT) { console.log('TIMEOUT waiting for metrics'); break; }
    await sleep(200);
  }
  // Let a few more frames run so the max-gap settles after reconstruction.
  await sleep(1500);

  const maxGap = await page.evaluate(() => window.__maxFrameGap);
  // The app's own view: longest update() since the design installed (the
  // install frame itself included) — separates the reply handler (a task
  // between frames) from the first frames of the new design.
  const appPerf = await page.evaluate(() => { try { return JSON.parse(window.__fluideda_perf || '{}'); } catch { return {}; } });
  const frames = await page.evaluate(() => window.__frames);
  const spikes = await page.evaluate(() => window.__spikes);


  await browser.close();
  console.log('frame spikes (>60ms) during load:', JSON.stringify(spikes));

  if (!metrics) { console.log('FAIL: no metrics'); process.exit(1); }
  const mb = (b) => (b / 1048576).toFixed(0);
  const workerHeap = metrics.parse_heap_bytes;   // worker path: the worker's parse peak
  const mainHeap = metrics.wasm_heap_bytes;       // main-thread heap after reconstruct
  const combined = workerHeap + mainHeap;

  console.log('\n=== results ===');
  console.log(`file:            ${metrics.name}  (${mb(metrics.file_bytes)} MB)`);
  console.log(`worker_used:     ${metrics.worker_used}`);
  console.log(`parse (worker):  ${metrics.parse_ms} ms`);
  console.log(`reconstruct:     ${metrics.reconstruct_ms.toFixed(1)} ms main-thread, over ${metrics.reconstruct_frames} frame(s) (budgeted)`);
  console.log(`TTFF:            ${metrics.ttff_ms.toFixed(0)} ms  (first frame of the new design — skeleton proxies)`);
  console.log(`geometry in:     ${metrics.geometry_ms.toFixed(0)} ms  (every cell's shapes in place)`);
  console.log(`worker heap:     ${mb(workerHeap)} MB  (parse peak, off main)`);
  console.log(`main heap:       ${mb(mainHeap)} MB`);
  console.log(`COMBINED peak:   ${mb(combined)} MB  (target << 4096)`);
  console.log(`longest frame:   ${maxGap.toFixed(0)} ms  (${frames} frames total)`);
  console.log(`longest update() since install: ${(appPerf.max_frame_ms ?? -1).toFixed(1)} ms`);

  const checks = [];
  const check = (c, m) => { console.log(`${c ? 'PASS' : 'FAIL'}  ${m}`); if (!c) checks.push(m); };
  check(metrics.worker_used === true, 'parsed in the worker (worker_used)');
  check(combined < 4096 * 1048576, 'combined peak heap under 4 GB');
  check(maxGap < 300, `no main-thread frame > 300 ms during load (was ${maxGap.toFixed(0)} ms)`);
  check(metrics.reconstruct_frames > 1, `reconstruction spread over frames (${metrics.reconstruct_frames})`);
  console.log(checks.length ? `\nWORKER-TEST: ${checks.length} FAILURE(S)` : '\nWORKER-TEST: all checks passed');
  process.exit(checks.length ? 1 : 0);
}
main();
