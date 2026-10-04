// Phase 7.4 SLICE A: picking and editing a streamed design, checked in real
// browsers against the same design loaded in-core.
//
//   1. PICKS — the same world points pick the same shape (cell, index, vertex
//      count) in both pages. The streamed page starts with no shapes in main
//      memory: a pick fetches the shape and placement chunks under its point,
//      then answers.
//   2. EDITS — at a view on remote placements (caravel_core's), delete one
//      shape, move a vertex of another and nudge a third, in both pages. The
//      streamed page redraws the edited chunks from the copies it fetched; the
//      screenshots must match pixel for pixel. Undoing all three restores the
//      original pixels. "Edit this placement only" there (a standard cell in
//      caravel_core, whose placements are remote) copies the same shapes.
//   3. MEMORY — with a 1 MB shape budget the streamed page evicts what its
//      picks fetched but keeps what its edits and selection need: after picks
//      all over the die, the edited view still matches in-core exactly. (Both
//      pages edit at the fit view here and are checked zoomed in.)
//
// Prereqs: server (:9000) holding caravel's blob, trunk serve (:8080).
// Run: GPU=webgpu node scripts/stream_edit_test.mjs
import { launch } from './launch.mjs';

const BASE = process.env.BASE || 'http://127.0.0.1:8080';
const H = process.env.DESIGN || '54b338e882819b4393279de3258337fad202718ff7dda9e4be413e07776c90d7';
// The stream_test views: four on the pad ring (inline placements), four on
// the densest chunks of the remote placement lists.
const VIEWS = [
  [3_150_000, 5_000_000, 30_000],
  [1_950_000, 5_000_000, 30_000],
  [150_000, 2_000_000, 30_000],
  [3_450_000, 1_700_000, 30_000],
  [1_907_290, 1_027_860, 30_000],
  [3_146_240, 675_220, 30_000],
  [653_950, 766_300, 30_000],
  [1_804_110, 441_100, 30_000],
];
const EDIT_VIEW = 4; // caravel_core
const GRID = 4; // picks per view: GRID × GRID points over the middle of the view
const CLIP = { x: 420, y: 0, width: 860, height: 800 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const perf = (p) => p.evaluate(() => JSON.parse(window.__fluideda_perf || '{}'));
const state = (p) => p.evaluate(() => JSON.parse(window.__fluideda_state || '{}'));
async function frames(p, n = 2) {
  const f0 = (await perf(p)).frames;
  await p.waitForFunction(([f0, n]) => JSON.parse(window.__fluideda_perf || '{}').frames >= f0 + n, [f0, n], {
    timeout: 20000,
    polling: 16,
  });
}
async function cmd(p, c) {
  await p.evaluate((c) => { window.__fluideda_cmd = c; }, c);
  await p.waitForFunction(() => !window.__fluideda_cmd, null, { timeout: 10000 });
  await frames(p); // the command's frame has published its perf and state
}
// Until nothing is in flight or waiting and the resident set has held still.
async function settle(p) {
  let last = '', stable = 0;
  for (let i = 0; i < 400; i++) {
    const q = await perf(p);
    const busy = q.in_flight || q.p_in_flight || q.s_in_flight || q.pick_pending || q.wants || q.retess;
    const sig = `${q.uploaded}/${q.stream_fetched}/${q.p_fetched}/${q.s_fetched}`;
    if (!busy && sig === last) { if (++stable >= 5) return q; } else stable = 0;
    last = sig;
    await sleep(200);
  }
  throw new Error('did not settle');
}

async function open(browser, url, ready) {
  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log(`  [console] ${m.text()}`); });
  await page.goto(url);
  await page.waitForFunction(ready, null, { timeout: 180000, polling: 100 });
  return page;
}
const inCoreReady = () => {
  const m = window.__fluideda_metrics && JSON.parse(window.__fluideda_metrics);
  return m && m.geometry_ms > 0 && m.file_bytes > 1e6;
};
const streamedReady = () => JSON.parse(window.__fluideda_perf || '{}').streaming;

async function shoot(page, [cx, cy, span]) {
  await cmd(page, `view ${cx} ${cy} ${span}`);
  await settle(page);
  await sleep(300);
  return page.screenshot({ clip: CLIP });
}

// Pick at a world point and wait for the answer (a streamed pick may wait
// for chunks): "cell:index:vertices", or "-" for nothing.
async function pick(page, [x, y]) {
  const t = Date.now();
  await cmd(page, `pick ${x} ${y}`);
  await page.waitForFunction(() => !JSON.parse(window.__fluideda_perf || '{}').pick_pending, null, {
    timeout: 30000,
    polling: 16,
  });
  await frames(page);
  const s = await state(page);
  return { key: s.selected < 0 ? '-' : `${s.selected_cell}:${s.selected}:${s.sel_points}`, ms: Date.now() - t };
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
    let px = 0;
    for (let i = 0; i < x.length; i += 4) {
      if (x[i] !== y[i] || x[i + 1] !== y[i + 1] || x[i + 2] !== y[i + 2]) px++;
    }
    return { px, total: x.length / 4 };
  }, [a.toString('base64'), b.toString('base64')]);
}

