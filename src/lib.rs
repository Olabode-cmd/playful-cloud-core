/// lib.rs — Tauri v2 plugin entry point for playful-cloud-core.
///
/// Commands exposed to the Tauri frontend:
///   `start_hook`  — install the OS hook and start the inactivity watchdog.
///   `stop_hook`   — uninstall the hook and signal the watchdog to exit.
///   `get_session` — non-blocking drain of completed `LocalSession` values.
///
/// Register in the desktop shell:
///   tauri::Builder::default()
///       .plugin(playful_cloud_core::init())
///       .run(tauri::generate_context!())
///       .expect("error running tauri app");
///
/// FIXES APPLIED (audit):
///   #3  — Watchdog is spawned AFTER a successful hook start, and receives the
///          same `shutdown` Arc that is set by `stop_hook`. No watchdog leaks
///          across start/stop cycles.
///   #9  — `get_session` returns `Result` instead of panicking on a poisoned
///          mutex, consistent with `start_hook` / `stop_hook`.
///   #10 — Watchdog uses `try_send` (see session.rs). Channel remains 256-deep
///          which is generous for typical polling intervals.
///   crate-type — removed staticlib / cdylib; only rlib is needed for a
///          library plugin consumed by Cargo.
use std::sync::{mpsc, Arc, Mutex};

use log::info;
use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Emitter, Manager, Runtime, State,
};

mod hook;
mod models;
mod session;

pub use models::LocalSession;

// ─── Plugin state ─────────────────────────────────────────────────────────────

struct PluginState {
    /// Active hook handle. `None` when the hook is not running.
    hook_handle: Mutex<Option<hook::HookHandle>>,
    /// Completed sessions ready for the frontend to drain.
    session_rx: Mutex<mpsc::Receiver<LocalSession>>,
    /// Sender half kept alive so the watchdog can always push.
    session_tx: mpsc::SyncSender<LocalSession>,
    /// Shared event buffer between the OS hook callbacks and the watchdog.
    session_buf: Arc<session::SessionBuffer>,
}

// ─── Tauri commands ───────────────────────────────────────────────────────────

/// Install the global keyboard hook and start the inactivity watchdog.
///
/// Safe to call repeatedly — idempotent if already running.
/// Emits `"pc://hook-error"` if the OS rejects the hook (e.g. missing
/// Accessibility permission on macOS).
///
/// FIX #3: Watchdog is spawned only after the hook starts successfully.
/// The watchdog holds the same `shutdown` Arc as the `HookHandle`, so
/// `stop_hook` tears both down atomically.
#[tauri::command]
fn start_hook<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, PluginState>,
) -> Result<(), String> {
    let mut guard = state.hook_handle.lock().map_err(|e| e.to_string())?;

    if guard.is_some() {
        info!("[plugin] start_hook: hook already running");
        return Ok(());
    }

    let buf = Arc::clone(&state.session_buf);

    // FIX #3: attempt hook install first — only spawn watchdog on success.
    let handle = hook::start_hook(Arc::clone(&buf)).map_err(|e| {
        let _ = app.emit("pc://hook-error", e.clone());
        e
    })?;

    // Watchdog shares the shutdown flag from the hook handle.
    let shutdown = Arc::clone(&handle.shutdown);
    let buf_handle = buf.handle();
    let tx = state.session_tx.clone();
    session::spawn_inactivity_watchdog(buf_handle, tx, shutdown);

    *guard = Some(handle);
    info!("[plugin] keyboard hook started");
    Ok(())
}

/// Uninstall the hook and signal the watchdog to exit.
/// Buffered partial-session events (since the last inactivity flush) are
/// discarded — only fully aggregated sessions remain in the channel.
#[tauri::command]
fn stop_hook(state: State<'_, PluginState>) -> Result<(), String> {
    let mut guard = state.hook_handle.lock().map_err(|e| e.to_string())?;

    if let Some(handle) = guard.take() {
        hook::stop_hook(handle);
        info!("[plugin] keyboard hook stopped");
    } else {
        info!("[plugin] stop_hook: no hook was running");
    }

    Ok(())
}

/// Drain all completed `LocalSession`s from the internal channel.
///
/// Non-blocking — returns an empty array if nothing is ready.
/// Call on a polling interval or after receiving a `"pc://session-ready"` event.
///
/// FIX #9: returns `Result` instead of panicking on a poisoned mutex.
#[tauri::command]
fn get_session(state: State<'_, PluginState>) -> Result<Vec<LocalSession>, String> {
    let rx = state.session_rx.lock().map_err(|e| e.to_string())?;
    let mut sessions = Vec::new();

    loop {
        match rx.try_recv() {
            Ok(s) => sessions.push(s),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }

    Ok(sessions)
}

// ─── Plugin initialiser ───────────────────────────────────────────────────────

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    let (tx, rx) = mpsc::sync_channel::<LocalSession>(256);

    PluginBuilder::new("playful-cloud-core")
        .setup(move |app, _api| {
            app.manage(PluginState {
                hook_handle: Mutex::new(None),
                session_rx: Mutex::new(rx),
                session_tx: tx,
                session_buf: Arc::new(session::SessionBuffer::new()),
            });
            info!("[plugin] playful-cloud-core initialised");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![start_hook, stop_hook, get_session])
        .build()
}
