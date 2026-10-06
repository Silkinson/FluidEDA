// Phase 6 SLICE 3: out-of-core streaming, verified in real browsers.
//
//   1. PIXELS — a design streamed from its server pack renders exactly like
//      the same design loaded in-core, wherever no chunk is too dense to
//      fetch: screenshots at several views, compared pixel by pixel.
//   2. RESIDENCY — panning a streamed design under a small GPU budget evicts
//      least-recently-drawn chunks (GPU bytes stay near the budget), and a
//      view revisited is fetched again and renders the same.
//   3. PLACEMENTS (pack v3) — the same for the remote placement lists, under
//      a tiny placement budget.
//
// Prereqs: the server (:9000) holding the design blob, trunk serve (:8080,
// proxying /blobs, /packs). The design defaults to caravel's GDS:
//   curl -X PUT --data-binary @testdata/caravel/gds/caravel.gds \
//        http://127.0.0.1:9000/blobs/$(shasum -a 256 testdata/caravel/gds/caravel.gds | cut -d' ' -f1)
// Run: GPU=webgpu node scripts/stream_test.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
// World views (nm): [cx, cy, span], tight enough that no chunk is dense.
// The first four are the busiest 30 µm windows of a 300 µm scan of caravel
// (pad-ring IO cells, placed inline by the top). The last four sit on the
// densest placement chunks of the cells whose placements are *remote* in a
// pack v3 (fetched by chunk): caravel_core, housekeeping, RAM128 and
// caravel_clocking. (The die's middle is the empty user-project area.)
const VIEWS = [
  [3_150_000, 5_000_000, 30_000],
  [1_950_000, 5_000_000, 30_000],
  [150_000, 2_000_000, 30_000],
  [3_450_000, 1_700_000, 30_000],
  [1_907_290, 1_027_860, 30_000],
  [3_146_240, 675_220, 30_000],
  [653_950, 766_300, 30_000],
  [1_804_110, 441_100, 30_000],
];
const CLIP = { x: 420, y: 0, width: 860, height: 800 }; // the canvas right of the side panel

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
const metrics = (p) => p.evaluate(() => JSON.parse(window.__fluideda_metrics || '{}'));
async function cmd(p, c) {
  await p.evaluate((c) => { window.__fluideda_cmd = c; }, c);
  await p.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
}
// Until nothing is in flight and the resident set has not changed for ~1 s.
async function settle(p) {
  let last = '', stable = 0;
  for (let i = 0; i < 400; i++) {
    const q = await perf(p);
    // Placement chunks (pack v3) arrive before the shape chunks they reveal.
    const sig = `${q.uploaded}/${q.stream_fetched}/${q.in_flight}/${q.p_fetched}/${q.p_in_flight ?? 0}`;
    if (q.in_flight === 0 && !q.p_in_flight && sig === last) { if (++stable >= 5) return q; } else stable = 0;
    last = sig;
    await sleep(200);
  }
  throw new Error('did not settle');
}

// A point on caravel's pad ring at parameter t in [0, 1).
function ring(t) {
  const [x0, y0, x1, y1] = [150_000, 200_000, 3_450_000, 5_000_000];
  const [w, h] = [x1 - x0, y1 - y0];
  let d = t * 2 * (w + h);
  if (d < w) return [x0 + d, y0];
  d -= w;
  if (d < h) return [x1, y0 + d];
  d -= h;
  if (d < w) return [x1 - d, y1];
  return [x0, y1 - (d - w)];
}

async function open(browser, url, ready) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(url);
  await page.waitForFunction(ready, null, { timeout: 180000, polling: 100 });
  return page;
}

async function shoot(page, [cx, cy, span]) {
  await cmd(page, `view ${cx} ${cy} ${span}`);
  const q = await settle(page);
  await sleep(300);
  return { png: await page.screenshot({ clip: CLIP }), perf: q };
}

// Decode two PNGs in the browser and count the pixels that differ.
async function diff(page, a, b) {
  return page.evaluate(async ([a, b]) => {
    const load = async (b64) => {
      const bmp = await createImageBitmap(await (await fetch(`data:image/png;base64,${b64}`)).blob());
      const c = new OffscreenCanvas(bmp.width, bmp.height);
      const g = c.getContext('2d');
      g.drawImage(bmp, 0, 0);
      return g.getImageData(0, 0, bmp.width, bmp.height).data;
    };
    const [x, y] = [await load(a), await load(b)];
    let px = 0, max = 0, lit = 0;
    for (let i = 0; i < x.length; i += 4) {
      const d = Math.max(Math.abs(x[i] - y[i]), Math.abs(x[i + 1] - y[i + 1]), Math.abs(x[i + 2] - y[i + 2]));
      if (d > 0) px++;
      if (d > max) max = d;
      if (x[i] + x[i + 1] + x[i + 2] > 60) lit++;
    }
    return { px, max, lit, total: x.length / 4 };
  }, [a.toString('base64'), b.toString('base64')]);
}

