// Phase 7.5: in-context DRC from the app, in real browsers.
//
//   1. caravel in-core: "check whole design in context" (on the server)
//      finds no violation — the 3,303 intra-cell flags are all context-only;
//   2. drawing a 100 nm met1 sliver below the die re-checks only the view
//      (an area, not the design), which now holds exactly one m1.1 width
//      violation; undo re-checks again and it is gone;
//   3. a streamed page (which holds none of the geometry) gets the same
//      clean answer.
//
// Prereqs: server (:9000) holding caravel, trunk serve (:8080).
// Run: GPU=webgpu node scripts/drc_context_test.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function cmd(page, c) {
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, c);
  await page.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
  await sleep(100);
}
const state = (page) => page.evaluate(() => {
  try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; }
});
async function waitFor(page, pred, what, timeout = 120000) {
  const t0 = Date.now();
  for (;;) {
    const s = await state(page);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}; last state ${JSON.stringify(s)}`);
    await sleep(200);
  }
}

async function main() {
  const browser = await launch();
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };
  try {
    // 1. In-core, the whole design.
    const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
    await page.goto(`${BASE}/?file=/blobs/${H}`);
    await waitFor(page, (s) => s.design_hash === H, 'caravel loaded and hashed', 240000);
    await cmd(page, 'drc_context');
    const s1 = await waitFor(page, (s) => s.ctx_drc_state === 'done', 'the in-context check');
    check(s1.ctx_drc === 0, `caravel in context: ${s1.ctx_drc} violations (${(s1.ctx_drc_ms / 1000).toFixed(1)} s on the server)`);

    // 2. An edit re-checks the view only.
    const [x, y] = [50_000, -20_000];
    await cmd(page, `view ${x + 50} ${y + 1000} 20000`);
    await cmd(page, 'edit_mode');
    await cmd(page, 'layer 68/20');
    await cmd(page, `draw_rect ${x} ${y} ${x + 100} ${y + 2000}`);
    const s2 = await waitFor(page, (s) => s.adds === 1 && s.ctx_drc_state === 'done_area' && s.ctx_drc === 1, 'the view re-checked after the edit');
    check(s2.ctx_drc === 1, `a 100 nm met1 sliver: the view re-checked in ${s2.ctx_drc_ms} ms, ${s2.ctx_drc} violation in the design`);
    await cmd(page, 'undo');
    await sleep(300);
    const s3 = await waitFor(page, (s) => s.adds === 0 && s.ctx_drc_state === 'done_area' && s.ctx_drc === 0, 'the re-check after undo');
    check(s3.ctx_drc === 0, 'undo: the view re-checked again, the violation gone');
    await page.context().close();

    // 3. Streamed: the page holds no geometry; the server checks.
    const st = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
    await st.goto(`${BASE}/?stream=${H}`);
    await st.waitForFunction(() => JSON.parse(window.__fluideda_perf || '{}').streaming, null, { timeout: 180000, polling: 100 });
    await waitFor(st, (s) => s.design_hash === H, 'the streamed design hash');
    await cmd(st, 'drc_context');
    const s4 = await waitFor(st, (s) => s.ctx_drc_state === 'done', 'the streamed in-context check');
    check(s4.ctx_drc === 0, `streamed caravel in context: ${s4.ctx_drc} violations (${(s4.ctx_drc_ms / 1000).toFixed(1)} s, the design parsed once on the server)`);
  } finally {
    await browser.close();
  }
  console.log(fails.length ? `\n${fails.length} check(s) FAILED` : '\nDRC-CONTEXT: all checks passed');
  process.exit(fails.length ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
