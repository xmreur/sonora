//! Shared backend: a Tokio runtime driving the `shell` service crate.
//!
//! Every UI-triggered backend call runs off-thread; the returned
//! [`oneshot::Receiver`] is polled once per frame in [`Pending`]. Tasks
//! repaint the UI when they finish, so results land without busy-polling.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;

pub type Reply<T> = oneshot::Receiver<Result<T, String>>;

/// The service layer (state + Tokio runtime), owned behind an `Arc` so
/// async tasks can hold it for the duration of a call.
pub struct Backend {
    pub state: Arc<shell::AppState>,
    /// `Some` until [`Self::shutdown`] takes it — the runtime cannot be
    /// moved out of `&self` otherwise.
    rt: Option<tokio::runtime::Runtime>,
    /// User-visible outcomes of fire-and-forget commands (success messages
    /// and errors), drained once per frame by the UI.
    notices: Arc<std::sync::Mutex<Vec<String>>>,
    /// Set when a call comes back with Apple rejecting our credentials
    /// (401); drained once per frame by the UI, which classifies it
    /// (expired dev token → auto-refresh vs expired MUT → re-login banner).
    auth_expired: Arc<std::sync::Mutex<bool>>,
    /// Set after a library playlist mutation is accepted. The UI drops
    /// the cached playlist list and open detail on the next frame so the
    /// refetch sees the write. Cleared up front would race the POST and
    /// store the old response again.
    playlist_dirty: Arc<AtomicBool>,
}

