/// session.rs — In-memory ring buffer, inactivity detection, and session aggregation.
///
/// This module owns the stateful side of the telemetry pipeline:
///   1. Accepts anonymous `RawEvent`s from the OS hook layer.
///   2. Watches for a 20-second inactivity window on a dedicated thread.
///   3. When the window fires, aggregates the buffer into a `LocalSession`
///      and sends it over an `mpsc` channel to the Tauri plugin layer.
///
/// Fixes applied (audit rounds 1 & 2):
///   #4  — Dwell pairing now matches KeyDown↔KeyUp by `slot` index rather than
///          a blind sequential zip. This is correct for normal sequential
///          typing. It does NOT fully resolve key rollover (overlapping
///          presses): a KeyUp is tagged with the most-recent KeyDown's slot,
///          so under overlap the pair can be mismatched or dropped. Dwell is
///          therefore a best-effort signal — see aggregate_and_clear.
///          Honoring the no-key-identity privacy contract makes exact rollover
///          pairing impossible by construction.
///   #5  — All interval arithmetic uses `Instant`-based monotonic deltas.
///          `SystemTime` is captured once per session open for `started_at`.
///   #8  — Session-open detection checks `last_keydown_us.is_none()` only,
///          not `events.is_empty()`, so a stray leading KeyUp can't suppress
///          process-name capture.
///   #10 — Watchdog uses `try_send`; drops the session with a warning rather
///          than blocking forever when the channel is full. In-buffer event
///          count is capped at MAX_BUFFERED_EVENTS to bound memory growth.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};
use uuid::Uuid;

use crate::models::{LocalSession, RawEvent};

// ─── Constants ────────────────────────────────────────────────────────────────

/// A session closes when no KeyDown is seen for this many seconds.
pub const INACTIVITY_TIMEOUT_SECS: u64 = 20;

/// Hard cap on buffered events per session. Prevents unbounded memory growth
/// when the frontend stops draining and the watchdog is blocked (fix #10).
const MAX_BUFFERED_EVENTS: usize = 16_384;

// ─── SessionBuffer ────────────────────────────────────────────────────────────

/// Thread-safe in-memory buffer holding anonymous timing events for the
/// current active session.
#[derive(Debug)]
pub struct SessionBuffer {
    inner: Arc<Mutex<BufferInner>>,
}

#[derive(Debug)]
pub(crate) struct BufferInner {
    /// All raw events for the current open session.
    events: Vec<RawEvent>,

    /// Monotonic instant of the last KeyDown event.
    /// Used by the inactivity watchdog.
    last_keydown: Option<Instant>,

    /// Wall-clock Unix milliseconds at the moment the session opened.
    /// Captured once — not updated per keystroke.
    session_wall_start_ms: Option<i64>,

    /// Monotonic instant at the moment the session opened.
    /// Used to compute `duration_secs` without wall-clock drift.
    session_mono_start: Option<Instant>,

    /// Process name captured when the session opened (first KeyDown after flush).
    process_name: String,
}

impl Default for BufferInner {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            last_keydown: None,
            session_wall_start_ms: None,
            session_mono_start: None,
            process_name: String::new(),
        }
    }
}

impl SessionBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BufferInner::default())),
        }
    }

    /// Returns a clone of the inner Arc so the watchdog thread can share state.
    pub(crate) fn handle(&self) -> Arc<Mutex<BufferInner>> {
        Arc::clone(&self.inner)
    }

    /// Push a single anonymous event into the buffer.
    ///
    /// FIX #8: Session open is detected solely by `last_keydown.is_none()`.
    /// A stray leading KeyUp no longer prevents process-name capture because
    /// we no longer check `events.is_empty()` as part of the condition.
    ///
    /// FIX #1 (partial): The `process_getter` closure is passed in by the
    /// hook callback only on KeyDown events, and called here only when opening
    /// a new session — never on every event. See windows.rs / macos.rs for the
    /// other half of this fix (the getter is not called at all on KeyUp).
    pub fn push(&self, event: RawEvent, get_process: Option<&dyn Fn() -> String>) {
        let mut buf = self.inner.lock().expect("session buffer lock poisoned");

        // Hard cap — drop the event silently rather than growing without bound.
        // This only fires if the watchdog is blocked and the hook keeps capturing.
        if buf.events.len() >= MAX_BUFFERED_EVENTS {
            warn!("[session] buffer cap reached — dropping event");
            return;
        }

        if event.state == 1 {
            // Opening a new session: first KeyDown after a flush or cold start.
            // FIX #8: check last_keydown only — not events.is_empty().
            if buf.last_keydown.is_none() {
                let now_mono = Instant::now();
                let now_wall_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                buf.session_wall_start_ms = Some(now_wall_ms);
                buf.session_mono_start = Some(now_mono);

                // FIX #1: process getter is called ONLY here, once per session.
                // The hook passes `Some(getter)` only on KeyDown events, so even
                // if this branch is somehow re-entered it only fires at session open.
                if let Some(getter) = get_process {
                    buf.process_name = getter();
                    debug!("[session] new session opened — process: {}", buf.process_name);
                }
            }
            buf.last_keydown = Some(Instant::now());
        }

        buf.events.push(event);
    }
}

