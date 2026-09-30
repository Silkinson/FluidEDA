// Phase 6 persistence, end to end in real browsers:
//   1. edits reach the server and live in the room's WAL (no snapshot yet);
//   2. the server is KILLED (SIGKILL — no flush) while a page is open;
//   3. that page keeps editing offline, sees the drop, and reconnects when the
//      server comes back — its offline edit merges into the recovered room;
//   4. a late joiner opening only `?room=` matches it exactly (the WAL
//      replayed + the merge), design fetched from the blob store;
//   5. once the room is quiet a snapshot is recorded: a version in the log,
//      an immutable blob at /blobs/<head>, and the WAL truncated;
//   6. a graceful restart (SIGTERM) serves the room from the snapshot alone.
//
// Prereqs: `trunk serve --release` (app on :8080, proxying :9000). This
// script runs the server itself — release build, a fresh temp store — so
// nothing else may be listening on :9000.
//
// Run: node scripts/persist_test.mjs
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const SERVER = 'http://127.0.0.1:9000';
const BIN = process.env.SERVER_BIN || 'target/release/fluideda-server';
const STORE = mkdtempSync(path.join(tmpdir(), 'fluideda-persist-'));
const ROOM = `persist-${Date.now()}`;
const ROOM_DIR = path.join(STORE, 'rooms', ROOM);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const serverLog = [];

