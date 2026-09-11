const $ = (s) => document.querySelector(s);
const $$ = (s) => Array.from(document.querySelectorAll(s));
function logActivity(text, cls) {
  const box = $('#activityLog');
  if (!box) return;
  const row = document.createElement('div');
  row.className = 'log-row' + (cls ? ' ' + cls : '');
  const time = document.createElement('time');
  time.textContent = new Date().toLocaleTimeString();
  row.appendChild(time);
  row.appendChild(document.createTextNode(text));
  box.appendChild(row);
  while (box.children.length > 40) box.removeChild(box.firstChild);
  box.scrollTop = box.scrollHeight;
}
const status = (t) => {
  const el = $('#status');
  if (el) el.textContent = t;
  try { if (settings.debug) logActivity(t); } catch (e) { /* settings not ready */ }
};
// Extra traces, only when the debug toggle is on.
function dlog(t) {
  try { if (settings.debug) logActivity(t, 'dbg'); } catch (e) { /* settings not ready */ }
}
function applyDebugUi() {
  const box = $('#activityLog');
  if (box) box.classList.toggle('hidden', !settings.debug);
}

// ---------- Tauri bridge (v1-compat / v1 / v2-internals) ----------
async function invoke(cmd, args = {}) {
  const t = window.__TAURI__;
  if (t?.core?.invoke) return t.core.invoke(cmd, args);
  if (typeof t?.invoke === 'function') return t.invoke(cmd, args);
  const inner = window.__TAURI_INTERNALS__;
  if (typeof inner?.invoke === 'function') return inner.invoke(cmd, args);
  throw new Error(`Tauri bridge not found for '${cmd}' — run inside the app, not a browser.`);
}

// ---------- helpers ----------
function fmtTime(ms) {
  if (!ms || ms < 0) return '0:00';
  const s = Math.floor(ms / 1000);
  return Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0');
}
// Apple artwork templates look like .../{w}x{h}bb.jpg
function art(url, size = 300) {
  if (!url) return '';
  return url.replace('{w}', size).replace('{h}', size);
}
function showView(name) {
  $$('.view').forEach(v => v.classList.add('hidden'));
  $('#view-' + name).classList.remove('hidden');
  $$('#sidebar nav button').forEach(b => b.classList.toggle('active', b.dataset.view === name));
}
function esc(s) {
  const d = document.createElement('div');
  d.textContent = s ?? '';
  return d.innerHTML;
}

// Bump when shipping UI changes so we can tell which build is on screen.
const BUILD_TAG = '2026-09-11x-fix-builds';

// ---------- player state ----------
let current = null;        // {id,title,artist,art,duration_ms}
let playQueue = [];        // [{track, source:'user'|'autoplay'}]
let queueIndex = -1;
let radioFetching = false;
let seeking = false;
let isPlaying = false;
let userPaused = false;
let jumpGen = 0;
let jumpTimer = null;
let pendingJumpIndex = -1;
let intendedTrackId = null;
let trackEndHandled = null;
let prevSidecarPlaying = false;
let jumpInFlight = false;
let jumpTimeout = null;
let needsJumpSnap = false;
let sawNearEndFor = null;
const JUMP_DEBOUNCE_MS = 180;
const JUMP_INFLIGHT_TIMEOUT_MS = 2500;
const JUMP_NEAR_ZERO_MS = 2500;
const APPEND_BATCH = 8;

// Normalize: API objects carry `artwork.url`; the player bar needs `art`.
function asCurrent(t) {
  return {
    id: t.id,
    title: t.title,
    artist: t.artist,
    album: t.album || '',
    art: t.artwork?.url || t.art || '',
    duration_ms: t.duration_ms,
  };
}

