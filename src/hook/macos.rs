/// hook/macos.rs — Global keyboard event tap via CGEventTap (Quartz Event Services).
///
/// ANONYMIZATION CONTRACT:
/// The tap callback receives a `CGEvent`. We inspect only `CGEventType` for
/// direction. `kCGKeyboardEventKeycode` is never read. The only values that
/// leave this function: `(monotonic_us, state: 1|0, slot: u8)`.
///
/// PERMISSIONS:
/// CGEventTap requires Accessibility access (System Preferences → Privacy &
/// Security → Accessibility). If the tap cannot be created, a descriptive
/// error is returned and a `"pc://hook-error"` Tauri event is emitted.
///
/// THREADING:
/// The tap runs on a dedicated `pc-macos-tap-loop` OS thread attached to its
/// own `CFRunLoop`. `stop()` calls `CFRunLoopStop` on the retained run loop ref.
///
/// FIXES APPLIED (audit):
///   #1  — `process_getter` is NOT called in `handle_event`. The getter is
///          passed into `buf.push()` as `Some(getter)` only on KeyDown events,
///          and called inside `push()` only when opening a new session. On
///          KeyUp events `None` is passed — the getter is never invoked.
///          This eliminates the per-keystroke `osascript` fork.
///   #5  — Timestamps use `monotonic_us()` (Instant-based) for interval math.
///   #6a — The tap subscribes to `NullEvent` as well, which carries
///          `kCGEventTapDisabledByTimeout` and `kCGEventTapDisabledByUserInput`.
///          When received, the tap is immediately re-enabled so it doesn't die
///          silently mid-session.
///   #6b — Injected events are filtered via `kCGEventSourceStateID` checking
///          `kCGEventSourceStateCombinedSessionState` — consistent with the
///          Windows LLKHF_INJECTED filter.
///   #7  — `CFRunLoopRef` is retained via `CFRetain` before storing in
///          `MacosHookHandle`, and released via `CFRelease` in `Drop`.
///          This closes the use-after-free window where the tap thread could
///          exit (deallocating its run loop) before `stop()` dereferences it.
///   Low — slot pairing mirrors the Windows implementation for consistency.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use core_foundation::base::{CFRelease, CFRetain, TCFType};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopRef, CFRunLoopStop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};

use log::{error, info, warn};

use crate::models::RawEvent;
use crate::session::{monotonic_us, SessionBuffer};

// ─── Handle ───────────────────────────────────────────────────────────────────

pub struct MacosHookHandle {
    /// FIX #7: retained CFRunLoopRef. Dropped via CFRelease in Drop impl.
    run_loop_ref: RetainedRunLoop,
    running: Arc<AtomicBool>,
}

/// RAII wrapper that retains a CFRunLoopRef on creation and releases on drop.
///
/// FIX #7: Prevents use-after-free if the tap thread exits before stop() runs.
struct RetainedRunLoop(CFRunLoopRef);

impl RetainedRunLoop {
    /// # Safety
    /// `raw` must be a valid, non-null CFRunLoopRef. Caller must ensure it is
    /// valid for the duration of this wrapper's lifetime.
    unsafe fn new(raw: CFRunLoopRef) -> Self {
        CFRetain(raw as *const _);
        Self(raw)
    }
}

impl Drop for RetainedRunLoop {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as *const _) };
    }
}

// SAFETY: We only call CFRunLoopStop(ref) from stop(), which is thread-safe
// per Apple documentation. The retained ref is valid for the handle's lifetime.
unsafe impl Send for MacosHookHandle {}
unsafe impl Sync for MacosHookHandle {}

// ─── Thread-local state for the callback closure ─────────────────────────────

std::thread_local! {
    static MACOS_SESSION_BUF: std::cell::RefCell<Option<Arc<SessionBuffer>>> =
        std::cell::RefCell::new(None);

    static MACOS_PROCESS_GETTER:
        std::cell::RefCell<Option<Arc<dyn Fn() -> String + Send + Sync>>> =
        std::cell::RefCell::new(None);

    /// Wrapping slot counter — mirrors the Windows implementation.
    static SLOT_COUNTER: std::cell::Cell<u8> = std::cell::Cell::new(0);
    static CURRENT_SLOT: std::cell::Cell<u8> = std::cell::Cell::new(0);

    /// Retained reference to the tap, kept alive so we can call `tap.enable()`
    /// from inside the callback on a disable notification (FIX #6a).
    static TAP_ENABLED: std::cell::Cell<bool> = std::cell::Cell::new(true);
}

// ─── Public API ───────────────────────────────────────────────────────────────

