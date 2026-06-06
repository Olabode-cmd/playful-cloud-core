/// hook/windows.rs — Low-level keyboard hook via SetWindowsHookEx(WH_KEYBOARD_LL).
///
/// ANONYMIZATION CONTRACT:
/// The `hook_proc` callback receives a `KBDLLHOOKSTRUCT` from Windows.
/// The only field ever accessed from that struct is `flags` — to test bit 4
/// (LLKHF_INJECTED). `vkCode` and `scanCode` are never read. All that leaves
/// this function is `(monotonic_us, state: 1|0, slot: u8)`.
///
/// THREADING:
/// `WH_KEYBOARD_LL` requires a message pump on the installing thread.
/// We spawn a dedicated `pc-win-hook-pump` OS thread, install the hook there,
/// and run `GetMessage` in a loop. `stop()` posts `WM_QUIT` to that thread,
/// which causes `GetMessage` to return 0 and the thread to call
/// `UnhookWindowsHookEx` before exiting.
///
/// FIXES APPLIED (audit):
///   #1  — `process_getter` is NOT called in `hook_proc`. The hook passes a
///          lazy `Option<&dyn Fn() -> String>` into `buf.push()` only on
///          KeyDown events when a new session is opening. The getter itself is
///          only invoked inside `SessionBuffer::push` when `last_keydown` is
///          None — i.e., once per session, not per keystroke.
///   #2  — `OpenProcess` handle is closed via `OwnedHandle` RAII in
///          `windows_foreground_process`. No raw handle leaks on any path.
///   #5  — Timestamps use `monotonic_us()` (Instant-based) for all interval
///          arithmetic. `kb.time` (tick count) is not used.
///   Low — Header comment corrected: we read `kb.flags`, not `kb.vkCode`,
///          to check the injected bit. The privacy story is better than the
///          original comment implied.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use log::{error, info, warn};
use windows::Win32::Foundation::{HANDLE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::PWSTR;

use crate::models::RawEvent;
use crate::session::{monotonic_us, SessionBuffer};

// ─── Thread-local hook state ──────────────────────────────────────────────────
// Stored in thread-locals so the `unsafe extern "system"` static callback can
// reach shared state without global mutable statics.

std::thread_local! {
    static HOOK_SESSION_BUF: std::cell::RefCell<Option<Arc<SessionBuffer>>> =
        std::cell::RefCell::new(None);

    static PROCESS_GETTER: std::cell::RefCell<Option<Arc<dyn Fn() -> String + Send + Sync>>> =
        std::cell::RefCell::new(None);

    /// Wrapping slot counter. Incremented on each KeyDown. The paired KeyUp
    /// reuses the most-recent KeyDown's slot. This gives correct dwell pairing
    /// for normal sequential typing, but is only a best-effort approximation
    /// under key rollover (overlapping presses) — see aggregate_and_clear.
    static SLOT_COUNTER: std::cell::Cell<u8> = std::cell::Cell::new(0);
    /// Slot assigned to the most-recent KeyDown, sent with the next KeyUp.
    static CURRENT_SLOT: std::cell::Cell<u8> = std::cell::Cell::new(0);
}

// ─── Handle ───────────────────────────────────────────────────────────────────

pub struct WindowsHookHandle {
    thread_id: u32,
    running: Arc<AtomicBool>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

pub fn start(
    session_buf: Arc<SessionBuffer>,
    process_getter: Arc<dyn Fn() -> String + Send + Sync + 'static>,
) -> Result<WindowsHookHandle, String> {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);

    let (tid_tx, tid_rx) = std::sync::mpsc::channel::<u32>();

    let buf_clone = Arc::clone(&session_buf);
    let getter_clone = Arc::clone(&process_getter);

    std::thread::Builder::new()
        .name("pc-win-hook-pump".into())
        .spawn(move || {
            let hook = unsafe {
                let hmod = GetModuleHandleW(None).unwrap_or_default();
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod, 0)
            };

            let hook = match hook {
                Ok(h) => {
                    info!("[windows hook] WH_KEYBOARD_LL installed");
                    h
                }
                Err(e) => {
                    error!("[windows hook] SetWindowsHookExW failed: {e}");
                    running_clone.store(false, Ordering::Release);
                    let _ = tid_tx.send(0);
                    return;
                }
            };

            // Force this thread's message queue into existence BEFORE we
            // publish the thread ID. Windows lazily creates the queue on the
            // first message call; without this, a stop() that fires between
            // start() returning and the first GetMessageW below could have its
            // PostThreadMessageW(WM_QUIT) silently dropped, leaving the pump
            // blocked forever and the hook leaked. PeekMessageW with
            // PM_NOREMOVE creates the queue without consuming anything.
            unsafe {
                let mut probe = MSG::default();
                let _ = PeekMessageW(&mut probe, None, WM_USER, WM_USER, PM_NOREMOVE);
            }

            // Publish thread ID — the queue now exists, so any WM_QUIT posted
            // from stop() after this point is guaranteed to be queued and
            // delivered to GetMessageW below (FIX low: startup race closed).
            let tid = unsafe { GetCurrentThreadId() };
            let _ = tid_tx.send(tid);

            HOOK_SESSION_BUF.with(|cell| *cell.borrow_mut() = Some(buf_clone));
            PROCESS_GETTER.with(|cell| *cell.borrow_mut() = Some(getter_clone));

            // Message pump — WH_KEYBOARD_LL callbacks are delivered here.
            let mut msg = MSG::default();
            loop {
                let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                match result.0 {
                    -1 => {
                        warn!("[windows hook] GetMessageW returned error");
                        break;
                    }
                    0 => {
                        info!("[windows hook] WM_QUIT — pump exiting");
                        break;
                    }
                    _ => {}
                }
            }

            let _ = unsafe { UnhookWindowsHookEx(hook) };
            running_clone.store(false, Ordering::Release);
            info!("[windows hook] hook uninstalled");
        })
        .map_err(|e| format!("failed to spawn hook thread: {e}"))?;

    let thread_id = tid_rx
        .recv()
        .map_err(|_| "hook thread failed to start".to_string())?;

    if thread_id == 0 {
        return Err("SetWindowsHookExW failed — see logs".to_string());
    }

    Ok(WindowsHookHandle { thread_id, running })
}

