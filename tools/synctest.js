// Sync + session-restore tests for ui/app.js.
// Drives the real script with scripted fake sidecars and asserts:
//   1. No fake progress: after clicking a song (IPC acked) but BEFORE the
//      sidecar reports playing, progress stays frozen at 0:00 and the UI
//      shows paused (previously it counted 0:00 -> 0:02 then snapped back).
//   2. Confirmation follows the REPORTED position (anchor from sidecar).
//   3. External track change (OS media-key skip) is adopted into the UI.
//   4. External stop (null track, not playing) pauses the UI.
//   5. Dead-end OS skip (stop + reset on separate polls) advances.
//   6. External restart/seek-back is followed, not frozen.
//   7. Reload with live audio but no stored session adopts the track.
//   8. Reload with a stored session + live audio restores queue + state,
//      and a later dead-end skip advances within the restored queue.
//   9. Reload with a silent sidecar still shows the restored song data.
// A fresh harness (fresh JS state, seeded localStorage) simulates a reload.
// Run: node tools/synctest.js
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const failures = [];
function assert(cond, msg) {
  if (!cond) failures.push(msg);
  else console.log('ok: ' + msg);
}
process.on('unhandledRejection', (e) => failures.push('unhandled rejection: ' + (e && e.stack || e)));

function makeEl(tag) {
  const handlers = {};
  const el = {
    tagName: (tag || 'div').toUpperCase(),
    children: [],
    dataset: {},
    style: {},
    value: '',
    checked: false,
    textContent: '',
    innerHTML: '',
    classList: {
      _s: new Set(),
      add(c) { this._s.add(c); },
      remove(c) { this._s.delete(c); },
      toggle(c, force) {
        if (force === undefined) { this._s.has(c) ? this._s.delete(c) : this._s.add(c); }
        else { force ? this._s.add(c) : this._s.delete(c); }
      },
      contains(c) { return this._s.has(c); },
    },
    addEventListener: (t, fn) => { (handlers[t] = handlers[t] || []).push(fn); },
    removeEventListener: () => {},
    querySelector: () => makeEl('div'),
    querySelectorAll: () => [],
    appendChild(c) { el.children.push(c); return c; },
    replaceChild: () => {},
    closest: () => null,
    remove: () => {},
    replaceWith: () => {},
    scrollIntoView: () => {},
    setAttribute: () => {},
    getAttribute: () => null,
    removeAttribute: () => {},
    hasAttribute: () => false,
    isConnected: true,
    onclick: null, onchange: null, onkeydown: null,
    offsetParent: null,
  };
  return el;
}

function transportButton(cmd) {
  const b = makeEl('button');
  b.dataset.cmd = cmd;
  return b;
}

const SRC = fs.readFileSync(path.join(__dirname, '..', 'ui', 'app.js'), 'utf8');