// The pick points: a GRID × GRID lattice over the middle half of each view.
function points(view) {
  const [cx, cy, span] = view;
  const out = [];
  for (let i = 0; i < GRID; i++) {
    for (let j = 0; j < GRID; j++) {
      const f = (k) => (k / (GRID - 1) - 0.5) * span * 0.5;
      out.push([Math.round(cx + f(i)), Math.round(cy + f(j))]);
    }
  }
  return out;
}

// The three edits, by pick point: delete, move vertex 0, nudge.
async function edit(page, targets) {
  await pick(page, targets[0]);
  await cmd(page, 'delete');
  await pick(page, targets[1]);
  await cmd(page, 'vertex 0 400 400');
  await pick(page, targets[2]);
  await cmd(page, 'nudge');
}

async function main() {
  const fails = [];
  const check = (cond, msg) => { console.log(`${cond ? 'PASS' : 'FAIL'}  ${msg}`); if (!cond) fails.push(msg); };
  const browser = await launch();
  try {
    const local = await open(browser, `${BASE}/?file=/blobs/${H}`, inCoreReady);
    const streamed = await open(browser, `${BASE}/?stream=${H}`, streamedReady);

    // --- 1. picks -----------------------------------------------------------
    const pts = VIEWS.flatMap(points);
    const want = [];
    for (const p of pts) want.push((await pick(local, p)).key);
    console.log(`      in-core: ${pts.length} picks done`);
    const p0 = await perf(streamed);
    let same = 0, hits = 0;
    const lat = [];
    const mismatches = [];
    for (let i = 0; i < pts.length; i++) {
      const got = await pick(streamed, pts[i]);
      lat.push(got.ms);
      if (got.key === want[i]) same++;
      else mismatches.push(`${pts[i]}: in-core ${want[i]}, streamed ${got.key}`);
      if (want[i] !== '-') hits++;
      if (i % 16 === 15) console.log(`      streamed: ${i + 1} picks, ${same} same, last ${got.ms} ms`);
    }
    const p1 = await perf(streamed);
    lat.sort((a, b) => a - b);
    for (const m of mismatches.slice(0, 5)) console.log(`      ${m}`);
    check(same === pts.length && hits > pts.length / 2,
      `picks: ${same} of ${pts.length} points pick the same shape in-core and streamed (${hits} hit a shape); ` +
      `streamed pick latency median ${lat[lat.length >> 1]} ms, max ${lat[lat.length - 1]} ms; ` +
      `fetched ${p1.s_fetched - p0.s_fetched} shape chunks (${((p1.s_fetched_bytes - p0.s_fetched_bytes) / 1e6).toFixed(1)} MB) ` +
      `+ ${p1.p_fetched - p0.p_fetched} placement chunks`);

    // --- 2. edits -------------------------------------------------------------
    // Three distinct shapes picked at the edit view.
    const ev = points(VIEWS[EDIT_VIEW]);
    const seen = new Set();
    const targets = [];
    for (let i = 0; i < ev.length && targets.length < 3; i++) {
      const k = want[EDIT_VIEW * GRID * GRID + i];
      if (k !== '-' && !seen.has(k)) { seen.add(k); targets.push(ev[i]); }
    }
    check(targets.length === 3, `edit view: ${targets.length} distinct shapes to edit`);
    const before = await shoot(local, VIEWS[EDIT_VIEW]);
    const beforeS = await shoot(streamed, VIEWS[EDIT_VIEW]);
    const d0 = await diff(local, before, beforeS);
    check(d0.px === 0, `edit view before editing: streamed = in-core (${d0.px} of ${d0.total} pixels differ)`);

    await edit(local, targets);
    await edit(streamed, targets);
    const sl = await state(local), ss = await state(streamed);
    check(sl.edited === 3 && ss.edited === 3 && sl.moves === ss.moves && sl.deletes === ss.deletes,
      `edits: in-core ${sl.edited} edited (${sl.moves} moved), streamed ${ss.edited} (${ss.moves} moved)`);
    const after = await shoot(local, VIEWS[EDIT_VIEW]);
    const afterS = await shoot(streamed, VIEWS[EDIT_VIEW]);
    const qe = await perf(streamed);
    const dChanged = await diff(local, before, after);
    const d1 = await diff(local, after, afterS);
    check(dChanged.px > 0 && d1.px === 0,
      `edited view: streamed = in-core (${d1.px} of ${d1.total} pixels differ; the edits changed ${dChanged.px}); ` +
      `streamed re-tessellated ${qe.last_edit_chunks} chunk(s) in ${qe.last_edit_ms.toFixed(1)} ms for the last edit`);

    for (let i = 0; i < 3; i++) {
      await cmd(local, 'undo');
      await cmd(streamed, 'undo');
    }
    const undone = await shoot(local, VIEWS[EDIT_VIEW]);
    const undoneS = await shoot(streamed, VIEWS[EDIT_VIEW]);
    const d2 = await diff(local, before, undone);
    const d3 = await diff(local, before, undoneS);
    check(d2.px === 0 && d3.px === 0,
      `undo ×3 restores the original view: in-core ${d2.px}, streamed ${d3.px} pixels differ`);
    // "Edit this placement only" at the first target: the standard cell
    // there is flattened into caravel_core, whose placements are remote in
    // the pack — the streamed page fetches the placement, then its subtree.
    const flat = async (page) => {
      const t = Date.now();
      await cmd(page, `flatten_at ${targets[0][0]} ${targets[0][1]}`);
      await page.waitForFunction(() => {
        const p = JSON.parse(window.__fluideda_perf || '{}');
        const s = JSON.parse(window.__fluideda_state || '{}');
        return !p.pick_pending && !p.flatten_pending && s.hidden === 1;
      }, null, { timeout: 30000, polling: 16 });
      const ms = Date.now() - t;
      return { s: await state(page), ms };
    };
    const fl = await flat(local), fs = await flat(streamed);
    const flatL = await shoot(local, VIEWS[EDIT_VIEW]);
    const flatS = await shoot(streamed, VIEWS[EDIT_VIEW]);
    const d5 = await diff(local, flatL, flatS);
    check(fs.s.adds === fl.s.adds && fl.s.adds > 0 && d5.px === 0 && fs.s.adds_nontop === fl.s.adds_nontop,
      `edit this placement only: in-core ${fl.s.adds} shapes copied into the parent, streamed ${fs.s.adds} ` +
      `(${fs.ms} ms incl. fetching); views ${d5.px === 0 ? 'identical' : `differ by ${d5.px} px`}`);
    await cmd(local, 'undo');
    await cmd(streamed, 'undo');

    const qs = await perf(streamed);
    check(qs.stream_failures === 0 && qs.s_failures === 0 && qs.p_failures === 0,
      `streamed: no failed chunk reads (draw ${qs.stream_failures}, shapes ${qs.s_failures}, placements ${qs.p_failures})`);
    console.log(`      streamed: ${qs.s_resident} shape chunks in memory (${(qs.s_resident_bytes / 1e6).toFixed(1)} MB), ` +
      `wasm heap ${(qs.wasm_heap / 1e6).toFixed(0)} MB; in-core heap ${((await perf(local)).wasm_heap / 1e6).toFixed(0)} MB`);
    await local.context().close();
    await streamed.context().close();

    // --- 3. memory ------------------------------------------------------------
    // Edits made at the fit view must show once zoomed in (the overlay is
    // culled to the view: it is re-uploaded when what is in view changes).
    const fit = await open(browser, `${BASE}/?file=/blobs/${H}`, inCoreReady);
    await edit(fit, targets);
    const fitEdited = await shoot(fit, VIEWS[EDIT_VIEW]);
    const df = await diff(fit, after, fitEdited);
    check(df.px === 0, `in-core, edited at the fit view then zoomed in: = edited in view (${df.px} pixels differ)`);
    await fit.context().close();

    const tight = await open(browser, `${BASE}/?stream=${H}&shape_budget=1`, streamedReady);
    await edit(tight, targets);
    for (const p of pts) await pick(tight, p); // fetches (and evicts) all over the die
    await cmd(tight, 'pick 0 0'); // nothing there: the selection's pin goes
    const edited = await shoot(tight, VIEWS[EDIT_VIEW]);
    const qt = await perf(tight);
    const d4 = await diff(tight, after, edited);
    check(qt.s_evicted > 0 && d4.px === 0,
      `1 MB shape budget: ${qt.s_evicted} shape chunks evicted, ${qt.s_resident} kept ` +
      `(${(qt.s_resident_bytes / 1e6).toFixed(1)} MB, edits pinned); the edited view still = in-core ` +
      `(${d4.px} pixels differ); wasm heap ${(qt.wasm_heap / 1e6).toFixed(0)} MB`);
    await tight.context().close();
  } finally {
    await browser.close();
  }
  console.log(fails.length ? `\n${fails.length} check(s) FAILED` : '\nall checks passed');
  process.exit(fails.length ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
