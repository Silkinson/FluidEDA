// Phase 7.1a: an idle tab should stop drawing. Same build, same session:
// `?continuous` (every frame, the old behaviour) against the default (draw
// while work is pending, else a 4 fps heartbeat).
//
//   1. IDLE — frames drawn and main-thread busy time over 5 s with nothing
//      happening: the embedded sample, caravel in-core and caravel streamed.
//   2. PAN — frames per second while the mouse drags the view (input must
//      still get every frame).
//   3. LOAD — caravel's first frame, in-core and streamed.
//   4. SYNC — how long a peer's edit takes to show in an idle page (the
//      socket must wake it).
//
// Prereqs: server (:9000) holding caravel's blob, trunk serve (:8080).
// Run: GPU=webgpu node scripts/idle_bench.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
const state = (p) => p.evaluate(() => JSON.parse(window.__fluideda_state || '{}'));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];

async function open(browser, query) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  const t = Date.now();
  await page.goto(`${BASE}/?${query}`);
  // A file load must report the file's own metrics, not the embedded
  // sample's (shown until it arrives).
  const big = query.includes('file=');
  await page.waitForFunction((big) => {
    const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
    const p = JSON.parse(window.__fluideda_perf || '{}');
    return m && m.ttff_ms > 0 && (p.streaming || (m.geometry_ms > 0 && (!big || m.file_bytes > 1e6)));
  }, big, { timeout: 180000, polling: 100 });
  return { page, wall: Date.now() - t };
}

// Wait for loading, fetching and tessellation to finish, then for the
// after-activity linger to run out.
async function quiet(page) {
  let last = '', stable = 0;
  for (let i = 0; i < 300 && stable < 5; i++) {
    const q = await perf(page);
    const sig = `${q.uploaded}/${q.in_flight}/${q.stream_fetched}/${q.p_fetched}`;
    stable = !q.in_flight && !q.p_in_flight && sig === last ? stable + 1 : 0;
    last = sig;
    await sleep(200);
  }
  await sleep(1500);
}

// Frames drawn and main-thread busy fraction over `ms` of nothing happening.
async function idle(page, ms = 5000) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Performance.enable');
  const m = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map((x) => [x.name, x.value]));
  const [p0, m0] = [await perf(page), await m()];
  await sleep(ms);
  const [p1, m1] = [await perf(page), await m()];
  return {
    fps: ((p1.frames - p0.frames) * 1000) / ms,
    busy: (m1.TaskDuration - m0.TaskDuration) / (m1.Timestamp - m0.Timestamp),
  };
}

// Drag the view for `ms`, one mouse move per 16 ms; frames per second.
async function pan(page, ms = 2000) {
  const p0 = await perf(page);
  const t0 = Date.now();
  await page.mouse.move(900, 400);
  await page.mouse.down();
  for (let i = 0; Date.now() - t0 < ms; i++) {
    await page.mouse.move(900 + 150 * Math.sin(i / 20), 400 + 100 * Math.cos(i / 25));
    await sleep(16);
  }
  await page.mouse.up();
  const p1 = await perf(page);
  return ((p1.frames - p0.frames) * 1000) / (Date.now() - t0);
}

async function main() {
  const browser = await launch();
  const rows = [];
  try {
    for (const [label, query] of [
      ['embedded sample', ''],
      ['caravel in-core', `file=/blobs/${H}`],
      ['caravel streamed', `stream=${H}`],
    ]) {
      const r = {};
      for (const mode of ['continuous', 'default']) {
        const q = [query, mode === 'continuous' ? 'continuous' : ''].filter(Boolean).join('&');
        const { page } = await open(browser, q);
        await quiet(page);
        r[mode] = await idle(page);
        r[`${mode}_pan`] = await pan(page);
        await page.context().close();
      }
      rows.push([label, r]);
      console.log(
        `IDLE ${label}: ${r.continuous.fps.toFixed(1)} → ${r.default.fps.toFixed(1)} frames/s, main thread busy ` +
          `${(100 * r.continuous.busy).toFixed(1)}% → ${(100 * r.default.busy).toFixed(1)}%; ` +
          `while panning ${r.continuous_pan.toFixed(1)} → ${r.default_pan.toFixed(1)} frames/s`
      );
    }

    // LOAD: first frame and (in-core) all geometry in, alternating modes,
    // 3 runs each.
    for (const [label, query, key] of [
      ['caravel in-core, first frame', `file=/blobs/${H}`, 'ttff_ms'],
      ['caravel in-core, geometry in', `file=/blobs/${H}`, 'geometry_ms'],
      ['caravel streamed, first frame', `stream=${H}`, 'ttff_ms'],
    ]) {
      const t = { continuous: [], default: [] };
      for (let i = 0; i < 3; i++) {
        for (const mode of ['continuous', 'default']) {
          const q = [query, mode === 'continuous' ? 'continuous' : ''].filter(Boolean).join('&');
          const { page } = await open(browser, q);
          const m = await page.evaluate(() => JSON.parse(window.__fluideda_metrics));
          t[mode].push(m[key]);
          await page.context().close();
        }
      }
      console.log(`LOAD ${label}: median ${median(t.continuous).toFixed(0)} → ${median(t.default).toFixed(0)} ms ` +
        `(${t.continuous.map((x) => x.toFixed(0)).join('/')} vs ${t.default.map((x) => x.toFixed(0)).join('/')})`);
    }

    // SYNC: A edits, B (idle) shows it.
    for (const mode of ['continuous', 'default']) {
      const room = `idle-${mode}-${Date.now()}`;
      const flag = mode === 'continuous' ? '&continuous' : '';
      const url = `${BASE}/?file=/assets/edit_fixture.gds&room=${room}${flag}`;
      const pages = [];
      for (let i = 0; i < 2; i++) {
        const page = await (await browser.newContext()).newPage();
        await page.goto(url);
        await page.waitForFunction(() => JSON.parse(window.__fluideda_state || '{}').synced, null, { timeout: 30000 });
        pages.push(page);
      }
      const [A, B] = pages;
      await sleep(2000); // both idle
      const lat = [];
      for (let k = 0; k < 5; k++) {
        await A.evaluate((k) => { window.__fluideda_cmd = `select ${k}`; }, k);
        await A.waitForFunction(() => !window.__fluideda_cmd);
        await sleep(100);
        const t = Date.now();
        await A.evaluate(() => { window.__fluideda_cmd = 'nudge'; });
        await B.waitForFunction((n) => JSON.parse(window.__fluideda_state || '{}').moves === n, k + 1, { timeout: 10000, polling: 5 });
        lat.push(Date.now() - t);
        await sleep(1500); // let both go idle again
      }
      console.log(`SYNC ${mode}: an edit in A shows in idle B after median ${median(lat)} ms (${lat.join('/')})`);
      for (const p of pages) await p.context().close();
    }
  } finally {
    await browser.close();
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
