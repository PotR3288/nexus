// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Config hot-reload machinery shared by xtminer and edgecase.
//!
//! Ported verbatim from xtminer's config_watch.rs (edgecase's copy already
//! says "Ported from xtminer's config_watch.rs"). A watcher thread polls the
//! config file's mtime (no inotify dependency); on change it debounces,
//! re-parses + validates, re-applies CLI overrides (a flag keeps winning over
//! the file for the whole run), logs what changed live vs restart-required,
//! and atomically swaps the new config into an [`ArcSwap`].
//!
//! The reload TIERING (which keys apply live vs need a restart) is
//! miner-specific knowledge — xtminer's is about RandomX VMs and pool
//! chains, edgecase's about solver devices and node endpoints. So this
//! module owns the loop/debounce/swap mechanics and the [`ReloadHooks`]
//! callback set, and each miner passes in its own describe/apply functions.

use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How often the watcher checks the config file's mtime.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// A reload younger than this is debounced (editors write files 2-3 times).
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Shared handle to the live configuration. Clone the `Arc` freely; reads
/// are wait-free.
pub type SharedConfig<C> = Arc<ArcSwap<C>>;

/// A config loader callback: parse a file path into a fresh `C`.
pub type Loader<C> = Arc<dyn Fn(&str) -> Result<C, LoadError> + Send + Sync>;
/// A describe callback: summarize old→new changes as human-readable text.
pub type DescribeFn<C> = Arc<dyn Fn(&C, &C) -> String + Send + Sync>;
/// An optional live-apply callback run after validation, before the swap.
pub type ApplyFn<C> = Arc<dyn Fn(&C, &C) + Send + Sync>;

/// Wrap an already-loaded config for sharing.
pub fn shared<C>(config: C) -> SharedConfig<C> {
    Arc::new(ArcSwap::from_pointee(config))
}

/// Result of loading configuration from a file. Kept here under a distinct
/// name because `crate::config::ConfigLoad` in each miner is the same
/// tri-state (see [`crate::config_util::ConfigLoad`]); the miners' own
/// `Config::load` implementations map onto it.
///
/// Miner-specific reload behavior, supplied as callbacks:
///
/// - `describe_live(old, new)` — human-readable summary of LIVE-applied
///   changes (logged at info when non-empty).
/// - `describe_restart(old, new)` — summary of restart-required changes
///   (logged at warn when non-empty).
/// - `on_applied(old, new)` — extra live-application work (xtminer: swap the
///   pool failover chain; edgecase: apply tracing log level / hashrate-only
///   flag). Runs after validation, before the swap.
pub struct ReloadHooks<C> {
    pub describe_live: DescribeFn<C>,
    pub describe_restart: DescribeFn<C>,
    pub on_applied: Option<ApplyFn<C>>,
}

impl<C> ReloadHooks<C> {
    pub fn new(describe_live: DescribeFn<C>, describe_restart: DescribeFn<C>) -> Self {
        Self {
            describe_live,
            describe_restart,
            on_applied: None,
        }
    }
}

/// State carried by the watcher thread.
struct Watcher<C> {
    path: String,
    shared: SharedConfig<C>,
    hooks: ReloadHooks<C>,
    /// Re-applied after every reload so an explicit CLI flag keeps outranking
    /// the file for the whole process lifetime.
    apply_overrides: Arc<dyn Fn(&mut C) + Send + Sync>,
    loader: Loader<C>,
    /// mtime of the last applied file (0 = never seen).
    last_mtime_ms: AtomicU64,
}