function toQueueItem(t) {
  return { id: t.id, kind: 'song' };
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function parseUnresolvedIds(text) {
  const m = /could not be resolved:\s*([0-9,\s]+)/i.exec(text || '');
  if (!m) return [];
  return m[1].split(',').map((s) => s.trim()).filter(Boolean);
}

function dropUnresolvedIds(ids) {
  if (!ids.length) return;
  playQueue = playQueue.filter((e) => !ids.includes(e.track.id));
  dlog('dropped unresolvable: ' + ids.join(', '));
  renderQueueView();
}

function handleResolveError(errText, batch) {
  const bad = parseUnresolvedIds(errText);
  if (bad.length) {
    dropUnresolvedIds(bad);
    return;
  }
  status(errText);
}

async function appendQueueBatched(_items) {
  // JS playQueue is the source of truth for Next/Previous; MusicKit only plays
  // the current song. Appending to MusicKit's queue is unreliable here.
}

async function maybeFillRadio() {
  if (!settings.radio || radioFetching || !current?.id) return;
  const remaining = playQueue.length - queueIndex - 1;
  const userRemaining = playQueue.slice(queueIndex + 1).filter((e) => e.source === 'user').length;
  if (remaining > 2 && userRemaining > 1) return;
  radioFetching = true;
  try {
    const similar = await invoke('similar_songs', { songId: current.id });
    const have = new Set(playQueue.map((e) => e.track.id));
    const fresh = (similar || []).filter((t) => t.id && !have.has(t.id));
    if (!fresh.length) return;
    for (const t of fresh) {
      playQueue.push({ track: asCurrent(t), source: 'autoplay' });
    }
    await appendQueueBatched(fresh.map((t) => toQueueItem(t)));
    renderQueueView();
  } catch (e) {
    dlog('radio: ' + String(e));
  } finally {
    radioFetching = false;
  }
}

function resetJumpAdvanceState() {
  prevSidecarPlaying = false;
  sawNearEndFor = null;
}

function finishJumpCommit() {
  jumpInFlight = false;
  needsJumpSnap = true;
  if (jumpTimeout) {
    clearTimeout(jumpTimeout);
    jumpTimeout = null;
  }
  const now = performance.now();
  anchor = { pos: 0, at: now };
  playStamp = now;
  hadForwardReport = false;
  lastSamePollAt = 0;
}

function beginJump() {
  jumpInFlight = true;
  needsJumpSnap = false;
  resetJumpAdvanceState();
  if (jumpTimeout) clearTimeout(jumpTimeout);
  jumpTimeout = setTimeout(() => {
    jumpTimeout = null;
    jumpInFlight = false;
  }, JUMP_INFLIGHT_TIMEOUT_MS);
}

function applyQueueJumpUI(i) {
  const t = playQueue[i].track;
  queueIndex = i;
  current = { ...t };
  lastReportedTrackId = t.id;
  intendedTrackId = t.id;
  if (lyric.trackId !== t.id) {
    lyric = { trackId: t.id, title: t.title || '', artist: t.artist || '', lines: [], text: '', source: '' };
    $('#lyricsTitle').textContent = lyricTitleFor(t);
    renderLyrics();
  }
  resetProgress();
  paintNowPlaying(isPlaying);
  renderQueueView();
}

async function commitQueueJump(gen) {
  if (gen !== jumpGen) return;
  const i = pendingJumpIndex;
  if (i < 0 || i >= playQueue.length) return;
  const t = playQueue[i].track;
  try {
    await invoke('sidecar_play', { items: [{ id: t.id, kind: 'song' }], startIndex: 0 });
    if (gen !== jumpGen) return;
    isPlaying = true;
    userPaused = false;
    trackEndHandled = null;
    finishJumpCommit();
    paintNowPlaying(true);
    autoFetchLyrics(t);
    maybeFillRadio();
  } catch (e) {
    jumpInFlight = false;
    needsJumpSnap = false;
    if (jumpTimeout) { clearTimeout(jumpTimeout); jumpTimeout = null; }
    status(String(e));
  }
}

function scheduleQueueJump(i) {
  if (i < 0 || i >= playQueue.length) return;
  jumpGen++;
  const gen = jumpGen;
  pendingJumpIndex = i;
  beginJump();
  applyQueueJumpUI(i);
  clearTimeout(jumpTimer);
  jumpTimer = setTimeout(() => {
    jumpTimer = null;
    commitQueueJump(gen);
  }, JUMP_DEBOUNCE_MS);
}

async function playTrack(t, queue) {
  const tracks = (queue && queue.length ? queue : [t]);
  playQueue = tracks.map((tr) => ({ track: asCurrent(tr), source: 'user' }));
  queueIndex = playQueue.findIndex((e) => e.track.id === t.id);
  if (queueIndex < 0) queueIndex = 0;
  jumpGen++;
  const gen = jumpGen;
  pendingJumpIndex = queueIndex;
  beginJump();
  applyQueueJumpUI(queueIndex);
  clearTimeout(jumpTimer);
  jumpTimer = null;
  try {
    const msg = await invoke('sidecar_play', {
      items: [{ id: t.id, kind: 'song' }],
      startIndex: 0,
    });
    if (gen !== jumpGen) return;
    isPlaying = true;
    userPaused = false;
    trackEndHandled = null;
    finishJumpCommit();
    paintNowPlaying(true);
    status(msg);
    const rest = playQueue.slice(queueIndex + 1).map((e) => toQueueItem(e.track));
    if (rest.length) await appendQueueBatched(rest);
    autoFetchLyrics(t);
    maybeFillRadio();
  } catch (e) {
    jumpInFlight = false;
    needsJumpSnap = false;
    if (jumpTimeout) { clearTimeout(jumpTimeout); jumpTimeout = null; }
    status(String(e));
  }
  renderQueueView();
}

async function addToQueue(t) {
  const entry = { track: asCurrent(t), source: 'user' };
  if (!current || queueIndex < 0) {
    await playTrack(t);
    return;
  }
  playQueue.push(entry);
  try {
    await invoke('sidecar_append', { items: [toQueueItem(t)] });
  } catch (e) { handleResolveError(String(e), [toQueueItem(t)]); }
  renderQueueView();
}

async function playNextInQueue(t) {
  const entry = { track: asCurrent(t), source: 'user' };
  if (!current || queueIndex < 0) {
    await playTrack(t);
    return;
  }
  playQueue.splice(queueIndex + 1, 0, entry);
  try {
    await invoke('sidecar_play_next', { items: [toQueueItem(t)] });
  } catch (e) { status(String(e)); }
  renderQueueView();
}

async function jumpToQueueIndex(i) {
  scheduleQueueJump(i);
}

function removeFromQueue(i) {
  if (i < 0 || i >= playQueue.length) return;
  playQueue.splice(i, 1);
  if (i < queueIndex) queueIndex--;
  else if (i === queueIndex) queueIndex = Math.min(queueIndex, playQueue.length - 1);
  renderQueueView();
}

async function clearQueue() {
  playQueue = [];
  queueIndex = -1;
  current = null;
  lastReportedTrackId = null;
  intendedTrackId = null;
  pendingJumpIndex = -1;
  clearTimeout(jumpTimer);
  jumpTimer = null;
  jumpInFlight = false;
  needsJumpSnap = false;
  if (jumpTimeout) { clearTimeout(jumpTimeout); jumpTimeout = null; }
  trackEndHandled = null;
  sawNearEndFor = null;
  prevSidecarPlaying = false;
  isPlaying = false;
  userPaused = false;
  try { await invoke('sidecar_clear'); } catch (e) { status(String(e)); }
  paintNowPlaying(false);
  $('#nowPlaying').textContent = 'Not playing.';
  renderQueueView();
}

function renderQueueView() {
  const box = $('#queueBody');
  if (!box) return;
  if (!playQueue.length) {
    box.innerHTML = '<p class="dim">Nothing queued.</p>';
    return;
  }
  box.innerHTML = '';
  const list = document.createElement('div');
  list.className = 'tracks';
  playQueue.forEach((entry, i) => {
    const t = entry.track;
    const d = document.createElement('div');
    d.className = 'track' + (i === queueIndex ? ' queue-now' : '') + (entry.source === 'autoplay' ? ' queue-autoplay' : '');
    d.dataset.id = t.id;
    d.innerHTML =
      `<span class="num">${i === queueIndex ? '▶' : i + 1}</span>` +
      `<img loading="lazy" />` +
      `<div><div class="tt"></div><div class="aa"></div></div>` +
      `<span class="dur">${fmtTime(t.duration_ms)}</span>` +
      `<button class="mini act-remove" title="Remove">✕</button>`;
    const img = d.querySelector('img');
    if (t.art) img.src = art(t.art, 100);
    d.querySelector('.tt').textContent = t.title || t.id;
    d.querySelector('.aa').textContent = t.artist || '';
    d.addEventListener('click', (ev) => {
      if (ev.target.closest('.act-remove')) return;
      jumpToQueueIndex(i);
    });
    d.querySelector('.act-remove').onclick = (ev) => {
      ev.stopPropagation();
      removeFromQueue(i);
    };
    list.appendChild(d);
  });
  box.appendChild(list);
}

function loadQueueView() {
  showView('queue');
  renderQueueView();
}

// One in-flight lyrics fetch per song id (autoFetch + openLyrics share it).
const lyricsFetchInFlight = new Map();
const lyricsCache = new Map();

function fetchLyrics(t) {
  if (!t?.id) return Promise.reject(new Error('no track'));
  const cached = lyricsCache.get(t.id);
  if (cached) return Promise.resolve(cached);
  const existing = lyricsFetchInFlight.get(t.id);
  if (existing) return existing;
  let resolve;
  let reject;
  const shared = new Promise((res, rej) => { resolve = res; reject = rej; });
  lyricsFetchInFlight.set(t.id, shared);
  invoke('get_lyrics', { songId: t.id, artist: t.artist || '', title: t.title || '' })
    .then((res) => { lyricsCache.set(t.id, res); return res; })
    .then(resolve, reject)
    .finally(() => { lyricsFetchInFlight.delete(t.id); });
  return shared;
}

function applyLyricsResult(t, res) {
  lyric = { trackId: t.id, title: t.title || '', artist: t.artist || '', lines: res.lines || [], text: res.text || '', source: res.source || '' };
  const words = (res.lines || []).reduce((n, l) => n + (l.words?.length || 0), 0);
  const karaoke = (res.lines || []).filter((l) => (l.words?.length || 0) > 1).length;
  dlog(`lyrics ready: ${lyric.lines.length} lines, ${words} words, ${karaoke} karaoke lines · ${res.source || '?'}`);
  $('#lyricsTitle').textContent = lyricTitleFor(t);
  renderLyrics();
}

// Keep lyrics fresh for fullscreen: fetch quietly on every track change
// (skipped if we already hold this track's lyrics).
function autoFetchLyrics(t) {
  if (!t || !t.id) return;
  if (lyric.trackId === t.id && (lyric.lines.length || lyric.text)) return;
  if (lyricsCache.has(t.id)) {
    applyLyricsResult(t, lyricsCache.get(t.id));
    return;
  }
  const pending = lyricsFetchInFlight.has(t.id);
  if (lyric.trackId !== t.id) {
    lyric = { trackId: t.id, title: t.title || '', artist: t.artist || '', lines: [], text: '', source: '' };
  }
  if (pending) return;
  dlog('lyrics fetch: ' + t.title);
  fetchLyrics(t)
    .then((res) => {
      if (lyric.trackId !== t.id) return; // moved on meanwhile
      applyLyricsResult(t, res);
    })
    .catch((e) => {
      if (lyric.trackId !== t.id) return;
      dlog('lyrics failed: ' + String(e));
      lyric = { trackId: t.id, title: t.title || '', artist: t.artist || '', lines: [], text: '', source: '' };
      renderLyrics();
    });
}

function paintNowPlaying(playing) {
  isPlaying = playing;
  $('#playPauseBtn').classList.toggle('hidden', playing);
  $('#pauseBtn').classList.toggle('hidden', !playing);
  const fsPlay = $('#fsPlay'), fsPause = $('#fsPause');
  if (fsPlay && fsPause) {
    fsPlay.classList.toggle('hidden', playing);
    fsPause.classList.toggle('hidden', !playing);
  }
  if (!current) return;
  $('#nowPlaying').textContent = current.title || '?';
  $('#npArtist').textContent = current.artist || '';
  $('#npAlbum').textContent = current.album || '';
  if (current.art) $('#npCover').src = art(current.art, 200);
  $('#durTime').textContent = fmtTime(current.duration_ms);
  syncFsMeta();
  $$('.track.playing').forEach(r => r.classList.remove('playing'));
  const row = document.querySelector(`.track[data-id="${CSS.escape(current.id)}"]`);
  if (row) row.classList.add('playing');
}

// ---------- smooth position: interpolate locally between sidecar reports ----------
// Monotonic while playing: never snap backward on laggy or duplicate polls.
const BACKWARD_TOLERANCE_MS = 800;
const STALL_FREEZE_MS = 1500;
let anchor = { pos: 0, at: performance.now() };
let playStamp = 0;
let hadForwardReport = false;
let seekStamp = 0;
let seekTarget = 0;
let prevPollPos = -1;
let lastSamePollAt = 0;
function markSeek(ms) {
  seekStamp = performance.now();
  seekTarget = ms;
  anchor = { pos: ms, at: performance.now() };
  lastSamePollAt = 0;
}
function estPos() {
  if (!isPlaying) return anchor.pos;
  const p = anchor.pos + (performance.now() - anchor.at);
  return current?.duration_ms ? Math.min(p, current.duration_ms) : p;
}
function canSnapJump(pos, trackId) {
  const idMatch = !!(trackId && intendedTrackId && trackId === intendedTrackId);
  const idOk = !trackId || idMatch;
  const posOk = pos < JUMP_NEAR_ZERO_MS || idMatch;
  return idOk && posOk;
}

function sidecarPosTrusted(trackId, pos) {
  const posMs = pos ?? 0;
  const pending = jumpInFlight || needsJumpSnap;
  if (!pending) return true;
  if (trackId && intendedTrackId && trackId !== intendedTrackId) return false;
  if (!trackId && posMs >= JUMP_NEAR_ZERO_MS) return false;
  return true;
}

function noteReport(pos, playing, trackId) {
  const now = performance.now();
  if ((jumpInFlight || needsJumpSnap) && !sidecarPosTrusted(trackId, pos)) return;
  if (seeking || (typeof seekingFs !== 'undefined' && seekingFs)) { anchor = { pos, at: now }; return; }
  if (!hadForwardReport) {
    if (now - playStamp < 3000 && pos < anchor.pos) return;
    if (pos >= anchor.pos) hadForwardReport = true;
  } else if (now - playStamp < 1500 && pos < estPos() - 400) {
    return;
  }
  if (playing) {
    const est = estPos();
    if (pos < est - BACKWARD_TOLERANCE_MS) return;
    if (pos >= est - 50) anchor = { pos, at: now };
    return;
  }
  anchor = { pos, at: now };
}
function resetProgress() {
  anchor = { pos: 0, at: performance.now() };
  playStamp = performance.now();
  prevPollPos = -1;
  lastSamePollAt = 0;
  hadForwardReport = false;
  const posEl = $('#posTime');
  if (posEl) posEl.textContent = fmtTime(0);
  const fsPos = $('#fsPos');
  if (fsPos) fsPos.textContent = fmtTime(0);
  if (!seeking) {
    const seek = $('#seek');
    if (seek) seek.value = 0;
  }
  if (!seekingFs) {
    const fsSeek = $('#fsSeek');
    if (fsSeek) fsSeek.value = 0;
  }
  lyricActive = -2;
  highlightLyric(0);
}

function trackRow(t, index, queue) {
  const d = document.createElement('div');
  d.className = 'track';
  d.dataset.id = t.id;
  d.innerHTML =
    `<span class="num">${index != null ? index + 1 : '♪'}</span>` +
    `<img loading="lazy" />` +
    `<div><div class="tt"></div><div class="aa"></div></div>` +
    `<span class="dur">${fmtTime(t.duration_ms)}</span>` +
    `<button class="mini act-lyrics">Lyrics</button>`;
  d.querySelector('img').src = art(t.artwork?.url, 100);
  d.querySelector('.tt').textContent = t.title || t.id;
  const aa = d.querySelector('.aa');
  aa.innerHTML = '';
  if (t.artist) {
    const aEl = document.createElement('span');
    aEl.className = 'aa-artist';
    aEl.textContent = t.artist;
    aEl.title = 'Open artist';
    aEl.onclick = (ev) => { ev.stopPropagation(); openArtistByName(t.artist); };
    aa.appendChild(aEl);
  }
  if (t.artist && t.album) aa.appendChild(document.createTextNode(' · '));
  if (t.album) {
    const alEl = document.createElement('span');
    alEl.className = 'aa-album';
    alEl.textContent = t.album;
    alEl.title = 'Open album';
    alEl.onclick = (ev) => { ev.stopPropagation(); openAlbumByName(t); };
    aa.appendChild(alEl);
  }
  d.addEventListener('click', (ev) => {
    const hit = ev.target && ev.target.closest ? ev.target : ev.target && ev.target.parentElement;
    if (hit && hit.closest('.mini')) return;
    playTrack(t, queue);
  });
  d.addEventListener('contextmenu', (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    openTrackMenu(ev.clientX, ev.clientY, t, queue);
  });
  d.querySelector('.act-lyrics').onclick = (ev) => { ev.stopPropagation(); openLyrics(t); };
  return d;
}

// ---------- right-click menu: playlists, artist/album links ----------
let ctxPlaylistsCache = null;
function hideCtx() {
  const m = $('#ctxMenu');
  if (m) m.classList.add('hidden');
}
document.addEventListener('click', hideCtx);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hideCtx(); });

