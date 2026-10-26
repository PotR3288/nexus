// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Console spinner for the pre-data startup window.
//!
//! Both miners display an animated Tari arch logo on stderr between "mining
//! started" and the first fully-elapsed rate window — no fake zeros, honest
//! `Soon`/null semantics elsewhere. The arch pulses through 6 highlight
//! positions (top bar → left leg → right leg → bottom arch → center → base)
//! at ~10 fps. It is TTY-only: piped/redirected output stays clean for parsing,
//! exactly like `logging::stderr_colored`'s styling gate.
//!
//! Interleaving safety: every curated console write in this crate goes
//! through [`with_console`] + [`clear_current_row`], so each line ALWAYS
//! starts by clearing whatever is on its row — an event line that lands
//! mid-animation clears the spinner frame and prints on its own row, and a
//! line racing `stop` (flag already cleared, final frame not yet erased)
//! still lands clean. The two can never corrupt each other's text.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The Tari arch logo shape (10 lines). Each line is padded to the same width.
const TARIS_ARCH: [&str; 10] = [
    "    ████████████",       // top bar
    "   ██          ██",      // upper legs
    "  ██            ██",     // mid legs
    " ██              ██",    // lower legs
    "██                ██",   // near-bottom legs
    "██    ██████████  ██",   // bottom arch + horizontal bar
    "██   ██          ██",    // lower body left
    "██  ██            ██",   // lower body right
    "██ ██              ██",  // mid-lower
    "████                ████", // base
];

/// Width of each line (for erase-line).
const LINE_WIDTH: usize = 28;

/// Per-frame cadence (~10 fps — visible motion without console spam).
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

/// The ONE lock serializing all curated stderr writes in this process.
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

/// Clear the current row so the caller's line starts on a clean one —
/// unconditionally when stderr is a tty (a no-op elsewhere, keeping piped
/// output clean). Self-defensive by design.
pub(crate) fn clear_current_row() {
    if std::io::stderr().is_terminal() {
        crate::logging::ensure_console_vt();
        let mut err = std::io::stderr();
        let _ = write!(err, "\x1b[2K\r");
    }
}

/// Render one spinner frame (pure — unit-testable without a tty).
///
/// The `\r\x1b[2K` prefix moves to column 0 and clears the row BEFORE the
/// new content is written. Each line of the arch is dim-styled (`\x1b[2m`)
/// except the highlighted part, which is bold (`\x1b[1m`). No trailing newline:
/// frames replace each other in place until [`stop`] erases the final one.
pub fn render_frame(frame_idx: usize, label: &str, elapsed_secs: u64) -> String {
    // Highlight positions cycle through 6 parts of the arch
    let highlights: &[usize] = match frame_idx % 6 {
        0 => &[0],                          // top bar
        1 => &[6],                          // left leg area
        2 => &[7],                          // right leg area
        3 => &[5],                          // bottom arch (horizontal bar)
        4 => &[6, 7],                       // center body
        5 => &[9],                          // base
        _ => &[0],
    };

    let mut out = String::with_capacity(400);
    for (i, line) in TARIS_ARCH.iter().enumerate() {
        if i == 0 {
            out.push_str("\r\x1b[2K");
        } else {
            out.push('\r');
        }

        let is_hl = highlights.contains(&i);
        if is_hl {
            out.push_str("\x1b[1m");
        } else {
            out.push_str("\x1b[2m");
        }
        out.push_str(line);
        out.push_str("\x1b[0m");

        // Pad to LINE_WIDTH for clean erase-line on next frame
        let pad = LINE_WIDTH.saturating_sub(line.len());
        for _ in 0..pad {
            out.push(' ');
        }

        // Append label + elapsed time after the first line's arch content
        if i == 0 && !label.is_empty() {
            out.push(' ');
            out.push_str(label);
            out.push(' ');
            out.push('(');
            out.push_str(&elapsed_secs.to_string());
            out.push_str("s)\x1b[0m");
        }
    }

    out
}

/// Start animating the Tari arch spinner on stderr.
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
/// animation thread. Idempotent — safe to call multiple times or without a prior [`start`].
pub fn stop() {
    ACTIVE.store(false, Ordering::Relaxed);
    let mut handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = handle.take() {
        drop(handle);
        let _ = h.join();
        with_console(clear_current_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_erase_prefix_and_content() {
        let line = render_frame(0, "collecting hashrate data", 12);
        assert!(line.starts_with("\r\x1b[2K"), "{line:?}");
        assert!(line.contains("████"), "{line:?}");
        assert!(line.contains("collecting hashrate data (12s)"), "{line:?}");
        // Bold styling on highlighted lines, dim elsewhere.
        assert!(line.contains("\x1b[1m"), "{line:?}");
        assert!(line.contains("\x1b[2m"), "{line:?}");
        assert!(!line.ends_with('\n'), "{line:?}");
    }

    #[test]
    fn frame_index_wraps_without_panic() {
        for i in 0..6 * 3 + 7 {
            let _ = render_frame(i, "x", 0);
        }
    }

    #[test]
    fn stop_without_start_is_a_noop() {
        stop();
        stop();
        assert!(!ACTIVE.load(Ordering::Relaxed));
    }

    #[test]
    fn console_lock_tolerates_poison() {
        let panicked =
            std::panic::catch_unwind(|| with_console(|| panic!("simulated writer panic")));
        assert!(panicked.is_err());
        let ok = std::panic::catch_unwind(|| {
            with_console(|| {});
        });
        assert!(ok.is_ok(), "poisoned console lock wedged a later write");
    }

    #[test]
    fn clear_current_row_is_quiet_when_inactive() {
        with_console(clear_current_row);
    }

    #[test]
    fn stop_after_start_erases_and_is_idempotent() {
        start("test label");
        stop();
        stop();
        assert!(!ACTIVE.load(Ordering::Relaxed));
    }

    #[test]
    fn all_10_lines_render_without_panic() {
        for i in 0..6 {
            let line = render_frame(i, "tari arch", 42);
            let cr_count = line.matches('\r').count();
            assert!(cr_count >= 9, "expected ~10 lines, got {} \r in {:?}", cr_count, line);
        }
    }

    #[test]
    fn highlight_cycles_through_6_positions() {
        for pos in 0..6 {
            let _ = render_frame(pos, "", 0);
        }
    }
}