function startServer() {
  const p = spawn(BIN, [], {
    env: { ...process.env, FLUIDEDA_STORE: STORE, RUST_LOG: 'info' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  for (const s of [p.stdout, p.stderr]) s.on('data', (d) => serverLog.push(...String(d).trim().split('\n')));
  return p;
}

async function stopServer(p, signal) {
  const exited = new Promise((r) => p.once('exit', r));
  p.kill(signal);
  await exited;
}

async function healthy(timeout = 10000) {
  const t0 = Date.now();
  for (;;) {
    try {
      if ((await fetch(`${SERVER}/health`)).ok) return;
    } catch {}
    if (Date.now() - t0 > timeout) throw new Error('server did not come up');
    await sleep(50);
  }
}

// One command per frame: wait until the app has taken this one.
async function cmd(page, c) {
  await page.evaluate((c) => { window.__fluideda_cmd = c; }, c);
  await page.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
}
async function state(page) {
  return await page.evaluate(() => {
    try { return JSON.parse(window.__fluideda_state || '{}'); } catch { return {}; }
  });
}
async function waitFor(page, pred, what, timeout = 20000) {
  const t0 = Date.now();
  for (;;) {
    const s = await state(page);
    if (pred(s)) return s;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}; last state ${JSON.stringify(s)}`);
    await sleep(100);
  }
}
async function until(pred, what, timeout = 20000) {
  const t0 = Date.now();
  for (;;) {
    const v = await pred();
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}`);
    await sleep(100);
  }
}
const roomInfo = async () => (await fetch(`${BASE}/rooms/${ROOM}`)).json();

const SHAPE_STATE = ['edited', 'moves', 'deletes', 'adds', 'geom_edits', 'hidden', 'top_shapes'];
const same = (a, b) => SHAPE_STATE.every((k) => a[k] === b[k]);
const pick = (s) => Object.fromEntries(SHAPE_STATE.map((k) => [k, s[k]]));

async function main() {
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };
  try {
    await fetch(`${SERVER}/health`);
    console.error('a server is already listening on :9000 — stop it first');
    process.exit(2);
  } catch {}

  let srv = startServer();
  await healthy();
  const browser = await launch();
  try {
    const A = await (await browser.newContext()).newPage();
    await A.goto(`${BASE}/?file=/assets/edit_fixture.gds&room=${ROOM}`);
    await waitFor(A, (s) => s.synced && s.top_shapes >= 5, 'A joined the room');

    // 1. Edits, then a second peer converging proves the server holds them.
    await cmd(A, 'select 0'); await cmd(A, 'nudge');
    await cmd(A, 'select 2'); await cmd(A, 'delete');
    await cmd(A, 'draw_rect 100 100 3100 2100');
    const a1 = await waitFor(A, (s) => s.edited === 2 && s.adds === 1, 'A made 3 edits');
    const B = await (await browser.newContext()).newPage();
    await B.goto(`${BASE}/?room=${ROOM}`);
    await waitFor(B, (s) => s.synced && same(s, a1), 'B converged (the server has the edits)');
    await B.context().close();
    const walBytes = statSync(path.join(ROOM_DIR, 'wal')).size;
    check(!existsSync(path.join(ROOM_DIR, 'head')) && walBytes > 0,
      `before the crash the room lives in its WAL only (${walBytes} bytes, no snapshot)`);

    // 2. Crash.
    await stopServer(srv, 'SIGKILL');
    const down = await waitFor(A, (s) => !s.connected && !s.synced, 'A sees the server gone');
    check(!down.connected, `A noticed the drop ("${down.join_error}")`);

    // 3. An edit while the server is down: local only, for now.
    await cmd(A, 'select 3'); await cmd(A, 'nudge');
    const aOff = await waitFor(A, (s) => s.edited === 3, 'A edited offline');

    const t0 = Date.now();
    srv = startServer();
    await healthy();
    const aBack = await waitFor(A, (s) => s.synced, 'A reconnected');
    const reconnectMs = Date.now() - t0;
    check(aBack.reconnects >= 1 && same(aBack, aOff),
      `A reconnected by itself after the restart (${reconnectMs} ms, attempt ${aBack.reconnects})`);
    const replay = serverLog.find((l) => l.includes(`room ${ROOM}: restored`));
    check(!!replay, `the restarted server replayed the WAL: ${replay && replay.replace(/^.*\] /, '')}`);

    // 4. A late joiner sees the pre-crash edits AND the offline one.
    const C = await (await browser.newContext()).newPage();
    await C.goto(`${BASE}/?room=${ROOM}`);
    const c4 = await waitFor(C, (s) => s.synced && same(s, aOff), 'C matches A', 30000);
    check(same(c4, aOff), `late joiner after the crash matches A ${JSON.stringify(pick(c4))}`);
    await C.context().close();

    // 5. Quiet room → snapshot: a version, an immutable blob, an empty WAL.
    const info = await until(async () => {
      const i = await roomInfo();
      return i.versions.length > 0 && i;
    }, 'a snapshot', 15000);
    check(info.head === info.versions.at(-1).hash && info.design.name === 'edit_fixture.gds',
      `snapshot v${info.versions.at(-1).seq}: ${info.versions.at(-1).bytes} bytes, room bound to ${info.design.name}`);
    const blob = await fetch(`${BASE}/blobs/${info.head}`);
    check(blob.ok && /immutable/.test(blob.headers.get('cache-control')) && blob.headers.get('etag') === `"${info.head}"`,
      `the snapshot is an immutable blob (${blob.headers.get('cache-control')}; ETag = its hash)`);
    const again = await fetch(`${BASE}/blobs/${info.head}`, { headers: { 'If-None-Match': `"${info.head}"` } });
    check(again.status === 304, `revalidation is a 304 (${again.status})`);
    check(statSync(path.join(ROOM_DIR, 'wal')).size === 0, 'the WAL was truncated by the snapshot');
    const log = readFileSync(path.join(ROOM_DIR, 'log'), 'utf8').trim().split('\n');
    check(log.length === info.versions.length, `version log: ${log.length} line(s)`);

    // 6. Graceful restart: the room comes back from the snapshot alone.
    await stopServer(srv, 'SIGTERM');
    srv = startServer();
    await healthy();
    const D = await (await browser.newContext()).newPage();
    await D.goto(`${BASE}/?room=${ROOM}`);
    const d6 = await waitFor(D, (s) => s.synced && same(s, aOff), 'D matches A', 30000);
    check(same(d6, aOff), 'after a graceful restart the snapshot alone restores the room');
  } catch (e) {
    check(false, `exception: ${e.message}`);
  } finally {
    await browser.close();
    await stopServer(srv, 'SIGTERM').catch(() => {});
  }
  console.log(`\nstore: ${STORE}`);
  console.log(fails.length ? `\nPERSIST: ${fails.length} FAILURE(S)` : '\nPERSIST: all checks passed');
  process.exit(fails.length ? 1 : 0);
}
main();