impl Default for SessionBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Inactivity watchdog ──────────────────────────────────────────────────────

/// Spawns a background thread that polls the buffer every second.
/// When the last KeyDown is older than `INACTIVITY_TIMEOUT_SECS`, flushes
/// the buffer into a `LocalSession` and sends it over `tx`.
///
/// FIX #3: The watchdog receives a `shutdown` flag tied to the `HookHandle`.
/// When `stop_hook` drops the handle, `shutdown` is set and the watchdog exits
/// cleanly on the next poll tick — no thread leak across start/stop cycles.
///
/// FIX #10: Uses `try_send` instead of blocking `send`. If the channel is full
/// (frontend not draining), the session is logged and dropped rather than
/// blocking the watchdog indefinitely.
pub(crate) fn spawn_inactivity_watchdog(
    handle: Arc<Mutex<BufferInner>>,
    tx: std::sync::mpsc::SyncSender<LocalSession>,
    shutdown: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("pc-inactivity-watchdog".into())
        .spawn(move || {
            info!("[watchdog] inactivity watchdog started");
            loop {
                std::thread::sleep(Duration::from_secs(1));

                // FIX #3: check shutdown before doing any work.
                if shutdown.load(Ordering::Acquire) {
                    info!("[watchdog] shutdown signal received — exiting");
                    break;
                }

                let session_opt = {
                    let mut buf = handle.lock().expect("watchdog lock poisoned");

                    let should_flush = buf.last_keydown.map_or(false, |last| {
                        last.elapsed() >= Duration::from_secs(INACTIVITY_TIMEOUT_SECS)
                    });

                    if should_flush {
                        debug!("[watchdog] inactivity threshold reached — flushing");
                        aggregate_and_clear(&mut buf)
                    } else {
                        None
                    }
                };

                if let Some(session) = session_opt {
                    info!(
                        "[watchdog] session flushed — {} keystrokes in {}s on '{}'",
                        session.total_keystrokes, session.duration_secs, session.process_name
                    );
                    // FIX #10: non-blocking send — drop rather than block.
                    match tx.try_send(session) {
                        Ok(_) => {}
                        Err(std::sync::mpsc::TrySendError::Full(s)) => {
                            warn!(
                                "[watchdog] session channel full — dropping session \
                                 ({} keystrokes). Call get_session more frequently.",
                                s.total_keystrokes
                            );
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            info!("[watchdog] channel disconnected — exiting");
                            break;
                        }
                    }
                }
            }
            info!("[watchdog] thread exiting");
        })
        .expect("failed to spawn inactivity watchdog thread");
}

// ─── Aggregation logic ────────────────────────────────────────────────────────

