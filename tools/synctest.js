// Sync test for ui/app.js player <-> sidecar synchronization.
// Drives the real script with a scripted fake sidecar and asserts:
//   1. No fake progress: after clicking a song (IPC acked) but BEFORE the
//      sidecar reports playing, progress stays frozen at 0:00 and the UI
//      shows paused (previously it counted 0:00 -> 0:02 then snapped back).
//   2. Confirmation follows the REPORTED position (anchor from sidecar).
//   3. External track change (OS media-key skip) is adopted into the UI.
//   4. External stop (null track, not playing) pauses the UI.
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

const elements = {};
function getEl(id) {
  if (!elements[id]) elements[id] = makeEl('div');
  return elements[id];
}
const selectorCache = {};
function queryAll(sel) {
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
}

// Controllable clock: the app must not advance progress on wall time alone.
let nowMs = 1000000;

// Scripted fake sidecar + command log.
const fakeSidecar = { playing: false, track_id: null, title: null, artist: null, position_ms: 0, duration_ms: 0, detail: '' };
const commands = [];
async function stubInvoke(cmd, args) {
  commands.push(cmd);
  switch (cmd) {
    case 'sidecar_status': return { ...fakeSidecar };
    case 'sidecar_play': return 'sent (stub)';
    case 'sidecar_append': return undefined;
    case 'sidecar_play_next': return undefined;
    case 'sidecar_pause': fakeSidecar.playing = false; return undefined;
    case 'sidecar_resume': return undefined;
    case 'sidecar_clear': return undefined;
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

let pollFn = null;
let frameFn = null;
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
  localStorage: { _s: {}, getItem(k) { return this._s[k] || null; }, setItem(k, v) { this._s[k] = v; } },
  performance: { now: () => nowMs },
  requestAnimationFrame: (fn) => { frameFn = fn; return 1; },
  setInterval: (fn) => { pollFn = fn; return 1; },
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
  parseInt, parseFloat, isNaN, Set, Map,
};
ctx.globalThis = ctx;
Object.defineProperty(ctx, 'window', { value: stubs.window });

vm.createContext(ctx);
const src = fs.readFileSync(path.join(__dirname, '..', 'ui', 'app.js'), 'utf8');
try {
  vm.runInContext(src, ctx, { filename: 'app.js' });
  console.log('load: OK');
} catch (e) {
  console.error('load FAILED:', e.stack);
  process.exit(1);
}

async function flush(n = 12) {
  for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r));
}
const hidden = (id) => getEl(id).classList.contains('hidden');
const text = (id) => getEl(id).textContent;

(async () => {
  try {
  await flush();
  assert(typeof pollFn === 'function', 'poll loop registered');
  assert(typeof frameFn === 'function', 'frame loop registered');
  assert(typeof ctx.playTrack === 'function', 'playTrack reachable');

  const t1 = { id: 's1', title: 'One', artist: 'A', album: 'Al1', duration_ms: 180000, artwork: { url: '' }, genres: ['Rock'] };
  const t2 = { id: 's2', title: 'Two', artist: 'B', album: 'Al2', duration_ms: 200000, artwork: { url: '' }, genres: ['Pop'] };

  // Click song 1: IPC acks, but the sidecar is still silent/loading.
  await ctx.playTrack(t1, [t1, t2]);
  await flush();
  assert(commands.includes('sidecar_play'), 'play command sent to sidecar');

  // 2s of wall time pass with no sidecar confirmation: progress must NOT move.
  nowMs += 2000;
  frameFn();
  assert(text('posTime') === '0:00', `no fake progress while loading (pos=${text('posTime')})`);
  assert(hidden('pauseBtn') === true, 'UI shows paused (not playing) while loading');

  // Sidecar starts audio and reports it: UI confirms from REPORTED position.
  Object.assign(fakeSidecar, { playing: true, track_id: 's1', title: 'One', artist: 'A', position_ms: 120, duration_ms: 180000 });
  await pollFn();
  await flush();
  assert(hidden('pauseBtn') === false, 'UI shows playing after sidecar confirms');
  assert(text('nowPlaying') === 'One', `now playing follows confirmed track (got ${text('nowPlaying')})`);

  nowMs += 1000;
  frameFn();
  assert(text('posTime') === '0:01', `progress follows sidecar clock (pos=${text('posTime')})`);

  // OS media-key skip: sidecar reports the next track the UI did not request.
  Object.assign(fakeSidecar, { playing: true, track_id: 's2', title: 'Two', artist: 'B', position_ms: 500, duration_ms: 200000 });
  await pollFn();
  await flush();
  assert(text('nowPlaying') === 'Two', `external skip adopted (got ${text('nowPlaying')})`);
  assert(hidden('pauseBtn') === false, 'still playing after external skip');

  // External stop: empty track report, not playing.
  Object.assign(fakeSidecar, { playing: false, track_id: null, title: null, artist: null, position_ms: 0, duration_ms: 0 });
  await pollFn();
  await flush();
  assert(hidden('pauseBtn') === true, 'external stop pauses the UI');

  if (failures.length) {
    console.error('FAILURES:\n- ' + failures.join('\n- '));
    process.exit(1);
  }
  console.log('sync: OK (no fake progress, confirm + external adopt + stop all synced)');
  process.exit(0);
  } catch (e) {
    console.error('HARNESS FAILED:', e && e.stack || e);
    console.error('FAILURES:\n- ' + failures.join('\n- '));
    process.exit(1);
  }
})();
