// Phase 7.9: saving an in-core design, two builds side by side (A = before,
// B = after). Each build loads the design, saves it as GDS and then as
// OASIS, and reports the main heap before and after each save (wasm memory
// never shrinks, so what a save adds stays for the life of the page), the
// time to the download, and the file's SHA-256 — both builds must write the
// same bytes. A save that fails (a wasm trap, a timeout) is reported as such.
//
//   A=http://127.0.0.1:8081 B=http://127.0.0.1:8080 GPU=webgpu node scripts/save_ab.mjs
//   (DESIGN=<sha256>, FORMATS=gds,oas, TIMEOUT ms per step)
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { launch } from './launch.mjs';

const A = process.env.A || 'http://127.0.0.1:8081';
const B = process.env.B || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const FORMATS = (process.env.FORMATS || 'gds,oas').split(',');
const TIMEOUT = Number(process.env.TIMEOUT || 300000);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));

async function frames(p, n = 2) {
  const f0 = (await perf(p)).frames;
  await p.waitForFunction(([f0, n]) => JSON.parse(window.__fluideda_perf || '{}').frames >= f0 + n, [f0, n], {
    timeout: TIMEOUT,
    polling: 16,
  });
}

async function run(browser, base) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true })).newPage();
  let trap = '';
  page.on('console', (m) => {
    if (/unreachable|RuntimeError|panicked|out of memory|memory allocation/i.test(m.text())) trap ||= m.text().slice(0, 160);
  });
  page.on('pageerror', (e) => { trap ||= e.message.slice(0, 160); });
  page.on('crash', () => { trap ||= 'page crashed'; });
  await page.goto(`${base}/?file=/blobs/${H}`);
  await page.waitForFunction(() => {
    const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
    return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
  }, null, { timeout: TIMEOUT, polling: 200 });
  let last = '', stable = 0;
  for (let i = 0; i < 300 && stable < 5; i++) {
    const p = await perf(page);
    const sig = `${p.uploaded}/${p.in_flight}`;
    stable = !p.in_flight && sig === last ? stable + 1 : 0;
    last = sig;
    await sleep(200);
  }
  const out = { heap0: (await perf(page)).wasm_heap };
  for (const f of FORMATS) {
    const t = Date.now();
    try {
      // The download, or the page's failure — whichever comes first.
      let settled = false;
      const trapped = (async () => {
        while (!trap && !settled) await sleep(500);
        if (trap) throw new Error(trap);
      })();
      const [dl] = await Promise.race([
        Promise.all([
          page.waitForEvent('download', { timeout: TIMEOUT }),
          page.evaluate((c) => { window.__fluideda_cmd = c; }, `save ${f}`),
        ]),
        trapped,
      ]).finally(() => { settled = true; });
      const bytes = readFileSync(await dl.path());
      await frames(page);
      out[f] = {
        mb: bytes.length / 1e6,
        sha: createHash('sha256').update(bytes).digest('hex'),
        ms: Date.now() - t,
        heap: (await perf(page)).wasm_heap,
      };
    } catch (e) {
      out[f] = { failed: trap || String(e.message).slice(0, 160) };
      break;
    }
  }
  await page.context().close();
  return out;
}

const browser = await launch();
try {
  const r = { A: await run(browser, A), B: await run(browser, B) };
  const mb = (b) => (b / 1e6).toFixed(0);
  console.log(`main heap after load: A ${mb(r.A.heap0)} MB, B ${mb(r.B.heap0)} MB`);
  for (const f of FORMATS) {
    const line = (k) => {
      const s = r[k][f];
      if (!s) return 'not reached';
      if (s.failed) return `FAILED (${s.failed})`;
      return `${s.ms} ms, heap → ${mb(s.heap)} MB`;
    };
    const [a, b] = [r.A[f], r.B[f]];
    const same = a?.sha && b?.sha
      ? a.sha === b.sha
        ? `same file (${b.mb.toFixed(1)} MB, sha ${b.sha.slice(0, 12)})`
        : `files differ: A ${a.mb.toFixed(3)} MB sha ${a.sha.slice(0, 12)}, B ${b.mb.toFixed(3)} MB sha ${b.sha.slice(0, 12)}`
      : '';
    console.log(`save ${f}: A ${line('A')} | B ${line('B')} ${same}`);
  }
} finally {
  await browser.close();
}