function createHarness(seedStore, seedReport) {
  const elements = {};
  const getEl = (id) => {
    if (!elements[id]) elements[id] = makeEl('div');
    return elements[id];
  };
  const selectorCache = {};
  const queryAll = (sel) => {
    if (!selectorCache[sel]) {
      if (sel.includes('.transport')) selectorCache[sel] = ['play', 'pause', 'next', 'previous'].map(transportButton);
      else if (sel.includes('#sidebar nav')) {
        selectorCache[sel] = ['browse', 'playlists', 'queue', 'lyrics', 'settings'].map((v) => {
          const b = makeEl('button');
          b.dataset.view = v;
          return b;
        });
      } else selectorCache[sel] = [];
    }
    return selectorCache[sel];
  };
  const h = {
    nowMs: 1000000,
    commands: [],
    sidecar: Object.assign(
      { playing: false, track_id: null, title: null, artist: null, position_ms: 0, duration_ms: 0, detail: '' },
      seedReport || {},
    ),
    store: seedStore || {},
    pollFn: null,
    frameFn: null,
    ctx: null,
  };
  async function stubInvoke(cmd, args) {
    h.commands.push(cmd);
    switch (cmd) {
      case 'sidecar_status': return { ...h.sidecar };
      case 'sidecar_play': return 'sent (stub)';
      case 'sidecar_append': return undefined;
      case 'sidecar_play_next': return undefined;
      case 'sidecar_pause': h.sidecar.playing = false; return undefined;
      case 'sidecar_resume': return undefined;
      case 'sidecar_clear': return undefined;
      case 'sidecar_reattach': return false; // no orphan in harness
      case 'sidecar_headless': return true;
      case 'sidecar_explicit': return true;
      case 'resolve_track_id': return args.trackId;
      case 'similar_songs': return [];
      case 'get_lyrics': throw new Error('no lyrics (stub)');
      case 'motion_artwork': return null;
      case 'browse_charts': return { tracks: [], albums: [], playlists: [] };
      case 'search_catalog': return { tracks: [], albums: [], playlists: [], artists: [] };
      case 'token_status': return '';
      case 'auth_state': return false;
      case 'set_discord_app_id': return undefined;
      case 'set_discord_enabled': return undefined;
      case 'clear_discord_presence': return undefined;
      default: return undefined;
    }
  }
  const stubs = {
    document: {
      querySelector: (s) => {
        const m = /^#([\w-]+)$/.exec(s);
        return m ? getEl(m[1]) : makeEl('div');
      },
      querySelectorAll: (s) => queryAll(s),
      createElement: (t) => makeEl(t),
      addEventListener: () => {},
      exitFullscreen: () => Promise.resolve(),
      fullscreenElement: null,
    },
    window: { __TAURI__: { core: { invoke: stubInvoke } } },
    localStorage: {
      get _s() { return h.store; },
      getItem(k) { return Object.prototype.hasOwnProperty.call(h.store, k) ? h.store[k] : null; },
      setItem(k, v) { h.store[k] = String(v); },
    },
    performance: { now: () => h.nowMs },
    requestAnimationFrame: (fn) => { h.frameFn = fn; return 1; },
    setInterval: (fn) => { h.pollFn = fn; return 1; },
    clearInterval: () => {},
    setTimeout: (...a) => setTimeout(...a),
    clearTimeout: (...a) => clearTimeout(...a),
    navigator: {},
    CSS: { escape: (s) => s },
    prompt: () => null,
    addEventListener: () => {},
    removeEventListener: () => {},
  };
  const ctx = {
    ...stubs,
    console,
    JSON, Math, Object, Array, String, Number, Boolean, Promise,
    parseInt, parseFloat, isNaN, Set, Map, Date,
  };
  ctx.globalThis = ctx;
  Object.defineProperty(ctx, 'window', { value: stubs.window });
  vm.createContext(ctx);
  vm.runInContext(SRC, ctx, { filename: 'app.js' });
  h.ctx = ctx;
  h.hidden = (id) => getEl(id).classList.contains('hidden');
  h.text = (id) => getEl(id).textContent;
  h.flush = async (n = 12) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };
  h.sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  h.poll = async () => { await h.pollFn(); await h.flush(); };
  h.frame = () => { h.frameFn(); };
  h.advance = (ms) => { h.nowMs += ms; };
  h.report = (obj) => { Object.assign(h.sidecar, obj); };
  return h;
}

const T1 = { id: 's1', title: 'One', artist: 'A', album: 'Al1', duration_ms: 180000, artwork: { url: '' }, genres: ['Rock'] };
const T2 = { id: 's2', title: 'Two', artist: 'B', album: 'Al2', duration_ms: 200000, artwork: { url: '' }, genres: ['Pop'] };
const T3 = { id: 's3', title: 'Three', artist: 'C', album: 'Al3', duration_ms: 210000, artwork: { url: '' }, genres: ['Jazz'] };

