// Browser measurement harness for Phase 1.5.
//
// Loads the release wasm app in Playwright's bundled Chromium, opens each GDS via
// the app's ?file= autoload, and scrapes window.__fluideda_metrics (parse /
// tessellate / index / TTFF / wasm-heap, all measured inside wasm) plus a
// live fps sample at fit / mid / leaf. Records OOM / crash as a failure result.
//
// CPU + memory numbers are representative of a real browser; fps is NOT if the
// GPU is software (SwiftShader) -- the harness reports the detected renderer so
// the findings can caveat it.
//
// Run:  node scripts/browser_bench.mjs
// Requires a running server (scripts/serve_bench.py) on PORT below.

import { chromium } from 'playwright';
import { launchArgs } from './launch.mjs';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;

// file = path served under /testdata; zoom cases measured for fps.
const CASES = [
  { name: 'hier',    file: null, zooms: ['fit', 'leaf'] }, // embedded default
  { name: 'sky130',  file: '/testdata/sky130_fd_sc_hd_merged.gds', zooms: ['fit', 'mid', 'leaf'] },
  { name: 'caravel', file: '/testdata/caravel/gds/caravel.gds',     zooms: ['fit', 'mid', 'leaf'] },
];

const LOAD_TIMEOUT_MS = 240_000; // caravel is 278MB: allow generous fetch+parse

// Count rAF ticks over a wall-clock window. setTimeout (not rAF) is the clock, so
// this always resolves even when headless throttles rAF -- a low count then just
// reflects the (non-representative) headless compositor, which we caveat.
async function measureFps(page, ms = 2000) {
  return await page.evaluate((ms) => {
    return new Promise((resolve) => {
      let frames = 0;
      const t0 = performance.now();
      const onRaf = () => { frames++; requestAnimationFrame(onRaf); };
      requestAnimationFrame(onRaf);
      setTimeout(() => {
        const dt = performance.now() - t0;
        resolve(dt > 0 ? Math.round((frames * 1000) / dt) : null);
      }, ms);
    });
  }, ms);
}

async function detectRenderer(page) {
  return await page.evaluate(() => {
    const out = { webgpu: !!navigator.gpu, gl: null };
    try {
      const c = document.createElement('canvas');
      const gl = c.getContext('webgl2') || c.getContext('webgl');
      if (gl) {
        const dbg = gl.getExtension('WEBGL_debug_renderer_info');
        out.gl = dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : 'webgl(masked)';
      }
    } catch (e) {}
    return out;
  });
}

async function runCase(browser, c) {
  const results = [];
  for (const zoom of c.zooms) {
    const context = await browser.newContext();
    // WebGL modes: hide navigator.gpu before any page script runs so wgpu
    // selects WebGL2 (the historical baseline). WebGPU modes leave it alone.
    if (!(process.env.GPU || 'webgl-metal').startsWith('webgpu')) {
      await context.addInitScript(() => {
        try { Object.defineProperty(navigator, 'gpu', { get: () => undefined, configurable: true }); } catch (e) {}
      });
    }
    const page = await context.newPage();
    const errors = [];
    let crashed = false;
    page.on('pageerror', (e) => errors.push(String(e)));
    page.on('crash', () => { crashed = true; });
    page.on('console', (m) => {
      const t = m.text();
      if (/panic|RuntimeError|out of memory|memory access|unreachable|abort/i.test(t)) {
        errors.push(t.slice(0, 300));
      }
    });

    const q = new URLSearchParams();
    if (c.file) q.set('file', c.file);
    q.set('zoom', zoom);
    const url = `${BASE}/?${q.toString()}`;

    const t0 = Date.now();
    let metrics = null;
    let renderer = null;
    try {
      await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 60_000 });
      renderer = await detectRenderer(page);
      // Wait for the wasm app to publish metrics. If a file was requested, wait
      // until the published metrics reflect it (file_bytes bigger than the ~1KB
      // embedded default) rather than the default that loads first.
      const minBytes = c.file ? 100_000 : 0;
      await page.waitForFunction(
        (min) => {
          try {
            const m = JSON.parse(window.__fluideda_metrics);
            return m && m.file_bytes >= min && m.ttff_ms > 0;
          } catch { return false; }
        },
        minBytes,
        { timeout: LOAD_TIMEOUT_MS, polling: 500 }
      );
      metrics = JSON.parse(await page.evaluate(() => window.__fluideda_metrics));
    } catch (e) {
      errors.push(`timeout/err after ${Date.now() - t0}ms: ${String(e).slice(0, 200)}`);
    }

    let fps = null;
    if (metrics && !crashed) {
      try { fps = await measureFps(page, 2000); } catch (e) { errors.push('fps: ' + e); }
    }

    results.push({ zoom, url, ok: !!metrics && !crashed, crashed, wall_ms: Date.now() - t0, metrics, fps, renderer, errors });
    await page.close();
    await context.close();
  }
  return { name: c.name, file: c.file, results };
}

(async () => {
  // GPU mode (scripts/launch.mjs): default here is WebGL2 on ANGLE-Metal, the
  // historical baseline (wgpu 22's device limits made this Chromium reject
  // WebGPU). Since wgpu 30, GPU=webgpu measures real WebGPU on Metal; the app
  // reports what it got in metrics.backend.
  const mode = process.env.GPU || 'webgl-metal';
  process.stderr.write(`GPU mode: ${mode}\n`);
  const browser = await chromium.launch({
    headless: true,
    args: [...launchArgs(mode), '--js-flags=--max-old-space-size=8192'],
  });
  const all = [];
  // CASE=<name>[,<name>…] runs a subset (e.g. CASE=caravel for repeat runs).
  const only = process.env.CASE ? process.env.CASE.split(',') : null;
  for (const c of CASES.filter((c) => !only || only.includes(c.name))) {
    process.stderr.write(`\n### ${c.name} (${c.file || 'embedded'})\n`);
    const r = await runCase(browser, c);
    for (const res of r.results) {
      const m = res.metrics;
      if (res.ok) {
        process.stderr.write(
          `  ${res.zoom.padEnd(5)} ok  ttff=${m.ttff_ms.toFixed(0)}ms parse=${m.parse_ms.toFixed(0)} ` +
          `tess=${m.tess_ms.toFixed(0)} heap=${(m.wasm_heap_bytes/1048576).toFixed(0)}MB fps=${res.fps} backend=${m.backend}\n`
        );
      } else {
        process.stderr.write(`  ${res.zoom.padEnd(5)} FAIL (${res.crashed ? 'crash' : 'timeout'}) ${res.errors.slice(0,1).join(' | ')}\n`);
      }
    }
    all.push(r);
  }
  await browser.close();
  console.log(JSON.stringify(all, null, 2));
})();
