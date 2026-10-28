// Behavior suite for the one-shot sign-in page (crates/shell/src/auth_page.html).
// Run directly with node, or via the `auth_page_js` Rust test (skipped when
// node is unavailable):
//
//   node crates/shell/tests/auth_page_behavior.js crates/shell/src/auth_page.html
//
// Guards the sign-in recovery paths: an injected JWT whose exp has passed must
// be refreshed through GET /dev-token, and *any* first MusicKit setup failure
// (expired text or not — a revoked/poisoned token looks fresh locally) earns
// exactly one forced retry via GET /dev-token?force=1, never a dead end like
// `MusicKit setup failed: Initialized with an expired token.`.
'use strict';
const fs = require('fs'), vm = require('vm'), assert = require('assert');

const htmlPath = process.argv[2];
if (!htmlPath) { console.error('usage: auth_page_behavior.js <auth_page.html>'); process.exit(2); }
const html = fs.readFileSync(htmlPath, 'utf8');
const scripts = [...html.matchAll(/<script(?![^>]*src=)[^>]*>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
const src = scripts.join('\n');
assert.ok(src.includes('__DEV_TOKEN__'), 'page must keep the server-side placeholder');

const b64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
const jwt = (exp) => `${b64({ alg: 'ES256' })}.${b64({ exp })}.` + 'S'.repeat(120);
const NOW = Math.floor(Date.now() / 1000);

function makeCtx(devToken, { fetchImpl, configureImpl }) {
  const listeners = {};
  const msgEl = { textContent: '', className: '' };
  const goEl = { disabled: true, addEventListener: (ev, cb) => { listeners['go:' + ev] = cb; } };
  const sandbox = {
    console, Date, JSON, Math,
    atob: (s) => Buffer.from(s, 'base64').toString('binary'),
    setTimeout: () => 0,
    document: {
      getElementById: (id) => (id === 'msg' ? msgEl : goEl),
      addEventListener: (ev, cb) => { listeners[ev] = cb; },
    },
    fetch: fetchImpl,
    MusicKit: { configure: configureImpl, getInstance: () => ({ authorize: () => Promise.resolve('MUT') }) },
    window: { fetch: fetchImpl },
  };
  vm.createContext(sandbox);
  // Same substitution render_auth_page() performs server-side (JSON-quoted).
  const code = src.replace('__DEV_TOKEN__', JSON.stringify(devToken));
  assert.ok(!code.includes('__DEV_TOKEN__'), 'placeholder not substituted');
  vm.runInContext(code, sandbox);
  return { sandbox, listeners, msgEl, goEl };
}

const flush = async () => { for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r)); };

