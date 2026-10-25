// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Console spinner for the pre-data startup window.
//!
//! Both miners go quiet on the console between "mining started" and the
//! first fully-elapsed rate window (no short-slice rates, no fake zeros —
//! honest `Soon`/null semantics elsewhere). That gap is covered by an
//! animated spinner line on stderr:
//!
//!   ⠹ collecting hashrate data (12s)
//!
//! The spinner animates in place (`\r` + erase-line, no trailing newline)
//! and retires permanently the moment real rate data exists — the miner's
//! monitor thread calls [`stop`] on its first tick that carries a real
//! value. It is TTY-only: piped/redirected output stays clean for parsing,
//! exactly like `logging::stderr_colored`'s styling gate.
//!
//! Interleaving safety: every curated console write in this crate goes
//! through [`with_console`] + [`erase_if_active`], so an event line that
//! lands mid-animation first clears the spinner frame and prints on its own
//! row; the next frame then re-establishes the spinner one row below. The
//! two can never corrupt each other's text.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Braille spinner frames (one full cycle).
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Per-frame cadence (~11 fps — visible motion without console spam).
const FRAME_INTERVAL: Duration = Duration::from_millis(90);

/// The ONE lock serializing all curated stderr writes in this process.
/// Spinner frames and `logging`'s line writers both take it for the duration
/// of a single write, so no two writers can interleave mid-line. Poison is
/// tolerated (a panic inside an eprintln must not wedge every later console
/// line — the display lock guards formatting, not data).
static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

/// True while the animation thread is running.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The animation thread's handle (taken by [`stop`] to join it exactly once).
static HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

fn lock_console() -> MutexGuard<'static, ()> {
    CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run one atomic console write under the shared lock.
pub(crate) fn with_console<F: FnOnce()>(f: F) {
    let _guard = lock_console();
    f();
}

/// Erase an in-flight spinner frame so the caller's line starts on a clean
/// row. No-op when no spinner is active or stderr is not a tty (the spinner
/// can only be active in that case). MUST be called inside [`with_console`].
pub(crate) fn erase_if_active() {
    if ACTIVE.load(Ordering::Relaxed) && std::io::stderr().is_terminal() {
        let mut err = std::io::stderr();
        // Stderr is unbuffered — the bytes land immediately.
        let _ = write!(err, "\x1b[2K\r");
    }
}

/// Render one spinner frame (pure — unit-testable without a tty).
///
/// The `\r\x1b[2K` prefix moves to column 0 and clears the row BEFORE the
/// new content is written; dim styling keeps it visually subordinate to the
/// event lines that share the console. No trailing newline: frames replace
/// each other in place until [`stop`] erases the final one.
pub fn render_frame(frame_idx: usize, label: &str, elapsed_secs: u64) -> String {
    format!(
        "\r\x1b[2K\x1b[2m{} {} ({}s)\x1b[0m",
        FRAMES[frame_idx % FRAMES.len()],
        label,
        elapsed_secs
    )
}

/// Start animating a spinner on stderr.
///
/// No-op when stderr is not a tty (piped/redirected output stays clean) or
/// when one is already running. The animation thread lives until [`stop`];
/// the process exiting first kills it without harm (a hard kill loses only
/// the dangling frame, which no later line can follow).
pub fn start(label: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let mut handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if handle.is_some() || ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    ACTIVE.store(true, Ordering::Relaxed);
    let label = label.to_string();
    let h = thread::Builder::new()
        .name("console-spinner".to_string())
        .spawn(move || spinner_loop(&label))
        .expect("failed to spawn console spinner thread");
    *handle = Some(h);
}

fn spinner_loop(label: &str) {
    // Windows consoles allocated by Explorer start with VT processing OFF —
    // enable it before the first escape byte (same mechanism as logging's
    // colored lines; no-op off-Windows).
    crate::logging::ensure_console_vt();
    let started = Instant::now();
    let mut frame = 0usize;
    while ACTIVE.load(Ordering::Relaxed) {
        with_console(|| {
            let line = render_frame(frame, label, started.elapsed().as_secs());
            let mut err = std::io::stderr();
            let _ = err.write_all(line.as_bytes());
        });
        frame += 1;
        thread::sleep(FRAME_INTERVAL);
    }
}

/// Stop the spinner: retire it permanently, erase its line, and join the
/// animation thread (so no late frame can land after a subsequent console
/// line). Idempotent — safe to call multiple times or without a prior
/// [`start`].
pub fn stop() {
    ACTIVE.store(false, Ordering::Relaxed);
    let mut handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = handle.take() {
        // Join WITHOUT holding CONSOLE_LOCK: the thread's final frame write
        // takes it, and holding both would invert nothing but is needless.
        drop(handle);
        let _ = h.join();
    }
    with_console(erase_if_active);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_erase_prefix_and_content() {
        let line = render_frame(0, "collecting hashrate data", 12);
        assert!(line.starts_with("\r\x1b[2K"), "{line:?}");
        assert!(line.contains("⠋"), "{line:?}");
        assert!(line.contains("collecting hashrate data (12s)"), "{line:?}");
        // Dim styling, reset at the end, no trailing newline.
        assert!(line.contains("\x1b[2m"), "{line:?}");
        assert!(line.ends_with("\x1b[0m"), "{line:?}");
        assert!(!line.ends_with('\n'), "{line:?}");
    }

    #[test]
    fn frame_index_wraps_without_panic() {
        for i in 0..FRAMES.len() * 3 + 7 {
            let line = render_frame(i, "x", 0);
            assert!(line.contains(FRAMES[i % FRAMES.len()]), "{line:?}");
        }
    }

    #[test]
    fn stop_without_start_is_a_noop() {
        // Must not panic (no handle to join) and must stay stopped.
        stop();
        stop();
        assert!(!ACTIVE.load(Ordering::Relaxed));
    }

    #[test]
    fn console_lock_tolerates_poison() {
        // A panic inside a console write must not wedge later writes — the
        // lock is re-acquired via into_inner.
        let panicked =
            std::panic::catch_unwind(|| with_console(|| panic!("simulated writer panic")));
        assert!(
            panicked.is_err(),
            "the simulated panic should have propagated"
        );
        let ok = std::panic::catch_unwind(|| {
            with_console(|| {});
        });
        assert!(ok.is_ok(), "poisoned console lock wedged a later write");
    }

    #[test]
    fn erase_if_active_is_quiet_when_inactive() {
        // No spinner running: the hook must not emit any bytes (it would
        // corrupt clean piped output). Not observable without capturing
        // stderr, but it must at least be side-effect-free and non-panicking.
        with_console(erase_if_active);
    }
}