function ctxButton(menu, label, fn, disabled) {
  const b = document.createElement('button');
  b.textContent = label;
  if (disabled) b.disabled = true;
  else b.onclick = (ev) => { ev.stopPropagation(); hideCtx(); fn(); };
  menu.appendChild(b);
  return b;
}

async function openTrackMenu(x, y, t, queue) {
  const m = $('#ctxMenu');
  m.innerHTML = '';
  const title = document.createElement('div');
  title.className = 'ctx-title';
  title.textContent = (t.title || t.id) + (t.artist ? ' — ' + t.artist : '');
  m.appendChild(title);
  ctxButton(m, '▶ Play', () => playTrack(t, queue));
  ctxButton(m, 'Play Next', () => playNextInQueue(t));
  ctxButton(m, 'Add to Queue', () => addToQueue(t));
  ctxButton(m, '♥ Add to favorites', async () => {
    try { status(await invoke('add_to_favorites', { songIds: [t.id] })); }
    catch (e) { status(String(e)); }
  });
  ctxButton(m, 'Lyrics', () => openLyrics(t));
  if (t.artist) ctxButton(m, 'Artist → ' + t.artist, () => openArtistByName(t.artist));
  if (t.album) ctxButton(m, 'Album → ' + t.album, () => openAlbumByName(t));
  const head = document.createElement('div');
  head.className = 'ctx-head';
  head.textContent = 'Add to playlist';
  m.appendChild(head);
  try {
    ctxPlaylistsCache = ctxPlaylistsCache || await invoke('library_playlists');
    if (!ctxPlaylistsCache.length) ctxButton(m, '(no playlists yet)', null, true);
    for (const p of ctxPlaylistsCache) {
      ctxButton(m, p.name || p.id, async () => {
        try { status(await invoke('add_to_playlist', { playlistId: p.id, songIds: [t.id] })); }
        catch (e) { status(String(e)); }
      });
    }
  } catch (e) { ctxButton(m, '(login to see playlists)', null, true); }
  ctxButton(m, '+ New playlist…', async () => {
    const name = window.prompt('Playlist name:');
    if (!name || !name.trim()) return;
    try {
      const id = await invoke('create_playlist', { name: name.trim() });
      ctxPlaylistsCache = null; // refresh list next time
      status(await invoke('add_to_playlist', { playlistId: id, songIds: [t.id] }));
    } catch (e) { status(String(e)); }
  });
  m.classList.remove('hidden');
  m.style.left = Math.min(x, window.innerWidth - 250) + 'px';
  m.style.top = Math.min(y, window.innerHeight - 320) + 'px';
}

