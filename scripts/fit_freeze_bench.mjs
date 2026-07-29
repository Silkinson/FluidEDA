// Fit-view tessellation offload measurement (Phase 4 STEP 2).
//
// Loads each design at fit and reads `window.__fluideda_perf.max_frame_ms` — the
// longest main-thread `update()` since the design installed, i.e. THE FREEZE. The
// pre-STEP-2 inline path spikes it to seconds; the worker offload keeps it small.
// Each design is run twice on ONE build for a clean before/after:
//   * offload  — the shipped path (worker tessellation + bbox proxies),
//   * inline   — `?tessinline=1`, the old main-thread path.
// Also: a leaf run (must stay inline — few chunks — so leaf is unchanged), and an
// edit run (drive select_biggest + nudge on the big flat cell, read last_edit_ms).
//
// Numbers are from headless Chromium (WebGL2 / SwiftShader — WebGPU is rejected
// headless), so absolute fps is NOT representative; but the freeze is a MAIN-THREAD
// (CPU tessellation) block, which `max_frame_ms` measures faithfully regardless of
// the GPU. Run:  python3 scripts/serve_bench.py 8090 &  then  node scripts/fit_freeze_bench.mjs

import { chromium } from 'playwright';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;
const LOAD_TIMEOUT = 300_000;

const CASES = [
  { name: 'caravel',         file: '/testdata/caravel/gds/caravel.gds' },
  { name: 'caravan-signoff', file: '/testdata/caravel/gds/caravan-signoff.gds' },
];

const perf = (page) => page.evaluate(() => (window.__fluideda_perf ? JSON.parse(window.__fluideda_perf) : null));
const metrics = (page) => page.evaluate(() => (window.__fluideda_metrics ? JSON.parse(window.__fluideda_metrics) : {}));

// Wait until the TARGET file has actually loaded (its metrics — not the embedded
// hier.gds default that loads first — with a matching name and a real size), then
// until geometry settles: no chunks in flight and the uploaded count stable ~2 s.
async function waitSettled(page, basename, timeoutMs) {
  await page.waitForFunction(
    (bn) => {
      const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
      return m && m.file_bytes > 1_000_000 && String(m.name).includes(bn);
    },
    basename,
    { timeout: timeoutMs, polling: 200 }
  );
  const t0 = Date.now();
  let last = -1, stableSince = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const p = await perf(page);
    if (p) {
      if (p.uploaded === last && p.in_flight === 0) {
        if (Date.now() - stableSince > 2000) return p;
      } else { last = p.uploaded; stableSince = Date.now(); }
    }
    await page.waitForTimeout(250);
  }
  return await perf(page);
}

const baseOf = (file) => file.split('/').pop();

async function sendCmd(page, cmd) {
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, cmd);
  // The in-app hook consumes the command and clears the global; wait for that.
  await page.waitForFunction(() => window.__fluideda_cmd === '', null, { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(250);
}

async function fitRun(page, file, inline) {
  const url = `${BASE}/?file=${encodeURIComponent(file)}&zoom=fit${inline ? '&tessinline=1' : ''}`;
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  const p = await waitSettled(page, baseOf(file), LOAD_TIMEOUT);
  const m = await metrics(page);
  return { p, m };
}

async function editRun(page, file) {
  const url = `${BASE}/?file=${encodeURIComponent(file)}&zoom=leaf`;
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  await waitSettled(page, baseOf(file), LOAD_TIMEOUT);
  // Viewport-independent first-edit of the big flat cell (finer-chunking + re-tess).
  await sendCmd(page, 'edit_bench');
  await page.waitForTimeout(500);
  return await perf(page);
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on('console', (m) => { const t = m.text(); if (/panic|error|OOM/i.test(t)) console.log('  [page]', t); });

  const renderer = await page.goto(BASE).then(() => page.evaluate(() => {
    try { const gl = document.createElement('canvas').getContext('webgl2');
      const dbg = gl && gl.getExtension('WEBGL_debug_renderer_info');
      return dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : 'unknown'; } catch { return 'unknown'; }
  }));
  console.log(`renderer: ${renderer}  (headless — fps not representative; max_frame_ms is)\n`);

  for (const c of CASES) {
    console.log(`=== ${c.name} ===`);
    try {
      const off = await fitRun(page, c.file, false);
      console.log(`  fit OFFLOAD : longest frame ${off.p.max_frame_ms.toFixed(0)} ms  |  ` +
        `TTFF ${off.m.ttff_ms?.toFixed(0)} ms, parse ${off.m.parse_ms?.toFixed(0)} ms, ` +
        `worker-tess ${off.p.worker_chunks} chunks, inline-tess ${off.p.inline_tess_ms.toFixed(0)} ms`);

      const inl = await fitRun(page, c.file, true);
      console.log(`  fit INLINE  : longest frame ${inl.p.max_frame_ms.toFixed(0)} ms  |  ` +
        `TTFF ${inl.m.ttff_ms?.toFixed(0)} ms  (worker off; the pre-STEP-2 freeze)`);

      // Leaf: should stay inline (few big chunks) — worker_chunks 0, small frame.
      await page.goto(`${BASE}/?file=${encodeURIComponent(c.file)}&zoom=leaf`, { waitUntil: 'domcontentloaded' });
      const lp = await waitSettled(page, baseOf(c.file), LOAD_TIMEOUT);
      console.log(`  leaf        : longest frame ${lp.max_frame_ms.toFixed(0)} ms, ` +
        `worker-tess ${lp.worker_chunks} chunks (0 ⇒ stayed inline = unchanged), inline-tess ${lp.inline_tess_ms.toFixed(0)} ms`);

      const ed = await editRun(page, c.file);
      console.log(`  edit        : last edit ${ed.last_edit_ms.toFixed(2)} ms over ${ed.last_edit_chunks} chunk(s)  ` +
        `(target < 16 ms)`);

      const speedup = inl.p.max_frame_ms / Math.max(off.p.max_frame_ms, 0.1);
      console.log(`  => freeze ${inl.p.max_frame_ms.toFixed(0)} ms → ${off.p.max_frame_ms.toFixed(0)} ms ` +
        `(${speedup.toFixed(0)}× shorter longest frame)\n`);
    } catch (e) {
      console.log(`  FAILED: ${e.message}\n`);
    }
  }

  await browser.close();
}

main().catch((e) => { console.error(e); process.exit(1); });
