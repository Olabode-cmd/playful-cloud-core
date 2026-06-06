# playful-cloud-core

The native keyboard telemetry engine for [Playful Cloud](https://github.com/playful-cloud). This repository is intentionally public — its entire purpose is to be auditable.

Playful Cloud is a gamified typing productivity platform. It rewards consistent daily typing volume with virtual coins, tracks personal WPM baselines, and powers competitive typing events. For any of that to be trustworthy, users need to be able to verify for themselves that the background capture layer never records what they typed. This codebase is that proof.

---

## What this crate is

`playful-cloud-core` is a [Tauri v2](https://tauri.app) plugin written in pure Rust. It installs a system-wide keyboard hook, captures anonymous timing data, and packages it into statistical session summaries. It is consumed as a library dependency by `playful-cloud-desktop` (the private Tauri shell repo) and exposes three commands to that app's frontend:

| Command | What it does |
|---|---|
| `start_hook` | Installs the OS-level keyboard hook and starts the inactivity watchdog |
| `stop_hook` | Uninstalls the hook and signals the watchdog to exit |
| `get_session` | Non-blocking drain of completed `LocalSession` structs ready for SQLite |

---

## Repository structure

```
src/
├── lib.rs              Plugin entry point. Registers Tauri commands, manages
│                       plugin state (hook handle, session channel, event buffer).
│
├── models.rs           The two data types that flow through the pipeline:
│                         RawEvent     — a single anonymous timing event (never persisted)
│                         LocalSession — an aggregated session block (persisted + synced)
│
├── session.rs          In-memory ring buffer and session lifecycle.
│                       Owns the 20-second inactivity watchdog thread that
│                       aggregates RawEvents into a LocalSession on timeout.
│
└── hook/
    ├── mod.rs          Unified start_hook / stop_hook interface. Platform
    │                   selection is compile-time via #[cfg(target_os)].
    ├── windows.rs      SetWindowsHookEx(WH_KEYBOARD_LL) implementation.
    │                   Runs on a dedicated message-pump thread.
    └── macos.rs        CGEventTap (Quartz Event Services) implementation.
                        Runs on a dedicated CFRunLoop thread.
```

---

## What gets captured — and what does not

### The only data that ever leaves a hook callback

Both platform implementations reduce every keyboard event to three numbers before anything else runs:

```rust
pub struct RawEvent {
    pub timestamp_us: u64,  // monotonic microseconds — WHEN the event happened
    pub state: u8,          // 1 = KeyDown, 0 = KeyUp — DIRECTION only
    pub slot: u8,           // pairing counter — lets us match a KeyDown to its KeyUp
}
```

That is the complete in-memory record of a keypress. No character. No key name. No virtual key code. No scan code.

### What a session summary looks like

After 20 seconds of inactivity, the ring buffer is drained and compressed into:

```rust
pub struct LocalSession {
    pub id: String,              // client-generated UUIDv4 (idempotency key for sync)
    pub started_at: i64,         // Unix ms — when the session opened
    pub duration_secs: i32,      // how long the typing window lasted
    pub total_keystrokes: i32,   // count of KeyDown events
    pub avg_dwell_ms: f32,       // mean key-hold duration in ms
    pub flight_variance: f32,    // variance of inter-key gaps — human rhythm signal
    pub process_name: String,    // foreground app at session open (see below)
}
```

`total_keystrokes` is a count. `avg_dwell_ms` and `flight_variance` are statistical aggregates of timing intervals. None of these fields can be reversed into the text that was typed.

---

## Proof: key identity is never read

The claim is not just that key values are not stored — they are not even read in the first place. Here is the evidence from both platform implementations.

### Windows — `src/hook/windows.rs`

The Windows callback receives a `KBDLLHOOKSTRUCT` pointer from the OS. That struct contains `vkCode` (the virtual key), `scanCode`, and `flags`. Here is every field access in the entire callback:

```rust
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        // wParam tells us direction — no key identity.
        let state: u8 = match wparam.0 as u32 {
            w if w == WM_KEYDOWN || w == WM_SYSKEYDOWN => 1,
            w if w == WM_KEYUP   || w == WM_SYSKEYUP   => 0,
            _ => { return CallNextHookEx(...); }
        };

        // kb.flags bit 4 = LLKHF_INJECTED (synthetic event filter).
        // This is the ONLY field read from kb. vkCode is never touched.
        if (kb.flags.0 & 0x10) != 0 {
            return CallNextHookEx(...);
        }

        let timestamp_us = monotonic_us();  // wall-clock-independent timer
        let slot = /* wrapping u8 counter, no key info */;

        let event = RawEvent { timestamp_us, state, slot };
        // ^ three numbers. nothing else.
    }
}
```

`kb.vkCode` — the field that identifies which key was pressed — is **never referenced**. The only struct field accessed is `kb.flags`, and only bit 4 of that is tested to filter software-injected keystrokes.

### macOS — `src/hook/macos.rs`

The macOS callback receives a `CGEvent` object. Extracting a keycode requires an explicit call to `event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)`. That call does not appear anywhere in this codebase. The comment in the code makes this explicit:

```rust
fn handle_event(event_type: CGEventType, event: &CGEvent) {
    // Filter injected events by source state — hardware input only.
    let source_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
    if source_state != 1 { return; }

    // Direction only — CGEventType::KeyDown or KeyUp.
    let state: u8 = match event_type {
        CGEventType::KeyDown => 1,
        CGEventType::KeyUp   => 0,
        _ => return,
    };

    // ── KEY IDENTITY IS NEVER EXTRACTED ──────────────────────────────────
    // EventField::KEYBOARD_EVENT_KEYCODE is never called.
    // ─────────────────────────────────────────────────────────────────────

    let timestamp_us = monotonic_us();
    let slot = /* same wrapping counter as Windows */;

    let raw = RawEvent { timestamp_us, state, slot };
}
```

The `EVENT_SOURCE_STATE_ID` field is read to filter synthetic events — the equivalent of the Windows injected-flag check. The keycode field is not accessed at any point.

### The data type enforces the contract at compile time

Because `RawEvent` has no field capable of holding a character or key identifier, there is no way for a future code change to accidentally start storing key identity without adding a new field to the struct — which would be immediately visible in a diff.

---

## Process name capture

Each `LocalSession` includes a `process_name` field (e.g. `"Code"`, `"Slack"`, `"chrome"`). This is the name of the foreground application at the moment the session opened.

**Why it's captured:** Playful Cloud's dashboard breaks down your typing volume by application context. Knowing that 40,000 keystrokes happened in your editor versus a browser is useful for understanding your own workflow, and it powers the per-app breakdown in the local analytics view.

**How it's captured:** The process name is resolved once — at the start of a new session — not on every keystroke. On Windows this calls `GetForegroundWindow` → `QueryFullProcessImageNameW`. On macOS it spawns a single `osascript` query. The result is stored as a plain string in `LocalSession.process_name`. The process name is never used to make inferences about what was typed — a session from `"Code"` contains the same anonymous timing arrays as one from any other app.

**What is not captured:** No window title. No document name. No URL. No file path. Just the process binary stem — the same name you would see in Task Manager or Activity Monitor.

---

## Session lifecycle

```
OS keyboard event
       │
       ▼
hook_proc / handle_event          ← platform callback, runs on dedicated thread
       │
       │  strips all key identity here
       │  emits RawEvent { timestamp_us, state, slot }
       ▼
SessionBuffer::push()             ← appends to in-memory Vec<RawEvent>
       │
       │  on first KeyDown: captures wall-clock start + process name (once)
       │  on every event:   updates last_keydown Instant
       ▼
inactivity watchdog               ← polls every 1 second on its own thread
       │
       │  when last_keydown.elapsed() >= 20s:
       │    aggregate_and_clear() computes statistics from the buffer
       │    emits LocalSession { id, started_at, duration_secs,
       │                         total_keystrokes, avg_dwell_ms,
       │                         flight_variance, process_name }
       │    buffer is cleared
       ▼
mpsc::SyncSender<LocalSession>    ← bounded channel (256 slots)
       │
       ▼
get_session Tauri command         ← frontend drains on poll / event
       │
       ▼
playful-cloud-desktop             ← persists to SQLite, syncs to backend
```

---

## Platform requirements

| Platform | Mechanism | Permission required |
|---|---|---|
| Windows | `SetWindowsHookEx(WH_KEYBOARD_LL)` | None (user-space, no elevation needed) |
| macOS | `CGEventTap` | Accessibility (System Preferences → Privacy & Security → Accessibility) |

On macOS, if Accessibility permission has not been granted, `start_hook` returns an error and the Tauri shell emits a `"pc://hook-error"` event so the UI can guide the user to the correct settings pane.

---

## Building

This crate is not a standalone binary. Add it as a path dependency in the `playful-cloud-desktop` `src-tauri/Cargo.toml`:

```toml
[dependencies]
playful-cloud-core = { path = "../../playful-cloud-core" }
```

Then register the plugin in the Tauri builder:

```rust
tauri::Builder::default()
    .plugin(playful_cloud_core::init())
    .run(tauri::generate_context!())
    .expect("error running tauri application");
```

To check the crate in isolation:

```bash
cargo check
```

---

## License

MIT