async function openAlbumByName(t) {
  if (!t.album) return;
  status('Finding album…');
  try {
    const r = await invoke('search_catalog', { term: (t.artist ? t.artist + ' ' : '') + t.album });
    const a = (r.albums || [])[0];
    if (!a) { status('No album found for ' + t.album); return; }
    openAlbum(a.id);
  } catch (e) { status(String(e)); }
}

// Release kind mirrors core `release_kind` (backend sends is_single /
// track_count; Apple exposes no isEp flag so EPs are heuristical).
function releaseKind(a) {
  if (!a || typeof a !== 'object') return 'album';
  if (a.is_single === true) return 'single';
  const n = Number(a.track_count);
  if (Number.isFinite(n) && n === 1) return 'single';
  const title = String(a.title || a.name || '');
  if (title.trimEnd().toLowerCase().endsWith(' - ep')) return 'ep';
  if (Number.isFinite(n) && n >= 2 && n <= 6) return 'ep';
  return 'album';
}

function groupReleases(albums) {
  const singles = [], eps = [], rest = [];
  for (const a of albums || []) {
    const k = releaseKind(a);
    if (k === 'single') singles.push(a);
    else if (k === 'ep') eps.push(a);
    else rest.push(a);
  }
  return { singles, eps, albums: rest };
}

// Render-time safety net against repeat entries (backend dedupes too).
function dedupeAlbums(albums) {
  const seen = new Set();
  const out = [];
  for (const a of albums || []) {
    const id = a && a.id != null ? String(a.id) : '';
    if (id) {
      if (seen.has(id)) continue;
      seen.add(id);
    }
    out.push(a);
  }
  return out;
}

// "2026-09-04" -> "Sep 2026" (falls back to the raw string).
function fmtReleaseDate(iso) {
  const m = /^(\d{4})-(\d{2})-\d{2}$/.exec(String(iso || ''));
  if (!m) return String(iso || '');
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  return `${months[Number(m[2]) - 1] || m[2]} ${m[1]}`;
}

// Featured newest release (click opens the album). Rendered once at the top;
// the entry is excluded from the Singles/EPs/Albums sections below.
function latestReleaseEl(a) {
  const d = document.createElement('div');
  d.className = 'latest-release';
  d.setAttribute('role', 'button');
  d.title = 'Open ' + (a.title || a.name || '');
  d.innerHTML = `<img loading="lazy" alt="" /><div><div class="latest-eyebrow">Latest release</div><div class="latest-title"></div><div class="latest-sub dim"></div></div>`;
  d.querySelector('img').src = art(a.artwork?.url, 300);
  d.querySelector('.latest-title').textContent = a.title || a.name || a.id;
  const kind = releaseKind(a);
  const bits = [kind === 'single' ? 'Single' : kind === 'ep' ? 'EP' : 'Album'];
  if (a.release_date) bits.push(fmtReleaseDate(a.release_date));
  const n = Number(a.track_count);
  if (Number.isFinite(n) && n > 0) bits.push(n + (n === 1 ? ' song' : ' songs'));
  d.querySelector('.latest-sub').textContent = bits.join(' · ');
  d.onclick = () => openAlbum(a.id);
  return d;
}

function appendReleaseSection(v, title, items) {
  if (!items || !items.length) return;
  const sec = document.createElement('div');
  sec.className = 'release-sec';
  const head = document.createElement('div');
  head.className = 'sec-head';
  const h = document.createElement('h2');
  h.textContent = `${title} (${items.length})`;
  head.appendChild(h);
  sec.appendChild(head);
  const grid = albumCards(items, (a) => openAlbum(a.id));
  sec.appendChild(grid);
  v.appendChild(sec);
  // Collapse to one row when the grid spans multiple rows. Column count
  // depends on viewport width, so the first row is measured after layout.
  const raf = (window.requestAnimationFrame || ((fn) => fn())).bind(window);
  raf(() => {
    const cards = Array.from(grid.children || []);
    if (cards.length < 2
      || typeof cards[0].offsetTop !== 'number'
      || typeof cards[0].getBoundingClientRect !== 'function') return;
    const firstTop = cards[0].offsetTop;
    const firstRow = cards.filter((c) => c.offsetTop === firstTop);
    if (items.length <= firstRow.length) return; // single row: nothing to collapse
    const rowHeight = () => firstRow[0].getBoundingClientRect().height;
    const btn = document.createElement('button');
    btn.className = 'sec-toggle';
    btn.type = 'button';
    const label = document.createElement('span');
    label.className = 'sec-toggle-label';
    const chev = document.createElement('span');
    chev.className = 'sec-toggle-chev';
    chev.setAttribute('aria-hidden', 'true');
    chev.textContent = '▾';
    btn.appendChild(label);
    btn.appendChild(chev);
    const setCollapsed = (collapsed) => {
      grid.classList.toggle('collapsed', collapsed);
      btn.classList.toggle('open', !collapsed);
      grid.style.maxHeight = collapsed ? rowHeight() + 'px' : '';
      label.textContent = collapsed ? `Show all ${items.length}` : 'Show less';
      btn.setAttribute('aria-expanded', String(!collapsed));
    };
    btn.onclick = () => setCollapsed(!grid.classList.contains('collapsed'));
    window.addEventListener('resize', () => {
      if (grid.classList.contains('collapsed')) grid.style.maxHeight = rowHeight() + 'px';
    });
    head.appendChild(btn);
    setCollapsed(true);
  });
}

function albumCards(items, onOpen) {
  const wrap = document.createElement('div');
  wrap.className = 'cards';
  for (const a of items) {
    const c = document.createElement('div');
    c.className = 'card';
    c.innerHTML = `<img loading="lazy" /><div class="t"></div><div class="a"></div>`;
    c.querySelector('img').src = art(a.artwork?.url, 300);
    c.querySelector('.t').textContent = a.title || a.name || a.id;
    c.querySelector('.a').textContent = a.artist || '';
    c.onclick = () => onOpen(a);
    wrap.appendChild(c);
  }
  return wrap;
}

// ---------- views ----------
async function loadBrowse() {
  const v = $('#view-browse');
  v.innerHTML = '<h2>Browse</h2><p class="dim">Loading charts…</p>';
  try {
    const r = await invoke('browse_charts');
    v.innerHTML = '<h2>Top Songs</h2>';
    const songs = document.createElement('div');
    songs.className = 'tracks';
    const q = r.tracks || [];
    (r.tracks || []).forEach((t, i) => songs.appendChild(trackRow(t, i, q)));
    v.appendChild(songs);
    if (r.albums?.length) {
      v.appendChild(Object.assign(document.createElement('h2'), { textContent: 'Top Albums' }));
      v.appendChild(albumCards(r.albums, (a) => openAlbum(a.id)));
    }
    if (r.playlists?.length) {
      v.appendChild(Object.assign(document.createElement('h2'), { textContent: 'Top Playlists' }));
      const cards = albumCards(r.playlists.map(p => ({ ...p, title: p.name })), (p) => openPlaylist(p.id));
      v.appendChild(cards);
    }
  } catch (e) { v.innerHTML = '<h2>Browse</h2><p>Failed: ' + esc(String(e)) + '</p>'; }
}

