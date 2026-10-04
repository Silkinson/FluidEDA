// Phase 7.3 SLICE C: how close a streamed design's zoomed-out view comes to
// the same view loaded in-core. Streaming draws a chunk too dense to fetch as
// a proxy box; SLICE C colours that box by the chunk's dominant layer instead
// of neutral grey. `?greyproxies` turns the colouring off — the same-build A/B.
//
// For each design and view: screenshot in-core, streamed (coloured) and
// streamed (grey), and compare each streamed image with the in-core one:
// the share of pixels that differ visibly (any channel off by more than 24 of
// 255) and the mean absolute difference.
//
// Prereqs: server (:9000) holding the designs' blobs, trunk serve (:8080).
// Run: GPU=webgpu node scripts/proxy_fidelity.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const DESIGNS = (process.env.DESIGNS ||
  'caravel:54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7,' +
  'flat_11M:d61d884a598dba5355d7d469642986748adda24703656be996e973e56f081c0d')
  .split(',').map((d) => d.split(':'));
const CLIP = { x: 420, y: 0, width: 860, height: 800 }; // the canvas right of the side panel
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function settled(page) {
  let last = '', stable = 0;
  for (let i = 0; i < 600; i++) {
    const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
    const sig = `${p.uploaded}/${p.in_flight}/${p.stream_fetched}/${p.p_fetched}`;
    if (p.in_flight === 0 && !p.p_in_flight && sig === last) { if (++stable >= 6) return p; } else stable = 0;
    last = sig;
    await sleep(150);
  }
  throw new Error('did not settle');
}

async function shot(browser, query, ready, zoom) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${BASE}/?${query}${zoom ? `&zoom=${zoom}` : ''}`);
  await page.waitForFunction(ready, null, { timeout: 240_000, polling: 100 });
  const p = await settled(page);
  await sleep(400);
  const png = await page.screenshot({ clip: CLIP });
  await page.context().close();
  return { png, p };
}

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
    let off = 0, sum = 0;
    for (let i = 0; i < x.length; i += 4) {
      const d = Math.max(Math.abs(x[i] - y[i]), Math.abs(x[i + 1] - y[i + 1]), Math.abs(x[i + 2] - y[i + 2]));
      if (d > 24) off++;
      sum += (Math.abs(x[i] - y[i]) + Math.abs(x[i + 1] - y[i + 1]) + Math.abs(x[i + 2] - y[i + 2])) / 3;
    }
    const n = x.length / 4;
    return { off: off / n, mad: sum / n };
  }, [a.toString('base64'), b.toString('base64')]);
}

const browser = await launch();
try {
  const scratch = await (await browser.newContext()).newPage();
  for (const [name, hash] of DESIGNS) {
    for (const zoom of ['', 'mid']) {
      const local = await shot(browser, `file=/blobs/${hash}`, () => {
        const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
        return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
      }, zoom);
      const streamedReady = () => JSON.parse(window.__fluideda_perf || '{}').streaming;
      const coloured = await shot(browser, `stream=${hash}`, streamedReady, zoom);
      const grey = await shot(browser, `stream=${hash}&greyproxies`, streamedReady, zoom);
      const dc = await diff(scratch, local.png, coloured.png);
      const dg = await diff(scratch, local.png, grey.png);
      console.log(
        `${name} ${zoom || 'fit'}: pixels visibly off in-core — grey ${(dg.off * 100).toFixed(1)}% → coloured ${(dc.off * 100).toFixed(1)}% ` +
          `(mean abs diff ${dg.mad.toFixed(1)} → ${dc.mad.toFixed(1)}); ${coloured.p.stream_dense} dense shape chunks, ` +
          `${coloured.p.dense_inst} dense placement chunks`
      );
    }
  }
} finally {
  await browser.close();
}
