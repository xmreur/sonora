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
// Apple artwork templates look like .../{w}x{h}bb.jpg — but uploads and
// newer assets use the generic .../{w}x{h}{c}.{f} form ({c} = crop code,
// {f} = file format, {-q} = quality), which must also be substituted or
// the URL 404s. Defaults mirror Apple's web client (bb crop, jpg, q60).
function art(url, size = 300) {
  if (!url) return '';
  return url
    .replace('{w}', size)
    .replace('{h}', size)
    .replace('{c}', 'bb')
    .replace('{f}', 'jpg')
    .replace('{q}', '60');
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
const BUILD_TAG = '2026-02-17-session-restore';

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
const JUMP_DEBOUNCE_MS = 180;
const JUMP_INFLIGHT_TIMEOUT_MS = 2500;
const JUMP_NEAR_ZERO_MS = 2500;
const APPEND_BATCH = 8;

// ---------- sidecar sync ----------
// The sidecar (MusicKit in Firefox) is the source of truth for playback
// state. `sidecar_play` IPC only enqueues a command — the sidecar picks it
// up on its 500ms poll and then setQueue/decrypt takes seconds. The UI
// must NOT pretend playback started at IPC-ack time (that faked 0:00→0:02
// progress which then snapped back to 0:00). Instead a jump arms
// `awaitingSidecar` and the UI stays paused/frozen at 0 until a report
// with the matching track id AND playing=true confirms audio really
// started (`confirmSidecarPlaying`). External changes (OS media keys,
// MusicKit queue advance) surface as reports with an unexpected track id
// (or no track id on stop) and are adopted into the JS queue state so the
// two never diverge.
// `mirroredIds` tracks what the sidecar queue should hold (current first),
// so OS next/previous move within tracks the UI knows. play-now resets the
// sidecar queue, so the mirror resets with it.
let awaitingSidecar = false;
let awaitingTrackId = null;
let mirroredIds = [];
const MIRROR_AHEAD = 25;

// Normalize: API objects carry `artwork.url`; the player bar needs `art`.
// Genres are kept for playlist-wide infinite-queue affinity ranking.
function asCurrent(t) {
  return {
    id: t.id,
    title: t.title,
    artist: t.artist,
    album: t.album || '',
    art: t.artwork?.url || t.art || '',
    duration_ms: t.duration_ms,
    genres: Array.isArray(t.genres) ? [...t.genres] : [],
  };
}

// Infinite-queue origin: when playback starts from a playlist/album (a
// multi-track queue), remember the full list's artists + genres so radio
// fills stay true to the whole collection, not just the last few tracks.
// Rotated per fill so every artist/genre gets coverage over time.
let queueOrigin = null; // { tracks:[{id,artist,genres}], artistKeys:[], genreSet:Set, artistGroups:Map }
let radioFillCount = 0;

function normGenreKey(s) {
  return String(s || '').toLowerCase().replace(/[^a-z0-9]/g, '');
}

function normArtistKey(s) {
  return String(s || '').toLowerCase().split(/[^a-z0-9\u00c0-\u024f]+/u).filter(Boolean).join(' ');
}

function setQueueOrigin(queue) {
  if (!queue || queue.length < 2) {
    queueOrigin = null;
    return;
  }
  const tracks = [];
  const seen = new Set();
  for (const t of queue.slice(0, 200)) {
    if (!t || !t.id || seen.has(t.id)) continue;
    seen.add(t.id);
    tracks.push({
      id: t.id,
      artist: t.artist || '',
      genres: Array.isArray(t.genres) ? [...t.genres] : [],
    });
  }
  if (tracks.length < 2) {
    queueOrigin = null;
    return;
  }
  const artistKeys = [];
  const artistSeen = new Set();
  const genreSet = new Set();
  const artistGroups = new Map(); // artistKey -> [trackIds]
  for (const t of tracks) {
    const ak = normArtistKey(t.artist);
    if (ak && !artistSeen.has(ak)) {
      artistSeen.add(ak);
      artistKeys.push(ak);
    }
    const gk = ak || '(unknown)';
    if (!artistGroups.has(gk)) artistGroups.set(gk, []);
    artistGroups.get(gk).push(t.id);
    for (const g of t.genres) {
      const k = normGenreKey(g);
      if (k) genreSet.add(k);
    }
  }
  queueOrigin = { tracks, artistKeys, genreSet, artistGroups };
  radioFillCount = 0;
  dlog(`origin: ${tracks.length} tracks, ${artistKeys.length} artists, ${genreSet.size} genres`);
}

// Extra seeds covering the origin playlist's full artist range: one track
// per distinct artist, rotated by fill count so successive fills favor
// different artists instead of always the playlist head. Seeds are only
// backend queries (results already queued are excluded downstream), so
// already-played origin tracks are fine — and usually the only option,
// since the whole origin sits in the queue by fill time.
function originCoverageSeeds(recentIds, n) {
  if (!queueOrigin) return [];
  const recent = new Set(recentIds || []);
  const groups = Array.from(queueOrigin.artistGroups.entries());
  if (!groups.length) return [];
  const start = radioFillCount % groups.length;
  const out = [];
  for (let k = 0; k < groups.length && out.length < n; k++) {
    const [, ids] = groups[(start + k) % groups.length];
    const pick = ids.find((id) => !recent.has(id) && !out.includes(id))
      || ids.find((id) => !out.includes(id));
    if (pick && !out.includes(pick)) out.push(pick);
  }
  return out;
}

// Affinity of a candidate to the origin playlist: +2 for naming any
// origin artist, +1 per shared genre tag. Union scoring keeps mixed
// playlists mixed — any branch of the collection scores well.
function playlistAffinity(track) {
  if (!queueOrigin) return 0;
  let s = 0;
  const a = normArtistKey(track.artist);
  if (a) {
    for (const oa of queueOrigin.artistKeys) {
      if (oa && (a.includes(oa) || oa.includes(a))) {
        s += 2;
        break;
      }
    }
  }
  const tg = Array.isArray(track.genres) ? track.genres : [];
  for (const g of tg) {
    if (queueOrigin.genreSet.has(normGenreKey(g))) s += 1;
  }
  return s;
}

function toQueueItem(t) {
  return { id: t.id, kind: 'song' };
}

// Library-song ids (i.…) never match the sidecar's catalog-id reports,
// which breaks end detection, queue sync and row highlight. Resolve to the
// catalog id once per track (session-cached); unmapped ids pass through
// unchanged (today's behavior when logged out).
const catalogIdCache = new Map();
async function toCatalogId(id) {
  if (!id || !String(id).startsWith('i.')) return id;
  if (catalogIdCache.has(id)) return catalogIdCache.get(id);
  try {
    const cid = await invoke('resolve_track_id', { trackId: id });
    if (cid) {
      catalogIdCache.set(id, cid);
      return cid;
    }
  } catch (e) { dlog('id resolve: ' + String(e)); }
  return id;
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
  mirroredIds = mirroredIds.filter((id) => !ids.includes(id));
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

async function appendQueueBatched(items) {
  // Mirror upcoming JS-queue tracks into the sidecar queue so OS media
  // keys (next/previous) move within tracks the UI knows and can adopt.
  // play-now resets the sidecar queue, so this only ever extends it.
  const ids = (items || []).map((it) => it && it.id).filter(Boolean);
  if (!ids.length) return;
  const fresh = ids.filter((id) => !mirroredIds.includes(id));
  if (!fresh.length) return;
  try {
    await invoke('sidecar_append', { items: fresh.map((id) => ({ id, kind: 'song' })) });
    mirroredIds.push(...fresh);
  } catch (e) { handleResolveError(String(e), fresh.map((id) => ({ id, kind: 'song' }))); }
}

// Top up the sidecar mirror after audio confirmed (the queue must exist
// first) and after external advances consume mirrored items.
function ensureMirror() {
  if (queueIndex < 0 || queueIndex >= playQueue.length) return;
  const ahead = playQueue
    .slice(queueIndex + 1, queueIndex + 1 + MIRROR_AHEAD)
    .map((e) => toQueueItem(e.track));
  if (ahead.length) appendQueueBatched(ahead);
}

// First trusted playing report for the pending track: audio really
// started. Unfreeze progress from the REPORTED position (never from the
// IPC-ack time — that gap is what caused the 0:02 → 0:00 snap-back).
function confirmSidecarPlaying(trackId, pos) {
  const now = performance.now();
  awaitingSidecar = false;
  awaitingTrackId = null;
  needsJumpSnap = false;
  lastReportedTrackId = trackId;
  intendedTrackId = trackId;
  anchor = { pos: pos || 0, at: now };
  playStamp = now;
  hadForwardReport = true;
  lastSamePollAt = 0;
  lastPlayingPos = pos || 0;
  trackEndHandled = null;
  if (userPaused) {
    // Paused while loading: hold the pause instead of starting audio.
    isPlaying = false;
    paintNowPlaying(false);
    invoke('sidecar_pause').catch((e) => status(String(e)));
    return;
  }
  isPlaying = true;
  paintNowPlaying(true);
  ensureMirror();
  if (current) {
    autoFetchLyrics(current);
    autoFetchMotion(current);
  }
  maybeFillRadio();
}

let lastRadioError = '';
// Per-track fill depth: consecutive dry fills for the SAME track paginate
// backend windows deeper (page 0 exhausted → page 1...). Keyed by track so
// a new seed always starts shallow while a stuck one keeps digging.
// Capped and pruned; success clears the track's entry.
const radioDepthByTrack = new Map();
function depthFor(id) {
  return Math.min(radioDepthByTrack.get(id) || 0, 8);
}
function bumpDepth(id) {
  const d = Math.min((radioDepthByTrack.get(id) || 0) + 1, 8);
  radioDepthByTrack.set(id, d);
  if (radioDepthByTrack.size > 50) {
    radioDepthByTrack.delete(radioDepthByTrack.keys().next().value);
  }
  return d;
}
async function maybeFillRadio() {
  lastRadioError = '';
  if (!settings.infinite || radioFetching || !current?.id) return false;
  const remaining = playQueue.length - queueIndex - 1;
  const userRemaining = playQueue.slice(queueIndex + 1).filter((e) => e.source === 'user').length;
  if (remaining > 2 && userRemaining > 1) return false;
  radioFetching = true;
  const depth = depthFor(current.id);
  if (depth > 0) dlog(`radio: fill depth ${depth} for ${current.id}`);
  try {
    const have = new Set(playQueue.map((e) => e.track.id));
    // Seed with the current track plus recent history, then expand
    // transitively: already-queued results become bridges into fresh
    // neighborhoods instead of dead ends. No mainstream fallback — a
    // regional top chart shares nothing with the vibe and poisons it.
    // When playback started from a playlist/album, extra seeds cover the
    // FULL origin (all artists/genres), rotated per fill, so a mixed
    // playlist yields a mixed radio instead of collapsing to the tail.
    const seeds = [];
    for (let i = queueIndex; i >= 0 && seeds.length < 6; i--) {
      const e = playQueue[i];
      const id = e && e.track && e.track.id;
      if (id && !seeds.includes(id)) seeds.push(id);
    }
    if (current.id && !seeds.includes(current.id)) seeds.unshift(current.id);
    const FRESH_TARGET = queueOrigin ? 8 : 6;
    const MAX_FETCHES = queueOrigin ? 12 : 8;
    // Origin fills draw from many artists at once: cap each seed's intake
    // so one broad seed (e.g. a 25-track station batch) can't flood the
    // pool before the other artists' seeds are even queried.
    const PER_SEED_CAP = queueOrigin ? 4 : 0; // 0 = uncapped (legacy path)
    if (queueOrigin) {
      const extra = originCoverageSeeds(seeds, 8);
      for (const id of extra) {
        if (id && !seeds.includes(id)) seeds.push(id);
      }
      if (extra.length) dlog(`radio: +${extra.length} playlist-origin seeds (${queueOrigin.tracks.length} tracks)`);
    }
    const tried = new Set();
    const pending = [...seeds];
    const fresh = [];
    let backendErr = '';
    let fetches = 0;
    while (pending.length && fetches < MAX_FETCHES && fresh.length < FRESH_TARGET) {
      const seed = pending.shift();
      if (!seed || tried.has(seed)) continue;
      tried.add(seed);
      fetches++;
      let similar;
      try {
        // Exclude what's already queued so the batch budget is spent on
        // genuinely fresh tracks (backend merges further sources to fill).
        const excludeIds = playQueue.slice(-200).map((e) => e.track.id);
        similar = await invoke('similar_songs', { songId: seed, excludeIds, depth });
      } catch (e) {
        backendErr = String(e).replace(/^Error:\s*/, '');
        dlog(`radio: seed ${seed}: ${backendErr}`);
        continue;
      }
      const items = similar || [];
      let added = 0;
      for (const t of items) {
        if (t.id && !have.has(t.id)) {
          if (PER_SEED_CAP && added >= PER_SEED_CAP) {
            // Pool budget for this seed spent: keep the leftover as a
            // bridge so its neighborhood isn't lost, but don't append it.
            if (!tried.has(t.id) && !pending.includes(t.id)) pending.push(t.id);
            continue;
          }
          have.add(t.id);
          fresh.push({ track: asCurrent(t), source: 'autoplay' });
          added++;
        } else if (t.id && !tried.has(t.id) && !pending.includes(t.id)) {
          pending.push(t.id); // bridge into a new neighborhood
        }
      }
      dlog(`radio: seed ${seed}: ${items.length} returned, ${added} fresh`);
    }
    // Playlist-origin fills: rank the merged pool by affinity to the FULL
    // origin (stable — per-seed backend order survives ties), then DROP
    // anything sharing neither an artist nor a genre with the playlist.
    // Ranking alone isn't enough: Apple's station/similar views return
    // broad batches, and without the filter the tail of the batch (zero
    // affinity) still lands in the queue. Every fill rotates artists, so
    // a filtered-out branch gets fresh chances on later fills/pages.
    if (queueOrigin) {
      radioFillCount++;
      const ranked = fresh
        .map((e) => ({ e, s: playlistAffinity(e.track) }))
        .sort((a, b) => b.s - a.s);
      const kept = ranked
        .filter((x) => x.s >= 1)
        .slice(0, FRESH_TARGET)
        .map((x) => x.e);
      dlog(`radio: origin filter ${fresh.length} candidates → ${kept.length} kept (affinity>=1)`);
      fresh.length = 0;
      fresh.push(...kept);
    }
    if (!fresh.length) {
      lastRadioError = backendErr || 'similar: none found for this song';
      bumpDepth(current.id);
      return false;
    }
    radioDepthByTrack.delete(current.id);
    for (const entry of fresh) playQueue.push(entry);
    await appendQueueBatched(fresh.map((e) => toQueueItem(e.track)));
    renderQueueView();
    return true;
  } catch (e) {
    lastRadioError = String(e).replace(/^Error:\s*/, '');
    dlog('radio: ' + String(e));
    return false;
  } finally {
    radioFetching = false;
  }
}

function resetJumpAdvanceState() {
  prevSidecarPlaying = false;
}

function finishJumpCommit() {
  jumpInFlight = false;
  needsJumpSnap = true;
  // awaitingSidecar stays armed: audio has NOT started yet
  // (IPC ack only enqueued the command). Progress stays frozen at 0 until
  // the sidecar confirms with a matching playing report.
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
  hideMotionCovers(); // stale animation out; the new track fetches its own
  // Do NOT pre-seed lastReportedTrackId: the first real sidecar report for
  // the new track must be adopted, not mistaken for a duplicate.
  intendedTrackId = t.id;
  awaitingSidecar = true;
  awaitingTrackId = t.id;
  mirroredIds = [t.id]; // play-now resets the sidecar queue to this track
  if (lyric.trackId !== t.id) {
    lyric = { trackId: t.id, title: t.title || '', artist: t.artist || '', lines: [], text: '', source: '' };
    $('#lyricsTitle').textContent = lyricTitleFor(t);
    renderLyrics();
  }
  resetProgress();
  // Loading, not playing: freeze progress at 0:00 until the sidecar
  // confirms audio actually started (see confirmSidecarPlaying).
  isPlaying = false;
  paintNowPlaying(false);
  renderQueueView();
}

async function commitQueueJump(gen) {
  if (gen !== jumpGen) return;
  const i = pendingJumpIndex;
  if (i < 0 || i >= playQueue.length) return;
  const raw = playQueue[i].track;
  const cid = await toCatalogId(raw.id);
  const t = cid === raw.id ? raw : { ...raw, id: cid };
  if (t !== raw) {
    // Swap the entry (and intent tracking) to the id the sidecar echoes.
    playQueue[i] = { ...playQueue[i], track: t };
    if (current && current.id === raw.id) current = { ...current, id: cid };
    if (lastReportedTrackId === raw.id) lastReportedTrackId = cid;
    if (intendedTrackId === raw.id) intendedTrackId = cid;
    if (awaitingTrackId === raw.id) awaitingTrackId = cid;
  }
  try {
    await invoke('sidecar_play', { items: [{ id: t.id, kind: 'song' }], startIndex: 0 });
    if (gen !== jumpGen) return;
    // Command enqueued — audio starts seconds later in the sidecar.
    // Stay in loading state (paused, frozen at 0) until it confirms.
    userPaused = false;
    trackEndHandled = null;
    finishJumpCommit();
    autoFetchLyrics(t);
    autoFetchMotion(t);
    maybeFillRadio();
    // Enqueue the mirror right behind play-now (order preserved by the
    // command pipe) so OS next works as early as possible; confirm-time
    // ensureMirror tops up whatever is still missing.
    appendQueueBatched(playQueue.slice(i + 1, i + 1 + MIRROR_AHEAD).map((e) => toQueueItem(e.track)));
  } catch (e) {
    jumpInFlight = false;
    needsJumpSnap = false;
    if (jumpTimeout) { clearTimeout(jumpTimeout); jumpTimeout = null; }
    status(String(e));
  }
}

function scheduleQueueJump(i) {
  if (i < 0 || i >= playQueue.length) return;
  cancelRadioRetry();
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
  cancelRadioRetry();
  setQueueOrigin(tracks);
  playQueue = tracks.map((tr) => ({ track: asCurrent(tr), source: 'user' }));
  queueIndex = playQueue.findIndex((e) => e.track.id === t.id);
  if (queueIndex < 0) queueIndex = 0;
  // Normalize the starting track now; the rest resolve at their jump.
  const tid = await toCatalogId(t.id);
  if (tid !== t.id) {
    playQueue[queueIndex] = { ...playQueue[queueIndex], track: { ...playQueue[queueIndex].track, id: tid } };
  }
  const nt = playQueue[queueIndex].track;
  jumpGen++;
  const gen = jumpGen;
  pendingJumpIndex = queueIndex;
  beginJump();
  applyQueueJumpUI(queueIndex);
  clearTimeout(jumpTimer);
  jumpTimer = null;
  try {
    const msg = await invoke('sidecar_play', {
      items: [{ id: nt.id, kind: 'song' }],
      startIndex: 0,
    });
    if (gen !== jumpGen) return;
    // Command enqueued — audio starts seconds later in the sidecar.
    // Stay in loading state (paused, frozen at 0) until it confirms.
    userPaused = false;
    trackEndHandled = null;
    finishJumpCommit();
    status(msg);
    autoFetchLyrics(nt);
    autoFetchMotion(nt);
    maybeFillRadio();
    // Enqueue the mirror right behind play-now (order preserved by the
    // command pipe) so OS next works as early as possible; confirm-time
    // ensureMirror tops up whatever is still missing.
    appendQueueBatched(playQueue.slice(queueIndex + 1, queueIndex + 1 + MIRROR_AHEAD).map((e) => toQueueItem(e.track)));
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
    if (!mirroredIds.includes(entry.track.id)) mirroredIds.push(entry.track.id);
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
    const at = mirroredIds.indexOf(current.id);
    const id = entry.track.id;
    if (at >= 0 && !mirroredIds.includes(id)) mirroredIds.splice(at + 1, 0, id);
    else if (!mirroredIds.includes(id)) mirroredIds.push(id);
  } catch (e) { status(String(e)); }
  renderQueueView();
}

async function jumpToQueueIndex(i) {
  scheduleQueueJump(i);
}

function removeFromQueue(i) {
  if (i < 0 || i >= playQueue.length) return;
  const [gone] = playQueue.splice(i, 1);
  // No sidecar remove command exists, so the mirror may still hold the id;
  // adoption below treats unknown sidecar tracks as authoritative anyway.
  if (gone) mirroredIds = mirroredIds.filter((id) => id !== gone.track.id);
  if (i < queueIndex) queueIndex--;
  else if (i === queueIndex) queueIndex = Math.min(queueIndex, playQueue.length - 1);
  renderQueueView();
}

async function clearQueue() {
  playQueue = [];
  queueIndex = -1;
  hideMotionCovers();
  queueOrigin = null;
  mirroredIds = [];
  awaitingSidecar = false;
  awaitingTrackId = null;
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
  prevSidecarPlaying = false;
  isPlaying = false;
  userPaused = false;
  cancelRadioRetry();
  try { await invoke('sidecar_clear'); } catch (e) { status(String(e)); }
  paintNowPlaying(false);
  $('#nowPlaying').textContent = 'Not playing.';
  renderQueueView();
  pushDiscord(true);
}

// Persisted session (queue + position): reloads and app restarts come
// back showing the queue and current song instead of "nothing playing".
const QUEUE_STORE_KEY = 'sonora-queue-v1';
const QUEUE_STORE_MAX = 300;
function persistQueue() {
  try {
    localStorage.setItem(QUEUE_STORE_KEY, JSON.stringify({
      v: 1,
      savedAt: Date.now(),
      queueIndex,
      queue: playQueue.slice(0, QUEUE_STORE_MAX),
    }));
  } catch {}
}

function renderQueueView() {
  const box = $('#queueBody');
  if (!box) return;
  persistQueue();
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
    d.addEventListener('contextmenu', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      openTrackMenu(ev.clientX, ev.clientY, t, null, null, {
        inQueue: true,
        onPlay: () => jumpToQueueIndex(i),
        onRemove: () => removeFromQueue(i),
      });
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

// ---------- animated covers (Apple Motion) ----------
// Some albums carry animated artwork: HLS renditions from Apple's
// `editorialVideo` field (square 1:1 for the player tile, tall 3:4 for
// fullscreen). Playback chain per slot: native HLS where the engine
// supports it, else the vendored hls.js, else the static cover stays.
// Videos are muted loops with the static artwork rendered underneath,
// so every failure mode degrades to today's static cover.
const motionCache = new Map(); // key -> hls url | null
const motionFetchInFlight = new Map();
let motionTrackId = null; // song id the visible motion belongs to
let npHls = null;
let fsHls = null;
let detailHls = null;

function motionKey(songId, albumId) {
  if (albumId) return 'a:' + albumId;
  return 's:' + (songId || '');
}

function fetchMotion(songId, albumId) {
  const key = motionKey(songId, albumId);
  if (motionCache.has(key)) return Promise.resolve(motionCache.get(key));
  const existing = motionFetchInFlight.get(key);
  if (existing) return existing;
  let resolve;
  let reject;
  const shared = new Promise((res, rej) => { resolve = res; reject = rej; });
  motionFetchInFlight.set(key, shared);
  invoke('motion_artwork', { songId: songId || null, albumId: albumId || null })
    .then((m) => {
      const url = (m && (m.square_hls || m.tall_hls)) || null;
      motionCache.set(key, url);
      return url;
    })
    .then(resolve, reject)
    .finally(() => { motionFetchInFlight.delete(key); });
  return shared;
}

function motionSlotEls(slot) {
  if (slot === 'np') return { video: $('#npCoverVideo'), img: $('#npCover') };
  if (slot === 'fs') return { video: $('#fsCoverVideo'), img: $('#fsCover') };
  return { video: $('#detailMotionVideo'), img: document.querySelector('#view-detail .detail-head img') };
}

function hlsForSlot(slot) {
  if (slot === 'np') return npHls;
  if (slot === 'fs') return fsHls;
  return detailHls;
}

function setHlsForSlot(slot, hls) {
  if (slot === 'np') npHls = hls;
  else if (slot === 'fs') fsHls = hls;
  else detailHls = hls;
}

// Stop motion in one slot, hiding the video (the static img shows through).
function stopMotionSlot(slot) {
  const { video } = motionSlotEls(slot);
  try {
    const hls = hlsForSlot(slot);
    if (hls) hls.destroy();
  } catch {}
  setHlsForSlot(slot, null);
  if (video) {
    // Clear first: tearing down the src can raise a spurious error event
    // that must not fall back (and kill) the next track's fresh video.
    video.onerror = null;
    try { video.pause(); } catch {}
    video.removeAttribute('src');
    try { video.load(); } catch {}
    video.classList.add('hidden');
  }
}

function hideMotionCovers() {
  motionTrackId = null;
  stopMotionSlot('np');
  stopMotionSlot('fs');
}

function motionFailed(slot) {
  const { video, img } = motionSlotEls(slot);
  stopMotionSlot(slot);
  if (img && img.getAttribute('src')) img.classList.remove('hidden');
  if (video) video.onerror = null;
  dlog('motion cover failed, static fallback');
}

// Attach an HLS motion url to a video element. True when playback was
// attempted (native or hls.js); false when unsupported (keep static).
function playMotionUrl(videoEl, url, slot) {
  if (!videoEl || !url) return false;
  videoEl.muted = true;
  videoEl.loop = true;
  const tryPlay = () => {
    try {
      const p = videoEl.play();
      if (p && p.catch) p.catch(() => {});
    } catch {}
  };
  const canNative = videoEl.canPlayType && videoEl.canPlayType('application/vnd.apple.mpegurl');
  if (canNative) {
    videoEl.onerror = () => motionFailed(slot);
    videoEl.src = url;
    tryPlay();
    return true;
  }
  const Hls = window.Hls;
  if (Hls && Hls.isSupported && Hls.isSupported()) {
    try {
      const hls = new Hls({ maxBufferLength: 12 });
      setHlsForSlot(slot, hls);
      hls.on(Hls.Events.ERROR, (_, data) => {
        if (data && data.fatal) motionFailed(slot);
      });
      hls.on(Hls.Events.MANIFEST_PARSED, tryPlay);
      hls.loadSource(url);
      hls.attachMedia(videoEl);
      videoEl.onerror = () => motionFailed(slot);
      tryPlay();
      return true;
    } catch (e) {
      dlog('motion hls init failed: ' + String(e));
      return false;
    }
  }
  return false;
}

function showMotionFor(t, url) {
  if (!url || !current || current.id !== t.id || motionTrackId !== t.id) return;
  let any = false;
  for (const slot of ['np', 'fs']) {
    const { video, img } = motionSlotEls(slot);
    if (video && playMotionUrl(video, url, slot)) {
      video.classList.remove('hidden');
      if (img) img.classList.add('hidden');
      if (!isPlaying) {
        try { video.pause(); } catch {}
      }
      any = true;
    }
  }
  if (any) dlog('motion cover on: ' + (t.title || t.id));
}

// Fetch motion art quietly on every track change (session-cached; misses
// cached too). Stale resolutions are dropped via motionTrackId.
function autoFetchMotion(t) {
  if (!t || !t.id) return;
  motionTrackId = t.id;
  const key = motionKey(t.id, null);
  if (motionCache.has(key)) {
    const url = motionCache.get(key);
    if (url) showMotionFor(t, url);
    return;
  }
  if (motionFetchInFlight.has(key)) {
    motionFetchInFlight.get(key).then(
      (url) => { if (url) showMotionFor(t, url); },
      () => {}
    );
    return;
  }
  dlog('motion fetch: ' + (t.title || t.id));
  fetchMotion(t.id, null).then(
    (url) => { if (url) showMotionFor(t, url); },
    (e) => { dlog('motion fetch failed: ' + String(e)); }
  );
}

function setCover(img, artUrl, size, ph) {
  if (!img) return;
  // Mutex with animated covers: while a motion video is showing in this
  // slot, the static img stays updated but hidden (poll-driven repaints
  // must not unhide it over the video).
  const motionVideo = img.id === 'npCover' ? $('#npCoverVideo')
    : img.id === 'fsCover' ? $('#fsCoverVideo') : null;
  const motionOn = !!(motionVideo && !motionVideo.classList.contains('hidden'));
  if (artUrl) {
    const src = art(artUrl, size);
    if (img.getAttribute('src') !== src) img.src = src;
    img.classList.toggle('hidden', motionOn);
  } else {
    img.removeAttribute('src');
    img.classList.add('hidden');
  }
  // Companion placeholder tile keeps the player height constant while idle.
  if (ph) ph.classList.toggle('hidden', !!artUrl);
  img.onerror = () => {
    img.removeAttribute('src');
    img.classList.add('hidden');
    if (ph) ph.classList.remove('hidden');
  };
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
  if (!current) {
    $('#nowPlaying').textContent = 'Not playing.';
    $('#npArtist').textContent = '';
    $('#npAlbum').textContent = '';
    setCover($('#npCover'), '', 200, $('#npCoverPh'));
    $('#durTime').textContent = fmtTime(0);
    return;
  }
  $('#nowPlaying').textContent = current.title || '?';
  $('#npArtist').textContent = current.artist || '';
  $('#npAlbum').textContent = current.album || '';
  setCover($('#npCover'), current.art, 200, $('#npCoverPh'));
  $('#durTime').textContent = fmtTime(current.duration_ms);
  syncFsMeta();
  // Animated covers follow playback state like the audio does.
  for (const vid of ['#npCoverVideo', '#fsCoverVideo']) {
    const v = $(vid);
    if (v && !v.classList.contains('hidden')) {
      try {
        if (playing) {
          const p = v.play();
          if (p && p.catch) p.catch(() => {});
        } else {
          v.pause();
        }
      } catch {}
    }
  }
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
// Highest position observed while the current track was audibly playing.
// Survives across poll ticks (unlike prevSidecarPlaying), so a stop whose
// position resets a tick LATER is still recognized as a skip-dead-end
// collapse. Reset on every jump/confirm/adopt, refreshed while playing.
let lastPlayingPos = 0;
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
    if (pos < est - BACKWARD_TOLERANCE_MS) {
      // Steady-state large backward jump with no local seek in flight:
      // external restart (OS previous past ~3s) or OS seek-back. Follow
      // it — without this every later report also looks "backward" and
      // the progress display freezes.
      if (hadForwardReport && now - seekStamp > 3000 && pos < est - 2500) {
        anchor = { pos, at: now };
      }
      return;
    }
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
  lastPlayingPos = 0;
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

function trackRow(t, index, queue, opts) {
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
    const pl = opts && opts.playlistId
      ? { id: opts.playlistId, onRemove: opts.onRemove }
      : null;
    openTrackMenu(ev.clientX, ev.clientY, t, queue, pl);
  });
  d.querySelector('.act-lyrics').onclick = (ev) => { ev.stopPropagation(); openLyrics(t); };
  if (opts && opts.playlistId) {
    const rm = document.createElement('button');
    rm.className = 'mini act-playlist-remove';
    rm.title = 'Remove from playlist';
    rm.textContent = '✕';
    rm.onclick = (ev) => {
      ev.stopPropagation();
      removeFromPlaylist(opts.playlistId, t, opts.onRemove);
    };
    d.appendChild(rm);
  }
  return d;
}

async function removeFromPlaylist(playlistId, t, onDone) {
  try {
    status(await invoke('remove_from_playlist', { playlistId, songIds: [t.id] }));
    if (onDone) await onDone();
  } catch (e) { status(String(e)); }
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

async function openTrackMenu(x, y, t, queue, playlist, opts) {
  const m = $('#ctxMenu');
  m.innerHTML = '';
  const title = document.createElement('div');
  title.className = 'ctx-title';
  title.textContent = (t.title || t.id) + (t.artist ? ' — ' + t.artist : '');
  m.appendChild(title);
  ctxButton(m, '▶ Play', opts && opts.onPlay ? opts.onPlay : () => playTrack(t, queue));
  ctxButton(m, 'Play Next', () => playNextInQueue(t));
  if (!(opts && opts.inQueue)) ctxButton(m, 'Add to Queue', () => addToQueue(t));
  ctxButton(m, '♥ Add to favorites', async () => {
    try { status(await invoke('add_to_favorites', { songIds: [t.id] })); }
    catch (e) { status(String(e)); }
  });
  if (opts && opts.onRemove) ctxButton(m, 'Remove from Queue', opts.onRemove);
  if (playlist && playlist.id) {
    ctxButton(m, 'Remove from playlist', () => removeFromPlaylist(playlist.id, t, playlist.onRemove));
  }
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

// Render-time safety net against repeat entries (backend dedupes too):
// exact id repeats collapse, and so does the same release listed under
// different ids (same title/artist/track-count/single flag — e.g. format
// variants), keeping the newest dated entry. Genuine variants survive.
function dedupeAlbums(albums) {
  const norm = (s) => String(s || '').split(/\s+/).filter(Boolean).join(' ').toLowerCase();
  const seenIds = new Set();
  const seenContent = new Map(); // content key -> index in out
  const out = [];
  for (const a of albums || []) {
    const id = a && a.id != null ? String(a.id) : '';
    if (id) {
      if (seenIds.has(id)) continue;
      seenIds.add(id);
    }
    const title = norm(a && (a.title || a.name));
    const key = title
      ? [title, norm(a && a.artist), a && a.track_count != null ? Number(a.track_count) : '',
        a && a.is_single === true].join('|')
      : null;
    if (key && seenContent.has(key)) {
      const idx = seenContent.get(key);
      const prev = (out[idx] && out[idx].release_date) || '';
      if ((a && a.release_date || '') > prev) out[idx] = a;
      continue;
    }
    if (key) seenContent.set(key, out.length);
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

// Initial-letter tile used when artwork is missing or fails to load
// (undecodable format, expired signature, 404 — never show a broken icon).
function artFallbackEl(title, cls) {
  const d = document.createElement('div');
  d.className = cls;
  d.setAttribute('aria-hidden', 'true');
  d.textContent = (String(title || '?').trim().charAt(0) || '?').toUpperCase();
  return d;
}

// Wire an <img> to swap itself for an initial tile when `url` is empty or
// the load fails. Returns the node to place (img or fallback).
function imgOrFallback(img, url, title, cls) {
  if (!img) return artFallbackEl(title, cls);
  if (!url) {
    dlog('artwork missing url for: ' + (title || '?'));
    const fb = artFallbackEl(title, cls);
    if (img.replaceWith) img.replaceWith(fb);
    return fb;
  }
  img.src = url;
  img.onerror = () => {
    dlog('artwork failed to load: ' + url);
    if (img.replaceWith) img.replaceWith(artFallbackEl(title, cls));
  };
  return img;
}

// Unique track artwork urls (up to 4) for generated playlist covers.
function trackArtworks(tracks) {
  const seen = new Set();
  const out = [];
  for (const t of tracks || []) {
    const u = t && t.artwork && t.artwork.url;
    if (u && !seen.has(u)) {
      seen.add(u);
      out.push(u);
      if (out.length >= 4) break;
    }
  }
  return out;
}

// 2x2 (or fewer) mosaic of track covers used when a playlist reports no
// artwork url of its own — the same idea as Apple's generated covers.
function playlistCollageEl(urls, cls) {
  const d = document.createElement('div');
  d.className = cls;
  d.setAttribute('data-n', String(Math.min(urls.length, 4)));
  d.setAttribute('aria-hidden', 'true');
  urls.slice(0, 4).forEach((u) => {
    const img = document.createElement('img');
    img.loading = 'lazy';
    img.src = art(u, 150);
    img.alt = '';
    img.onerror = () => {
      dlog('collage thumb failed: ' + u);
      if (img.remove) img.remove();
    };
    d.appendChild(img);
  });
  return d;
}

// List view: playlists report no tracks, so fetch the detail for art-less
// entries and swap the letter tile for a track-cover mosaic.
async function backfillPlaylistArt(playlistId, card) {
  try {
    const d = await invoke('get_playlist', { id: playlistId });
    const arts = trackArtworks(d.tracks);
    if (!arts.length) return;
    const fb = card.querySelector('.card-fallback');
    if (fb && fb.replaceWith) fb.replaceWith(playlistCollageEl(arts, 'card-collage'));
  } catch (e) { dlog('art backfill failed: ' + String(e)); }
}

function albumCards(items, onOpen) {
  const wrap = document.createElement('div');
  wrap.className = 'cards';
  for (const a of items) {
    const title = a.title || a.name || a.id;
    const c = document.createElement('div');
    c.className = 'card';
    c.innerHTML = `<img loading="lazy" alt="" /><div class="t"></div><div class="a"></div>`;
    imgOrFallback(c.querySelector('img'), art(a.artwork?.url, 300), title, 'card-fallback');
    c.querySelector('.t').textContent = title;
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
    // Loose songs (charts/search): play just the picked track, not the
    // whole result list. Albums/playlists below keep their full queues.
    (r.tracks || []).forEach((t, i) => songs.appendChild(trackRow(t, i, [t])));
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
      // Loose search hits: queue only the picked song (infinite mode can
      // extend it with similar tracks). Albums/playlists keep full queues.
      r.tracks.forEach((t, i) => box.appendChild(trackRow(t, i, [t])));
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
    dlog(`playlists: ${pls.length} loaded, ${pls.filter((p) => !p.artwork?.url).length} without artwork url`);
    const grid = albumCards(pls.map(p => ({ ...p, title: p.name })), (p) => openPlaylist(p.id));
    v.appendChild(grid);
    Array.from(grid.children).forEach((card, i) => {
      if (!pls[i].artwork?.url) backfillPlaylistArt(pls[i].id, card);
    });
  } catch (e) { v.innerHTML = '<h2>Playlists</h2><p>Failed (need saved MUT?): ' + esc(String(e)) + '</p>'; }
}

function detailHead({ img, title, sub, extra, onPlayAll }) {
  const h = document.createElement('div');
  h.innerHTML = `<div class="detail-head"><img alt="" /><div><h1></h1><p class="sub"></p><p class="xtra dim"></p><button class="btn-accent">Play</button></div></div>`;
  imgOrFallback(h.querySelector('img'), img, title, 'detail-fallback');
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
  stopMotionSlot('detail');
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
    // Animated cover for albums that carry motion art (static stays otherwise).
    fetchMotion(null, id).then((url) => {
      if (!url) return;
      const headImg = v.querySelector('.detail-head img');
      if (!headImg || !headImg.isConnected) return;
      const video = document.createElement('video');
      video.id = 'detailMotionVideo';
      video.className = 'detail-motion hidden';
      video.muted = true;
      video.loop = true;
      video.playsInline = true;
      video.preload = 'auto';
      headImg.before(video);
      if (playMotionUrl(video, url, 'detail')) {
        video.classList.remove('hidden');
        headImg.classList.add('hidden');
        dlog('motion cover on (album)');
      } else {
        video.remove();
      }
    }).catch((e) => { dlog('motion fetch failed: ' + String(e)); });
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
  stopMotionSlot('detail');
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
  stopMotionSlot('detail');
  try {
    const d = await invoke('get_playlist', { id });
    v.innerHTML = '';
    const q = d.tracks;
    const head = detailHead({
      img: art(d.playlist.artwork?.url, 400),
      title: d.playlist.name, sub: d.playlist.description || '',
      extra: (d.tracks.length || '') + (d.tracks.length === 1 ? ' song' : ' songs'),
      onPlayAll: () => q.length && playTrack(q[0], q),
    });
    v.appendChild(head);
    if (!d.playlist.artwork?.url) {
      const arts = trackArtworks(q);
      if (arts.length) {
        const fb = head.querySelector('.detail-fallback');
        if (fb && fb.replaceWith) fb.replaceWith(playlistCollageEl(arts, 'detail-collage'));
      }
    }
    const box = document.createElement('div');
    box.className = 'tracks';
    // Only library playlists (p.…) are mutable — catalog playlists are read-only.
    const playlistId = id.startsWith('p.') ? id : null;
    const opts = playlistId ? { playlistId, onRemove: () => openPlaylist(id) } : null;
    q.forEach((t, i) => box.appendChild(trackRow(t, i, q, opts)));
    v.appendChild(box);
  } catch (e) { v.innerHTML = '<p>Failed: ' + esc(String(e)) + '</p>'; }
}

// ---------- display settings (persisted) ----------
const settings = Object.assign(
  { fsLyrics: true, fsLayout: 'vertical', lyricsFocus: false, debug: false, radio: true, discord: false, discordAppId: '', loop: false, nativeFs: false },
  JSON.parse(localStorage.getItem('aml-settings') || '{}')
);
// Migrate the old Radio flag to the Infinite queue switch (same behavior,
// default on). Kept out of the defaults above so this runs for everyone.
if (settings.infinite === undefined) settings.infinite = settings.radio !== false;
delete settings.radio;
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
  updateFsLyricPane();
  lyricActive = -2;
  highlightLyric(estPos());
  if (!synced) {
    const meta = $('#lyricsMeta');
    if (meta) meta.textContent = '';
  }
}

// Fullscreen centers the cover when there is nothing to show in the lyrics
// pane (track without lyrics, or nothing loaded yet) — otherwise the empty
// pane reserves space and pushes the thumbnail aside (horizontal layout).
function updateFsLyricPane() {
  const o = $('#fsOverlay');
  if (!o) return;
  const has = lyric.lines.length > 0 || (lyric.text || '').trim().length > 0;
  o.classList.toggle('empty-lyrics', !has);
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
  try { await invoke('submit_user_token', { token: $('#mut').value.trim() }); status('MUT saved.'); refreshTokenStatus(); refreshAuthState(); }
  catch (e) { status(String(e)); }
};
$('#authUrlBtn').onclick = async () => {
  try { status(await invoke('submit_auth_url', { url: $('#authUrl').value.trim() })); refreshTokenStatus(); refreshAuthState(); }
  catch (e) { status(String(e)); }
};
// Automatic sign-in: the backend opens a localhost auth page in the
// browser; approve there and the token lands here by itself (up to
// ~5 minutes). Manual paste above stays as a fallback.
$('#signinBtn').onclick = async () => {
  const btn = $('#signinBtn');
  btn.disabled = true;
  status('Opening the browser for Apple Music approval… approve, then return here.');
  try {
    status(await invoke('start_signin'));
  } catch (e) { status(String(e)); }
  finally { btn.disabled = false; }
  refreshTokenStatus();
  refreshAuthState();
};
$('#cancelSigninBtn').onclick = async () => {
  try { status(await invoke('cancel_signin')); }
  catch (e) { status(String(e)); }
};
$('#signoutBtn').onclick = async () => {
  if (!window.confirm('Log out of Apple Music? Playback stops and the saved credentials are removed.')) return;
  try {
    status(await invoke('logout'));
    await clearQueue();
  } catch (e) { status(String(e)); }
  refreshTokenStatus();
  refreshAuthState();
};
async function refreshAuthState() {
  try {
    const signedIn = await invoke('auth_state');
    $('#authStateLine').textContent = signedIn ? 'Signed in' : 'Not signed in';
    // Mutually exclusive: sign-in controls only when signed out, and
    // log out only when signed in.
    $('#signinBtn').classList.toggle('hidden', signedIn);
    $('#cancelSigninBtn').classList.toggle('hidden', signedIn);
    $('#signoutBtn').classList.toggle('hidden', !signedIn);
    const al = $('#accountLine');
    if (al) al.textContent = signedIn ? 'Signed in' : 'Not signed in';
  } catch {}
}
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
      else if (c === 'mode') {
        toggleMode();
      }
    } catch (e) { status(String(e)); }
  };
});

// Playback mode: one button cycling off → loop-one → infinite queue.
// Loop replays the current track at its end; infinite appends similar
// songs forever. Manual next/previous are unaffected in every mode.
function playMode() {
  if (settings.loop) return 'loop';
  if (settings.infinite) return 'infinite';
  return 'off';
}
function setMode(mode) {
  settings.loop = mode === 'loop';
  settings.infinite = mode === 'infinite';
  saveSettings();
  paintMode();
  const box = $('#setRadio');
  if (box) box.checked = settings.infinite;
  if (mode === 'infinite') {
    cancelRadioRetry();
    maybeFillRadio();
    status('Infinite queue on — similar songs will keep playing');
  } else {
    cancelRadioRetry();
    status(mode === 'loop' ? 'Loop on (repeating this song)' : 'Playback mode off');
  }
}
function toggleMode() {
  const order = ['off', 'loop', 'infinite'];
  setMode(order[(order.indexOf(playMode()) + 1) % order.length]);
}
function paintMode() {
  const mode = playMode();
  $$('.mode-btn').forEach((b) => {
    b.classList.toggle('on', mode !== 'off');
    b.classList.toggle('is-infinite', mode === 'infinite');
    b.setAttribute('aria-pressed', mode === 'off' ? 'false' : 'true');
    b.title = mode === 'loop'
      ? 'Loop this song (click for infinite queue)'
      : mode === 'infinite'
        ? 'Infinite queue (click to turn off)'
        : 'Playback mode: off (click for loop)';
  });
}

// Infinite queue (Apple-Music-style autoplay): when the queue runs dry,
// similar songs are appended forever. Same switch as the Queue-view
// checkbox; turning it on seeds the queue immediately.
function setInfinite(on) {
  if (on) setMode('infinite');
  else if (settings.infinite) setMode('off');
  else paintMode(); // checkbox already off and mode is loop/off: just repaint
}
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
  const nfs = $('#setNativeFs');
  if (nfs) {
    nfs.checked = settings.nativeFs !== false;
    nfs.onchange = () => {
      settings.nativeFs = nfs.checked;
      saveSettings();
      status('Native fullscreen ' + (nfs.checked ? 'on' : 'off (overlay only)'));
    };
  }
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
  const adv = $('#setAdvancedAuth'), mbox = $('#manualAuthBox');
  if (adv && mbox) {
    adv.checked = !!settings.advancedAuth;
    mbox.classList.toggle('hidden', !adv.checked);
    adv.onchange = () => {
      settings.advancedAuth = adv.checked;
      saveSettings();
      mbox.classList.toggle('hidden', !adv.checked);
    };
  }
  applyDebugUi();
  const radio = $('#setRadio');
  if (radio) {
    radio.checked = !!settings.infinite;
    radio.onchange = () => setInfinite(radio.checked);
  }
  const clearBtn = $('#clearQueueBtn');
  if (clearBtn) clearBtn.onclick = () => clearQueue();
  paintMode();
  const dc = $('#setDiscord'), dcId = $('#discordAppId');
  if (dc && dcId) {
    dc.checked = !!settings.discord;
    dcId.value = settings.discordAppId || '';
    dc.onchange = async () => {
      settings.discord = dc.checked;
      saveSettings();
      try { await invoke('set_discord_enabled', { enabled: dc.checked }); } catch (e) { status(String(e)); }
      if (dc.checked) pushDiscord(true);
      else { try { await invoke('clear_discord_presence'); } catch {} }
      status('Discord status ' + (dc.checked ? 'on' : 'off'));
    };
    dcId.onchange = async () => {
      settings.discordAppId = dcId.value.trim();
      saveSettings();
      try { await invoke('set_discord_app_id', { appId: settings.discordAppId }); } catch (e) { status(String(e)); }
      if (settings.discord) pushDiscord(true);
    };
  }
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
  if (!current) {
    setCover($('#fsCover'), '', 600);
    return;
  }
  $('#fsTitle').textContent = current.title || '?';
  $('#fsArtist').textContent = current.artist || '';
  $('#fsAlbum').textContent = current.album || '';
  setCover($('#fsCover'), current.art, 600);
  if (current.art) updateAmbient(current.art);
}
$('#fsBtn').onclick = () => {
  syncFsMeta();
  buildLyricList($('#fsLyrics'), true);
  updateFsLyricPane();
  const o = $('#fsOverlay');
  o.classList.remove('hidden');
  // Native fullscreen crashes some GPU/compositor combos (freeze then
  // SIGABRT); the overlay already covers the viewport, so it is optional.
  if (settings.nativeFs === false) return;
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

// ---------- discord status ----------
// Push playback snapshots to Discord (backend no-ops unless enabled with an
// app id). Sent on track/play flips, and at most every 15s while playing
// to keep the progress timestamps fresh.
let discordLast = { trackId: null, playing: null, at: 0 };
async function pushDiscord(force) {
  if (!settings.discord) return; // backend falls back to the built-in app id
  if (!current) {
    if (discordLast.trackId !== null || discordLast.playing !== false) {
      discordLast = { trackId: null, playing: false, at: Date.now() };
      try { await invoke('clear_discord_presence'); } catch {}
    }
    return;
  }
  const now = Date.now();
  const playing = !!isPlaying;
  if (!force && discordLast.trackId === current.id && discordLast.playing === playing
    && now - discordLast.at < 15000) return;
  discordLast = { trackId: current.id, playing, at: now };
  try {
    await invoke('update_discord_presence', { payload: {
      title: current.title || '',
      artist: current.artist || '',
      album: current.album || '',
      playing,
      position_ms: Math.max(0, Math.floor(estPos())),
      duration_ms: current.duration_ms || 0,
    }});
  } catch {}
}

function sidecarTrackMatchesIntent(tid) {
  if (!tid) return false;
  if ((intendedTrackId && tid === intendedTrackId)
    || (awaitingTrackId && tid === awaitingTrackId)) return true;
  if (intendedTrackId || awaitingTrackId) return false;
  if (jumpTimer && pendingJumpIndex >= 0) {
    const pending = playQueue[pendingJumpIndex]?.track?.id;
    if (pending && tid !== pending) return false;
  }
  return true;
}

// Locate the queue entry for a sidecar report: exact id first
// (neighbors before full scan, so duplicate ids resolve directionally),
// then title/artist fallback for storefront re-resolved ids that match
// nothing by id. Returns -1 when the queue holds no candidate.
function findQueueIndexForReport(tid, s) {
  if (queueIndex >= 0 && playQueue[queueIndex + 1]?.track?.id === tid) return queueIndex + 1;
  if (queueIndex > 0 && playQueue[queueIndex - 1]?.track?.id === tid) return queueIndex - 1;
  const at = playQueue.findIndex((e) => e.track.id === tid);
  if (at >= 0) return at;
  const title = (s.title || '').trim().toLowerCase();
  if (!title) return -1;
  const artist = (s.artist || '').trim().toLowerCase();
  const scored = [];
  playQueue.forEach((e, i) => {
    const t = e.track || {};
    if ((t.title || '').trim().toLowerCase() !== title) return;
    const ta = (t.artist || '').trim().toLowerCase();
    const artistOk = !artist || !ta || ta.includes(artist) || artist.includes(ta);
    if (!artistOk) return;
    scored.push(i);
  });
  if (!scored.length) return -1;
  scored.sort((a, b) => Math.abs(a - queueIndex) - Math.abs(b - queueIndex));
  return scored[0];
}

// Adopt a sidecar track the UI did not request (OS media keys, MusicKit
// queue advance). Unknown ids become an ad-hoc entry — audible truth wins
// over queue bookkeeping.
function adoptExternalTrack(tid, s) {
  const now = performance.now();
  const qi = findQueueIndexForReport(tid, s);
  if (qi >= 0) {
    queueIndex = qi;
    // Consumed a mirrored item: drop everything through it.
    const mi = mirroredIds.indexOf(tid);
    if (mi >= 0) mirroredIds = mirroredIds.slice(mi);
    else if (!mirroredIds.includes(tid)) mirroredIds.unshift(tid);
  }
  const qe = qi >= 0 ? playQueue[qi].track : null;
  const prev = current || {};
  current = {
    id: tid,
    title: qe?.title || prev.title || s.title || '?',
    artist: qe?.artist || prev.artist || s.artist || '',
    album: qe?.album || prev.album || '',
    art: qe?.art || prev.art || '',
    duration_ms: s.duration_ms || qe?.duration_ms || prev.duration_ms,
    genres: qe?.genres || prev.genres || [],
  };
  intendedTrackId = tid;
  lastReportedTrackId = tid;
  awaitingSidecar = false;
  awaitingTrackId = null;
  needsJumpSnap = false;
  trackEndHandled = null;
  anchor = { pos: s.position_ms || 0, at: now };
  playStamp = now;
  hadForwardReport = true;
  lastSamePollAt = 0;
  lastPlayingPos = s.position_ms || 0;
  resetJumpAdvanceState();
  prevSidecarPlaying = !!s.playing;
  isPlaying = !!s.playing;
  paintNowPlaying(!!s.playing);
  if (!lyricsCache.has(tid) && lyric.trackId !== tid) autoFetchLyrics(current);
  if (motionTrackId !== tid) autoFetchMotion(current);
  renderQueueView();
  ensureMirror();
  maybeFillRadio();
  dlog('adopted external track: ' + (current.title || tid));
  return true;
}

async function maybeAutoAdvance(s) {
  if (!current || userPaused || jumpInFlight || awaitingSidecar) return;
  if (trackEndHandled === current.id) return;
  if (s.track_id && current.id && s.track_id !== current.id) return;
  const dur = current.duration_ms || s.duration_ms || 0;
  if (!dur) return;
  const pos = s.position_ms ?? estPos();
  const wasPlaying = prevSidecarPlaying;
  const nowPlaying = !!s.playing;
  // Natural end signature: was playing, now stopped AT the end of the
  // track. The position-at-pause check (not a latched "was near end")
  // keeps an OS/user pause mid-track from triggering an advance.
  const completed = wasPlaying && !nowPlaying && pos >= dur - 1500;
  if (!completed) return;
  // The sidecar may have advanced its own (mirrored) queue between the
  // polled report and this decision — re-read once so we adopt instead of
  // double-jumping (which would restart the already-playing next track).
  try {
    const fresh = await invoke('sidecar_status');
    if (fresh && fresh.track_id && current && fresh.track_id !== current.id) return;
    if (fresh && fresh.playing) return; // audio resumed on its own; not an end
  } catch {}
  trackEndHandled = current.id;
  if (settings.loop && queueIndex >= 0) {
    status(`Looping “${current.title || current.id}” — turn loop off to advance`);
    jumpToQueueIndex(queueIndex); // replay the current song
    return;
  }
  if (queueIndex + 1 < playQueue.length) {
    cancelRadioRetry();
    jumpToQueueIndex(queueIndex + 1);
    return;
  }
  await maybeFillRadio();
  if (queueIndex + 1 < playQueue.length) {
    cancelRadioRetry();
    jumpToQueueIndex(queueIndex + 1);
  } else if (settings.infinite) {
    if (radioStallFor !== current.id) {
      radioStallFor = current.id;
      status('Infinite queue: ' + (lastRadioError || 'no similar songs found') + ' — retrying');
    }
    scheduleRadioRetry(); // fill failed: try again later instead of stalling
  }
}

// One pending retry while the queue sits exhausted with Infinite on —
// cancelled by any navigation, a fresh play, clearing, or toggling off.
// Keeps re-arming on persistent failure so transient backend outages heal,
// backing off 15s → 30s → 60s so a hopeless stall doesn't hammer the API.
let radioRetryTimer = null;
let radioStallFor = null; // track id already reported as stalled (message once)
let radioDryStreak = 0; // consecutive dry episodes; resets on any navigation
function cancelRadioRetry() {
  if (radioRetryTimer) { clearTimeout(radioRetryTimer); radioRetryTimer = null; }
  radioStallFor = null;
  radioDryStreak = 0;
}
function scheduleRadioRetry() {
  if (!settings.infinite || radioRetryTimer) return;
  const id = current && current.id;
  if (!id) return;
  const delay = Math.min(15000 * 2 ** radioDryStreak, 60000);
  radioDryStreak++;
  radioRetryTimer = setTimeout(async () => {
    radioRetryTimer = null;
    if (!settings.infinite || !current || current.id !== id) return;
    if (userPaused) { scheduleRadioRetry(); return; }
    await maybeFillRadio();
    if (!settings.infinite || !current || current.id !== id) return;
    if (queueIndex + 1 < playQueue.length && trackEndHandled === id) {
      jumpToQueueIndex(queueIndex + 1);
    } else if (trackEndHandled === id) {
      scheduleRadioRetry(); // still dry: keep trying
    }
  }, delay);
}

function syncFromSidecarReport(s) {
  const tid = s.track_id || null;
  if (s.duration_ms && s.duration_ms > 0) {
    if (current) current.duration_ms = s.duration_ms;
    $('#durTime').textContent = fmtTime(s.duration_ms);
  }
  // Loading: only the awaited track confirms (handled by the poll snap
  // block); stale old-track reports are ignored, not adopted.
  if (awaitingSidecar) return;
  if (!tid) {
    // No track reported: an external stop / emptied queue freezes the UI
    // paused (metadata kept). The loading case returned above, and our
    // own clearQueue already nulled `current`.
    if (current && !s.playing) {
      anchor = { pos: Math.min(s.position_ms || 0, current.duration_ms || Infinity), at: performance.now() };
      if (isPlaying) {
        isPlaying = false;
        paintNowPlaying(false);
      }
    }
    return;
  }
  if (current && tid === current.id) return; // steady state: poll loop paints
  // During load only the awaited track counts; anything else is the old
  // audio draining. (Deliberately keyed on awaitingSidecar, not the
  // 2.5s jumpInFlight window, so OS keys keep working right after audio
  // starts.)
  if (awaitingSidecar && !sidecarTrackMatchesIntent(tid)) return; // stale
  if (!current) {
    // Nothing playing locally (e.g. fresh boot with sidecar already
    // going): follow whatever is audible.
    adoptExternalTrack(tid, s);
    return;
  }
  adoptExternalTrack(tid, s);
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
    // Load confirmation: the awaited track is actually audible. Anchor
    // from the REPORTED position — never from IPC-ack time.
    if (awaitingSidecar && s.playing && s.track_id
      && (s.track_id === awaitingTrackId || s.track_id === intendedTrackId)
      && canSnapJump(p, s.track_id)) {
      confirmSidecarPlaying(s.track_id, p);
      prevPollPos = p;
      prevSidecarPlaying = !!s.playing;
      syncFromSidecarReport(s);
      if (current) paintNowPlaying(!!s.playing);
      pushDiscord(false);
      return;
    }
    // Safety valve: if the sidecar echoes an equivalent-but-different id
    // (storefront re-resolve), id-match never fires. After 15s of audible
    // playback, confirm anyway — the next ticks adopt the real metadata.
    if (awaitingSidecar && s.playing && s.track_id && now - playStamp > 15000) {
      dlog('load confirm by timeout, echo: ' + s.track_id);
      confirmSidecarPlaying(s.track_id, p);
      prevPollPos = p;
      prevSidecarPlaying = !!s.playing;
      syncFromSidecarReport(s);
      if (current) paintNowPlaying(!!s.playing);
      pushDiscord(false);
      return;
    }
    if (!awaitingSidecar) {
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
    }
    prevPollPos = p;
    // High-water mark while audibly playing the current track (feeds the
    // skip detector below; foreign pre-adopt reports must not pollute it).
    if (!awaitingSidecar && s.playing && current
      && (!s.track_id || s.track_id === current.id) && p > lastPlayingPos) {
      lastPlayingPos = p;
    }
    // Adopt external changes (OS keys / MusicKit advance / stop) BEFORE
    // the advance decision so it sees the authoritative track.
    syncFromSidecarReport(s);
    // OS next with nothing ahead in the sidecar queue (mirror not yet
    // filled, or a lone track) stops playback at ~0 instead of advancing.
    // The stop and the position reset often land on SEPARATE polls, so
    // this keys on the high-water mark, not a single-tick transition:
    // collapsed from clearly-in-track to ~0 while paused, with a next
    // track queued and no local seek involved. (Natural ends report pos
    // ~= duration, plain pauses keep their position, and our own seeks
    // refresh seekStamp — none of those trip this.)
    if (!awaitingSidecar && !userPaused && current && !s.playing
      && (!s.track_id || s.track_id === current.id)
      && queueIndex + 1 < playQueue.length
      && now - seekStamp > 3000
      && lastPlayingPos - p > 2000 && p < 3000) {
      dlog('external skip at dead end -> advancing to next');
      jumpToQueueIndex(queueIndex + 1);
    } else {
      await maybeAutoAdvance(s);
    }
    prevSidecarPlaying = !!s.playing;
    if (!awaitingSidecar && current) {
      paintNowPlaying(!!s.playing);
      pushDiscord(false);
    } else if (current) {
      pushDiscord(false);
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

// Restore the previous session after a UI reload / app restart: the
// persisted queue comes back, and live sidecar state (if any) is adopted
// so a still-playing song shows with data instead of "nothing playing".
async function restoreSession() {
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem(QUEUE_STORE_KEY) || 'null'); } catch {}
  const entries = saved && Array.isArray(saved.queue)
    ? saved.queue.filter((e) => e && e.track && e.track.id)
    : [];
  if (entries.length) {
    playQueue = entries.slice(0, QUEUE_STORE_MAX).map((e) => ({
      track: { ...e.track },
      source: e.source === 'autoplay' ? 'autoplay' : 'user',
    }));
    queueIndex = Math.min(Math.max(0, saved.queueIndex | 0), playQueue.length - 1);
    if (playQueue.length >= 2) setQueueOrigin(playQueue.map((e) => e.track));
    current = { ...playQueue[queueIndex].track };
    // Assume the pre-reload mirror still holds the upcoming tracks so a
    // restore doesn't duplicate them into the sidecar queue (an explicit
    // jump resets the mirror anyway).
    mirroredIds = playQueue.slice(queueIndex, queueIndex + 1 + MIRROR_AHEAD).map((e) => e.track.id);
    renderQueueView();
  }
  let s = null;
  try { s = await invoke('sidecar_status'); } catch {}
  if (!s) {
    if (current) { isPlaying = false; paintNowPlaying(false); }
    return;
  }
  if (s.duration_ms && current) {
    current.duration_ms = s.duration_ms;
    $('#durTime').textContent = fmtTime(s.duration_ms);
  }
  const tid = s.track_id || null;
  if (tid) {
    // Live audio: adopt it (queue match or ad-hoc), exactly like a fresh
    // external report.
    adoptExternalTrack(tid, s);
    status(`Restored “${current.title || current.id}”`);
    return;
  }
  // Sidecar silent (fresh backend / orphaned player): keep the restored
  // queue metadata visible, paused, instead of "nothing playing".
  if (current) {
    anchor = { pos: Math.min(s.position_ms || 0, current.duration_ms || Infinity), at: performance.now() };
    playStamp = performance.now();
    hadForwardReport = false;
    isPlaying = false;
    paintNowPlaying(false);
    autoFetchLyrics(current);
    autoFetchMotion(current);
    status('Queue restored — press play to resume');
  }
}

// boot
(async function boot() {
  const bt = $('#buildTag');
  if (bt) bt.textContent = 'build ' + BUILD_TAG;
  const names = Object.keys(window).filter(k => k.includes('TAURI'));
  status(names.length ? 'Ready. Browse is loading…' : 'No Tauri bridge — restart via cargo tauri dev.');
  try { $('#headless').checked = await invoke('sidecar_headless'); } catch {}
  try { $('#explicit').checked = await invoke('sidecar_explicit'); } catch {}
  initDisplaySettings();
  // Reattach runs concurrently with the fast init calls below: it binds
  // the fixed rendezvous port and gives a pre-restart orphan ~1s to phone
  // home, so restoreSession (which reads sidecar_status next) sees it.
  const reattachP = invoke('sidecar_reattach').catch(() => false);
  try { await invoke('set_discord_app_id', { appId: settings.discordAppId || '' }); } catch {}
  try { await invoke('set_discord_enabled', { enabled: !!settings.discord }); } catch {}
  refreshTokenStatus();
  refreshAuthState();
  try { await reattachP; } catch {}
  await restoreSession();
  loadBrowse();
})();
