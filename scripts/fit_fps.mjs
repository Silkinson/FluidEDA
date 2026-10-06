// Phase 7.6: frame rate at caravel's fit, mid and leaf views, loaded in-core,
// counted from the app's own frames (`?continuous`: every frame drawn, as the
// old fps numbers were). One or two builds (A/B), WebGPU or WebGL.
//
//   GPU=webgpu A=http://127.0.0.1:8081 B=http://127.0.0.1:8080 node scripts/fit_fps.mjs
//   (B alone measures one build; ZOOMS=fit,mid,leaf; MS = window per view;
//   GPU=webgl-* adds `?webgl`, which keeps the app on WebGL even though
//   headless Chromium now offers WebGPU too)
import { launch, GPU_MODE } from './launch.mjs';

const A = process.env.A || '';
const B = process.env.B || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const ZOOMS = (process.env.ZOOMS || 'fit,mid,leaf').split(',');
const MS = Number(process.env.MS || 4000);
const RUNS = Number(process.env.RUNS || 2);
const WEBGL = GPU_MODE.startsWith('webgl') ? '&webgl' : '';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (v) => [...v].sort((a, b) => a - b)[v.length >> 1];

async function measure(browser, base, zoom) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${base}/?file=/blobs/${H}&continuous${WEBGL}&zoom=${zoom}`);
  await page.waitForFunction(() => {
    const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
    return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
  }, null, { timeout: 240000, polling: 200 });
  // Everything on screen uploaded (or proxied) before counting.
  let last = '', stable = 0;
  for (let i = 0; i < 300 && stable < 6; i++) {
    const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
    const sig = `${p.uploaded}/${p.in_flight}`;
    stable = !p.in_flight && sig === last ? stable + 1 : 0;
    last = sig;
    await sleep(250);
  }
  const perf = () => page.evaluate(() => JSON.parse(window.__fluideda_perf));
  const p0 = await perf();
  const t0 = Date.now();
  await sleep(MS);
  const p1 = await perf();
  const fps = ((p1.frames - p0.frames) * 1000) / (Date.now() - t0);
  const m = await page.evaluate(() => JSON.parse(window.__fluideda_metrics));
  await page.context().close();
  return { fps, cull_ms: p1.cull_ms, draws: p1.draw_calls, proxies: p1.proxies, gpu_mb: p1.gpu_bytes / 1e6, backend: m.backend };
}

const browser = await launch();
try {
  for (const zoom of ZOOMS) {
    const r = { A: [], B: [] };
    for (let i = 0; i < RUNS; i++) {
      for (const k of A ? ['A', 'B'] : ['B']) r[k].push(await measure(browser, k === 'A' ? A : B, zoom));
    }
    const line = (k) => {
      const v = r[k];
      const f = (key) => median(v.map((x) => x[key]));
      return `${f('fps').toFixed(1)} fps (cull ${f('cull_ms').toFixed(1)} ms, ${f('draws')} draws, ${f('proxies')} proxies, ${f('gpu_mb').toFixed(0)} MB GPU)`;
    };
    console.log(`${zoom.padEnd(4)} ${r.B[0].backend}: ${A ? `A ${line('A')} → ` : ''}B ${line('B')}`);
  }
} finally {
  await browser.close();
}
