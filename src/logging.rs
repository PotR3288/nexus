//! Shared tracing subscriber setup for the miners.
//!
//! One registry, one file fmt layer writing `logs/miner.log`, governed by a
//! reloadable `EnvFilter`. The console is NOT a tracing sink: verbose log
//! lines live in the file only, and the console shows curated `eprintln!`
//! lines from the apps (`stderr_colored` below) — plain for ordinary output,
//! colored for important events (block/share submission, rejection, lost
//! connection). `GET /log?lines=N` tails the file, so the path is API
//! surface: keep it stable.
//!
//! This crate owns the subscriber *wiring* only. Individual apps keep their
//! own filter-construction policy (RUST_LOG handling, verbose flags,
//! hashrate-only overrides) and pass the initial filter in. nexus itself must
//! never install a subscriber, only expose these entry points for apps to
//! call once from main.

use std::io::IsTerminal;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};

/// `logs/miner.log` next to the process cwd (the cwd holding config.toml).
/// The `GET /log?lines=N` endpoint tails this file — keep the path stable.
pub fn miner_log_dir() -> PathBuf {
    std::env::current_dir()
        .map(|dir| dir.join("logs"))
        .unwrap_or_else(|_| PathBuf::from("logs"))
}

/// Install the shared subscriber: an ansi-free file layer writing
/// `logs/miner.log`, behind one reloadable `EnvFilter` seeded with
/// `initial_filter`. Returns the reload handle that drives live level
/// changes from the config watcher (the file thins/restores with it; the
/// console never carries tracing lines).
///
/// The non-blocking file worker must live for the process lifetime; its
/// guard is deliberately leaked so it survives main's stack unwinding (a
/// hard kill loses only the buffered tail of the last second).
pub fn init_file_only(
    initial_filter: EnvFilter,
) -> reload::Handle<EnvFilter, tracing_subscriber::Registry> {
    let log_dir = miner_log_dir();
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::never(&log_dir, "miner.log");
    let (file_writer, _file_guard): (_, WorkerGuard) =
        tracing_appender::non_blocking(file_appender);
    // Leak the guard: the worker thread outlives main's stack frame.
    std::mem::forget(_file_guard);

    // reload::Layer::new returns (layer, handle): the layer goes into the
    // subscriber; the handle drives live level changes from the watcher.
    let (filter_layer, filter_handle) = reload::Layer::new(initial_filter);

    tracing_subscriber::registry()
        .with(filter_layer)
        // file only: ansi off so the file never carries escape codes.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();

    filter_handle
}

/// Print a curated console line to stderr: styled when stderr is a tty,
/// plain otherwise (so piped/redirected output stays clean for parsing).
///
/// This is the console path for important extra events — block/share
/// submission, acceptance/rejection, lost connection — and for the app's
/// ordinary curated output (start banner, periodic hashrate line). Log
/// lines themselves never reach the console; they live in the file.
///
/// `code` is an SGR parameter string, e.g. "1;32" for bold green,
/// "1;31" for bold red, "2" for dim, "1" for bold. Pass "" for plain.
pub fn stderr_colored(line: &str, code: &str) {
    // std::io::IsTerminal is the portable tty check (isatty(2) on unix,
    // GetConsoleMode on windows) — libc's STDERR_FILENO does not exist on
    // Windows.
    if code.is_empty() || !std::io::stderr().is_terminal() {
        eprintln!("{line}");
    } else {
        #[cfg(windows)]
        win_vt::enable();
        eprintln!("\x1b[{code}m{line}\x1b[0m");
    }
}

/// Windows console VT processing, enabled lazily on first colored line.
///
/// A console allocated by Explorer (double-clicking the .exe) starts with
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` OFF, so raw SGR escape bytes render
/// as garbage characters; a pre-existing terminal (cmd, PowerShell) already
/// has it ON, which is why the same binary looks fine from a shell. Enabling
/// the mode on both std handles makes double-clicked launches format
/// identically to command-line ones. kernel32 is linked by default on all
/// Windows targets — no extra dependency.
#[cfg(windows)]
mod win_vt {
    use std::sync::Once;

    const STD_OUTPUT_HANDLE: i32 = -11;
    const STD_ERROR_HANDLE: i32 = -12;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    static ENABLED: Once = Once::new();

    pub fn enable() {
        ENABLED.call_once(|| unsafe {
            for n in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let h = GetStdHandle(n);
                // NULL (no console) or INVALID_HANDLE_VALUE (-1): skip.
                if h == 0 || h == -1 {
                    continue;
                }
                let mut mode: u32 = 0;
                // Fails when the handle is not a real console (redirected);
                // nothing to enable there.
                if GetConsoleMode(h as *mut std::ffi::c_void, &mut mode) != 0 {
                    SetConsoleMode(
                        h as *mut std::ffi::c_void,
                        mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    );
                }
            }
        });
    }

    extern "system" {
        fn GetStdHandle(nStdHandle: i32) -> isize;
        fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
    }
}
