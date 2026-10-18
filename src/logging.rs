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

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter,
};

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
    if code.is_empty() || unsafe { libc::isatty(libc::STDERR_FILENO) == 0 } {
        eprintln!("{line}");
    } else {
        eprintln!("\x1b[{code}m{line}\x1b[0m");
    }
}