/// Why a reload did not produce a config.
#[derive(Debug, Clone)]
pub enum LoadError {
    /// The config file does not exist.
    NotFound,
    /// The file exists but could not be read or parsed.
    Parse(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotFound => write!(f, "config file not found"),
            LoadError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl<C> Watcher<C> {
    fn mtime_ms(path: &str) -> u64 {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Poll once: if the file's mtime changed and is stable, reload.
    fn poll_once(&self) -> bool {
        let mtime = Self::mtime_ms(&self.path);
        let last = self.last_mtime_ms.load(Ordering::Relaxed);
        if mtime == last || mtime == 0 {
            return false;
        }
        // Debounce: wait for the write to settle before re-reading.
        std::thread::sleep(DEBOUNCE);
        let settled = Self::mtime_ms(&self.path);
        if settled != mtime {
            return false; // still being written; catch it next poll
        }

        let applied = self.try_reload();
        if applied {
            self.last_mtime_ms.store(settled, Ordering::Relaxed);
        }
        applied
    }

    /// Re-parse, re-apply CLI overrides, log live vs restart-required changes,
    /// run the live-application hooks, and atomically swap into `shared`.
    /// A missing or unparseable file is logged and leaves the current config
    /// untouched.
    fn try_reload(&self) -> bool {
        match (self.loader)(&self.path) {
            Ok(new_cfg) => {
                apply_external_reload(
                    &self.shared,
                    new_cfg,
                    self.apply_overrides.as_ref(),
                    &self.hooks,
                    &format!("file '{}'", self.path),
                );
                true
            }
            Err(LoadError::NotFound) => {
                log::warn!(
                    "Config file {} vanished — keeping the current config",
                    self.path
                );
                false
            }
            Err(LoadError::Parse(e)) => {
                log::warn!("Invalid config change ignored ({e}) — keeping the current config");
                false
            }
        }
    }
}

/// Apply an EXTERNALLY produced config through the same pipeline a file
/// reload uses: re-apply CLI overrides (a flag keeps outranking any source
/// for the whole run), log live vs restart-required changes, run the
/// live-application hooks, and atomically swap into `shared`.
///
/// The CentCom poller calls this with its merged document so a
/// centrally pushed change behaves exactly like an operator editing the file
/// — same tiering, same side effects, same "restart required" warnings.
/// `source` labels the log lines (e.g. "CentCom").
pub fn apply_external_reload<C>(
    shared: &SharedConfig<C>,
    new_cfg: C,
    apply_overrides: &(dyn Fn(&mut C) + Send + Sync),
    hooks: &ReloadHooks<C>,
    source: &str,
) {
    let mut cfg = new_cfg;
    (apply_overrides)(&mut cfg);
    let old = shared.load();
    let applied = (hooks.describe_live)(old.as_ref(), &cfg);
    if !applied.is_empty() {
        log::info!("Config reloaded from {source} — applied: {applied}");
    }
    if let Some(on_applied) = &hooks.on_applied {
        on_applied(old.as_ref(), &cfg);
    }
    let restart = (hooks.describe_restart)(old.as_ref(), &cfg);
    if !restart.is_empty() {
        log::warn!("Config change from {source} requires RESTART to take effect: {restart}");
    }
    shared.store(Arc::new(cfg));
}

/// Spawn the config watcher thread. Runs until `shutdown` is set. Returns
/// None when there is no file to watch (config came from defaults or was
/// disabled).
///
/// - `path` — the config file to watch.
/// - `loader` — parse the file into a fresh `C` (`Err(LoadError::NotFound)`
///   when the file is missing; `Err(LoadError::Parse)` when unparseable).
///   Use [`crate::config_util::load_toml_config`] to get the tri-state
///   mapping both miners use.
/// - `apply_overrides` — re-apply CLI flags onto each freshly parsed config.
/// - `hooks` — miner-specific tiering + live-application callbacks.
pub fn spawn_watcher<C>(
    path: String,
    shared: SharedConfig<C>,
    loader: Loader<C>,
    apply_overrides: Arc<dyn Fn(&mut C) + Send + Sync>,
    hooks: ReloadHooks<C>,
    shutdown: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>>
where
    C: Send + Sync + 'static,
{
    if !std::path::Path::new(&path).exists() {
        log::info!("Config hot-reload disabled: '{path}' does not exist");
        return None;
    }
    let watcher: Watcher<C> = Watcher {
        last_mtime_ms: AtomicU64::new(Watcher::<C>::mtime_ms(&path)),
        path,
        shared,
        hooks,
        apply_overrides,
        loader,
    };
    log::info!(
        "Config hot-reload enabled: watching '{}' (poll {}s)",
        watcher.path,
        POLL_INTERVAL.as_secs()
    );
    Some(
        std::thread::Builder::new()
            .name("config-watch".to_string())
            .spawn(move || loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                watcher.poll_once();
                std::thread::sleep(POLL_INTERVAL);
            })
            .expect("failed to spawn config watcher thread"),
    )
}

/// True when two floats differ by more than epsilon (used by both miners'
/// describe functions for threshold-style fields).
pub fn f64_changed(a: f64, b: f64) -> bool {
    (a - b).abs() > f64::EPSILON
}

/// `{name}: {old} → {new}` when different (the shared diff-line format).
pub fn diff_line<T: PartialEq + std::fmt::Display>(
    old: T,
    new: T,
    name: &str,
    out: &mut Vec<String>,
) {
    if old != new {
        out.push(format!("{name}: {old} → {new}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    struct TestConfig {
        #[serde(default = "d_threads")]
        threads: u64,
        #[serde(default)]
        level: String,
    }
    fn d_threads() -> u64 {
        4
    }
    impl Default for TestConfig {
        fn default() -> Self {
            Self {
                threads: 4,
                level: "info".into(),
            }
        }
    }

    fn loader() -> Loader<TestConfig> {
        Arc::new(
            |p: &str| match crate::config_util::load_toml_config::<TestConfig>(p) {
                crate::config_util::ConfigLoad::Loaded(c) => Ok(c),
                crate::config_util::ConfigLoad::NotFound => Err(LoadError::NotFound),
                crate::config_util::ConfigLoad::Error(e) => Err(LoadError::Parse(e)),
            },
        )
    }

    fn hooks() -> ReloadHooks<TestConfig> {
        ReloadHooks::new(
            Arc::new(|_o, _n| String::new()),
            Arc::new(|_o, _n| String::new()),
        )
    }

    #[test]
    fn missing_file_disables_watcher() {
        let sh = shared(TestConfig::default());
        let shutdown = Arc::new(AtomicBool::new(true));
        let overrides: Arc<dyn Fn(&mut TestConfig) + Send + Sync> = Arc::new(|_| {});
        let w = spawn_watcher(
            "/nonexistent/config.toml".to_string(),
            Arc::clone(&sh),
            loader(),
            Arc::clone(&overrides),
            hooks(),
            shutdown,
        );
        assert!(w.is_none());
    }

    #[test]
    fn overrides_reapplied_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // File says 80, CLI pins 77 — the flag must win after reload.
        std::fs::write(&path, "threads = 80\n").unwrap();
        let sh = shared(TestConfig::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let overrides: Arc<dyn Fn(&mut TestConfig) + Send + Sync> = Arc::new(|c| c.threads = 77);
        let w = spawn_watcher(
            path.to_str().unwrap().to_string(),
            Arc::clone(&sh),
            loader(),
            Arc::clone(&overrides),
            hooks(),
            Arc::clone(&shutdown),
        )
        .unwrap();
        // Wait for the baseline poll (first observation records mtime without
        // reloading), then touch the file and wait for the reload cycle.
        std::thread::sleep(POLL_INTERVAL + DEBOUNCE + Duration::from_millis(300));
        std::fs::write(&path, "threads = 80\n").unwrap();
        std::thread::sleep(POLL_INTERVAL + DEBOUNCE + Duration::from_millis(300));
        shutdown.store(true, Ordering::Relaxed);
        w.join().unwrap();
        assert_eq!(sh.load().threads, 77, "CLI flag must outrank the file");
        drop(overrides);
    }

    #[test]
    fn diff_line_format() {
        let mut out = Vec::new();
        diff_line(10u64, 30, "report_secs", &mut out);
        diff_line(30u64, 30, "same", &mut out);
        assert_eq!(out, vec!["report_secs: 10 → 30".to_string()]);
    }
}
