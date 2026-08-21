// DRC skeleton measurement (Phase 5 SLICE 3), in the real environment (wasm).
//
// Loads caravel, then:
//   1. `drc_bench` — synchronous check of the biggest cell (caravel_core):
//      COLD (incl. the one-time lazy R-tree build) and WARM (the honest
//      re-check-after-one-edit cost — edits never mutate the base, so an edit
//      re-checks the whole cell against a cached index).
//   2. The interactive path: enable DRC, draw a 40 nm-wide sliver on TOP →
//      the width violation must appear via the budgeted queue (composite check:
//      overlay shapes are checked with the base).
//
// Prereq: python3 scripts/serve_bench.py 8090   (serves dist/ + /testdata)
// Run:    node scripts/drc_bench.mjs

import { chromium } from 'playwright';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;
const FILE = '/testdata/caravel/gds/caravel.gds';

const state = (p) => p.evaluate(() => { try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; } });
const perf = (p) => p.evaluate(() => { try { return JSON.parse(window.__fluideda_perf || '{}'); } catch { return {}; } });
const cmd = async (p, c) => { await p.evaluate((c) => { window.__fluideda_cmd = c; }, c); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitPerf(p, pred, what, timeout) {
  const t0 = Date.now();
  for (;;) {
    const s = await perf(p);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}: ${JSON.stringify(s)}`);
    await sleep(400);
  }
}
async function waitState(p, pred, what, timeout) {
  const t0 = Date.now();
  for (;;) {
    const s = await state(p);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}: ${JSON.stringify(s)}`);
    await sleep(250);
  }
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on('console', (m) => { const t = m.text(); if (/DRC bench|panic/i.test(t)) console.log('  [page]', t); });

  // Load caravel at leaf (fast settle; DRC does not need the viewport).
  await page.goto(`${BASE}/?file=${encodeURIComponent(FILE)}&zoom=leaf`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(
    () => { const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics); return m && m.file_bytes > 1e6; },
    null, { timeout: 300000 }
  );
  await sleep(1500);

  // 1. The biggest-cell bench (synchronous inside one frame; generous timeout).
  await cmd(page, 'drc_bench');
  const b = await waitPerf(page, (s) => s.drc_cold_ms >= 0, 'drc_bench result', 300000);
  console.log(`caravel biggest cell (${b.drc_shapes} shapes = caravel_core):`);
  console.log(`  cold full check (incl. R-tree build): ${b.drc_cold_ms.toFixed(0)} ms`);
  console.log(`  warm re-check (after one edit):       ${b.drc_warm_ms.toFixed(0)} ms`);
  console.log(`  violations: ${b.drc_violations} (blanket 100/100 table; see STATUS on fracture over-flagging)`);

  // 2. Interactive: draw-too-narrow flags via the budgeted queue.
  await cmd(page, 'edit_mode'); await sleep(100);
  await cmd(page, 'drc_on'); await sleep(100);
  await cmd(page, 'draw_rect 500000 500000 500040 502000'); // 40 nm x 2 µm sliver on TOP
  const s = await waitState(page, (x) => x.drc_viols > 0, 'sliver width violation', 30000);
  console.log(`  drawn 40 nm sliver flags via budgeted queue: ${s.drc_viols} violation(s) on TOP  (adds=${s.adds})`);

  await browser.close();
  console.log('DRC-BENCH: done');
}
main().catch((e) => { console.error('DRC-BENCH FAILED:', e.message); process.exit(1); });