/// Drains `buf`, computes statistics, and returns a `LocalSession`.
/// Resets buf to empty state unconditionally.
///
/// FIX #4: Dwell times are computed by matching KeyDown↔KeyUp pairs by `slot`
/// index rather than a blind sequential zip. This is correct for normal
/// sequential typing (down A, up A, down B, up B). It is only a best-effort
/// approximation under key rollover (down A, down B, up A, up B): because a
/// KeyUp carries the most-recent KeyDown's slot, overlapping presses can pair
/// a release with the wrong press or drop it entirely. Exact rollover pairing
/// would require per-key identity, which the privacy contract forbids. Treat
/// `avg_dwell_ms` as an approximate signal, not a precise per-key measurement.
///
/// FIX #5: All interval arithmetic operates on monotonic `timestamp_us` deltas
/// (captured via `Instant` in the hook callbacks). `started_at` uses the
/// wall-clock value recorded once at session open, kept separate to avoid NTP
/// drift corrupting timing stats.
///
/// FIX variance precision: intermediate sums use f64 and are narrowed to f32
/// only at the final assignment — guarding against precision loss on the tight
/// σ²_D < 4ms² anti-cheat threshold.
fn aggregate_and_clear(buf: &mut BufferInner) -> Option<LocalSession> {
    if buf.events.is_empty() {
        return None;
    }

    // Collect KeyDown timestamps in arrival order (for flight-time computation).
    // Collect (slot → down_timestamp_us) for dwell pairing.
    let mut keydown_times: Vec<u64> = Vec::new();
    // Pending down-timestamps keyed by slot, waiting for their KeyUp pair.
    let mut pending_downs: HashMap<u8, u64> = HashMap::new();
    // Completed (down_us, up_us) pairs for dwell calculation.
    let mut dwell_pairs: Vec<(u64, u64)> = Vec::new();

    for event in &buf.events {
        match event.state {
            1 => {
                // KeyDown
                keydown_times.push(event.timestamp_us);
                // Store (or overwrite on slot collision at wrap-around).
                pending_downs.insert(event.slot, event.timestamp_us);
            }
            0 => {
                // KeyUp — look up the matching KeyDown by slot.
                if let Some(down_us) = pending_downs.remove(&event.slot) {
                    if event.timestamp_us > down_us {
                        dwell_pairs.push((down_us, event.timestamp_us));
                    }
                }
                // Unpaired KeyUps (e.g. stray leading event) are silently dropped.
            }
            _ => {}
        }
    }

    let total_keystrokes = keydown_times.len() as i32;
    if total_keystrokes == 0 {
        buf.events.clear();
        buf.last_keydown = None;
        buf.session_wall_start_ms = None;
        buf.session_mono_start = None;
        return None;
    }

    // ── Session boundaries (FIX #5: monotonic for duration) ───────────────
    let started_at_ms = buf.session_wall_start_ms.unwrap_or(0);
    let duration_secs = buf
        .session_mono_start
        .map(|start| start.elapsed().as_secs() as i32)
        .unwrap_or(0);

    // ── Dwell times (FIX #4: slot-paired, FIX variance-precision: f64) ────
    let dwell_times_ms: Vec<f64> = dwell_pairs
        .iter()
        .map(|(down, up)| (up - down) as f64 / 1000.0)
        .collect();

    let avg_dwell_ms = if dwell_times_ms.is_empty() {
        0.0_f32
    } else {
        (dwell_times_ms.iter().sum::<f64>() / dwell_times_ms.len() as f64) as f32
    };

    // ── Flight time variance (FIX #5: monotonic deltas) ───────────────────
    // Flight = gap between consecutive KeyDown monotonic timestamps.
    let flight_times_ms: Vec<f64> = keydown_times
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]) as f64 / 1000.0)
        .collect();

    let flight_variance = variance_f64(&flight_times_ms) as f32;

    // ── Build session ──────────────────────────────────────────────────────
    let session = LocalSession {
        id: Uuid::new_v4().to_string(),
        started_at: started_at_ms,
        duration_secs,
        total_keystrokes,
        avg_dwell_ms,
        flight_variance,
        process_name: std::mem::take(&mut buf.process_name),
    };

    // Reset buffer.
    buf.events.clear();
    buf.last_keydown = None;
    buf.session_wall_start_ms = None;
    buf.session_mono_start = None;

    Some(session)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Monotonic microsecond counter.
/// Returns microseconds elapsed since an arbitrary fixed point (process start).
/// Used for all interval arithmetic — immune to NTP steps and clock adjustments.
pub fn monotonic_us() -> u64 {
    // Lazy static origin point so the values fit comfortably in u64 arithmetic.
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    origin.elapsed().as_micros() as u64
}

/// Population variance computed in f64 for precision.
fn variance_f64(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let sq_sum: f64 = values.iter().map(|v| (v - mean).powi(2)).sum();
    sq_sum / values.len() as f64
}