async function doSearch() {
  const term = $('#q').value.trim();
  if (!term) return;
  showView('search');
  const v = $('#view-search');
  v.innerHTML = '<h2>Results</h2><p class="dim">Searching…</p>';
  try {
    const r = await invoke('search_catalog', { term });
    v.innerHTML = '<h2>Results for “' + esc(term) + '”</h2>';
    if (r.tracks?.length) {
      v.appendChild(Object.assign(document.createElement('h3'), { textContent: 'Songs' }));
      const box = document.createElement('div');
      box.className = 'tracks';
      const q = r.tracks;
      r.tracks.forEach((t, i) => box.appendChild(trackRow(t, i, q)));
      v.appendChild(box);
    }
    if (r.albums?.length) {
      v.appendChild(Object.assign(document.createElement('h3'), { textContent: 'Albums' }));
      v.appendChild(albumCards(r.albums, (a) => openAlbum(a.id)));
    }
    if (r.playlists?.length) {
      v.appendChild(Object.assign(document.createElement('h3'), { textContent: 'Playlists' }));
      v.appendChild(albumCards(r.playlists.map(p => ({ ...p, title: p.name })), (p) => openPlaylist(p.id)));
    }
    if (r.artists?.length) {
      v.appendChild(Object.assign(document.createElement('h3'), { textContent: 'Artists' }));
      const wrap = document.createElement('div');
      wrap.className = 'cards artists';
      for (const a of r.artists) {
        const c = document.createElement('div');
        c.className = 'card artist-card';
        c.innerHTML = artistAvatar(a, 300) + '<div class="t"></div><div class="a"></div>';
        c.querySelector('.t').textContent = a.name || a.id;
        c.querySelector('.a').textContent = (a.genres || []).join(' · ');
        c.onclick = () => openArtist(a.id);
        wrap.appendChild(c);
      }
      v.appendChild(wrap);
    }
    if (!r.tracks?.length && !r.albums?.length && !r.playlists?.length && !r.artists?.length) {
      v.innerHTML += '<p class="dim">Nothing found.</p>';
    }
  } catch (e) { v.innerHTML = '<h2>Search</h2><p>Failed: ' + esc(String(e)) + '</p>'; }
}

async function loadPlaylists() {
  showView('playlists');
  const v = $('#view-playlists');
  v.innerHTML = '<h2>Your Playlists</h2><p class="dim">Loading library…</p>';
  try {
    const pls = await invoke('library_playlists');
    v.innerHTML = '<h2>Your Playlists</h2>';
    if (!pls?.length) { v.innerHTML += '<p class="dim">No library playlists (save MUT first).</p>'; return; }
    v.appendChild(albumCards(pls.map(p => ({ ...p, title: p.name })), (p) => openPlaylist(p.id)));
  } catch (e) { v.innerHTML = '<h2>Playlists</h2><p>Failed (need saved MUT?): ' + esc(String(e)) + '</p>'; }
}

function detailHead({ img, title, sub, extra, onPlayAll }) {
  const h = document.createElement('div');
  h.innerHTML = `<div class="detail-head"><img /><div><h1></h1><p class="sub"></p><p class="xtra dim"></p><button class="btn-accent">Play</button></div></div>`;
  h.querySelector('img').src = img || '';
  h.querySelector('h1').textContent = title || '?';
  h.querySelector('.sub').textContent = sub || '';
  h.querySelector('.xtra').textContent = extra || '';
  h.querySelector('button').onclick = onPlayAll;
  return h;
}

async function openAlbum(id) {
  showView('detail');
  const v = $('#view-detail');
  v.innerHTML = '<p class="dim">Loading album…</p>';
  try {
    const d = await invoke('get_album', { id });
    v.innerHTML = '';
    const q = d.tracks.map((t) => ({ ...t, artwork: t.artwork || d.album.artwork, album: t.album || d.album.title }));
    v.appendChild(detailHead({
      img: art(d.album.artwork?.url, 400),
      title: d.album.title, sub: d.album.artist,
      extra: (d.tracks.length || '') + (d.tracks.length === 1 ? ' song' : ' songs'),
      onPlayAll: () => q.length && playTrack(q[0], q),
    }));
    const box = document.createElement('div');
    box.className = 'tracks';
    q.forEach((t, i) => box.appendChild(trackRow(t, i, q)));
    v.appendChild(box);
  } catch (e) { v.innerHTML = '<p>Failed: ' + esc(String(e)) + '</p>'; }
}

async function openArtistByName(name) {
  if (!name) return;
  status('Finding artist…');
  try {
    const r = await invoke('search_catalog', { term: name });
    const list = r.artists || [];
    const a = list.find(x => (x.name || '').toLowerCase() === name.toLowerCase()) || list[0];
    if (!a) { status('No artist found for ' + name); return; }
    openArtist(a.id);
  } catch (e) { status(String(e)); }
}

function artistAvatar(a, size) {
  if (a.artwork?.url) return `<img loading="lazy" src="${esc(art(a.artwork.url, size || 300))}" />`;
  const initial = (a.name || '?').trim().charAt(0).toUpperCase() || '?';
  return `<div class="avatar" aria-hidden="true">${esc(initial)}</div>`;
}

async function openArtist(id) {
  showView('detail');
  const v = $('#view-detail');
  v.innerHTML = '<p class="dim">Loading artist…</p>';
  try {
    const d = await invoke('get_artist', { id });
    v.innerHTML = '';
    const h = document.createElement('div');
    h.className = 'detail-head artist-head';
    h.innerHTML = `<div class="avatar big"></div><div><h1></h1><p class="sub"></p><p class="xtra dim"></p></div>`;
    if (d.artist.artwork?.url) {
      const img = document.createElement('img');
      img.src = art(d.artist.artwork.url, 400);
      h.replaceChild(img, h.querySelector('.avatar'));
    } else {
      h.querySelector('.avatar').textContent = (d.artist.name || '?').trim().charAt(0).toUpperCase();
    }
    h.querySelector('h1').textContent = d.artist.name || '?';
    h.querySelector('.sub').textContent = (d.artist.genres || []).join(' · ');
    const albums = dedupeAlbums(d.albums);
    // Newest dated release is featured up top (backend sorts newest-first).
    const latestIdx = albums.findIndex((a) => a && a.release_date);
    const latest = latestIdx >= 0 ? albums[latestIdx] : null;
    const rest = latest ? albums.filter((_, i) => i !== latestIdx) : albums;
    const groups = groupReleases(rest);
    const bits = [];
    if (groups.singles.length) bits.push(groups.singles.length + (groups.singles.length === 1 ? ' single' : ' singles'));
    if (groups.eps.length) bits.push(groups.eps.length + (groups.eps.length === 1 ? ' EP' : ' EPs'));
    if (groups.albums.length) bits.push(groups.albums.length + (groups.albums.length === 1 ? ' album' : ' albums'));
    h.querySelector('.xtra').textContent = bits.join(' · ') || 'no releases yet';
    v.appendChild(h);
    if (latest) v.appendChild(latestReleaseEl(latest));
    appendReleaseSection(v, 'Singles', groups.singles);
    appendReleaseSection(v, 'EPs', groups.eps);
    appendReleaseSection(v, 'Albums', groups.albums);
  } catch (e) { v.innerHTML = '<p>Failed: ' + esc(String(e)) + '</p>'; }
}

async function openPlaylist(id) {
  showView('detail');
  const v = $('#view-detail');
  v.innerHTML = '<p class="dim">Loading playlist…</p>';
  try {
    const d = await invoke('get_playlist', { id });
    v.innerHTML = '';
    const q = d.tracks;
    v.appendChild(detailHead({
      img: art(d.playlist.artwork?.url, 400),
      title: d.playlist.name, sub: d.playlist.description || '',
      extra: (d.tracks.length || '') + (d.tracks.length === 1 ? ' song' : ' songs'),
      onPlayAll: () => q.length && playTrack(q[0], q),
    }));
    const box = document.createElement('div');
    box.className = 'tracks';
    q.forEach((t, i) => box.appendChild(trackRow(t, i, q)));
    v.appendChild(box);
  } catch (e) { v.innerHTML = '<p>Failed: ' + esc(String(e)) + '</p>'; }
}

