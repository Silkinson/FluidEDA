// Phase 7.6: the fit view, two builds side by side (A = before, B = after),
// caravel loaded in-core, default (idle-repaint) mode:
//   * idle cost at fit — main-thread busy share over 5 s (each heartbeat
//     frame re-culls the whole view);
//   * fidelity — B's view against A's at fit and mid: the share of pixels
//     visibly off (any channel > 24/255) and the mean absolute difference.
//
//   A=http://127.0.0.1:8081 B=http://127.0.0.1:8080 GPU=webgpu node scripts/fit_ab.mjs
import { launch } from './launch.mjs';

const A = process.env.A || 'http://127.0.0.1:8081';
const B = process.env.B || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const CLIP = { x: 420, y: 0, width: 860, height: 800 };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function view(browser, base, zoom) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await page.goto(`${base}/?file=/blobs/${H}&zoom=${zoom}`);
  await page.waitForFunction(() => {
    const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
    return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
  }, null, { timeout: 240000, polling: 200 });
  let last = '', stable = 0;
  for (let i = 0; i < 300 && stable < 6; i++) {
    const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
    const sig = `${p.uploaded}/${p.in_flight}`;
    stable = !p.in_flight && !p.busy && sig === last ? stable + 1 : 0;
    last = sig;
    await sleep(250);
  }
  await sleep(1000); // past the after-activity linger
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Performance.enable');
  const m = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map((x) => [x.name, x.value]));
  const m0 = await m();
  await sleep(5000);
  const m1 = await m();
  const busy = (m1.TaskDuration - m0.TaskDuration) / (m1.Timestamp - m0.Timestamp);
  const p = await page.evaluate(() => JSON.parse(window.__fluideda_perf));
  const png = await page.screenshot({ clip: CLIP });
  await page.context().close();
  return { busy, png, cull: p.cull_ms, proxies: p.proxies, dense: p.dense_chunks ?? 0, uploaded: p.uploaded, gpu: p.gpu_bytes };
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
    let off = 0, sum = 0, any = 0;
    for (let i = 0; i < x.length; i += 4) {
      const d = Math.max(Math.abs(x[i] - y[i]), Math.abs(x[i + 1] - y[i + 1]), Math.abs(x[i + 2] - y[i + 2]));
      if (d > 24) off++;
      if (d > 0) any++;
      sum += (Math.abs(x[i] - y[i]) + Math.abs(x[i + 1] - y[i + 1]) + Math.abs(x[i + 2] - y[i + 2])) / 3;
    }
    const n = x.length / 4;
    return { off: off / n, any: any / n, mad: sum / n };
  }, [a.toString('base64'), b.toString('base64')]);
}

const browser = await launch();
try {
  const scratch = await (await browser.newContext()).newPage();
  for (const zoom of ['fit', 'mid']) {
    const a = await view(browser, A, zoom);
    const b = await view(browser, B, zoom);
    const d = await diff(scratch, a.png, b.png);
    console.log(
      `${zoom}: idle main thread ${(100 * a.busy).toFixed(1)}% → ${(100 * b.busy).toFixed(1)}%; ` +
        `proxies ${a.proxies} → ${b.proxies}; chunks uploaded ${a.uploaded} → ${b.uploaded}; GPU ${(a.gpu / 1e6).toFixed(0)} → ${(b.gpu / 1e6).toFixed(0)} MB; ` +
        `B vs A: ${(100 * d.any).toFixed(2)}% of pixels differ, ${(100 * d.off).toFixed(2)}% visibly, mean abs diff ${d.mad.toFixed(2)}`
    );
  }
} finally {
  await browser.close();
}