(async () => {
  try {
    const h = createHarness();
    console.log('load: OK');
    await h.flush();
    assert(typeof h.pollFn === 'function', 'poll loop registered');
    assert(typeof h.frameFn === 'function', 'frame loop registered');
    assert(typeof h.ctx.playTrack === 'function', 'playTrack reachable');
    assert(h.commands.includes('sidecar_reattach'), 'boot attempts sidecar reattach');

    // Click song 1: IPC acks, but the sidecar is still silent/loading.
    await h.ctx.playTrack(T1, [T1, T2]);
    await h.flush();
    assert(h.commands.includes('sidecar_play'), 'play command sent to sidecar');

    // 2s of wall time pass with no sidecar confirmation: progress must NOT move.
    h.advance(2000);
    h.frame();
    assert(h.text('posTime') === '0:00', `no fake progress while loading (pos=${h.text('posTime')})`);
    assert(h.hidden('pauseBtn') === true, 'UI shows paused (not playing) while loading');

    // Sidecar starts audio and reports it: UI confirms from REPORTED position.
    h.report({ playing: true, track_id: 's1', title: 'One', artist: 'A', position_ms: 120, duration_ms: 180000 });
    await h.poll();
    assert(h.hidden('pauseBtn') === false, 'UI shows playing after sidecar confirms');
    assert(h.text('nowPlaying') === 'One', `now playing follows confirmed track (got ${h.text('nowPlaying')})`);

    h.advance(1000);
    h.frame();
    assert(h.text('posTime') === '0:01', `progress follows sidecar clock (pos=${h.text('posTime')})`);

    // OS media-key skip: sidecar reports the next track the UI did not request.
    h.report({ playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 500, duration_ms: 200000 });
    await h.poll();
    assert(h.text('nowPlaying') === 'Two', `external skip adopted (got ${h.text('nowPlaying')})`);
    assert(h.hidden('pauseBtn') === false, 'still playing after external skip');

    // External stop: empty track report, not playing.
    h.report({ playing: false, track_id: null, title: null, artist: null, position_ms: 0, duration_ms: 0 });
    await h.poll();
    assert(h.hidden('pauseBtn') === true, 'external stop pauses the UI');

    // OS next at a sidecar dead end (nothing mirrored ahead yet): playback
    // stops at ~0 instead of advancing. The stop and the position reset land
    // on SEPARATE polls (paused at old pos first).
    await h.ctx.playTrack(T1, [T1, T2]);
    await h.flush();
    h.report({ playing: true, track_id: 's1', title: 'One', artist: 'A', position_ms: 120, duration_ms: 180000 });
    await h.poll();
    assert(h.hidden('pauseBtn') === false, 'replay confirmed');
    assert(h.commands.includes('sidecar_append'), 'upcoming tracks mirrored into sidecar');
    h.advance(60000);
    h.frame();
    assert(h.text('posTime') === '1:00', `mid-track position (pos=${h.text('posTime')})`);
    h.report({ playing: true, track_id: 's1', title: 'One', artist: 'A', position_ms: 60000, duration_ms: 180000 });
    await h.poll();
    const playsBefore = h.commands.filter((c) => c === 'sidecar_play').length;
    h.report({ playing: false, track_id: 's1', title: 'One', artist: 'A', position_ms: 60000, duration_ms: 180000 });
    await h.poll();
    assert(h.text('nowPlaying') === 'One', `no premature advance on stop (got ${h.text('nowPlaying')})`);
    assert(h.hidden('pauseBtn') === true, 'stopped UI shows paused');
    h.report({ playing: false, track_id: 's1', title: 'One', artist: 'A', position_ms: 0, duration_ms: 180000 });
    await h.poll();
    assert(h.text('nowPlaying') === 'Two', `dead-end OS skip advances (got ${h.text('nowPlaying')})`);
    await h.sleep(400);
    await h.flush();
    assert(h.commands.filter((c) => c === 'sidecar_play').length === playsBefore + 1, 'advance sent to sidecar');
    h.report({ playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 300, duration_ms: 200000 });
    await h.poll();
    assert(h.hidden('pauseBtn') === false, 'advanced track plays');

    // OS previous past ~3s restarts the track: the display must follow back
    // to ~0 instead of freezing at the old position.
    await h.sleep(1600); // leave the post-confirm guard window
    h.advance(60000);
    h.frame();
    h.report({ playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 800, duration_ms: 200000 });
    await h.poll();
    h.frame();
    assert(h.text('posTime') === '0:00', `external restart followed (pos=${h.text('posTime')})`);

    // Reload with live audio but no stored session: adopt the track.
    const hr = createHarness({}, { playing: true, track_id: 's1', title: 'One', artist: 'A', position_ms: 5000, duration_ms: 180000 });
    await hr.flush();
    await hr.poll();
    assert(hr.text('nowPlaying') === 'One', `reload adopts live audio (got ${hr.text('nowPlaying')})`);
    assert(hr.hidden('pauseBtn') === false, 'reload shows playing state');

    // Reload with a stored session + live audio: restore queue + state,
    // then dead-end skip advances within the restored queue.
    await h.ctx.playTrack(T2, [T1, T2, T3]);
    await h.flush();
    h.report({ playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 5000, duration_ms: 200000 });
    await h.poll();
    assert(h.text('nowPlaying') === 'Two', 'session cue playing');
    const persisted = JSON.parse(h.store['sonora-queue-v1'] || 'null');
    assert(persisted && persisted.queueIndex === 1 && persisted.queue.length === 3,
      'queue + index persisted for reload');
    assert(persisted && typeof persisted.positionMs === 'number' && persisted.positionMs >= 0,
      `listening position persisted (got ${persisted && persisted.positionMs})`);
    const sessionSnapshot = { ...h.store };
    const silentSeed = { ...h.store };
    const h2 = createHarness(sessionSnapshot, { playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 20000, duration_ms: 200000 });
    await h2.flush();
    assert(h2.text('nowPlaying') === 'Two', `reload restores song data (got ${h2.text('nowPlaying')})`);
    assert(h2.hidden('pauseBtn') === false, 'reload restores playing state');
    h2.advance(20000);
    h2.report({ playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 40000, duration_ms: 200000 });
    await h2.poll();
    h2.report({ playing: false, track_id: 's2', title: 'Two', artist: 'B', position_ms: 40000, duration_ms: 200000 });
    await h2.poll();
    h2.report({ playing: false, track_id: 's2', title: 'Two', artist: 'B', position_ms: 0, duration_ms: 200000 });
    await h2.poll();
    assert(h2.text('nowPlaying') === 'Three', `restored queue advances on OS skip (got ${h2.text('nowPlaying')})`);

    // Reload with a silent sidecar: restored song data still visible.
    const h3 = createHarness(silentSeed, {});
    await h3.flush();
    assert(h3.text('nowPlaying') === 'Two', `silent reload keeps song data (got ${h3.text('nowPlaying')})`);
    assert(h3.hidden('pauseBtn') === true, 'silent reload shows paused');

    // Silent reload resumes near the persisted listening position.
    const posStore = {
      'sonora-queue-v1': JSON.stringify({
        v: 1, savedAt: Date.now(), queueIndex: 0, positionMs: 42000,
        queue: [{ track: { id: 's1', title: 'One', artist: 'A', album: 'Al1', art: '', duration_ms: 180000, genres: ['Rock'] }, source: 'user' }],
      }),
    };
    const h4 = createHarness(posStore, {});
    await h4.flush();
    h4.frame();
    assert(h4.text('nowPlaying') === 'One', `position restore keeps song (got ${h4.text('nowPlaying')})`);
    assert(h4.text('posTime') === '0:42', `resumes near persisted position (pos=${h4.text('posTime')})`);

    if (failures.length) {
      console.error('FAILURES:\n- ' + failures.join('\n- '));
      process.exit(1);
    }
    console.log('sync: OK (no fake progress, confirm + external adopt + stop + restore all synced)');
    process.exit(0);
  } catch (e) {
    console.error('HARNESS FAILED:', e && e.stack || e);
    console.error('FAILURES:\n- ' + failures.join('\n- '));
    process.exit(1);
  }
})();