// ---------- display settings (persisted) ----------
const settings = Object.assign(
  { fsLyrics: true, fsLayout: 'vertical', lyricsFocus: false, debug: false, radio: true },
  JSON.parse(localStorage.getItem('aml-settings') || '{}')
);
function saveSettings() {
  localStorage.setItem('aml-settings', JSON.stringify(settings));
}
function applyFsSettings() {
  const o = $('#fsOverlay');
  o.classList.toggle('fs-vertical', settings.fsLayout !== 'horizontal');
  o.classList.toggle('fs-horizontal', settings.fsLayout === 'horizontal');
  o.classList.toggle('no-lyrics', !settings.fsLyrics);
}

// ---------- timed lyrics (line + word-level karaoke, Apple-style) ----------
let lyric = { trackId: null, title: '', artist: '', lines: [], text: '', source: '' };
let lyricActive = -2;

// Parentheticals (often background vocals) render smaller, Apple-style.
function lyricHTML(text) {
  return String(text).split(/(\([^)]*\))/g).map((p) =>
    /^\(.*\)$/.test(p) ? `<span class="lyr-bracket">${esc(p)}</span>` : esc(p)
  ).join('');
}

function wordHTML(text) {
  return lyricHTML(text);
}

function needsSpaceBetweenWords(prev, cur) {
  if (!prev || !cur) return false;
  if (prev.endsWith(' ') || cur.startsWith(' ')) return false;
  if (!prev.trim() || !cur.trim()) return false;
  return true;
}

function buildLyricLineContent(l) {
  if (l.words?.length) {
    let html = '';
    for (let wi = 0; wi < l.words.length; wi++) {
      const t = l.words[wi].text ?? '';
      if (!t) continue;
      if (wi > 0 && needsSpaceBetweenWords(l.words[wi - 1].text, t)) {
        html += ' ';
      }
      html += `<span class="lyr-word" data-wi="${wi}">${wordHTML(t)}</span>`;
    }
    return html;
  }
  return lyricHTML(l.text);
}

function wordMs(w) {
  const n = Number(w?.ms ?? 0);
  return Number.isFinite(n) ? n : 0;
}

function wordEndMs(w, line, wi) {
  const start = wordMs(w);
  if (w.end_ms != null && w.end_ms > start) return w.end_ms;
  const next = line.words[wi + 1];
  if (next) return wordMs(next);
  if (line.end_ms != null && line.end_ms > start) return line.end_ms;
  return start + 400;
}

function lineEndMs(l, i) {
  if (l.end_ms != null && l.end_ms > l.ms) return l.end_ms;
  const next = lyric.lines[i + 1];
  if (next) return lineMs(next);
  return current?.duration_ms || Infinity;
}

function lineInRange(l, i, pos) {
  return pos >= lineMs(l) && pos < lineEndMs(l, i);
}

function lyricAgentSide(agent) {
  const n = parseInt(String(agent || '').replace(/^v/i, ''), 10);
  if (!Number.isFinite(n) || n < 1) return '';
  return n % 2 === 1 ? 'agent-left' : 'agent-right';
}

function applyWordFill(pos) {
  if (!lyric.lines.length) return;
  if (!current || lyric.trackId !== current.id) return;
  for (const container of [$('#lyricsBody'), $('#fsLyrics')]) {
    if (!container) continue;
    container.querySelectorAll('.lyr-line').forEach((lineEl) => {
      const i = parseInt(lineEl.dataset.i, 10);
      const line = lyric.lines[i];
      if (!line?.words?.length) return;
      const inRange = lineInRange(line, i, pos);
      lineEl.querySelectorAll('.lyr-word').forEach((el, wi) => {
        const w = line.words[wi];
        if (!w) return;
        let fill = 0;
        if (inRange) {
          const start = wordMs(w);
          const end = wordEndMs(w, line, wi);
          if (pos >= end) fill = 100;
          else if (pos > start && end > start) {
            fill = Math.min(100, Math.max(0, ((pos - start) / (end - start)) * 100));
          }
        }
        el.style.setProperty('--fill', fill + '%');
      });
    });
  }
}

function lyricTitleFor(t) {
  return (t.title || '?') + (t.artist ? ' — ' + t.artist : '');
}

function lineMs(l) {
  const n = Number(l?.ms ?? l.start_ms ?? l.startMs ?? 0);
  return Number.isFinite(n) ? n : 0;
}

async function openLyrics(t) {
  showView('lyrics');
  $('#lyricsTitle').textContent = lyricTitleFor(t);
  if (lyric.trackId === t.id && (lyric.lines.length || lyric.text)) {
    renderLyrics();
    return;
  }
  $('#lyricsBody').innerHTML = '<p class="dim">Loading…</p>';
  try {
    const res = await fetchLyrics(t);
    applyLyricsResult(t, res);
  } catch (e) {
    lyric = { trackId: null, title: '', artist: '', lines: [], text: '', source: '' };
    $('#lyricsBody').innerHTML = '<p>No lyrics available (' + esc(String(e)) + ')</p>';
    $('#fsLyrics').innerHTML = '';
  }
}

function buildLyricList(container, focused) {
  container.innerHTML = '';
  container.dataset.focused = focused && lyric.lines.length ? '1' : '';
  if (!lyric.lines.length) {
    const d = document.createElement('div');
    d.className = 'lyr-static';
    d.textContent = lyric.text || 'No lyrics.';
    container.appendChild(d);
    return;
  }
  const agents = new Set(lyric.lines.map((l) => l.agent).filter(Boolean));
  const duet = agents.size > 1;
  lyric.lines.forEach((l, i) => {
    const d = document.createElement('div');
    let cls = 'lyr-line' + (l.words?.length ? ' karaoke' : '');
    if (l.bg) cls += ' bg';
    if (duet && l.agent) cls += ' ' + lyricAgentSide(l.agent);
    d.className = cls;
    d.dataset.i = String(i);
    if (l.agent) d.dataset.agent = l.agent;
    d.innerHTML = buildLyricLineContent(l);
    container.appendChild(d);
  });
  applyLyricClasses(container);
}

function applyLyricClasses(container) {
  const focused = container.dataset.focused === '1';
  const center = lyricActive >= 0 ? lyricActive : 0;
  container.querySelectorAll('.lyr-line').forEach((el) => {
    const i = parseInt(el.dataset.i, 10);
    const isActive = i === lyricActive && lyricActive >= 0;
    const dist = Math.abs(i - center);
    el.classList.toggle('active', isActive);
    el.classList.toggle('near', focused && !isActive && dist <= 2);
    el.classList.toggle('far', focused && dist > 2);
  });
}

function renderLyrics() {
  const synced = lyric.lines.length > 0;
  $('#lyricsBody').classList.toggle('focused', settings.lyricsFocus && synced);
  buildLyricList($('#lyricsBody'), settings.lyricsFocus);
  buildLyricList($('#fsLyrics'), true);
  lyricActive = -2;
  highlightLyric(estPos());
  if (!synced) {
    const meta = $('#lyricsMeta');
    if (meta) meta.textContent = '';
  }
}

function lyricCaption() {
  const meta = $('#lyricsMeta');
  if (!meta) return;
  if (!lyric.lines.length) { meta.textContent = ''; return; }
  const live = current && lyric.trackId === current.id;
  const mode = settings.lyricsFocus ? 'focused ±2' : 'full text';
  const where = live ? `live · line ${lyricActive + 1}/${lyric.lines.length}`
    : 'not the playing track — play it to follow';
  const src = lyric.source ? ` · ${lyric.source}` : '';
  meta.textContent = `live synced lyrics · ${mode} · ${where}${src}`;
}

