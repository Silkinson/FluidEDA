// Phase 6: what the design digest costs a load. The parse worker hashes the
// file (WebCrypto SHA-256) after replying with the parse; `?nohash` skips it.
// Alternating runs on ONE build, per design:
//   geometry_ms — load start → every cell's geometry in place (the load)
//   ttff_ms     — load start → the skeleton's first frame
//   parse_ms    — the worker's wasm parse alone
//   hash_ms     — the digest, in the worker, after its reply
// (The first version hashed BEFORE the parse, overlapping it: the digest was
// hidden but WebCrypto's input copy was not — +130 ms TTFF on caravel.gds.)
//
// Run:  python3 scripts/serve_bench.py 8090 &  then  node scripts/hash_bench.mjs
import { launch } from './launch.mjs';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;
const RUNS = Number(process.env.RUNS || 3);
const CASES = [
  { name: 'caravel.gds', file: '/testdata/caravel/gds/caravel.gds' },
  { name: 'caravel.oas', file: '/testdata/caravel/gds/caravel.oas' },
];

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

async function loadOnce(browser, file, nohash) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  const base = file.split('/').pop();
  await page.goto(`${BASE}/?file=${file}${nohash ? '&nohash' : ''}`);
  await page.waitForFunction(
    (bn) => {
      const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
      return m && String(m.name).includes(bn) && m.geometry_ms > 0;
    },
    base,
    { timeout: 300_000, polling: 100 }
  );
  const m = await page.evaluate(() => JSON.parse(window.__fluideda_metrics));
  await ctx.close();
  return m;
}

const browser = await launch();
try {
  for (const c of CASES) {
    const runs = { hash: [], nohash: [] };
    for (let i = 0; i < RUNS; i++) {
      for (const variant of ['hash', 'nohash']) {
        const m = await loadOnce(browser, c.file, variant === 'nohash');
        runs[variant].push(m);
        console.log(
          `${c.name} ${variant.padEnd(6)} geometry ${m.geometry_ms.toFixed(0).padStart(5)} ms  ttff ${m.ttff_ms
            .toFixed(0)
            .padStart(5)} ms  parse ${m.parse_ms.toFixed(0).padStart(5)} ms  hash ${m.hash_ms.toFixed(0)} ms  ${
            m.hash ? m.hash.slice(0, 12) : '(none)'
          }`
        );
      }
    }
    const med = (v, k) => median(runs[v].map((m) => m[k]));
    console.log(
      `== ${c.name} (${(runs.hash[0].file_bytes / 1e6).toFixed(1)} MB) median of ${RUNS}: geometry ${med('hash', 'geometry_ms').toFixed(0)} vs ${med(
        'nohash',
        'geometry_ms'
      ).toFixed(0)} ms (hash vs nohash), ttff ${med('hash', 'ttff_ms').toFixed(0)} vs ${med('nohash', 'ttff_ms').toFixed(0)} ms, parse ${med(
        'hash',
        'parse_ms'
      ).toFixed(0)} vs ${med('nohash', 'parse_ms').toFixed(0)} ms, hash ${med('hash', 'hash_ms').toFixed(0)} ms`
    );
  }
} finally {
  await browser.close();
}
