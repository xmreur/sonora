// Headless wiring test for ui/app.js: stubs the DOM/Tauri, loads the real
// script, fires every click handler, and fails on ReferenceError/TypeError.
// Run: node tools/uitest.js
const fs = require('fs');
const path = require('path');

const failures = [];
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
      add() {}, remove() {}, toggle() {},
      contains() { return false; },
    },
    addEventListener: (t, fn) => { (handlers[t] = handlers[t] || []).push(fn); },
    removeEventListener: () => {},
    querySelector: () => makeEl('div'),
    querySelectorAll: () => [],
    appendChild(c) { el.children.push(c); return c; },
    replaceChild: () => {},
    closest: () => null,
    remove: () => {},
    scrollIntoView: () => {},
    setAttribute: () => {},
    getAttribute: () => null,
    removeAttribute: () => {},
    hasAttribute: () => false,
    onclick: null, onchange: null, onkeydown: null,
    offsetParent: null,
    _fire(type, ev) {
      (handlers[type] || []).forEach((f) => f(ev || {}));
      if (type === 'click' && typeof el.onclick === 'function') el.onclick(ev || {});
      if (type === 'change' && typeof el.onchange === 'function') el.onchange(ev || {});
    },
  };
  return el;
}

// Transport buttons share one handler each; give them dataset.cmd values.
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

const invoked = [];
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
  window: {},
  localStorage: { _s: {}, getItem(k) { return this._s[k] || null; }, setItem(k, v) { this._s[k] = v; } },
  performance: { now: () => Date.now() },
  requestAnimationFrame: () => 0, // don't loop headless
  setInterval: () => 0,
  setTimeout: (fn) => 0,
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
  parseInt, parseFloat, isNaN,
};
ctx.globalThis = ctx;
Object.defineProperty(ctx, 'window', { value: stubs.window });

const vm = require('vm');
vm.createContext(ctx);
const src = fs.readFileSync(path.join(__dirname, '..', 'ui', 'app.js'), 'utf8');
try {
  vm.runInContext(src, ctx, { filename: 'app.js' });
  console.log('load: OK');
} catch (e) {
  console.error('load FAILED:', e.stack);
  process.exit(1);
}

// Fire every wired click/change handler and flush async work.
async function fireAll() {
  const ids = ['fsBtn', 'fsClose', 'searchBtn', 'authBtn', 'mutBtn', 'authUrlBtn',
    'relaunchBtn', 'npLyricsBtn', 'npQueueBtn', 'playPauseBtn', 'pauseBtn'];
  for (const id of ids) {
    const el = getEl(id);
    if (typeof el.onclick === 'function') {
      try { await el.onclick({}); } catch (e) { failures.push(id + '.onclick: ' + e.stack); }
    }
  }
  for (const [sel, els] of [['transport', ctx.document.querySelectorAll('.transport [data-cmd]')]]) {
    for (const b of els) {
      try { await b.onclick({}); } catch (e) { failures.push(sel + '.' + b.dataset.cmd + ': ' + e.stack); }
    }
  }
  // change handlers (seek/vol/engine/headless/explicit/layout/focus)
  for (const id of ['engine', 'headless', 'explicit', 'setFsLayout', 'setFsLyrics', 'setLyricsFocus', 'seek', 'fsSeek', 'vol', 'q']) {
    const el = getEl(id);
    if (typeof el.onchange === 'function') {
      try { await el.onchange({ target: el }); } catch (e) { failures.push(id + '.onchange: ' + e.stack); }
    }
  }
  await new Promise((r) => setImmediate(r));
  await new Promise((r) => setImmediate(r));
}

(async () => {
  await fireAll();
  // Referenced-but-undefined guard, scoped to the app's own camelCase
  // helpers (the class that caused both fullscreen regressions).
  const defined = new Set([...src.matchAll(/function (\w+)\s*\(/g)].map((m) => m[1]));
  const called = new Set([...src.matchAll(/(?<![\w$.])((?:render|sync|build|highlight|paint|open|load|play|note|est|reset|wire|init|refresh|auto|hide|ctx|focused|paint|update)[A-Z]\w*)\s*\(/g)].map((m) => m[1]));
  for (const name of called) {
    if (!defined.has(name)) failures.push('undefined helper called: ' + name);
  }
  if (failures.length) {
    console.error('FAILURES:\n- ' + failures.join('\n- '));
    process.exit(1);
  }
  console.log('wiring: OK (all click/change handlers ran without throwing)');
})();