function highlightLyric(pos) {
  if (!lyric.lines.length) return;
  let idx = -1;
  for (let i = 0; i < lyric.lines.length; i++) {
    const l = lyric.lines[i];
    if (l.bg) continue;
    if (lineMs(l) <= pos) idx = i;
    else break;
  }
  if (!current || lyric.trackId !== current.id) idx = -1;
  if (idx !== lyricActive) {
    lyricActive = idx;
    dlog(`lyric ${idx < 0 ? '—' : (idx + 1)}/${lyric.lines.length} @ ${Math.floor(pos)}ms`);
    for (const c of [$('#lyricsBody'), $('#fsLyrics')]) {
      if (!c) continue;
      applyLyricClasses(c);
      const el = c.querySelector('.lyr-line.active');
      if (el && el.offsetParent !== null) el.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
    lyricCaption();
  }
  applyWordFill(pos);
}

// Click a line → jump to its timing.
for (const c of [$('#lyricsBody'), $('#fsLyrics')]) {
  c.addEventListener('click', async (ev) => {
    const line = ev.target.closest('.lyr-line');
    if (!line || line.classList.contains('far')) return;
    const i = parseInt(line.dataset.i ?? '-1', 10);
    if (!(i >= 0 && lyric.lines[i])) return;
    const wordEl = ev.target.closest('.lyr-word');
    let ms = lineMs(lyric.lines[i]);
    if (wordEl) {
      const wi = parseInt(wordEl.dataset.wi ?? '-1', 10);
      const w = lyric.lines[i].words?.[wi];
      if (w) ms = wordMs(w);
    }
    markSeek(ms);
    try { await invoke('sidecar_seek', { positionMs: ms }); }
    catch (err) { status(String(err)); }
  });
}

// ---------- wiring ----------
$$('#sidebar nav button').forEach(b => {
  b.onclick = () => {
    const w = b.dataset.view;
    if (w === 'browse') { showView('browse'); loadBrowse(); }
    else if (w === 'playlists') loadPlaylists();
    else if (w === 'queue') loadQueueView();
    else if (w === 'lyrics') {
      showView('lyrics');
      if (current && (!lyric.trackId || lyric.trackId !== current.id)) {
        openLyrics(current);
      } else if (lyric.trackId) {
        $('#lyricsTitle').textContent = lyricTitleFor(lyric);
        renderLyrics();
      }
    }
    else showView(w);
  };
});

$('#searchBtn').onclick = doSearch;
$('#q').addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch(); });

$('#authBtn').onclick = async () => {
  try {
    const url = await invoke('authorize_url');
    if (!url) { status('Backend returned empty URL.'); return; }
    try { await invoke('open_auth_url', { url }); } catch {}
    try { window.open(url, '_blank'); } catch {}
    try { await navigator.clipboard.writeText(url); } catch {}
    status('Approve in the browser, then paste MUT / redirect URL below.');
  } catch (e) { status(String(e)); }
};
$('#mutBtn').onclick = async () => {
  try { await invoke('submit_user_token', { token: $('#mut').value.trim() }); status('MUT saved.'); refreshTokenStatus(); }
  catch (e) { status(String(e)); }
};
$('#authUrlBtn').onclick = async () => {
  try { status(await invoke('submit_auth_url', { url: $('#authUrl').value.trim() })); refreshTokenStatus(); }
  catch (e) { status(String(e)); }
};
async function refreshTokenStatus() {
  try { $('#tokenStatus').textContent = await invoke('token_status'); } catch {}
}

$('#engine').onchange = async (e) => {
  try { status(await invoke('set_engine', { engine: e.target.value }) || 'ok'); }
  catch (err) { status(String(err)); }
};
$('#headless').onchange = async (e) => {
  try {
    status(await invoke('set_sidecar_headless', { headless: e.target.checked }) + ' — relaunch sidecar to apply.');
  } catch (err) { status(String(err)); }
};
$('#relaunchBtn').onclick = async () => {
  try { await invoke('sidecar_relaunch'); status('Sidecar will relaunch on next Play.'); }
  catch (e) { status(String(e)); }
};
$('#explicit').onchange = async (e) => {
  try {
    status(await invoke('set_sidecar_explicit', { explicit: e.target.checked }) + ' — relaunch sidecar to apply.');
  } catch (err) { status(String(err)); }
};

// transport (player bar + fullscreen share one handler)
$$('.transport [data-cmd]').forEach(b => {
  b.onclick = async () => {
    const c = b.dataset.cmd;
    try {
      if (c === 'play') {
        userPaused = false;
        // Resume in place (no re-queue): works after pause AND starts
        // playback if something is already queued in the sidecar.
        await invoke('sidecar_resume');
      }
      else if (c === 'pause') {
        userPaused = true;
        anchor = { pos: estPos(), at: performance.now() };
        await invoke('sidecar_pause');
        paintNowPlaying(false);
      }
      else if (c === 'next') {
        if (queueIndex + 1 < playQueue.length) await jumpToQueueIndex(queueIndex + 1);
        else { await invoke('sidecar_next'); maybeFillRadio(); }
      }
      else if (c === 'previous') {
        if (queueIndex > 0) await jumpToQueueIndex(queueIndex - 1);
        else await invoke('sidecar_previous');
      }
    } catch (e) { status(String(e)); }
  };
});
$('#npLyricsBtn').onclick = () => { if (current) openLyrics(current); };
const npQueueBtn = $('#npQueueBtn');
if (npQueueBtn) npQueueBtn.onclick = () => loadQueueView();
$('#npArtist').onclick = () => { if (current?.artist) openArtistByName(current.artist); };
$('#fsArtist').onclick = () => { if (current?.artist) openArtistByName(current.artist); };
$('#npAlbum').onclick = () => { if (current?.album) openAlbumByName(current); };
$('#fsAlbum').onclick = () => { if (current?.album) openAlbumByName(current); };
// Real-time volume: fire on input (drag), not change (release).
// Squared (log-ish) curve: linear gain bunches all audible change into 0-25%.
async function onVolumeInput(e) {
  const raw = e.target.value;
  const v = raw / 100;
  for (const id of ['#vol', '#fsVol']) {
    const el = $(id);
    if (el && el !== e.target) el.value = raw;
  }
  try { await invoke('sidecar_volume', { level: v * v }); } catch (err) { status(String(err)); }
}
$('#vol').addEventListener('input', onVolumeInput);
const fsVolEl = $('#fsVol');
if (fsVolEl) fsVolEl.addEventListener('input', onVolumeInput);
function wireSeek(el, setFlag) {
  el.addEventListener('change', async () => {
    if (!current?.duration_ms) return;
    const target = Math.floor(el.value / 1000 * current.duration_ms);
    markSeek(target); // jump instantly; fence stale polls until caught up
    try { await invoke('sidecar_seek', { positionMs: target }); }
    catch (err) { status(String(err)); }
  });
  el.addEventListener('pointerdown', () => setFlag(true));
}
let seekingFs = false;
wireSeek($('#seek'), (v) => { seeking = v; });
wireSeek($('#fsSeek'), (v) => { seekingFs = v; });
addEventListener('pointerup', () => { seeking = false; seekingFs = false; });

// display settings
function initDisplaySettings() {
  const a = $('#setFsLyrics'), b = $('#setLyricsFocus'), c = $('#setFsLayout');
  if (!a || !b || !c) return;
  a.checked = settings.fsLyrics;
  b.checked = settings.lyricsFocus;
  c.value = settings.fsLayout;
  a.onchange = () => { settings.fsLyrics = a.checked; saveSettings(); applyFsSettings(); };
  b.onchange = () => { settings.lyricsFocus = b.checked; saveSettings(); renderLyrics(); };
  c.onchange = () => { settings.fsLayout = c.value; saveSettings(); applyFsSettings(); };
  applyFsSettings();
  const dbg = $('#setDebug');
  if (dbg) {
    dbg.checked = !!settings.debug;
    dbg.onchange = () => {
      settings.debug = dbg.checked;
      saveSettings();
      applyDebugUi();
      status('Debug ' + (dbg.checked ? 'on' : 'off'));
    };
  }
  applyDebugUi();
  const radio = $('#setRadio');
  if (radio) {
    radio.checked = settings.radio !== false;
    radio.onchange = () => {
      settings.radio = radio.checked;
      saveSettings();
      status('Radio ' + (radio.checked ? 'on' : 'off'));
    };
  }
  const clearBtn = $('#clearQueueBtn');
  if (clearBtn) clearBtn.onclick = () => clearQueue();
}

