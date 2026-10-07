// Phase 7.2: capability links in real browsers. The server requires links
// (FLUIDEDA_AUTH=on); OWNER is an owner token for ROOM, made with
// `fluideda-server token <room> owner`.
//
//   1. The owner opens the fixture with its link and joins: the room binds
//      to the design, and the design is uploaded with the owner's link.
//   2. The owner shares a viewer link (`share viewer`, what the Share button
//      does).
//   3. A viewer opens that link (no file: the room's design comes from the
//      server), joins, and follows the owner's edits live.
//   4. The viewer cannot edit: edit mode stays off, and its delete changes
//      nothing, on its screen or the owner's.
//   5. Pages with no link, or a forged one, are refused.
//   6. The owner revokes every link: the old viewer link is refused, and the
//      owner, now holding the server's fresh token, shares a link that works.
//
// Run (services with AUTH=on): ROOM=<room> OWNER=<token> node scripts/auth_test.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const ROOM = process.env.ROOM;
const OWNER = process.env.OWNER;
const FIXTURE = `${BASE}/?file=/assets/edit_fixture.gds&room=${ROOM}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function cmd(page, c) {
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, c);
  await page.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
  await sleep(100);
}
const state = (page) => page.evaluate(() => {
  try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; }
});
async function waitFor(page, pred, what, timeout = 20000) {
  const t0 = Date.now();
  for (;;) {
    const s = await state(page);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}; last state ${JSON.stringify(s)}`);
    await sleep(150);
  }
}

async function main() {
  if (!ROOM || !OWNER) throw new Error('ROOM and OWNER (an owner token for ROOM) are required');
  const browser = await launch();
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };
  const page = async (url) => {
    const p = await (await browser.newContext()).newPage();
    await p.goto(url);
    return p;
  };
  try {
    // 1. The owner.
    const A = await page(`${FIXTURE}&t=${OWNER}`);
    await waitFor(A, (s) => s.top_shapes >= 5, 'the owner: fixture loaded');
    await cmd(A, 'join');
    const a0 = await waitFor(A, (s) => s.connected && s.synced && s.room_design, 'the owner joined and bound the room');
    check(a0.role === 'owner' && !a0.read_only, `the owner's link: role ${a0.role}, can edit`);

    // 2. A viewer link, minted by the server for the owner.
    await cmd(A, 'share viewer');
    const shared = await waitFor(A, (s) => s.link.startsWith('http'), 'the viewer link');
    const viewerLink = shared.link;
    check(viewerLink.includes(`room=${ROOM}`) && viewerLink.includes('.viewer.'), `share viewer gives a room link (${viewerLink.slice(0, 60)}…)`);

    // 3. The viewer opens it: the room's design comes from the server.
    const V = await page(viewerLink);
    const v0 = await waitFor(V, (s) => s.connected && s.synced && s.top_shapes >= 5, 'the viewer joined with the room design', 40000);
    check(v0.role === 'viewer' && v0.read_only, `the viewer's link: role ${v0.role}, read-only`);
    await cmd(A, 'select 2'); await cmd(A, 'delete');
    await waitFor(A, (s) => s.edited === 1, 'the owner deleted a shape');
    const v1 = await waitFor(V, (s) => s.edited === 1, 'the viewer sees the delete');
    check(v1.deletes === 1, "the viewer follows the owner's edit live");

    // 4. The viewer tries to edit.
    await cmd(V, 'edit_mode');
    await cmd(V, 'select 3'); await cmd(V, 'delete'); await cmd(V, 'nudge');
    await cmd(V, 'draw_rect 100 100 3100 2100');
    await sleep(1500);
    const [v2, a2] = [await state(V), await state(A)];
    check(v2.edited === 1 && v2.adds === 0 && a2.edited === 1 && a2.adds === 0,
      `the viewer's delete, nudge and drawn rectangle change nothing (viewer ${v2.edited} edited/${v2.adds} added, owner ${a2.edited}/${a2.adds})`);

    // 5. No link, a forged link.
    for (const [who, t] of [['no link', ''], ['a forged link', `${ROOM}.owner.0.0.${'0'.repeat(64)}`]]) {
      const S = await page(`${FIXTURE}${t ? `&t=${t}` : ''}`);
      await waitFor(S, (s) => s.top_shapes >= 5, `${who}: fixture loaded`);
      await cmd(S, 'join');
      const s = await waitFor(S, (s) => s.join_error.includes('unauthorized'), `${who}: refused`);
      check(!s.connected && s.edited === 0, `a page with ${who} is refused (${s.join_error.slice(0, 60)})`);
      await S.context().close();
    }

    // 6. Revoke every link.
    await cmd(A, 'rotate');
    await waitFor(A, (s) => s.link === '', 'the rotation');
    const R = await page(viewerLink.replace(`/?room=`, `/?file=/assets/edit_fixture.gds&room=`));
    await waitFor(R, (s) => s.top_shapes >= 5, 'the revoked viewer: fixture loaded');
    await cmd(R, 'join');
    const r = await waitFor(R, (s) => s.join_error.includes('unauthorized'), 'the revoked link refused');
    check(!r.connected, `the viewer link no longer joins after the rotation (${r.join_error.slice(0, 60)})`);
    await cmd(A, 'share viewer');
    const fresh = await waitFor(A, (s) => s.link.startsWith('http'), 'a fresh viewer link');
    const F = await page(fresh.link);
    const f = await waitFor(F, (s) => s.connected && s.synced && s.edited === 1, 'the fresh viewer joined', 40000);
    check(f.read_only && fresh.link !== viewerLink, "the owner's fresh token shares a new viewer link that works");
  } finally {
    await browser.close();
  }
  console.log(fails.length ? `\n${fails.length} check(s) FAILED` : '\nAUTH: all checks passed');
  process.exit(fails.length ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
