/// models.rs — Shared data types for the telemetry pipeline.
///
/// PRIVACY CONTRACT:
/// Neither `RawEvent` nor `LocalSession` stores any character, key name,
/// key code, or any value that could identify what was typed. The only
/// information retained is *when* events happened and *how long* keys
/// were held — purely mechanical timing signals.
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// In-memory ring buffer entry (never persisted, never transmitted)
// ─────────────────────────────────────────────────────────────────────────────

/// A single anonymous keyboard timing event.
///
/// Fields:
///   `timestamp_us` — monotonic microseconds since an arbitrary epoch.
///                    Used only for computing deltas (dwell, flight).
///                    A separate wall-clock value is recorded once per session
///                    for the `started_at` field.
///   `state`        — 1 = KeyDown, 0 = KeyUp.
///   `slot`         — wrapping counter incremented on each KeyDown. The next
///                    KeyUp reuses that slot value so aggregate_and_clear can
///                    pair presses with releases without storing any key
///                    identity. This is exact for sequential typing and a
///                    best-effort approximation under key rollover (overlapping
///                    presses). Counter wraps at 255; collisions at that depth
///                    drop the affected dwell pair harmlessly.
///
/// The physical key identity is stripped at the OS callback level before
/// this struct is ever populated. No vkCode, no keychar, no scancode survives.
#[derive(Debug, Clone, Copy)]
pub struct RawEvent {
    /// Monotonic microseconds at the moment the OS event fired.
    pub timestamp_us: u64,
    /// 1 for KeyDown, 0 for KeyUp.
    pub state: u8,
    /// Slot index for exact KeyDown↔KeyUp pairing without key identity.
    pub slot: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregated session block (written to SQLite, synced to backend)
// ─────────────────────────────────────────────────────────────────────────────

/// A compacted summary of one continuous typing session.
///
/// Created when the 20-second inactivity window triggers a flush.
/// Matches the `LocalSession` struct in the technical specification exactly.
///
/// This is the unit of data that leaves the native layer. It contains
/// zero recoverable information about *what* was typed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSession {
    /// Client-generated UUIDv4. Used as the idempotency key during backend sync.
    /// The server's `ON CONFLICT DO NOTHING` guard targets this field.
    pub id: String,

    /// Unix timestamp in milliseconds when the first keystroke of this session fired.
    /// Derived from the system wall clock captured once at session open — not from
    /// the monotonic timer used for interval arithmetic.
    pub started_at: i64,

    /// Total wall-clock duration of active typing in seconds.
    pub duration_secs: i32,

    /// Count of valid KeyDown events captured in this session.
    pub total_keystrokes: i32,

    /// Mean dwell time across all key presses in this session, in milliseconds.
    /// Dwell = elapsed monotonic time between a KeyDown and its paired KeyUp,
    /// matched by slot index. Exact for sequential typing; a best-effort
    /// approximation under key rollover (overlapping presses). Treat as an
    /// approximate signal, not a precise per-key measurement.
    pub avg_dwell_ms: f32,

    /// Statistical variance of inter-key flight times in milliseconds.
    /// Flight = monotonic gap between consecutive KeyDown events.
    /// High variance = natural human rhythm. Near-zero = potential script signal.
    pub flight_variance: f32,

    /// Name of the foreground process during this session (e.g. "Code", "Slack").
    /// Captured once when the session opens (first KeyDown after a flush) — never
    /// per-keystroke.
    pub process_name: String,
}
