//! Shared tracing subscriber setup for the miners.
//!
//! One registry, two fmt layers: a stdout layer and an ansi-free file layer
//! writing `logs/miner.log`, both governed by a single reloadable
//! `EnvFilter` — one global level controls both sinks, and a live level
//! change (config hot-reload, hashrate-only toggle) thins/restores both at
//! once. `GET /log?lines=N` tails the same file, so the path is API surface:
//! keep it stable.
//!
//! This crate owns the subscriber *wiring* only. Individual apps keep their
//! own filter-construction policy (RUST_LOG handling, verbose flags,
//! hashrate-only overrides) and pass the initial filter in. `tracing-log`'s
//! bridge (installed by `.init()`) still forwards `log::` events from this
//! crate into the app's subscriber — nexus itself must never install a
//! subscriber, only expose this entry point for apps to call once from main.

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

/// Install the shared subscriber: a stdout fmt layer plus an ansi-free file
/// layer writing `logs/miner.log`, both behind one reloadable `EnvFilter`
/// seeded with `initial_filter`. Returns the reload handle that drives live
/// level changes from the config watcher.
///
/// The non-blocking file worker must live for the process lifetime; its
/// guard is deliberately leaked so it survives main's stack unwinding (a
/// hard kill loses only the buffered tail of the last second).
pub fn init_stdout_and_file(
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
        // stdout: reloadable filter (live level / hashrate-only changes).
        .with(tracing_subscriber::fmt::layer())
        // file: same reloadable filter — one global level governs both sinks;
        // ansi off so the file never carries escape codes from styled messages.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();

    filter_handle
}
