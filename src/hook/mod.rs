/// hook/mod.rs — Unified platform-dispatch interface for the keyboard capture layer.
///
/// Consumers (lib.rs) only see `start_hook` and `stop_hook`.
/// Platform selection is entirely compile-time via `#[cfg(target_os)]` flags.
use std::sync::{atomic::AtomicBool, Arc};

use crate::session::SessionBuffer;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

// ─── Hook handle ─────────────────────────────────────────────────────────────

/// Opaque handle returned by `start_hook`.
///
/// Dropping this handle does NOT stop the hook — call `stop_hook` explicitly.
/// The handle owns the `shutdown` flag; dropping it without stopping first
/// will cause the watchdog to exit on its next poll tick (the AtomicBool is
/// shared by reference, so the watchdog sees the drop on the next Acquire load).
pub struct HookHandle {
    #[cfg(target_os = "windows")]
    inner: windows::WindowsHookHandle,

    #[cfg(target_os = "macos")]
    inner: macos::MacosHookHandle,

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    _phantom: (),

    /// FIX #3: shared with the inactivity watchdog. Set to true by stop_hook
    /// so the watchdog exits cleanly on the next 1-second tick.
    pub(crate) shutdown: Arc<AtomicBool>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Install the global keyboard hook on the current platform.
///
/// Returns a `HookHandle` that must be passed to `stop_hook` to release
/// OS resources. The handle also carries the watchdog shutdown signal —
/// the caller (lib.rs) spawns the watchdog *after* this returns Ok, passing
/// the same `shutdown` Arc so stop/start cycles don't leak threads (FIX #3).
pub fn start_hook(
    session_buf: Arc<SessionBuffer>,
) -> Result<HookHandle, String> {
    let shutdown = Arc::new(AtomicBool::new(false));

    // Platform-specific process name getter — resolved once per session open.
    #[cfg(target_os = "windows")]
    let process_getter: Arc<dyn Fn() -> String + Send + Sync + 'static> =
        Arc::new(windows::foreground_process_name);

    #[cfg(target_os = "macos")]
    let process_getter: Arc<dyn Fn() -> String + Send + Sync + 'static> =
        Arc::new(macos::foreground_process_name);

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _process_getter: Arc<dyn Fn() -> String + Send + Sync + 'static> =
        Arc::new(|| String::new());

    #[cfg(target_os = "windows")]
    {
        let inner = windows::start(Arc::clone(&session_buf), process_getter)?;
        return Ok(HookHandle { inner, shutdown });
    }

    #[cfg(target_os = "macos")]
    {
        let inner = macos::start(Arc::clone(&session_buf), process_getter)?;
        return Ok(HookHandle { inner, shutdown });
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = session_buf;
        Err("playful-cloud-core: unsupported platform".to_string())
    }
}

/// Stop the global keyboard hook and signal the watchdog to exit.
pub fn stop_hook(handle: HookHandle) {
    use std::sync::atomic::Ordering;

    // Signal the watchdog first — it exits on its next 1s tick.
    handle.shutdown.store(true, Ordering::Release);

    #[cfg(target_os = "windows")]
    windows::stop(handle.inner);

    #[cfg(target_os = "macos")]
    macos::stop(handle.inner);
}