impl Backend {
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("backend runtime is running")
    }

    /// Spawn a fire-and-forget task on the runtime (artwork fetches and
    /// other UI-side producers that deliver results through their own
    /// channel instead of a [`Reply`]).
    pub fn spawn<F>(&self, f: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt().spawn(f);
    }

    /// Boot the runtime, init the config dir, build shared state, and
    /// start the media-session bridge + status publish loop.
    pub fn new() -> Self {
        shell::paths::init_default_config_dir();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let state = Arc::new(shell::AppState::new());
        let _guard = rt.enter();
        shell::spawn_background_tasks(
            state.native.clone(),
            shell::media_session::MediaSessionConfig::new(),
        );
        drop(_guard);
        Self {
            state,
            rt: Some(rt),
            notices: Arc::new(std::sync::Mutex::new(Vec::new())),
            auth_expired: Arc::new(std::sync::Mutex::new(false)),
            playlist_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Boot the runtime and shared state without the media-session bridge.
    ///
    /// Tests drive the sync engine directly: they must not claim the
    /// MPRIS name on the session bus, run the publish loop, or hold the
    /// background tasks alive past the test.
    #[cfg(test)]
    pub fn new_headless() -> Self {
        shell::paths::init_default_config_dir();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let state = Arc::new(shell::AppState::new());
        Self {
            state,
            rt: Some(rt),
            notices: Arc::new(std::sync::Mutex::new(Vec::new())),
            auth_expired: Arc::new(std::sync::Mutex::new(false)),
            playlist_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Quit path: silence audio without ever blocking on a lock, then
    /// *abandon* the runtime. A normal `Runtime` drop joins every worker
    /// and blocking-pool thread — so a CDM decrypt or whole-track
    /// `prime_file` in flight made closing the window hang until the
    /// process was killed. `shutdown_background` detaches them instead;
    /// the process is about to exit anyway (any leftovers, like an
    /// unfinished PCM `.part` file, are swept on next boot).
    pub fn shutdown(&mut self) {
        self.state.native.try_silence();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    /// Run one shell call off-thread; the task repaints `ctx` when the
    /// result is ready. Poll the receiver in [`Pending`] each frame.
    pub fn call<T: Send + 'static>(
        &self,
        ctx: &egui::Context,
        fut: impl Future<Output = Result<T, String>> + Send + 'static,
    ) -> Reply<T> {
        let ctx = ctx.clone();
        let auth_expired = Arc::clone(&self.auth_expired);
        let (tx, rx) = oneshot::channel();
        self.rt().spawn(async move {
            let out = fut.await;
            if let Err(e) = &out {
                note_auth_error(&auth_expired, e);
            }
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        rx
    }

    /// Fire-and-forget command: errors land in the notice queue.
    pub fn fire<T: Send + 'static>(
        &self,
        ctx: &egui::Context,
        label: &str,
        fut: impl Future<Output = Result<T, String>> + Send + 'static,
    ) {
        let ctx = ctx.clone();
        let label = label.to_owned();
        let notices = Arc::clone(&self.notices);
        let auth_expired = Arc::clone(&self.auth_expired);
        self.rt().spawn(async move {
            if let Err(e) = fut.await {
                note_auth_error(&auth_expired, &e);
                notices.lock().unwrap().push(format!("{label}: {e}"));
                ctx.request_repaint();
            }
        });
    }

    /// Like [`Self::fire`], but a non-empty `Ok(String)` message is shown
    /// too (the old UI's `status(await invoke(…))` pattern).
    pub fn fire_msg(
        &self,
        ctx: &egui::Context,
        label: &str,
        fut: impl Future<Output = Result<String, String>> + Send + 'static,
    ) {
        self.spawn_msg(ctx, label, fut, false);
    }

    /// [`Self::fire_msg`] for a library playlist write (create, rename,
    /// delete, add, remove, share). On success the playlist list and the
    /// open detail are marked stale so the next frame refetches them
    /// without blanking what's already on screen. Failure leaves the
    /// cache alone — the library did not change.
    pub fn fire_playlist_edit(
        &self,
        ctx: &egui::Context,
        label: &str,
        fut: impl Future<Output = Result<String, String>> + Send + 'static,
    ) {
        self.spawn_msg(ctx, label, fut, true);
    }

    fn spawn_msg(
        &self,
        ctx: &egui::Context,
        label: &str,
        fut: impl Future<Output = Result<String, String>> + Send + 'static,
        refresh_playlists: bool,
    ) {
        let ctx = ctx.clone();
        let label = label.to_owned();
        let notices = Arc::clone(&self.notices);
        let auth_expired = Arc::clone(&self.auth_expired);
        let playlist_dirty = Arc::clone(&self.playlist_dirty);
        self.rt().spawn(async move {
            match fut.await {
                Ok(msg) => {
                    if refresh_playlists {
                        playlist_dirty.store(true, Ordering::Release);
                    }
                    if !msg.is_empty() {
                        notices.lock().unwrap().push(msg);
                    }
                }
                Err(e) => {
                    note_auth_error(&auth_expired, &e);
                    notices.lock().unwrap().push(format!("{label}: {e}"));
                }
            }
            ctx.request_repaint();
        });
    }

    /// `true` once since the last take, after a successful playlist edit.
    pub fn take_playlist_dirty(&self) -> bool {
        self.playlist_dirty.swap(false, Ordering::AcqRel)
    }

    /// Drain pending notices (oldest first).
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }

    /// Drain the one-shot "Apple rejected the session (401)" signal; the
    /// UI classifies the cause and reacts (refresh vs re-login banner).
    pub fn take_auth_expired(&self) -> bool {
        std::mem::take(&mut *self.auth_expired.lock().unwrap())
    }
}

/// Raise the shared auth signal when a backend error is Apple rejecting
/// our credentials (see `shell::commands::is_auth_error`).
fn note_auth_error(flag: &std::sync::Mutex<bool>, e: &str) {
    if shell::commands::is_auth_error(e) {
        *flag.lock().unwrap() = true;
    }
}

/// Plain-words rendering of a backend error for the status line: a 401 is
/// a dead (or just-refreshed) session, not a status code to stare at.
pub fn friendly_error(err: &str) -> String {
    if shell::commands::is_auth_error(err) {
        "Apple rejected the request (401) — session or access token expired. Retry; if it \
         persists, sign in again in Settings → Account."
            .to_string()
    } else {
        err.to_string()
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

/// One async backend call plus its latest outcome, polled once per frame.
///
/// Views hold a `Pending<T>` per data slot (charts, search, playlist
/// detail, …): `start()` when the query changes, `poll()` every frame,
/// then read `value`/`error`/`loading` while rendering.
pub struct Pending<T> {
    rx: Option<Reply<T>>,
    pub value: Option<T>,
    pub error: Option<String>,
    pub loading: bool,
    /// When `value` last arrived. Library playlists use this to refetch
    /// while a screen stays open (other devices, collaborators).
    pub(crate) loaded_at: Option<std::time::Instant>,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            rx: None,
            value: None,
            error: None,
            loading: false,
            loaded_at: None,
        }
    }
}

impl<T> Pending<T> {
    /// Begin a new call, dropping any stale result or in-flight request.
    pub fn start(&mut self, rx: Reply<T>) {
        self.rx = Some(rx);
        self.value = None;
        self.error = None;
        self.loading = true;
        self.loaded_at = None;
    }

    /// Fetch again, keeping `value` on screen (no skeleton) until the
    /// new response arrives. Drops an in-flight request so a refetch
    /// started after a write cannot lose to an older GET.
    pub fn reload(&mut self, rx: Reply<T>) {
        self.rx = Some(rx);
        self.error = None;
        self.loading = false;
    }

    /// `true` when `value` is showing and older than `ttl` (or has no
    /// timestamp). Empty slots are not stale — callers treat those as
    /// a first load.
    pub(crate) fn older_than(&self, ttl: std::time::Duration) -> bool {
        self.value.is_some() && self.loaded_at.is_none_or(|t| t.elapsed() >= ttl)
    }

    /// Whether a library-playlist slot should start a fetch.
    /// `restart` drops an in-flight call (a write just landed, or the
    /// screen just opened). Otherwise a busy slot is left alone, and a
    /// failed first load waits for Retry.
    pub(crate) fn wants_fetch(&self, max_age: std::time::Duration, restart: bool) -> bool {
        if self.busy() && !restart {
            return false;
        }
        let missing = self.value.is_none();
        if missing && self.error.is_some() && !restart {
            return false;
        }
        restart || missing || self.older_than(max_age)
    }

    /// Drop the current result (e.g. when navigating away).
    pub fn clear(&mut self) {
        self.rx = None;
        self.value = None;
        self.error = None;
        self.loading = false;
        self.loaded_at = None;
    }

    /// Consume the result if the task finished this frame.
    pub fn poll(&mut self) {
        let Some(rx) = &mut self.rx else { return };
        match rx.try_recv() {
            Ok(Ok(v)) => {
                self.value = Some(v);
                self.loading = false;
                self.loaded_at = Some(std::time::Instant::now());
                self.rx = None;
            }
            Ok(Err(e)) => {
                self.loading = false;
                self.rx = None;
                if self.value.is_none() {
                    self.error = Some(e);
                } else {
                    // A background refresh failed. Keep what's on screen
                    // and let the poll interval elapse before retrying.
                    self.loaded_at = Some(std::time::Instant::now());
                }
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.loading = false;
                self.rx = None;
                if self.value.is_none() {
                    self.error = Some("backend task failed".into());
                } else {
                    self.loaded_at = Some(std::time::Instant::now());
                }
            }
        }
    }

    /// `true` while a call is in flight.
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn reload_keeps_the_value_until_the_new_one_lands() {
        let (tx, rx) = oneshot::channel();
        let mut slot = Pending {
            value: Some(1),
            ..Pending::default()
        };
        slot.reload(rx);
        assert_eq!(slot.value, Some(1));
        assert!(!slot.loading);
        tx.send(Ok(2)).unwrap();
        slot.poll();
        assert_eq!(slot.value, Some(2));
        assert!(!slot.older_than(Duration::from_secs(60)));
    }

    #[test]
    fn background_refresh_error_keeps_the_value() {
        let (tx, rx) = oneshot::channel();
        let mut slot = Pending {
            value: Some(1),
            ..Pending::default()
        };
        slot.reload(rx);
        tx.send(Err("nope".into())).unwrap();
        slot.poll();
        assert_eq!(slot.value, Some(1));
        assert!(slot.error.is_none());
        assert!(!slot.older_than(Duration::from_secs(60)));
    }

    #[test]
    fn wants_fetch_restarts_a_busy_slot_only_when_asked() {
        let (_tx, rx) = oneshot::channel();
        let mut slot = Pending {
            value: Some(1),
            loaded_at: Some(std::time::Instant::now()),
            ..Pending::default()
        };
        slot.reload(rx);
        assert!(!slot.wants_fetch(Duration::from_secs(15), false));
        assert!(slot.wants_fetch(Duration::from_secs(15), true));
    }
}
