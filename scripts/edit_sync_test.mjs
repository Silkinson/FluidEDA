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
import { launch } from './launch.mjs';

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
  const browser = await launch();
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

    // 4. NON-TOP editing (SLICE 2): edit a shape in the LEAF *child* cell (id 0),
    //    not TOP. The CRDT op is cell-scoped ("cell:idx"), so it must converge the
    //    same way — proving hierarchy editing syncs with no protocol change.
    await cmd(A, 'select 0 0');
    const asel = await waitFor(A, (s) => s.selected_cell === 0, 'A selected a non-TOP (LEAF) shape');
    check(asel.selected === 0, 'selection resolved to LEAF shape 0');
    await cmd(A, 'nudge');
    const a4 = await waitFor(A, (s) => s.edited === 3, 'A recorded the non-TOP move');
    const b4 = await waitFor(B, (s) => s.edited === 3, 'B sees the non-TOP move');
    check(a4.moves === 3 && b4.moves === 3, 'non-TOP (LEAF) edit converges A→B (cell-scoped CRDT)');

    // --- Phase 5 SLICE 1: draw-new-shape --------------------------------------

    // 5. A draws a rect; the add ships its geometry (there is no base to rebuild
    //    from), so B must see the same shape.
    await cmd(A, 'draw_rect 100 100 3100 2100');
    const a5 = await waitFor(A, (s) => s.adds === 1, 'A recorded its drawn rect');
    const b5 = await waitFor(B, (s) => s.adds === 1, "B sees A's drawn rect");
    check(a5.adds === 1 && b5.adds === 1, 'drawn rect propagates A→B (geometry-carrying add)');

    // 6. CONCURRENT adds: both peers draw before waiting on either — distinct
    //    creators mint distinct AddIds, so BOTH shapes must survive convergence.
    await cmd(A, 'draw_rect 5000 5000 8000 7000');
    await cmd(B, 'draw_poly 9000 9000 12000 9000 10500 11500');
    const a6 = await waitFor(A, (s) => s.adds === 3, 'A converges to 3 drawn shapes');
    const b6 = await waitFor(B, (s) => s.adds === 3, 'B converges to 3 drawn shapes');
    check(a6.adds === 3 && b6.adds === 3, 'concurrent adds converge: both survive (distinct AddIds)');

    // 7. B deletes A's first drawn shape BY ITS AddId (`select_added` indexes the
    //    sorted add list — deterministic, so index 0 is the same shape on both).
    await cmd(B, 'select_added 0');
    await waitFor(B, (s) => s.selected_added === 0, 'B selected the added shape');
    await cmd(B, 'delete');
    const a7 = await waitFor(A, (s) => s.adds === 2, "A sees B's delete of the added shape");
    check(a7.adds === 2, 'a peer deletes an added shape by AddId; converges');

    // 8. Undo in B restores the add it just deleted (per-user undo covers adds).
    await cmd(B, 'undo');
    const a8 = await waitFor(A, (s) => s.adds === 3, "A sees B's undo restore the add");
    const b8 = await waitFor(B, (s) => s.adds === 3, 'B restored the add locally');
    check(a8.adds === 3 && b8.adds === 3, 'undo of an added-shape delete restores it everywhere');

    // --- Phase 5 SLICE 2: vertex editing --------------------------------------

    // 9. A vertex-edits a fresh TOP shape (4): the record ships the full ring
    //    ("P ..."), so B must converge (geom_edits counts SetPoints records).
    await cmd(A, 'select 4'); await sleep(150);
    await cmd(A, 'vertex 1 900 700');
    const a9 = await waitFor(A, (s) => s.geom_edits === 1, 'A recorded the vertex edit');
    const b9 = await waitFor(B, (s) => s.geom_edits === 1, "B sees A's vertex edit");
    check(a9.geom_edits === 1 && b9.geom_edits === 1, 'vertex edit (SetPoints) converges A→B');

    // 10. NON-TOP vertex edit: LEAF (cell 0) shape 1 — master semantics, same
    //     cell-scoped key, must converge identically.
    await cmd(A, 'select 0 1'); await sleep(150);
    await cmd(A, 'vertex 0 40 30');
    const a10 = await waitFor(A, (s) => s.geom_edits === 2, 'A recorded the non-TOP vertex edit');
    const b10 = await waitFor(B, (s) => s.geom_edits === 2, 'B sees the non-TOP vertex edit');
    check(a10.geom_edits === 2 && b10.geom_edits === 2, 'non-TOP vertex edit converges (master semantics)');

    // 11. CONCURRENT vertex edits of the SAME shape (TOP 4): last-writer-wins —
    //     both sides must settle on identical state (one record; counts equal).
    await cmd(A, 'select 4'); await cmd(B, 'select 4'); await sleep(200);
    await cmd(A, 'vertex 1 -500 0');
    await cmd(B, 'vertex 2 0 -500');
    await sleep(1200); // let LWW settle both ways
    const a11 = await state(A); const b11 = await state(B);
    check(
      a11.geom_edits === 2 && b11.geom_edits === 2 && a11.edited === b11.edited,
      'concurrent same-shape vertex edits converge LWW (one record, equal state)'
    );

    // 12. Moving a vertex-edited shape must keep its geometry record (a Move op
    //     would LWW the whole record back to translated-base, losing the edit).
    await cmd(A, 'nudge');
    await sleep(800);
    const a12 = await state(A); const b12 = await state(B);
    check(
      a12.geom_edits === 2 && b12.geom_edits === 2,
      'move-after-vertex-edit stays a geometry record (edit preserved)'
    );

    // --- Phase 5d: insert / delete vertex --------------------------------------

    // 13. A inserts a vertex on an untouched TOP shape (3, a 4-vertex strip):
    //     the ring grows by one and ships whole (SetPoints), so B must see the
    //     same count.
    await cmd(A, 'select 3'); await sleep(150);
    await cmd(B, 'select 3'); await sleep(150);
    await waitFor(A, (s) => s.selected === 3 && s.sel_points === 4, 'A selected TOP shape 3');
    const before = (await state(A)).sel_points;
    await cmd(A, 'vertex_insert 0 1 1');
    const a13 = await waitFor(A, (s) => s.sel_points === before + 1, 'A inserted a vertex');
    const b13 = await waitFor(B, (s) => s.sel_points === before + 1, "B sees A's inserted vertex");
    check(a13.sel_points === b13.sel_points, `insert-vertex converges A→B (${before} → ${b13.sel_points})`);

    // 14. B deletes that vertex again (a peer edits a count-changed ring).
    await cmd(B, 'vertex_delete 1');
    const b14 = await waitFor(B, (s) => s.sel_points === before, 'B deleted the vertex');
    const a14 = await waitFor(A, (s) => s.sel_points === before, "A sees B's vertex delete");
    check(a14.sel_points === before && b14.sel_points === before, 'delete-vertex converges B→A');

    // --- Phase 5d: boolean ops -------------------------------------------------

    // 15. A draws two overlapping rects on one layer, multi-selects them, and
    //     unions them: two adds become one, as ONE transaction — B converges,
    //     and a single undo in A brings both inputs back everywhere.
    await cmd(A, 'layer 0'); await sleep(100);
    const adds0 = (await state(A)).adds;
    await cmd(A, 'draw_rect 20000 20000 22000 21000');
    const r1 = await waitFor(A, (s) => s.adds === adds0 + 1, 'A drew rect 1');
    await cmd(A, 'draw_rect 21000 20500 23000 22000');
    const r2 = await waitFor(A, (s) => s.adds === adds0 + 2, 'A drew rect 2');
    await cmd(A, `select_added ${r1.selected_added}`); await sleep(100);
    await cmd(A, `select_added_add ${r2.selected_added}`);
    await waitFor(A, (s) => s.sel_count === 2, 'A multi-selected both rects');
    await waitFor(B, (s) => s.adds === adds0 + 2, 'B sees both rects');
    await cmd(A, 'boolean union');
    const a15 = await waitFor(A, (s) => s.adds === adds0 + 1, 'A: union replaced 2 shapes with 1');
    const b15 = await waitFor(B, (s) => s.adds === adds0 + 1, "B converges on A's union");
    check(a15.adds === b15.adds, `boolean union converges A→B (${adds0 + 2} → ${b15.adds} drawn shapes)`);
    await cmd(A, 'undo');
    const a16 = await waitFor(A, (s) => s.adds === adds0 + 2, 'A undid the union in one step');
    const b16 = await waitFor(B, (s) => s.adds === adds0 + 2, "B sees A's undo of the union");
    check(a16.adds === adds0 + 2 && b16.adds === adds0 + 2, 'one undo restores both boolean inputs everywhere');

    // --- Phase 5d: drawing into an instanced master ----------------------------

    // 17. A draws into the LEAF master (cell 0) at its placement (200, 4200) in
    //     TOP. The add carries cell 0, converges, and — master semantics — is
    //     part of LEAF wherever LEAF is placed.
    await cmd(A, 'draw_into 0 200 4200'); await sleep(100);
    await cmd(A, 'layer 0'); await sleep(100);
    await cmd(A, 'draw_rect 20 20 180 60'); // LEAF-local coordinates
    const a17 = await waitFor(A, (s) => s.adds_nontop === 1, 'A drew into the LEAF master');
    const b17 = await waitFor(B, (s) => s.adds_nontop === 1, "B sees A's add in LEAF");
    check(a17.adds_nontop === 1 && b17.adds_nontop === 1, 'a shape drawn into a master converges A→B (cell-scoped add)');
    await cmd(A, 'draw_into top');

    // 18. B clicks it at LEAF's placement: the composite pick resolves the drawn
    //     shape (smaller than LEAF's base tile under the same point).
    await cmd(B, 'pick 300 4240');
    const b18 = await waitFor(B, (s) => s.selected_added >= 0, 'B picked the master add at its placement');
    check(b18.selected_added >= 0 && b18.selected === -1, 'a shape drawn into a master is pickable at its placement');

    // --- Phase 5d: edit one placement only (flatten) ---------------------------

    // 19. B flattens LEAF's placement (TOP is placed once ⇒ the TOP→LEAF hop):
    //     the placement is hidden and its composite — LEAF's 2 base shapes plus
    //     the shape A drew into LEAF — becomes 3 TOP shapes, in ONE transaction.
    const s0 = await state(A);
    await cmd(B, 'flatten_at 300 4300'); // inside LEAF's inner box at the placement
    const b19 = await waitFor(B, (s) => s.hidden === 1, 'B flattened the placement');
    const a19 = await waitFor(A, (s) => s.hidden === 1 && s.adds === b19.adds, 'A converges on the flatten');
    check(b19.adds === s0.adds + 3 && a19.adds === b19.adds, `flatten converges: placement hidden, +3 TOP shapes (${s0.adds} → ${a19.adds})`);
    check(a19.edited === s0.edited, 'the LEAF master is untouched (no base edits)');

    // 20. One undo in B restores the placement everywhere.
    await cmd(B, 'undo');
    const a20 = await waitFor(A, (s) => s.hidden === 0 && s.adds === s0.adds, "A sees B's undo of the flatten");
    check(a20.hidden === 0 && a20.adds === s0.adds, 'one undo restores the flattened placement everywhere');
  } catch (e) {
    check(false, `exception: ${e.message}`);
  } finally {
    await browser.close();
  }

  console.log(fails.length ? `\nEDIT-SYNC: ${fails.length} FAILURE(S)` : '\nEDIT-SYNC: all checks passed');
  process.exit(fails.length ? 1 : 0);
}
main();