(async () => {
  // 1. exp decoding.
  let c = makeCtx(jwt(NOW + 3600), { fetchImpl: () => { throw new Error('no fetch'); }, configureImpl: () => Promise.resolve() });
  assert.strictEqual(c.sandbox.tokenExpired(jwt(NOW + 3600)), false, 'fresh token flagged expired');
  assert.strictEqual(c.sandbox.tokenExpired(jwt(NOW - 60)), true, 'expired token accepted');
  assert.strictEqual(c.sandbox.tokenExpired('garbage'), true, 'garbage accepted');

  // 2. Expired injected token → auto-refresh → configure with fresh token → button enabled.
  const calls = { fetch: [], configure: [] };
  c = makeCtx(jwt(NOW - 60), {
    fetchImpl: (url) => { calls.fetch.push(url); return Promise.resolve({ ok: true, json: () => Promise.resolve({ token: jwt(NOW + 3600) }) }); },
    configureImpl: (opts) => { calls.configure.push(opts.developerToken); return Promise.resolve(); },
  });
  c.listeners['musickitloaded']();
  await flush();
  assert.strictEqual(calls.fetch.length, 1, 'expected one /dev-token fetch');
  assert.strictEqual(calls.fetch[0], '/dev-token');
  assert.strictEqual(calls.configure.length, 1);
  assert.strictEqual(calls.configure[0], jwt(NOW + 3600), 'configured with the refreshed token');
  assert.strictEqual(c.goEl.disabled, false, 'authorize button must enable');
  assert.strictEqual(c.msgEl.textContent, 'Ready — click Authorize Apple Music.', 'progress line must be replaced');
  assert.strictEqual(c.msgEl.className, 'ok');

  // 3. Fresh token but MusicKit still rejects with "expired" → one forced
  //    refetch (force=1 drops the app's caches), then success.
  const calls2 = { fetch: [], configure: [] };
  c = makeCtx(jwt(NOW + 3600), {
    fetchImpl: (url) => { calls2.fetch.push(url); return Promise.resolve({ ok: true, json: () => Promise.resolve({ token: jwt(NOW + 7200) }) }); },
    configureImpl: (opts) => {
      calls2.configure.push(opts.developerToken);
      return calls2.configure.length === 1
        ? Promise.reject(new Error('Initialized with an expired token.'))
        : Promise.resolve();
    },
  });
  c.listeners['musickitloaded']();
  await flush();
  assert.strictEqual(calls2.fetch.length, 1, 'expected exactly one retry fetch');
  assert.strictEqual(calls2.fetch[0], '/dev-token?force=1', 'retry must force a cache drop');
  assert.strictEqual(calls2.configure.length, 2, 'expected a second configure');
  assert.strictEqual(c.goEl.disabled, false);

  // 4. Unrelated-looking failure ("boom", no "expired" anywhere) → still one
  //    forced retry: a revoked/poisoned token looks fine locally, so the
  //    cache-dropping refetch is the only self-heal. The retry succeeds here.
  const calls3 = { fetch: [], configure: [] };
  c = makeCtx(jwt(NOW + 3600), {
    fetchImpl: (url) => { calls3.fetch.push(url); return Promise.resolve({ ok: true, json: () => Promise.resolve({ token: jwt(NOW + 7200) }) }); },
    configureImpl: (opts) => {
      calls3.configure.push(opts.developerToken);
      return calls3.configure.length === 1
        ? Promise.reject(new Error('boom'))
        : Promise.resolve();
    },
  });
  c.listeners['musickitloaded']();
  await flush();
  assert.strictEqual(calls3.fetch.length, 1, 'any first failure earns one forced refetch');
  assert.strictEqual(calls3.fetch[0], '/dev-token?force=1');
  assert.strictEqual(calls3.configure.length, 2, 'recovered on the second configure');
  assert.strictEqual(c.goEl.disabled, false);
  assert.strictEqual(c.msgEl.textContent, 'Ready — click Authorize Apple Music.');
  assert.strictEqual(c.msgEl.className, 'ok');

  // 5. Retry also fails → exactly two configures, then the plain error.
  //    No refetch loops beyond the single retry.
  const calls5 = { fetch: [], configure: [] };
  c = makeCtx(jwt(NOW + 3600), {
    fetchImpl: (url) => { calls5.fetch.push(url); return Promise.resolve({ ok: true, json: () => Promise.resolve({ token: jwt(NOW + 7200) }) }); },
    configureImpl: (opts) => { calls5.configure.push(opts.developerToken); return Promise.reject(new Error('boom')); },
  });
  c.listeners['musickitloaded']();
  await flush();
  assert.strictEqual(calls5.fetch.length, 1, 'no refetch loop');
  assert.strictEqual(calls5.configure.length, 2, 'exactly one retry');
  assert.strictEqual(c.msgEl.textContent, 'MusicKit setup failed: boom');
  assert.strictEqual(c.msgEl.className, 'err');
  assert.strictEqual(c.goEl.disabled, true, 'failed setup must keep the button disabled');

  // 6. Refresh endpoint down → the app's own message, no raw MusicKit error.
  const calls4 = { fetch: [] };
  c = makeCtx(jwt(NOW - 60), {
    fetchImpl: (url) => { calls4.fetch.push(url); return Promise.resolve({ ok: false, status: 503, json: () => Promise.resolve({ error: 'scrape broke' }) }); },
    configureImpl: () => Promise.resolve(),
  });
  c.listeners['musickitloaded']();
  await flush();
  assert.strictEqual(calls4.fetch.length, 1);
  assert.ok(/Couldn't refresh Apple's access token: scrape broke/.test(c.msgEl.textContent), c.msgEl.textContent);
  assert.ok(/Sign in with Apple Music/.test(c.msgEl.textContent), 'must say what to do next');
  assert.ok(!/MusicKit setup failed/.test(c.msgEl.textContent), 'raw/misattributed error must not leak');
  assert.strictEqual(c.goEl.disabled, true);

  console.log('auth_page_behavior: all 6 scenarios OK');
})().catch((e) => { console.error('FAILED:', e.message); process.exit(1); });