async function main() {
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };
  const browser = await launch();
  try {
    // --- 1. pixels ----------------------------------------------------------
    const local = await open(browser, `${BASE}/?file=/blobs/${H}`, () => {
      const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
      return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
    });
    const inCore = [];
    for (const v of VIEWS) inCore.push(await shoot(local, v));
    const localHeap = (await perf(local)).wasm_heap;
    await local.context().close();

    const streamed = await open(browser, `${BASE}/?stream=${H}`, () => JSON.parse(window.__fluideda_perf || '{}').streaming);
    const sm = await metrics(streamed);
    for (let i = 0; i < VIEWS.length; i++) {
      const s = await shoot(streamed, VIEWS[i]);
      const d = await diff(streamed, inCore[i].png, s.png);
      check(d.px === 0 && s.perf.stream_dense === 0 && s.perf.dense_inst === 0 && s.perf.instances > 100,
        `view ${i + 1} ${VIEWS[i].join(',')}: streamed = in-core pixel for pixel (${d.px} of ${d.total} differ; ` +
        `${s.perf.instances} placements; ${s.perf.uploaded} chunks + ${s.perf.p_resident} placement chunks resident, ` +
        `${(s.perf.gpu_bytes / 1e6).toFixed(1)} MB)`);
    }
    const sp = await perf(streamed);
    console.log(`      streamed: first frame ${sm.ttff_ms.toFixed(0)} ms, fetched ${sp.stream_fetched} chunks ` +
      `(${(sp.stream_fetched_bytes / 1e6).toFixed(1)} MB of ${(sp.stream_geometry_bytes / 1e6).toFixed(0)} MB), ` +
      `wasm heap ${(sp.wasm_heap / 1e6).toFixed(0)} MB (in-core: ${(localHeap / 1e6).toFixed(0)} MB)`);
    await streamed.context().close();

    // --- 2. residency ---------------------------------------------------------
    const BUDGET = 16; // MB
    const pan = await open(browser, `${BASE}/?stream=${H}&stream_budget=${BUDGET}`,
      () => JSON.parse(window.__fluideda_perf || '{}').streaming);
    const first = await shoot(pan, VIEWS[0]);
    // Around the pad ring, where the geometry is. After each view settles the
    // GPU holds at most the budget — or more only when every resident chunk
    // is on screen (eviction never drops what is drawn).
    let peak = 0, steps = 0, over = 0;
    for (let t = 0; t < 1; t += 1 / 24) {
      const [x, y] = ring(t);
      await cmd(pan, `view ${x} ${y} 120000`);
      const q = await settle(pan);
      peak = Math.max(peak, q.gpu_bytes);
      if (q.gpu_bytes > BUDGET * 1_048_576 && q.stream_stale > 0) over++;
      steps++;
    }
    const after = await perf(pan);
    check(after.stream_evicted > 0 && over === 0,
      `panning ${steps} views under a ${BUDGET} MB budget: ${after.stream_evicted} chunks evicted, ` +
      `${after.stream_fetched} fetched; never over budget with an off-screen chunk resident ` +
      `(${over} violations; peak ${(peak / 1e6).toFixed(1)} MB = one view's own geometry)`);
    const back = await shoot(pan, VIEWS[0]);
    const d = await diff(pan, first.png, back.png);
    check(d.px === 0 && back.perf.stream_fetched > after.stream_fetched - 1,
      `the first view, revisited after eviction, is fetched again and matches (${d.px} px differ)`);
    await pan.context().close();

    // --- 3. placement residency (pack v3) -------------------------------------
    // The remote placement lists under a 1 MB placement budget: cycling the
    // four remote-placement views twice must evict placement chunks, and a
    // view revisited after its placements were evicted must render the same.
    const tight = await open(browser, `${BASE}/?stream=${H}&placement_budget=0.25`,
      () => JSON.parse(window.__fluideda_perf || '{}').streaming);
    const remoteViews = VIEWS.slice(4);
    const firstRemote = await shoot(tight, remoteViews[0]);
    for (let round = 0; round < 2; round++) {
      for (const v of remoteViews.slice(1)) await shoot(tight, v);
    }
    const again = await shoot(tight, remoteViews[0]);
    const dr = await diff(tight, firstRemote.png, again.png);
    check(again.perf.p_evicted > 0 && dr.px === 0,
      `placement chunks under a 0.25 MB budget: ${again.perf.p_evicted} evicted, ${again.perf.p_fetched} fetched; ` +
      `the first view re-fetched its placements and matches (${dr.px} px differ)`);
    await tight.context().close();
  } catch (e) {
    check(false, `exception: ${e.message}`);
  } finally {
    await browser.close();
  }
  console.log(fails.length ? `\nSTREAM: ${fails.length} FAILURE(S)` : '\nSTREAM: all checks passed');
  process.exit(fails.length ? 1 : 0);
}
main();
