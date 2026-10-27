//! Shared backend: a Tokio runtime driving the `shell` service crate.
//!
//! Every UI-triggered backend call runs off-thread; the returned
//! [`oneshot::Receiver`] is polled once per frame in [`Pending`]. Tasks
//! repaint the UI when they finish, so results land without busy-polling.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::oneshot;

pub type Reply<T> = oneshot::Receiver<Result<T, String>>;

/// The Tauri-free service layer, owned behind an `Arc` so async tasks can
/// hold it for the duration of a call.
pub struct Backend {
    pub state: Arc<shell::AppState>,
    rt: tokio::runtime::Runtime,
    /// User-visible outcomes of fire-and-forget commands (success messages
    /// and errors), drained once per frame by the UI.
    notices: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Backend {
    /// Spawn a fire-and-forget task on the runtime (artwork fetches and
    /// other UI-side producers that deliver results through their own
    /// channel instead of a [`Reply`]).
    pub fn spawn<F>(&self, f: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(f);
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
            rt,
            notices: Arc::new(std::sync::Mutex::new(Vec::new())),
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
            rt,
            notices: Arc::new(std::sync::Mutex::new(Vec::new())),
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
        let (tx, rx) = oneshot::channel();
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
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
        self.rt.spawn(async move {
            if let Err(e) = fut.await {
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
        let ctx = ctx.clone();
        let label = label.to_owned();
        let notices = Arc::clone(&self.notices);
        self.rt.spawn(async move {
            match fut.await {
                Ok(msg) if !msg.is_empty() => {
                    notices.lock().unwrap().push(msg);
                }
                Err(e) => {
                    notices.lock().unwrap().push(format!("{label}: {e}"));
                }
                _ => {}
            }
            ctx.request_repaint();
        });
    }

    /// Drain pending notices (oldest first).
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
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
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            rx: None,
            value: None,
            error: None,
            loading: false,
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
    }

    /// Drop the current result (e.g. when navigating away).
    pub fn clear(&mut self) {
        self.rx = None;
        self.value = None;
        self.error = None;
        self.loading = false;
    }

    /// Consume the result if the task finished this frame.
    pub fn poll(&mut self) {
        let Some(rx) = &mut self.rx else { return };
        match rx.try_recv() {
            Ok(Ok(v)) => {
                self.value = Some(v);
                self.loading = false;
                self.rx = None;
            }
            Ok(Err(e)) => {
                self.error = Some(e);
                self.loading = false;
                self.rx = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.error = Some("backend task failed".into());
                self.loading = false;
                self.rx = None;
            }
        }
    }

    /// `true` while a call is in flight.
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}