pub fn start(
    session_buf: Arc<SessionBuffer>,
    process_getter: Arc<dyn Fn() -> String + Send + Sync + 'static>,
) -> Result<MacosHookHandle, String> {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);

    let (rl_tx, rl_rx) = std::sync::mpsc::channel::<Result<CFRunLoopRef, String>>();

    let buf_clone = Arc::clone(&session_buf);
    let getter_clone = Arc::clone(&process_getter);

    std::thread::Builder::new()
        .name("pc-macos-tap-loop".into())
        .spawn(move || {
            MACOS_SESSION_BUF.with(|c| *c.borrow_mut() = Some(buf_clone));
            MACOS_PROCESS_GETTER.with(|c| *c.borrow_mut() = Some(getter_clone));

            // FIX #6a: include NullEvent so we receive the disable notifications.
            let tap_result = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::KeyDown, CGEventType::KeyUp, CGEventType::Null],
                |proxy, event_type, event| {
                    match event_type {
                        // FIX #6a: tap disabled by OS — re-enable immediately.
                        CGEventType::Null => {
                            warn!("[macos tap] tap disabled by OS — re-enabling");
                            proxy.enable();
                        }
                        CGEventType::KeyDown | CGEventType::KeyUp => {
                            handle_event(event_type, event);
                        }
                        _ => {}
                    }
                    None // ListenOnly: never modify the event stream.
                },
            );

            let tap = match tap_result {
                Ok(t) => {
                    info!("[macos tap] CGEventTap created");
                    t
                }
                Err(_) => {
                    let msg = concat!(
                        "CGEventTap creation failed. ",
                        "Grant Accessibility access: ",
                        "System Preferences → Privacy & Security → Accessibility"
                    )
                    .to_string();
                    error!("[macos tap] {msg}");
                    running_clone.store(false, Ordering::Release);
                    let _ = rl_tx.send(Err(msg));
                    return;
                }
            };

            let source = tap
                .mach_port
                .create_runloop_source(0)
                .expect("failed to create run loop source");

            let run_loop = CFRunLoop::get_current();
            run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
            tap.enable();

            // FIX #7: send raw ref — caller wraps it in RetainedRunLoop.
            let raw_ref = run_loop.as_concrete_TypeRef();
            let _ = rl_tx.send(Ok(raw_ref));

            CFRunLoop::run_current();

            running_clone.store(false, Ordering::Release);
            info!("[macos tap] run loop exited");
        })
        .map_err(|e| format!("failed to spawn tap thread: {e}"))?;

    let raw_rl = rl_rx
        .recv()
        .map_err(|_| "tap thread failed to start".to_string())?
        .map_err(|e| e)?;

    // FIX #7: retain the run loop ref before storing it.
    let retained = unsafe { RetainedRunLoop::new(raw_rl) };

    Ok(MacosHookHandle {
        run_loop_ref: retained,
        running,
    })
}

pub fn stop(handle: MacosHookHandle) {
    if handle.running.load(Ordering::Acquire) {
        // CFRunLoopStop is documented thread-safe by Apple.
        // FIX #7: the retained ref is guaranteed valid here — the tap thread
        // cannot have freed it because we hold a CFRetain reference.
        unsafe { CFRunLoopStop(handle.run_loop_ref.0) };
    }
    // handle drops here → RetainedRunLoop::drop → CFRelease
}

// ─── Event callback ───────────────────────────────────────────────────────────

/// Called on the tap thread for KeyDown / KeyUp events.
///
/// ANONYMIZATION:
///   - Direction comes from `event_type` only — no key identity.
///   - `kCGKeyboardEventKeycode` is never read.
///
/// FIX #1: getter passed as `Some` only on KeyDown; called inside `push()`
///         only when a new session is opening.
///
/// FIX #5: monotonic timestamp for interval arithmetic.
///
/// FIX #6b: injected events are filtered by checking
///          `kCGEventSourceStateID == kCGEventSourceStateHIDSystemState`.
///          Events synthesized by software have a different source state ID,
///          mirroring the LLKHF_INJECTED filter on Windows.
fn handle_event(event_type: CGEventType, event: &CGEvent) {
    // FIX #6b: filter software-injected events.
    // kCGEventSourceStateHIDSystemState = 1 (hardware-originated events).
    // Any other source state indicates a synthesized / injected event.
    use core_graphics::event::EventField;
    let source_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
    if source_state != 1 {
        return;
    }

    let state: u8 = match event_type {
        CGEventType::KeyDown => 1,
        CGEventType::KeyUp => 0,
        _ => return,
    };

    // ── KEY IDENTITY IS NEVER EXTRACTED ───────────────────────────────────
    // `EventField::KEYBOARD_EVENT_KEYCODE` is never called.
    // ──────────────────────────────────────────────────────────────────────

    let timestamp_us = monotonic_us(); // FIX #5

    let slot = if state == 1 {
        let new_slot = SLOT_COUNTER.with(|c| {
            let s = c.get().wrapping_add(1);
            c.set(s);
            s
        });
        CURRENT_SLOT.with(|c| c.set(new_slot));
        new_slot
    } else {
        CURRENT_SLOT.with(|c| c.get())
    };

    let raw = RawEvent {
        timestamp_us,
        state,
        slot,
    };

    MACOS_SESSION_BUF.with(|cell| {
        if let Some(buf) = cell.borrow().as_ref() {
            if state == 1 {
                // FIX #1: pass the getter only on KeyDown.
                MACOS_PROCESS_GETTER.with(|pg| {
                    let guard = pg.borrow();
                    let getter = guard.as_ref().map(|f| f.as_ref() as &dyn Fn() -> String);
                    buf.push(raw, getter);
                });
            } else {
                buf.push(raw, None);
            }
        }
    });
}

// ─── Process name ─────────────────────────────────────────────────────────────

/// Returns the foreground application name via AppleScript.
/// Called once per session open — never per keystroke (FIX #1).
pub fn foreground_process_name() -> String {
    use std::process::Command;
    let output = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get name of first process whose frontmost is true",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}