pub fn stop(handle: WindowsHookHandle) {
    if !handle.running.load(Ordering::Acquire) {
        return;
    }

    // Post WM_QUIT to break the pump's GetMessageW loop. The message queue is
    // guaranteed to exist (see the PeekMessageW probe in start()), so this
    // should succeed on the first try. We still retry a few times as defense
    // in depth: if every attempt fails the pump would otherwise block forever
    // and leak the hook, so we log loudly rather than swallow the error.
    for attempt in 0..10 {
        let posted =
            unsafe { PostThreadMessageW(handle.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if posted.is_ok() {
            return;
        }
        // Thread may already be tearing down, or the queue isn't ready yet.
        if !handle.running.load(Ordering::Acquire) {
            return; // Pump already exited on its own — nothing to do.
        }
        warn!("[windows hook] PostThreadMessageW(WM_QUIT) attempt {attempt} failed — retrying");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    error!("[windows hook] failed to post WM_QUIT after retries — pump thread may leak");
}

// ─── Static hook callback ─────────────────────────────────────────────────────

/// Called by Windows on the hook-pump thread for every keyboard event.
///
/// ANONYMIZATION:
///   - `wParam` determines direction (KeyDown / KeyUp). No key identity.
///   - `kb.flags` bit 4 (LLKHF_INJECTED) filters synthetic input.
///   - `kb.vkCode` and `kb.scanCode` are NEVER read or stored.
///   - The only values that leave this function: `(monotonic_us, state, slot)`.
///
/// PROCESS GETTER (FIX #1):
///   The getter closure is passed into `buf.push()` as `Some(getter)` only on
///   KeyDown events. `SessionBuffer::push` calls it only when opening a new
///   session (`last_keydown.is_none()`). On KeyUp events `None` is passed so
///   the getter is never even considered, let alone invoked.
///
/// SLOT PAIRING (FIX #4 — best effort):
///   Each KeyDown increments a wrapping u8 counter and records the new slot.
///   The next KeyUp reuses that slot value. This pairs releases correctly for
///   normal sequential typing, but only approximates dwell under key rollover
///   (overlapping presses), where a KeyUp may be tagged with a different key's
///   slot. Exact pairing is impossible without key identity, which the privacy
///   contract forbids. No key identity is stored either way.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        let state: u8 = match wparam.0 as u32 {
            w if w == WM_KEYDOWN || w == WM_SYSKEYDOWN => 1,
            w if w == WM_KEYUP || w == WM_SYSKEYUP => 0,
            _ => {
                return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
            }
        };

        // Filter injected synthetic events (LLKHF_INJECTED = bit 4 of flags).
        // NOTE: we read `kb.flags`, not `kb.vkCode` — the key code is never
        // accessed anywhere in this function.
        if (kb.flags.0 & 0x10) != 0 {
            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
        }

        // FIX #5: monotonic timestamp — no wall-clock NTP sensitivity.
        let timestamp_us = monotonic_us();

        let slot = if state == 1 {
            // KeyDown: advance the slot counter and record it for the paired KeyUp.
            let new_slot = SLOT_COUNTER.with(|c| {
                let s = c.get().wrapping_add(1);
                c.set(s);
                s
            });
            CURRENT_SLOT.with(|c| c.set(new_slot));
            new_slot
        } else {
            // KeyUp: use the slot assigned to the most-recent KeyDown.
            CURRENT_SLOT.with(|c| c.get())
        };

        let event = RawEvent {
            timestamp_us,
            state,
            slot,
        };

        HOOK_SESSION_BUF.with(|cell| {
            if let Some(buf) = cell.borrow().as_ref() {
                if state == 1 {
                    // FIX #1: only pass the getter on KeyDown, and only called
                    // inside push() when opening a new session.
                    PROCESS_GETTER.with(|pg| {
                        let guard = pg.borrow();
                        let getter = guard.as_ref().map(|f| f.as_ref() as &dyn Fn() -> String);
                        buf.push(event, getter);
                    });
                } else {
                    // KeyUp: no process lookup ever needed.
                    buf.push(event, None);
                }
            }
        });
    }

    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

// ─── Process name (FIX #2: RAII handle, no leaks) ────────────────────────────

/// Returns the name of the foreground process (e.g. "Code", "Slack").
/// Called once per session open — never per keystroke.
///
/// FIX #2: `OpenProcess` returns a handle wrapped in a local RAII guard
/// (`OwnedHandle`) that calls `CloseHandle` on drop. Every exit path —
/// including the early-return error paths — closes the handle.
pub fn foreground_process_name() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }

        let raw_handle =
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) => h,
                Err(_) => return String::new(),
            };

        // RAII wrapper — CloseHandle fires on drop at end of scope.
        let _owned = OwnedHandle(raw_handle);

        let mut name_buf = vec![0u16; 512];
        let mut size = name_buf.len() as u32;
        let pw = PWSTR(name_buf.as_mut_ptr());

        if QueryFullProcessImageNameW(raw_handle, PROCESS_NAME_WIN32, pw, &mut size).is_ok() {
            let full_path = String::from_utf16_lossy(&name_buf[..size as usize]);
            std::path::Path::new(&full_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        }
        // _owned drops here → CloseHandle(raw_handle)
    }
}

/// Minimal RAII wrapper that closes a Win32 HANDLE on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}
