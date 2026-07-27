// Phase 3 two-browser verification: collaborative edit sync + per-user undo.
//
// Opens the wasm app in two headless Chromium pages against a running relay
// (server on :9000) + trunk serve (:8080, which proxies /ws to the relay), drives
// edits through the app's window test hook (window.__fluideda_cmd / __fluideda_state),
// and asserts:
//   1. an edit in A shows in B (move + delete);
//   2. concurrent edits of DIFFERENT shapes converge (both see both);
//   3. undo in A reverts only A's edit (B's survives).
//
// Prereqs (started by the caller):
//   cargo run -p fluideda-server &        # relay on :9000
//   trunk serve --release &               # app on :8080, proxies /ws
//
// Run: node scripts/edit_sync_test.mjs
import { chromium } from 'playwright';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const URL = `${BASE}/?file=/assets/edit_fixture.gds`; // TOP has direct editable shapes

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function cmd(page, c) {
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, c);
}
async function state(page) {
  return await page.evaluate(() => {
    try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; }
  });
}
// Poll until predicate(state) is true, or throw.
async function waitFor(page, pred, what, timeout = 15000) {
  const t0 = Date.now();
  for (;;) {
    const s = await state(page);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}; last state ${JSON.stringify(s)}`);
    await sleep(150);
  }
}

async function main() {
  const browser = await chromium.launch();
  const ctxA = await browser.newContext();
  const ctxB = await browser.newContext();
  const A = await ctxA.newPage();
  const B = await ctxB.newPage();
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };

  try {
    // The fixture must autoload (TOP shapes available) in both pages.
    for (const [p, name] of [[A, 'A'], [B, 'B']]) {
      await p.goto(URL);
      await waitFor(p, (s) => s.top_shapes >= 5, `${name}: fixture loaded`);
    }

    // Both join the same room.
    await cmd(A, 'join'); await cmd(B, 'join');
    await waitFor(A, (s) => s.connected, 'A connected');
    await waitFor(B, (s) => s.connected, 'B connected');
    await sleep(500);

    // 1a. A moves shape 0 -> B sees one moved edit.
    await cmd(A, 'select 0'); await sleep(150);
    await cmd(A, 'nudge');
    await waitFor(A, (s) => s.moves === 1, 'A recorded its move');
    const b1 = await waitFor(B, (s) => s.moves === 1, 'B sees A\'s move');
    check(b1.moves === 1 && b1.edited === 1, 'move in A propagates to B');

    // 1b + 2. Concurrent DIFFERENT shapes: A deletes shape 2, B moves shape 3.
    await cmd(A, 'select 2'); await sleep(150); await cmd(A, 'delete');
    await cmd(B, 'select 3'); await sleep(150); await cmd(B, 'nudge');
    // Both should converge to: 3 edited shapes = 2 moves (0, 3) + 1 delete (2).
    const af = await waitFor(A, (s) => s.edited === 3, 'A converges to 3 edited shapes');
    const bf = await waitFor(B, (s) => s.edited === 3, 'B converges to 3 edited shapes');
    check(af.deletes === 1 && af.moves === 2, 'A sees 1 delete + 2 moves (converged)');
    check(bf.deletes === 1 && bf.moves === 2, 'B sees 1 delete + 2 moves (converged)');

    // 3. Undo in A reverts only A's last edit (the delete of shape 2). B's move
    //    of shape 3 and A's earlier move of shape 0 survive.
    await cmd(A, 'undo');
    const au = await waitFor(A, (s) => s.edited === 2, 'A: one edit undone');
    const bu = await waitFor(B, (s) => s.edited === 2, 'B: sees A\'s undo');
    check(au.deletes === 0 && au.moves === 2, 'A undo removed the delete, kept both moves');
    check(bu.deletes === 0 && bu.moves === 2, 'B reflects A\'s undo (per-user: B\'s move intact)');
  } catch (e) {
    check(false, `exception: ${e.message}`);
  } finally {
    await browser.close();
  }

  console.log(fails.length ? `\nEDIT-SYNC: ${fails.length} FAILURE(S)` : '\nEDIT-SYNC: all checks passed');
  process.exit(fails.length ? 1 : 0);
}
main();
