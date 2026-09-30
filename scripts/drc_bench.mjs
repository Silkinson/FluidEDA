// Merge-first tiled DRC measurement (Phase 5b), in the real environment (wasm).
//
// Loads caravel, then:
//   1. `drc_bench` (blanket, then sky130 deck) — synchronous, single-threaded check of the biggest cell
//      (caravel_core): GRID (the budgeted tile-grid build that replaced the
//      atomic lazy R-tree), COLD (grid + every tile), the slowest tile (the
//      unit the frame budget schedules), and WARM (the incremental re-check
//      after one edit: the tiles near one shape).
//   2. The interactive path: enable DRC, draw a 40 nm-wide sliver on TOP → the
//      width violation must appear (edit-local tiles only).
//   3. A background "check visible" pass over caravel_core: through the DRC
//      worker pool, then on a fresh page forced inline (`?drcinline=1`) — wall
//      time to finish, the longest frame while it ran (the never-block-a-frame
//      proof), and the two violation totals (must match).
//
// Prereq: trunk build --release && python3 scripts/serve_bench.py 8090
// Run:    node scripts/drc_bench.mjs

import { chromium } from 'playwright';

const PORT = process.env.PORT || 8090;
const BASE = `http://127.0.0.1:${PORT}`;
const FILE = '/testdata/caravel/gds/caravel.gds';

const state = (p) => p.evaluate(() => { try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; } });
const perf = (p) => p.evaluate(() => { try { return JSON.parse(window.__fluideda_perf || '{}'); } catch { return {}; } });
const cmd = async (p, c) => { await p.evaluate((c) => { window.__fluideda_cmd = c; }, c); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(get, p, pred, what, timeout) {
  const t0 = Date.now();
  for (;;) {
    const s = await get(p);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}: ${JSON.stringify(s)}`);
    await sleep(100);
  }
}

async function load(page, extra) {
  await page.goto(`${BASE}/?file=${encodeURIComponent(FILE)}&zoom=leaf${extra}`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(
    () => { const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics); return m && m.file_bytes > 1e6; },
    null, { timeout: 300000 }
  );
  await sleep(1500);
}

// "Check visible" on `page`: wall time to finish + the perf snapshot at the end
// (longest frame since the reset, violation total, pool size).
async function backgroundPass(page) {
  await cmd(page, 'perf_reset'); await sleep(200);
  const t1 = Date.now();
  await cmd(page, 'drc_check');
  await waitFor(perf, page, (x) => x.drc_busy === true, 'DRC pass start', 10000);
  const done = await waitFor(perf, page, (x) => x.drc_busy === false, 'DRC pass end', 600000);
  return { ...done, wall: (Date.now() - t1) / 1000 };
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on('console', (m) => { const t = m.text(); if (/DRC bench|panic/i.test(t)) console.log('  [page]', t); });

  // Load caravel at leaf (fast settle; the bench does not need the viewport).
  await load(page, '');

  // 1. The biggest-cell bench (synchronous inside one frame; generous timeout),
  //    under each rule deck (caravel auto-selects sky130 on load).
  for (const deck of ['blanket', 'sky130']) {
    await cmd(page, `drc_deck ${deck}`); await sleep(300);
    await cmd(page, 'drc_bench');
    const b = await waitFor(perf, page, (s) => s.drc_cell >= 0, 'drc_bench result', 300000);
    console.log(`caravel biggest cell (${b.drc_shapes} shapes = caravel_core), ${deck} deck (${b.drc_deck}), single-threaded wasm:`);
    console.log(`  grid build:                     ${b.drc_grid_ms.toFixed(0)} ms`);
    console.log(`  cold full check (grid + tiles): ${b.drc_cold_ms.toFixed(0)} ms over ${b.drc_tiles} tiles (slowest tile ${b.drc_tile_max_ms.toFixed(1)} ms)`);
    console.log(`  warm re-check after one edit:   ${b.drc_warm_ms.toFixed(1)} ms over ${b.drc_warm_tiles} tile(s)`);
    console.log(`  violations: ${b.drc_violations}`);
  }

  // 2. Interactive: a drawn too-narrow sliver flags (edit-local tiles only).
  await cmd(page, 'edit_mode'); await sleep(100);
  await cmd(page, 'drc_on'); await sleep(100);
  await cmd(page, 'layer 68/20'); await sleep(100); // met1 — ruled by both decks
  const t0 = Date.now();
  await cmd(page, 'draw_rect 500000 500000 500040 502000'); // 40 nm x 2 µm sliver on TOP
  const s = await waitFor(state, page, (x) => x.drc_viols > 0, 'sliver width violation', 30000);
  console.log(`  drawn 40 nm sliver flags in ~${Date.now() - t0} ms (harness poll): ${s.drc_viols} violation(s) on TOP (adds=${s.adds})`);

  // 3. Background full pass over the visible cells (caravel_core spans the die),
  //    through the worker pool — then the same on a fresh page forced inline.
  const pool = await backgroundPass(page);
  console.log(`  background "check visible" [${pool.drc_workers} worker(s)]: done in ${pool.wall.toFixed(1)} s wall, ` +
    `${pool.drc_total} violation(s), longest frame meanwhile ${pool.max_frame_ms.toFixed(1)} ms over ${pool.frames} frames` +
    (pool.drc_errors ? `, ${pool.drc_errors} TILE ERRORS` : ''));

  const inl = await browser.newPage();
  await load(inl, '&drcinline=1');
  // Same state as the first page: DRC on, the same sliver drawn on met1.
  for (const c of ['edit_mode', 'drc_on', 'layer 68/20', 'draw_rect 500000 500000 500040 502000']) {
    await cmd(inl, c); await sleep(150);
  }
  await waitFor(state, inl, (x) => x.drc_viols > 0, 'inline sliver', 30000);
  const inline = await backgroundPass(inl);
  console.log(`  background "check visible" [inline]:      done in ${inline.wall.toFixed(1)} s wall, ` +
    `${inline.drc_total} violation(s), longest frame meanwhile ${inline.max_frame_ms.toFixed(1)} ms over ${inline.frames} frames`);
  if (pool.drc_total !== inline.drc_total) throw new Error(`pool and inline disagree: ${pool.drc_total} vs ${inline.drc_total}`);
  console.log(`  pool vs inline: ${(inline.wall / pool.wall).toFixed(1)}x faster, identical violation count`);

  await browser.close();
  console.log('DRC-BENCH: done');
}
main().catch((e) => { console.error('DRC-BENCH FAILED:', e.message); process.exit(1); });