// ---------- fullscreen ----------
// Ambient glow sampled from the cover (canvas needs CORS; falls back silent).
let lastAmbientUrl = '';
function updateAmbient(artUrl) {
  const overlay = $('#fsOverlay');
  if (!overlay || !artUrl || artUrl === lastAmbientUrl) return;
  lastAmbientUrl = artUrl;
  try {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.onload = () => {
      try {
        const c = document.createElement('canvas');
        c.width = c.height = 24;
        const g = c.getContext('2d');
        g.drawImage(img, 0, 0, 24, 24);
        const d = g.getImageData(0, 0, 24, 24).data;
        let r = 0, gg = 0, b = 0, n = 0;
        for (let i = 0; i < d.length; i += 32) { r += d[i]; gg += d[i + 1]; b += d[i + 2]; n++; }
        r = Math.round(r / n); gg = Math.round(gg / n); b = Math.round(b / n);
        // Deepen so white text stays readable.
        r = Math.round(r * 0.55); gg = Math.round(gg * 0.55); b = Math.round(b * 0.55);
        overlay.style.setProperty('--ambient', `rgb(${r},${gg},${b})`);
      } catch (e) { /* tainted canvas → keep default */ }
    };
    img.src = art(artUrl, 96);
  } catch (e) { /* ignore */ }
}
function syncFsMeta() {
  if (!current) return;
  $('#fsTitle').textContent = current.title || '?';
  $('#fsArtist').textContent = current.artist || '';
  $('#fsAlbum').textContent = current.album || '';
  if (current.art) {
    $('#fsCover').src = art(current.art, 600);
    updateAmbient(current.art);
  }
}
$('#fsBtn').onclick = () => {
  syncFsMeta();
  buildLyricList($('#fsLyrics'), true);
  const o = $('#fsOverlay');
  o.classList.remove('hidden');
  try {
    const p = o.requestFullscreen && o.requestFullscreen();
    if (p && p.catch) p.catch(() => {});
  } catch {}
};
function closeFs() {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else $('#fsOverlay').classList.add('hidden');
}
$('#fsClose').onclick = closeFs;
document.addEventListener('fullscreenchange', () => {
  if (!document.fullscreenElement) $('#fsOverlay').classList.add('hidden');
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') $('#fsOverlay').classList.add('hidden');
});

// status poll → now playing + sidecar errors (progress runs on rAF below)
let lastDetail = '';
let lastReportedTrackId = null;

function sidecarTrackMatchesIntent(tid) {
  if (!tid) return false;
  if (intendedTrackId && tid !== intendedTrackId) return false;
  if (jumpTimer && pendingJumpIndex >= 0) {
    const pending = playQueue[pendingJumpIndex]?.track?.id;
    if (pending && tid !== pending) return false;
  }
  return true;
}

async function maybeAutoAdvance(s) {
  if (!current || userPaused || jumpInFlight) return;
  if (trackEndHandled === current.id) return;
  if (s.track_id && current.id && s.track_id !== current.id) return;
  const dur = current.duration_ms || s.duration_ms || 0;
  if (!dur) return;
  const pos = s.position_ms ?? estPos();
  const wasPlaying = prevSidecarPlaying;
  const nowPlaying = !!s.playing;
  if (nowPlaying && pos >= dur - 1500) sawNearEndFor = current.id;
  const completed = wasPlaying && !nowPlaying && sawNearEndFor === current.id;
  if (!completed) return;
  trackEndHandled = current.id;
  if (queueIndex + 1 < playQueue.length) {
    jumpToQueueIndex(queueIndex + 1);
    return;
  }
  await maybeFillRadio();
  if (queueIndex + 1 < playQueue.length) jumpToQueueIndex(queueIndex + 1);
}

function syncFromSidecarReport(s) {
  const tid = s.track_id || null;
  if (s.duration_ms && s.duration_ms > 0) {
    if (current) current.duration_ms = s.duration_ms;
    $('#durTime').textContent = fmtTime(s.duration_ms);
  }
  if (!tid || tid === lastReportedTrackId) return;
  if (!sidecarTrackMatchesIntent(tid)) return;
  lastReportedTrackId = tid;
  intendedTrackId = tid;
  resetProgress();
  const qi = playQueue.findIndex((e) => e.track.id === tid);
  if (qi >= 0) queueIndex = qi;
  const qe = qi >= 0 ? playQueue[qi].track : null;
  const prev = current || {};
  current = {
    id: tid,
    title: qe?.title || prev.title || s.title || '?',
    artist: qe?.artist || prev.artist || s.artist || '',
    album: qe?.album || prev.album || '',
    art: qe?.art || prev.art || '',
    duration_ms: s.duration_ms || qe?.duration_ms || prev.duration_ms,
  };
  paintNowPlaying(!!s.playing);
  if (!lyricsCache.has(tid) && lyric.trackId !== tid) autoFetchLyrics(current);
  renderQueueView();
  maybeFillRadio();
}
setInterval(async () => {
  try {
    const s = await invoke('sidecar_status');
    if (!s) return;
    if (s.detail && s.detail !== lastDetail) {
      lastDetail = s.detail;
      if (s.detail.includes('could not be resolved')) {
        dropUnresolvedIds(parseUnresolvedIds(s.detail));
      } else {
        status('Sidecar: ' + s.detail);
      }
    }
    const p = s.position_ms || 0;
    const now = performance.now();
    if (needsJumpSnap && s.playing && canSnapJump(p, s.track_id)) {
      needsJumpSnap = false;
      anchor = { pos: p, at: now };
      hadForwardReport = true;
      playStamp = now;
      lastSamePollAt = 0;
    } else {
      const trustPos = sidecarPosTrusted(s.track_id, p);
      if (now - seekStamp < 3000 && Math.abs(p - seekTarget) > 1500) {
        // Player hasn't caught up to our seek yet.
      } else if (trustPos && s.playing && p === prevPollPos) {
        if (!lastSamePollAt) lastSamePollAt = now;
        if (now - lastSamePollAt >= STALL_FREEZE_MS) {
          anchor = { pos: p, at: now };
        }
      } else if (trustPos) {
        lastSamePollAt = 0;
        noteReport(p, !!s.playing, s.track_id);
      }
    }
    prevPollPos = p;
    await maybeAutoAdvance(s);
    prevSidecarPlaying = !!s.playing;
    if (s.playing || s.title || s.track_id) {
      syncFromSidecarReport(s);
      paintNowPlaying(!!s.playing);
    }
  } catch {}
}, 500);

// frame loop: buttery progress + lyric highlight between polls
(function frame() {
  try {
    const pos = estPos();
    $('#posTime').textContent = fmtTime(pos);
    const fsPos = $('#fsPos');
    if (fsPos) fsPos.textContent = fmtTime(pos);
    if (!seeking && current?.duration_ms) {
      $('#seek').value = Math.floor(pos / current.duration_ms * 1000);
    }
    if (!seekingFs && current?.duration_ms) {
      $('#fsSeek').value = Math.floor(pos / current.duration_ms * 1000);
      $('#fsDur').textContent = fmtTime(current.duration_ms);
    }
    highlightLyric(pos);
  } catch {}
  requestAnimationFrame(frame);
})();

// boot
(async function boot() {
  const bt = $('#buildTag');
  if (bt) bt.textContent = 'build ' + BUILD_TAG;
  const names = Object.keys(window).filter(k => k.includes('TAURI'));
  status(names.length ? 'Ready. Browse is loading…' : 'No Tauri bridge — restart via cargo tauri dev.');
  try { $('#headless').checked = await invoke('sidecar_headless'); } catch {}
  try { $('#explicit').checked = await invoke('sidecar_explicit'); } catch {}
  initDisplaySettings();
  refreshTokenStatus();
  loadBrowse();
})();
