// Copyright (c) 2026 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

//! Multi-work-source failover across an ordered `[[pool]]` chain.
//!
//! This is the generalized form of xtminer's `pool_failover.rs`: one manager
//! tracks every entry in the config's enabled-pool chain and fails over when
//! the current entry stops serving work — within a work-source type AND
//! across types (a node chain can fall through to a stratum pool and back).
//! The manager is protocol-agnostic: it only knows entries, indices, and
//! success/failure reports. Building a connection (gRPC channel, HTTP client
//! or stratum socket) from a [`Selection`] is the driver's job — which is
//! what lets one chain mix work sources.
//!
//! Semantics (identical to xtminer's, verified by the same test suite):
//!
//! - **3-strike connection failure** ([`FAIL_THRESHOLD`]) fails over to the
//!   next enabled entry; a failed entry is retried after [`RETRY_AFTER`] so
//!   the miner can fail BACK to higher-priority entries once they recover.
//! - **Login rejection is one strike** ([`PoolManager::fail_immediately`]):
//!   retrying the same entry cannot help (bad wallet).
//! - **Share rejections** arrive over an ALIVE connection (the source still
//!   serves jobs), so they never trip the work-failure threshold — but a
//!   source whose validation backend is broken earns nothing. After
//!   [`REJECT_THRESHOLD`] consecutive rejections the manager fails over with
//!   the longer [`REJECT_COOLDOWN`]; only an ACCEPTED share
//!   ([`PoolManager::clear_share_rejections`]) clears the streak.
//! - **Verified entries**: an entry is *verified* once work on it succeeded.
//!   The fact is keyed by identity so it survives config hot-reloads.
//!   Display surfaces report a verified+connected entry only, and a failure
//!   on a NEVER-verified entry while a previously working one exists reverts
//!   selection at once (one strike) — this is what makes a live pool edit
//!   safe.
//! - **Live chain edits**: [`PoolManager::replace`] applies a reloaded list
//!   without a restart; verified history carries over by identity for
//!   entries whose credentials did not change.
//!
//! Entries are described by the [`WorkSourceEntry`] trait — each miner
//! implements it for its own config struct (xtminer's carries
//! algorithm/nonce_width keys edgecase does not have; edgecase's carries its
//! own defaults). [`WorkSource`] is the union of both miners' vocabularies:
//! `node` (edgecase gRPC base node), `proxy` (xtminer's xmrig_proxy), and
//! `stratum`. The manager never branches on the kind — only drivers do.

use log::{error, info, warn};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where mining work comes from — the union of both miners' vocabularies.
///
/// The manager treats every variant identically (it is protocol-agnostic);
/// the kind exists for driver dispatch and operator-facing logs. Each miner
/// maps its config spelling 1:1: edgecase uses `node` (direct gRPC base
/// node) and `stratum`; xtminer uses `proxy` (xmrig_proxy) and `stratum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkSource {
    /// Direct daemon work source, edgecase vocabulary: gRPC to a Tari base
    /// node (solo mining).
    Node,
    /// Direct daemon work source, xtminer vocabulary: an xmrig_proxy HTTP
    /// endpoint in front of a Tari base node.
    Proxy,
    /// A stratum pool (`host:port`) pushing work.
    Stratum,
}

impl WorkSource {
    /// Parse a config string. Accepts both miners' spellings plus the shared
    /// one; an empty string means the caller's default (pass it in).
    pub fn parse(s: &str, default: WorkSource) -> Option<WorkSource> {
        match s.trim().to_lowercase().as_str() {
            "node" => Some(WorkSource::Node),
            "proxy" => Some(WorkSource::Proxy),
            "stratum" => Some(WorkSource::Stratum),
            "" => Some(default),
            _ => None,
        }
    }
}

impl std::fmt::Display for WorkSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WorkSource::Node => "node",
            WorkSource::Proxy => "proxy",
            WorkSource::Stratum => "stratum",
        })
    }
}

/// One entry of a miner's `[[pool]]` chain, as the manager sees it.
///
/// Implement this for the miner's own config struct. `identity` and
/// `reload_key` separate two different comparisons:
///
/// - `identity` (URL + login keys) is what the selection watcher compares:
///   an edit to the CURRENTLY CONNECTED entry's login fields must trigger a
///   re-login even when the URL is unchanged.
/// - `reload_key` (everything except `enabled`) is what config hot-reload
///   compares: entries equal under it keep their verified history across a
///   reload. The `enabled` flag is excluded — standby surgery (disable +
///   re-enable) is not a credential change.
pub trait WorkSourceEntry: Clone + Send + Sync + 'static {
    /// Endpoint identifier: URL (daemon work sources) or `host:port`
    /// (stratum). The failover machinery keys cooldown sets on it.
    fn url(&self) -> &str;
    /// Which driver this entry selects.
    fn work_source(&self) -> WorkSource;
    /// Login-relevant identity: URL plus every field that changes what we
    /// log in with. Two entries with equal identities are the same pool for
    /// selection-watching purposes.
    fn identity(&self) -> String;
    /// Full fingerprint modulo `enabled` — the hot-reload carryover key.
    fn reload_key(&self) -> String;
    /// Standby flag: `false` entries keep their index but are never selected.
    fn enabled(&self) -> bool;
}

/// Log clean text to logfile AND print bold-red on the console. Failover and
/// pool-outage decisions are operator-facing incidents — they must be visible
/// in a live screen/SSH session, not only in the log file. Colors emit
/// unconditionally unless NO_COLOR is set (miners typically run under
/// screen/systemd where stderr is not a tty but output is still watched).
pub fn console_error(line: &str) {
    error!("{}", line);
    static NO_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some()) {
        eprintln!("\x1b[1;31m{}\x1b[0m", line);
    }
}

/// Consecutive work failures before failing over to the next entry.
pub const FAIL_THRESHOLD: u32 = 3;
/// How long a failed entry is skipped before it may be probed again.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Extended cooldown for an entry that was REVERTED from (never worked, while
/// a previously working entry exists). Longer than [`RETRY_AFTER`] so the
/// miner does not ping-pong dead→revert→dead every minute; still finite so a
/// recovered entry is picked up eventually. A config reload resets it at once.
pub const REVERT_COOLDOWN: Duration = Duration::from_secs(300);
/// Consecutive share/block rejections before failing over to the next entry.
/// Rejections arrive over an ALIVE connection (the source still serves jobs),
/// so they never trip [`FAIL_THRESHOLD`] — but a source whose validation
/// backend is broken earns nothing for the miner, and every rejected share is
/// lost work. Higher than FAIL_THRESHOLD: sporadic rejections (a stale job
/// after a reorg) are normal; only a sustained streak means the source cannot
/// validate anything right now.
pub const REJECT_THRESHOLD: u32 = 5;
/// Cooldown for an entry banned by share rejections — longer than
/// [`RETRY_AFTER`]: the ban persists (the rejection streak is NOT reset), so
/// when this cooldown expires and selection probes the entry again, ONE
/// rejected submission re-bans it at once. A short RETRY_AFTER would
/// ping-pong dead→failover→dead every minute, each cycle burning a lost
/// share; 300 s keeps probe cost to one rejected share per five minutes while
/// still letting a recovered validator be picked up (an accepted share clears
/// the ban immediately).
pub const REJECT_COOLDOWN: Duration = Duration::from_secs(300);

/// One candidate entry and its runtime state (indexed by config order).
struct Candidate {
    /// Consecutive work failures (reset on success).
    consecutive_fails: u32,
    /// Unix millis when the failed entry may be probed again (0 = healthy).
    retry_at_ms: AtomicU64,
    /// This entry has actually served work at least once since it entered the
    /// chain (a work fetch or stratum login on it succeeded). Drives the
    /// revert rule: a failure on an UNverified entry reverts to the previously
    /// working one immediately instead of burning three strikes.
    was_verified: bool,
    /// This entry was REVERTED from — it never worked while a verified entry
    /// did, so selection must not hand it back even after its cooldown
    /// expires (that would ping-pong dead→revert→dead every cooldown).
    /// Cleared by the first success on this entry and by
    /// [`PoolManager::replace`] (a fresh config gives a new primary its first
    /// chance again).
    reverted: bool,
    /// Consecutive share/block rejections (reset by an ACCEPTED share via
    /// [`PoolManager::clear_share_rejections`], NOT by work-fetch success —
    /// see [`PoolManager::report_share_rejection`]). Trips failover at
    /// [`REJECT_THRESHOLD`]: a source whose validation backend is broken
    /// still serves jobs, so the work-failure threshold never sees it. The
    /// streak PERSISTS across bans (rotate does not reset it): when a banned
    /// entry's cooldown expires and selection probes it again, ONE more
    /// rejection re-bans it immediately instead of burning another full
    /// threshold. An accepted share resets everything — that is the failback
    /// signal (the source's validator demonstrably works again).
    consecutive_rejects: u32,
}

/// Shared work-source-selection state. One instance per miner; safe to share
/// across the mining loop, background poller, and config watcher.
pub struct PoolManager<E: WorkSourceEntry> {
    /// The full `[[pool]]` list in config order (disabled entries included —
    /// they occupy their index but are never selected).
    pools: Mutex<Vec<E>>,
    /// Per-index failure/cooldown state, parallel to `pools`.
    candidates: Mutex<Vec<Candidate>>,
    /// Index into `pools` of the entry currently in use (usize::MAX = none).
    active: AtomicUsize,
    /// URLs with a CURRENTLY LIVE connection: the most recent report on that
    /// URL was a success (a later failure removes it). Display surfaces
    /// ([`active_pool`](Self::active_pool)) advertise only entries in this
    /// set — so after a live config switch to an unreachable entry, the
    /// hashrate line shows "none" instead of advertising a connection that
    /// cannot be established. Cleared wholesale on [`replace`](Self::replace)
    /// for URLs that left the chain (a reloaded chain has no proven CURRENT
    /// connection until a driver proves one).
    connected_urls: Mutex<HashSet<String>>,
    /// Monotonic failover counter — surfaced for logs/tests.
    failovers: AtomicU64,
}

/// Which entry to use for the next work fetch/submit, decided by the manager.
pub struct Selection<E: WorkSourceEntry> {
    /// The selected entry (owned clone — drivers build their connection from
    /// it; stratum login keys ride along too).
    pub pool: E,
    /// 0-based config-order index of the selected entry.
    pub index: usize,
    /// True when this selection is an entry other than the first enabled one.
    pub failed_over: bool,
}

impl<E: WorkSourceEntry> PoolManager<E> {
    /// Build from the `[[pool]]` list (config order). Disabled entries are
    /// kept in place but never selected. An empty / all-disabled list yields
    /// an empty manager — [`select`](Self::select) returns `None` and the
    /// caller idles until a hot-reload adds an enabled entry.
    pub fn new(pools: Vec<E>) -> Self {
        let n = pools.len();
        info!(
            "Pool failover: {} candidate(s), {} enabled — {}",
            n,
            pools.iter().filter(|p| p.enabled()).count(),
            pools
                .iter()
                .map(|p| format!("{}[{}]", p.url(), if p.enabled() { "" } else { "(off)" }))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Self::from_pools(pools)
    }

    fn from_pools(pools: Vec<E>) -> Self {
        let candidates = pools
            .iter()
            .map(|_| Candidate {
                consecutive_fails: 0,
                retry_at_ms: AtomicU64::new(0),
                was_verified: false,
                reverted: false,
                consecutive_rejects: 0,
            })
            .collect();
        Self {
            pools: Mutex::new(pools),
            candidates: Mutex::new(candidates),
            active: AtomicUsize::new(usize::MAX),
            connected_urls: Mutex::new(HashSet::new()),
            failovers: AtomicU64::new(0),
        }
    }

    /// Replace the entry list (config hot-reload). Cooldown/failure state is
    /// reset — a changed chain gets fresh strikes, and selection re-derives
    /// to the highest-priority enabled entry on the next fetch. This is what
    /// makes list edits (add/remove/enable/disable/reorder/url/wallet) live:
    /// no restart required. Verified history ("ever served work") IS carried
    /// over by [`WorkSourceEntry::reload_key`] for entries that survive the
    /// reload — it is what lets [`report_failure`](Self::report_failure)
    /// revert to a known-good entry when a newly promoted primary cannot be
    /// reached (the live-pool-switch case).
    ///
    /// The active pointer moves to the new primary so display surfaces point
    /// at the entry the drivers will try NEXT — but `connected_urls` is
    /// pruned to URLs still present in the chain, and a reloaded chain whose
    /// URLs are not currently serving still shows "none" in
    /// [`active_pool`](Self::active_pool) until a fetch/login on it succeeds.
    /// If that entry turns out to be unreachable while a previously working
    /// one (same URL) is still enabled, [`report_failure`](Self::report_failure)
    /// reverts selection to the working one at once instead of burning
    /// strikes against the dead entry. The previous active index is returned
    /// so callers can log what they are falling back to.
    pub fn replace(&self, pools: Vec<E>) -> Option<usize> {
        let prev_active = self.active_index();
        // Cooldown/failure state is reset — a changed chain gets fresh strikes
        // and no stale cooldowns. But verified history ("ever served work") IS
        // carried over by reload key for entries that survive the reload
        // unchanged: it is what the revert rule needs to keep preferring a
        // known-good entry over an unproven new primary after a live config
        // switch, and forgetting it would let a dead new primary displace the
        // working one. An entry whose login fields (wallet, worker, password)
        // were edited does NOT carry its verified status — the edited
        // credentials are unproven and must re-earn their spot. The `enabled`
        // flag is EXCLUDED from this comparison: toggling an entry off and
        // back on is standby surgery, not a credential change — a re-enabled
        // entry keeps its verified history. `reverted` resets — selection
        // eligibility re-derives from what actually answers.
        let prev_verified: HashSet<String> = {
            // Lock order pools → candidates, as everywhere else.
            let old_pools = self.pools.lock().unwrap();
            let cands = self.candidates.lock().unwrap();
            pools
                .iter()
                .filter(|p| {
                    old_pools
                        .iter()
                        .position(|q| q.reload_key() == p.reload_key())
                        .map(|i| cands[i].was_verified)
                        .unwrap_or(false)
                })
                .map(|p| p.url().to_string())
                .collect()
        };
        let candidates = pools
            .iter()
            .map(|p| Candidate {
                consecutive_fails: 0,
                retry_at_ms: AtomicU64::new(0),
                was_verified: prev_verified.contains(p.url()),
                reverted: false,
                // A reloaded chain gets a fresh rejection streak — the source
                // may have fixed its validator in the meantime.
                consecutive_rejects: 0,
            })
            .collect();
        *self.pools.lock().unwrap() = pools.clone();
        *self.candidates.lock().unwrap() = candidates;
        // A connection to an entry that is no longer in the chain cannot be
        // live — prune it from the "currently serving" set. Entries still in
        // the chain keep their status (an unrelated config edit must not make
        // a healthy long-lived stratum session display as "none").
        let present: HashSet<String> = pools.iter().map(|p| p.url().to_string()).collect();
        self.connected_urls
            .lock()
            .unwrap()
            .retain(|u| present.contains(u));
        self.active.store(
            Self::primary_index(&pools).unwrap_or(usize::MAX),
            Ordering::Relaxed,
        );
        prev_active
    }

    /// True when `url` has a CURRENTLY LIVE connection (its most recent report
    /// was a success). Display surfaces advertise only entries in this set.
    fn is_connected_url(&self, url: &str) -> bool {
        self.connected_urls.lock().unwrap().contains(url)
    }

    /// The index of the last VERIFIED working entry — a candidate that has
    /// actually served work (a work fetch or stratum login on it succeeded)
    /// and is still enabled in the current chain — if any. Used to revert
    /// selection when a newly selected entry cannot be reached: the miner
    /// keeps mining where it was actually connected, not where the config
    /// points. Prefers the current active entry (it is what the drivers are
    /// serving from), then the highest-priority verified one.
    pub fn last_verified_index(&self) -> Option<usize> {
        let pools = self.pools.lock().unwrap();
        let cands = self.candidates.lock().unwrap();
        let active = self.active.load(Ordering::Relaxed);
        if active != usize::MAX {
            if let Some(p) = pools.get(active) {
                if p.enabled() && cands[active].was_verified {
                    return Some(active);
                }
            }
        }
        pools
            .iter()
            .enumerate()
            .find(|(i, p)| p.enabled() && cands[*i].was_verified)
            .map(|(i, _)| i)
    }

    /// Record a successful work fetch/submit on the given entry index. Marks
    /// the entry's URL as verified (the "ever worked" fact for the revert
    /// rule) AND currently connected, so display surfaces advertise it — but
    /// only for entries mining actually goes to: this index if it is the
    /// active selection, plus whatever [`select`](Self::select) would return
    /// right now (the `active` pointer can lag one cycle behind a hot-reload
    /// or failover while an in-flight window finishes on the previous entry).
    /// A success on some other non-serving entry must not make it appear
    /// active. Also clears the entry's failure/cooldown state.
    pub fn report_success(&self, index: usize) {
        let pools = self.pools.lock().unwrap();
        if let Some(p) = pools.get(index) {
            // An entry that was on cooldown is healthy again — say so once per
            // recovery (the counter reset below keeps this from repeating).
            let cands = self.candidates.lock().unwrap();
            if let Some(c) = cands.get(index) {
                if c.consecutive_fails > 0 || c.retry_at_ms.load(Ordering::Relaxed) != 0 {
                    info!("Pool {} is healthy again", p.url());
                }
            }
            drop(cands);
            // "Currently serving" for display: the active selection and/or the
            // entry select() would return right now (covers the one-cycle lag
            // where an in-flight window still mines the previous entry). Lock
            // order pools→candidates is preserved; connected_urls is taken
            // after both are released.
            let active_idx = self.active.load(Ordering::Relaxed);
            let selected_idx = {
                let cands = self.candidates.lock().unwrap();
                Self::select_locked(&pools, &cands, active_idx).map(|s| s.index)
            };
            if index == active_idx || Some(index) == selected_idx {
                self.connected_urls
                    .lock()
                    .unwrap()
                    .insert(p.url().to_string());
            }
        }
        // Clear the failure/cooldown state; this entry has now PROVEN its
        // connection — it may be reverted to in future, and selection must
        // not keep skipping it as unproven.
        let mut cands = self.candidates.lock().unwrap();
        if let Some(c) = cands.get_mut(index) {
            c.consecutive_fails = 0;
            c.retry_at_ms.store(0, Ordering::Relaxed);
            c.was_verified = true;
            c.reverted = false;
        }
        self.active.store(index, Ordering::Relaxed);
    }

    /// Record a work failure on the given entry index. Fails over when the
    /// threshold is hit. Returns the index to try next (the same index while
    /// below threshold or when nothing else is available).
    ///
    /// **Revert rule:** if the failing entry has NEVER been verified and a
    /// previously working entry (verified, still enabled) exists, fail over
    /// to it at once — one strike. This is the live-pool-switch case: the
    /// operator pointed the chain at a new primary that cannot be reached;
    /// retrying it three times before falling back would leave the miner
    /// idle (or hashing stale work) for no reason, and the hashrate line must
    /// not advertise the dead entry. A verified entry failing still takes the
    /// full threshold — a working source blipping is normal and the 60 s
    /// cooldown/failback machinery handles it.
    pub fn report_failure(&self, index: usize) -> Option<usize> {
        // Snapshot what we need before taking the long-held locks (the revert
        // helpers below take their own — lock order pools → candidates must
        // never be inverted).
        let (url, enabled, was_verified) = {
            let pools = self.pools.lock().unwrap();
            match pools.get(index) {
                Some(p) => {
                    let cands = self.candidates.lock().unwrap();
                    (p.url().to_string(), p.enabled(), cands[index].was_verified)
                }
                None => return None,
            }
        };
        if !enabled {
            return None;
        }
        // Never-worked entry + a working alternative available → revert now.
        if !was_verified {
            let last = self.last_verified_index();
            // No verified entry exists at all (fresh start, nothing has ever
            // connected): the normal threshold applies — every candidate is
            // equally unproven and the chain simply walks through them.
            if last.is_some() && last != Some(index) {
                let fb = last.unwrap();
                // The failed entry goes on an extended cooldown — long enough
                // that selection keeps preferring the working fallback for a
                // while (a short RETRY_AFTER would ping-pong: once it expires,
                // select() re-prefers this highest-priority entry and the
                // miner bounces dead→revert→dead every minute). The cooldown
                // is NOT infinite so a recovered entry still gets picked up
                // eventually (and a config reload resets all state
                // immediately). If the user wants to force this entry back
                // sooner they edit the config or disable the other entries.
                {
                    let mut cands = self.candidates.lock().unwrap();
                    cands[index].retry_at_ms.store(
                        Self::now_ms() + REVERT_COOLDOWN.as_millis() as u64,
                        Ordering::Relaxed,
                    );
                    // Remember this entry was reverted from — selection must
                    // not hand it back even after the cooldown expires (that
                    // would ping-pong dead→revert→dead every cooldown). A
                    // success on it clears the flag; a config reload resets
                    // everything.
                    cands[index].reverted = true;
                }
                // The failed entry is not currently serving — stop advertising it.
                self.connected_urls.lock().unwrap().remove(&url);
                self.active.store(fb, Ordering::Relaxed);
                let (failed_url, fb_url) = {
                    let pools = self.pools.lock().unwrap();
                    (pools[index].url().to_string(), pools[fb].url().to_string())
                };
                console_error(&format!(
                    "Pool {} unreachable — reverting to the previously working pool {}",
                    failed_url, fb_url
                ));
                self.failovers.fetch_add(1, Ordering::Relaxed);
                return Some(fb);
            }
        }
        // Lock order: pools→candidates (everywhere-else order; replace()
        // holds them in this order, so the reverse would deadlock).
        let pools = self.pools.lock().unwrap();
        let mut cands = self.candidates.lock().unwrap();
        if index >= pools.len() || !pools[index].enabled() {
            return None;
        }
        // NOTE: a single failed probe does NOT drop the entry from
        // `connected_urls` — an in-flight job window keeps hashing while one
        // fetch/poll times out, and the display must not flicker to "none" on
        // every blip. The entry is removed from the "currently serving" set
        // only when it is actually ABANDONED (threshold failover / revert in
        // `rotate`), or re-proven by a success.
        cands[index].consecutive_fails += 1;
        if cands[index].consecutive_fails < FAIL_THRESHOLD {
            return Some(index); // retry the same entry
        }
        Self::rotate(
            &pools,
            &mut cands,
            index,
            &self.failovers,
            &self.active,
            &self.connected_urls,
        )
    }

    /// Fail an entry IMMEDIATELY (one strike): for conditions where retrying
    /// the same entry cannot help — e.g. a stratum login rejection (bad
    /// wallet). Marks the cooldown and returns the next candidate to try (the
    /// same index when nothing else is available; the caller should back off
    /// before retrying it, since it is now on cooldown).
    pub fn fail_immediately(&self, index: usize) -> Option<usize> {
        // Lock order: pools→candidates (everywhere-else order; replace()
        // holds them in this order, so the reverse would deadlock).
        let pools = self.pools.lock().unwrap();
        let mut cands = self.candidates.lock().unwrap();
        if index >= pools.len() || !pools[index].enabled() {
            return None;
        }
        Self::rotate(
            &pools,
            &mut cands,
            index,
            &self.failovers,
            &self.active,
            &self.connected_urls,
        )
    }

    /// Record a share/block REJECTION on the given entry. Rejections arrive
    /// over an ALIVE connection — the source still serves jobs (stratum
    /// pushes them; daemon template fetches keep succeeding) — so they never
    /// trip [`FAIL_THRESHOLD`]. But a source whose validation backend is
    /// broken earns nothing for the miner, and every rejected share is lost
    /// work: after [`REJECT_THRESHOLD`] consecutive rejections this fails
    /// over exactly like a threshold failure (cooldown + rotate). Returns
    /// the index to try next.
    ///
    /// **What clears the streak:** only an ACCEPTED share or block
    /// ([`clear_share_rejections`](Self::clear_share_rejections)). A
    /// successful work fetch or stratum login does NOT — that is deliberate:
    /// in the daemon path a healthy work fetch on every iteration would
    /// otherwise zero the counter before the submit rejection could ever
    /// accumulate, and the whole point of this method is to catch "serves
    /// work fine but cannot validate anything".
    pub fn report_share_rejection(&self, index: usize) -> Option<usize> {
        // Lock order: pools→candidates (everywhere-else order; replace()
        // holds them in this order, so the reverse would deadlock).
        let pools = self.pools.lock().unwrap();
        let mut cands = self.candidates.lock().unwrap();
        if index >= pools.len() || !pools[index].enabled() {
            return None;
        }
        cands[index].consecutive_rejects += 1;
        if cands[index].consecutive_rejects < REJECT_THRESHOLD {
            return Some(index); // keep mining here for now
        }
        console_error(&format!(
            "Pool {} rejected {n} shares in a row — failing over",
            pools[index].url(),
            n = cands[index].consecutive_rejects
        ));
        let next = Self::rotate(
            &pools,
            &mut cands,
            index,
            &self.failovers,
            &self.active,
            &self.connected_urls,
        );
        // Rejection bans get the LONGER cooldown: the streak persists (see
        // rotate's note), so when this expires and selection probes the entry
        // again, one rejected submission re-bans it at once. RETRY_AFTER
        // would ping-pong dead→failover→dead every minute, each cycle losing
        // a share.
        cands[index].retry_at_ms.store(
            Self::now_ms() + REJECT_COOLDOWN.as_millis() as u64,
            Ordering::Relaxed,
        );
        next
    }

    /// Clear the rejection streak on the given entry — called when a share or
    /// block is ACCEPTED. A single accepted submission proves the source's
    /// validation path works again; sporadic rejections (stale jobs after a
    /// reorg, etc.) must not accumulate toward failover.
    pub fn clear_share_rejections(&self, index: usize) {
        let mut cands = self.candidates.lock().unwrap();
        if let Some(c) = cands.get_mut(index) {
            c.consecutive_rejects = 0;
        }
    }

    /// Mark `index` on cooldown and find the next enabled candidate not on
    /// cooldown (wrapping). Shared by threshold failover and immediate fail.
    /// Moves the `active` pointer to wherever mining is going NEXT so display
    /// surfaces ([`active_pool`](Self::active_pool)) track reality: without
    /// this, a failed-over chain would keep advertising the dead entry (or,
    /// after a live config switch, show an entry whose connection was never
    /// established) until the first successful fetch on the new one. The
    /// abandoned entry is removed from `connected_urls` — it is no longer
    /// serving, so the hashrate line shows "none" (or the fallback's URL
    /// after its next success), never a dead entry. An unverified next
    /// candidate likewise makes [`active_pool`](Self::active_pool) report
    /// `None` ("none") — honest: no proven connection exists yet.
    fn rotate(
        pools: &[E],
        cands: &mut [Candidate],
        index: usize,
        failovers: &AtomicU64,
        active: &AtomicUsize,
        connected_urls: &Mutex<HashSet<String>>,
    ) -> Option<usize> {
        let n = pools.len();
        // Threshold hit — mark this entry on cooldown.
        let failed_url = pools[index].url().to_string();
        cands[index].retry_at_ms.store(
            Self::now_ms() + RETRY_AFTER.as_millis() as u64,
            Ordering::Relaxed,
        );
        cands[index].consecutive_fails = 0;
        // NOTE: `consecutive_rejects` is deliberately NOT reset here. An entry
        // banned for share rejections keeps its streak at/above threshold:
        // when its cooldown expires and selection probes it again, ONE more
        // rejected submission re-bans it immediately (one lost share) instead
        // of burning another full REJECT_THRESHOLD. The only ways back are an
        // ACCEPTED share on this entry ([`clear_share_rejections`] — the
        // validator demonstrably works again) or a config reload (fresh
        // strikes). This is what keeps a broken-validator source out of
        // rotation without ping-pong.
        // Abandoned — stop advertising it (display shows "none" until a
        // success re-proves the connection).
        connected_urls.lock().unwrap().remove(&failed_url);
        warn!("Pool {} failed — failing over", failed_url);

        // Find the next ENABLED candidate not on cooldown (wraps around).
        for step in 1..=n {
            let next = (index + step) % n;
            if next == index {
                break; // everything else is disabled or on cooldown
            }
            if !pools[next].enabled() {
                continue;
            }
            if cands[next].retry_at_ms.load(Ordering::Relaxed) <= Self::now_ms() {
                failovers.fetch_add(1, Ordering::Relaxed);
                info!(
                    "Failing over to pool {} ({} — priority {})",
                    pools[next].url(),
                    pools[next].work_source(),
                    next
                );
                active.store(next, Ordering::Relaxed);
                return Some(next);
            }
        }
        warn!(
            "No other pool available — staying on {} and retrying",
            failed_url
        );
        active.store(index, Ordering::Relaxed);
        Some(index)
    }

    /// Select the entry for the next work fetch. Prefers the highest-priority
    /// (lowest-index) ENABLED candidate that isn't on cooldown; falls back to
    /// the current active, then the first enabled entry, when all are cooling
    /// down (the probe attempt itself is the retry). `None` when no entry is
    /// enabled at all.
    pub fn select(&self) -> Option<Selection<E>> {
        // Re-qualify reverted entries whose cooldown has expired: the
        // preferred (highest-priority) entry gets ONE probing opportunity per
        // REVERT_COOLDOWN cycle. This is the failback path — if the entry has
        // recovered, selection prefers it again (config order) and its first
        // success clears the state permanently; if it is still dead,
        // report_failure re-reverts immediately with a fresh cooldown, so the
        // cost of a dead preferred entry is one failed probe (~seconds) per
        // cooldown window — bounded, not the per-minute ping-pong a short
        // RETRY_AFTER would cause. Without this, an entry reverted from is
        // locked out forever: selection never picks it, so it can never serve
        // work, so nothing ever clears `reverted` (only a config reload did).
        {
            let requalified = {
                let mut cands = self.candidates.lock().unwrap();
                let now = Self::now_ms();
                let mut out = Vec::new();
                for (i, c) in cands.iter_mut().enumerate() {
                    let until = c.retry_at_ms.load(Ordering::Relaxed);
                    if c.reverted && until != 0 && until <= now {
                        c.reverted = false;
                        out.push(i);
                    }
                }
                out
            };
            if !requalified.is_empty() {
                // Lock order: pools AFTER candidates is released
                // (pools→candidates is the everywhere-else order; never hold
                // both in reverse).
                let urls: Vec<String> = {
                    let pools = self.pools.lock().unwrap();
                    requalified
                        .iter()
                        .filter_map(|&i| pools.get(i).map(|p| p.url().to_string()))
                        .collect()
                };
                info!(
                    "Re-probing reverted pool(s) after cooldown expiry: {}",
                    urls.join(", ")
                );
            }
        }
        let pools = self.pools.lock().unwrap();
        let cands = self.candidates.lock().unwrap();
        Self::select_locked(&pools, &cands, self.active.load(Ordering::Relaxed))
    }

    /// Selection logic over already-locked state (lock order: `pools` then
    /// `candidates`, always — callers must hold both in that order). Shared
    /// by [`select`](Self::select) and the display-truthfulness check in
    /// [`report_success`](Self::report_success).
    fn select_locked(pools: &[E], cands: &[Candidate], active: usize) -> Option<Selection<E>> {
        let now = Self::now_ms();

        // Prefer the highest-priority healthy enabled candidate. An entry that
        // was REVERTED from (it never worked while a verified one did) is
        // skipped even after its cooldown expires — otherwise selection would
        // hand it back every cooldown and the miner would ping-pong
        // dead→revert→dead. It becomes eligible again once it actually serves
        // work (a success clears `reverted`) or a config reload resets state.
        let mut chosen: Option<usize> = None;
        for (i, p) in pools.iter().enumerate() {
            if !p.enabled() {
                continue;
            }
            if cands[i].retry_at_ms.load(Ordering::Relaxed) > now {
                continue; // on cooldown
            }
            if cands[i].reverted {
                continue; // reverted from — not eligible until it proves itself
            }
            chosen = Some(i);
            break;
        }
        // All enabled candidates on cooldown — use the current active if it is
        // still an enabled entry, else the first enabled one anyway.
        let index = match chosen {
            Some(i) => i,
            None => {
                if active != usize::MAX && pools.get(active).map(|p| p.enabled()) == Some(true) {
                    active
                } else {
                    Self::primary_index(pools)?
                }
            }
        };

        let primary = Self::primary_index(pools);
        Some(Selection {
            pool: pools[index].clone(),
            index,
            failed_over: Some(index) != primary,
        })
    }

    /// Spawn a watcher thread that flips `selection_changed` when the selected
    /// entry's identity moves away from the one named by `job_entry_identity`.
    ///
    /// Why this exists: a live config edit (the operator switches sources) or
    /// a failover rotation can move selection away from the entry an in-flight
    /// job was fetched from. Without this watcher the miner would keep hashing
    /// that stale work until something else stopped it. The flag is passed
    /// into the mining loop as a stop flag (the loop exits at its next
    /// iteration); the caller consumes it after each window (skip submission —
    /// the work belongs to an abandoned source — and re-fetch/reconnect from
    /// whatever selection now points at). Set `job_entry_identity` before
    /// starting a job; set it to `None` when no job is in flight.
    ///
    /// Comparison is by [`WorkSourceEntry::identity`] (URL + login keys), not
    /// just URL: a hot-reload may re-index or reorder the chain without
    /// changing which entry we are actually mining on, but an edit to the
    /// login keys of the CURRENTLY CONNECTED entry must also fire — otherwise
    /// the connection would keep mining under the credentials captured at
    /// connect time. Same URL + changed identity means "re-login with the new
    /// credentials"; different URL means "connect to the new selection". The
    /// watcher shares the SAME manager instance (Arc) so hot-reload replaces
    /// are visible to it.
    pub fn spawn_selection_watcher(
        pools: Arc<Self>,
        job_entry_identity: Arc<Mutex<Option<String>>>,
        selection_changed: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("pool-selection-watch".to_string())
            .spawn(move || loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // No job in flight — nothing to protect.
                let current = job_entry_identity.lock().unwrap().clone();
                if let Some(identity) = current {
                    match pools.select() {
                        Some(sel) if sel.pool.identity() == identity => {}
                        Some(sel) => {
                            // Distinguish the two cases for the log line: the
                            // flag consumers treat them identically (abort +
                            // re-select), but the operator needs to know
                            // whether this was a source switch or a login-key
                            // edit on the entry already in use.
                            if sel.pool.url() == identity.split('\u{0}').next().unwrap_or("") {
                                info!(
                                    "Pool login keys changed for {} — re-login with new credentials",
                                    sel.pool.url()
                                );
                            }
                            selection_changed.store(true, Ordering::SeqCst);
                        }
                        _ => selection_changed.store(true, Ordering::SeqCst),
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            })
            .expect("failed to spawn pool-selection watcher thread")
    }

    /// Number of enabled entries in the chain.
    pub fn candidate_count(&self) -> usize {
        self.pools
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.enabled())
            .count()
    }

    /// Identities of ALL entries in config order (enabled and disabled) —
    /// lets consumers diff the live chain against a hot-reloaded one without
    /// exposing the entries themselves.
    pub fn pool_identities(&self) -> Vec<String> {
        self.pools
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.identity())
            .collect()
    }

    /// Same as [`Self::pool_identities`], but each identity is paired with the
    /// entry's `enabled` flag. Diffing on this catches a hot-reload that ONLY
    /// toggles `enabled` — identity deliberately excludes the flag (reload_key
    /// carryover semantics), so an identity-only diff would silently drop an
    /// enable/disable edit.
    pub fn pool_identity_enabled(&self) -> Vec<(String, bool)> {
        self.pools
            .lock()
            .unwrap()
            .iter()
            .map(|p| (p.identity(), p.enabled()))
            .collect()
    }

    /// Monotonic failover counter — used by tests and available for
    /// diagnostics/logging surfaces.
    pub fn failover_count(&self) -> u64 {
        self.failovers.load(Ordering::Relaxed)
    }

    /// The entry currently in use (config-order index), if any.
    pub fn active_index(&self) -> Option<usize> {
        let a = self.active.load(Ordering::Relaxed);
        if a == usize::MAX {
            None
        } else {
            Some(a)
        }
    }

    /// The entry currently in use, or `None` when the active entry has no
    /// CURRENTLY LIVE connection (its most recent report was a failure, it
    /// was never connected, or its URL left the chain). Read-only — for
    /// API/log surfaces; selection itself goes through [`select`](Self::select).
    /// An entry that was merely selected (e.g. a new primary after a live
    /// config switch whose connection cannot be established) is NOT reported:
    /// the hashrate line and the API show "none" until a work fetch or login
    /// on it succeeds, so an unreachable entry is never advertised as the
    /// active one.
    pub fn active_pool(&self) -> Option<E> {
        let a = self.active.load(Ordering::Relaxed);
        if a == usize::MAX {
            return None;
        }
        let pools = self.pools.lock().unwrap();
        let p = pools.get(a)?;
        if !self.is_connected_url(p.url()) {
            return None;
        }
        Some(p.clone())
    }

    /// The first enabled entry's index (the chain primary), if any.
    fn primary_index(pools: &[E]) -> Option<usize> {
        pools.iter().position(|p| p.enabled())
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for a miner's config entry: enough surface to exercise every
    /// manager behavior. Real miners implement the trait on their own structs
    /// (xtminer's carries algorithm/nonce_width; edgecase's has its own
    /// defaults) — the manager only ever touches these four questions.
    #[derive(Debug, Clone)]
    struct TestEntry {
        url: String,
        ws: WorkSource,
        wallet: String,
        enabled: bool,
    }

    impl TestEntry {
        fn node(url: &str) -> Self {
            Self {
                url: url.to_string(),
                ws: WorkSource::Node,
                wallet: String::new(),
                enabled: true,
            }
        }
        fn stratum(url: &str, wallet: &str) -> Self {
            Self {
                url: url.to_string(),
                ws: WorkSource::Stratum,
                wallet: wallet.to_string(),
                enabled: true,
            }
        }
        fn stratum_disabled(url: &str, wallet: &str) -> Self {
            Self {
                url: url.to_string(),
                ws: WorkSource::Stratum,
                wallet: wallet.to_string(),
                enabled: false,
            }
        }
    }

    impl WorkSourceEntry for TestEntry {
        fn url(&self) -> &str {
            &self.url
        }
        fn work_source(&self) -> WorkSource {
            self.ws
        }
        fn identity(&self) -> String {
            format!("{}\u{0}{}\u{0}{}", self.url, self.ws, self.wallet)
        }
        fn reload_key(&self) -> String {
            self.identity()
        }
        fn enabled(&self) -> bool {
            self.enabled
        }
    }

    fn chain(entries: Vec<TestEntry>) -> PoolManager<TestEntry> {
        PoolManager::new(entries)
    }

    #[test]
    fn work_source_parses_both_vocabularies() {
        assert_eq!(
            WorkSource::parse("node", WorkSource::Stratum),
            Some(WorkSource::Node)
        );
        assert_eq!(
            WorkSource::parse("proxy", WorkSource::Stratum),
            Some(WorkSource::Proxy)
        );
        assert_eq!(
            WorkSource::parse("stratum", WorkSource::Node),
            Some(WorkSource::Stratum)
        );
        assert_eq!(
            WorkSource::parse("", WorkSource::Node),
            Some(WorkSource::Node)
        );
        assert_eq!(WorkSource::parse("bogus", WorkSource::Node), None);
        assert_eq!(WorkSource::Node.to_string(), "node");
        assert_eq!(WorkSource::Proxy.to_string(), "proxy");
        assert_eq!(WorkSource::Stratum.to_string(), "stratum");
    }

    #[test]
    fn single_pool_never_fails_over() {
        let pm = chain(vec![TestEntry::node("http://a")]);
        assert_eq!(pm.candidate_count(), 1);
        // Below threshold: same entry.
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(0));
        // Threshold hit: no other candidate → same entry again.
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.failover_count(), 0);
    }

    #[test]
    fn fails_over_after_threshold() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert_eq!(pm.candidate_count(), 2);
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(0));
        // Third consecutive failure trips the threshold.
        assert_eq!(pm.report_failure(0), Some(1));
        assert_eq!(pm.failover_count(), 1);
        // select() now prefers the failed-over entry (a is on cooldown).
        let sel = pm.select().unwrap();
        assert_eq!(sel.index, 1);
        assert!(sel.failed_over);
    }

    #[test]
    fn success_resets_failure_count() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_failure(0);
        pm.report_failure(0);
        pm.report_success(0); // reset
        assert_eq!(pm.report_failure(0), Some(0)); // fail count restarted
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(1)); // now it trips
    }

    #[test]
    fn failover_wraps_and_skips_cooldown() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
            TestEntry::node("http://c"),
        ]);
        // Each entry takes FAIL_THRESHOLD consecutive failures to trip.
        for _ in 0..FAIL_THRESHOLD - 1 {
            assert_eq!(pm.report_failure(0), Some(0));
        }
        // a trips → b.
        assert_eq!(pm.report_failure(0), Some(1));
        for _ in 0..FAIL_THRESHOLD - 1 {
            assert_eq!(pm.report_failure(1), Some(1));
        }
        // b trips → c.
        assert_eq!(pm.report_failure(1), Some(2));
        for _ in 0..FAIL_THRESHOLD - 1 {
            assert_eq!(pm.report_failure(2), Some(2));
        }
        // c trips; a and b are on cooldown → stay on c.
        assert_eq!(pm.report_failure(2), Some(2));
        assert_eq!(pm.failover_count(), 2);
    }

    #[test]
    fn disabled_pools_are_skipped_in_failover() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
            TestEntry::stratum("c:7038", "W"),
        ]);
        // Disable the middle entry.
        let mut pools = vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
            TestEntry::stratum("c:7038", "W"),
        ];
        pools[1].enabled = false;
        let _ = pm.replace(pools);
        assert_eq!(pm.candidate_count(), 2);
        // a trips → must skip disabled b and land on the stratum entry.
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(0);
        }
        let sel = pm.select().unwrap();
        assert_eq!(sel.index, 2);
        assert_eq!(sel.pool.work_source(), WorkSource::Stratum);
    }

    #[test]
    fn cross_type_failover_node_to_stratum_and_back() {
        // A node chain that falls through to a stratum pool — the whole point
        // of one mixed failover list.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::stratum("xtm-rx:7038", "W"),
        ]);
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(0);
        }
        assert_eq!(pm.select().unwrap().index, 1); // on the stratum entry now

        // Simulate the node's cooldown expiring — selection returns to it.
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0].retry_at_ms.store(
                PoolManager::<TestEntry>::now_ms().saturating_sub(1000),
                Ordering::Relaxed,
            );
        }
        assert_eq!(pm.select().unwrap().index, 0); // back to the node

        // And a stratum failure fails over BACK to the recovered node.
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(1);
        }
        assert_eq!(pm.select().unwrap().index, 0);
    }

    #[test]
    fn replace_resets_state_and_reselects() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        // Trip a's cooldown.
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(0);
        }
        assert_eq!(pm.select().unwrap().index, 1);
        // Hot-reload with the same list: state resets → back to primary.
        let _ = pm.replace(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert_eq!(pm.select().unwrap().index, 0);

        // Disabling the primary live moves selection to the next entry.
        let mut pools = vec![TestEntry::node("http://a"), TestEntry::node("http://b")];
        pools[0].enabled = false;
        let _ = pm.replace(pools);
        assert_eq!(pm.select().unwrap().index, 1);

        // Disabling everything → no selection (caller idles).
        let mut pools = vec![TestEntry::node("http://a"), TestEntry::node("http://b")];
        for p in &mut pools {
            p.enabled = false;
        }
        let _ = pm.replace(pools);
        assert!(pm.select().is_none());
    }

    #[test]
    fn unverified_pool_reverts_to_previously_working_one() {
        // The live-pool-switch case: the miner is working on entry a
        // (verified), then a hot-reload points the chain at a new primary
        // that cannot be reached. Selection must revert to the previously
        // working entry at once — one strike, not three — and display must
        // never advertise the dead entry as active.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a is verified (the miner was working on it)

        // Hot-reload: new primary c (unreachable), old entry a demoted to #2.
        let mut pools = vec![TestEntry::node("http://c"), TestEntry::node("http://a")];
        for p in &mut pools {
            p.enabled = true;
        }
        let _ = pm.replace(pools);

        // The hashrate line must NOT advertise c — it was never verified.
        assert!(pm.active_pool().is_none());

        // First failure on the unverified entry reverts to a immediately.
        assert_eq!(pm.report_failure(0), Some(1));
        // Selection now lands on the previously working entry.
        let sel = pm.select().unwrap();
        assert_eq!(sel.index, 1);
        assert_eq!(sel.pool.url(), "http://a");
        // Display is truthful again: a is verified and active.
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );
    }

    #[test]
    fn reverted_pool_stays_out_of_rotation_during_cooldown() {
        // Anti-ping-pong: once a never-worked entry is reverted from, its
        // cooldown must be LONGER than the normal failover RETRY_AFTER — with
        // the short one, select() would re-prefer this highest-priority entry
        // every minute and the miner would bounce dead→revert→dead. The
        // working fallback keeps being selected for the whole window; after
        // REVERT_COOLDOWN expires, the entry is re-qualified for ONE probe
        // per cooldown cycle (see reverted_pool_fails_back_after_cooldown_expiry).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a working

        // Hot-reload: new primary c (unreachable), old entry a demoted to #2.
        let _ = pm.replace(vec![
            TestEntry::node("http://c"),
            TestEntry::node("http://a"),
        ]);
        assert_eq!(pm.report_failure(0), Some(1)); // revert to a at once

        // The failed entry's cooldown extends well past the normal failover
        // window — selection keeps landing on the working fallback.
        {
            let cands = pm.candidates.lock().unwrap();
            let until = cands[0].retry_at_ms.load(Ordering::Relaxed);
            assert!(
                until
                    >= PoolManager::<TestEntry>::now_ms() + REVERT_COOLDOWN.as_millis() as u64
                        - 1000,
                "revert cooldown must be at least REVERT_COOLDOWN long"
            );
        }
        assert_eq!(pm.select().unwrap().index, 1);

        // A config reload resets all state — c becomes selectable again (and
        // will revert once more if it still cannot connect).
        let _ = pm.replace(vec![
            TestEntry::node("http://c"),
            TestEntry::node("http://a"),
        ]);
        assert_eq!(pm.select().unwrap().index, 0);
    }

    #[test]
    fn reverted_pool_fails_back_after_cooldown_expiry() {
        // The failback path: an entry reverted from is re-qualified once its
        // REVERT_COOLDOWN expires — the preferred entry gets ONE probing
        // opportunity per cooldown cycle. If it has recovered, selection
        // prefers it again (config order) and its success clears the state;
        // if it is still dead, the next failure re-reverts with a fresh
        // cooldown (bounded cost: one probe per cycle, not per-minute
        // ping-pong). Before this, `reverted` was a dead-end lockout: only a
        // config reload could give the entry another chance.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(1); // fallback b is the verified one

        // Primary a is unreachable → one strike reverts to b.
        assert_eq!(pm.report_failure(0), Some(1));
        assert_eq!(pm.select().unwrap().index, 1);

        // Simulate REVERT_COOLDOWN elapsing.
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0]
                .retry_at_ms
                .store(PoolManager::<TestEntry>::now_ms() - 1, Ordering::Relaxed);
        }
        // select() re-qualifies the expired entry — the preferred entry is
        // probed again even though b is still working.
        assert_eq!(pm.select().unwrap().index, 0);

        // If it is STILL dead, one failure re-reverts at once with a fresh
        // cooldown (bounded probe cost per cycle).
        assert_eq!(pm.report_failure(0), Some(1));
        assert_eq!(pm.select().unwrap().index, 1);

        // Now it recovers: after the next cooldown expiry, select() picks it
        // and a success clears the reverted state permanently.
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0]
                .retry_at_ms
                .store(PoolManager::<TestEntry>::now_ms() - 1, Ordering::Relaxed);
        }
        assert_eq!(pm.select().unwrap().index, 0);
        pm.report_success(0);
        assert_eq!(pm.select().unwrap().index, 0);
        assert_eq!(pm.last_verified_index(), Some(0));
    }

    #[test]
    fn transient_blip_does_not_drop_the_display() {
        // A working entry that blips ONE probe must keep being advertised: an
        // in-flight job window keeps hashing while a single fetch times out,
        // and the hashrate line flickering to "none" on every blip is exactly
        // what this set exists to prevent. Only ABANDONMENT (threshold
        // failover / revert) removes an entry from the display.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a is working and advertised
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );

        // One failed probe — below the failover threshold.
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );

        // Recovery re-proves it (no-op on the set, but must not break it).
        pm.report_success(0);
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );
    }

    #[test]
    fn threshold_failover_stops_advertising_the_dead_pool() {
        // Once an entry is actually ABANDONED (threshold failover), the
        // display must not keep advertising it — and must show "none" until
        // the new selection proves its connection.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a working
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );

        // Three strikes → fail over to b.
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(0);
        }
        assert_eq!(pm.select().unwrap().index, 1);
        // a is abandoned: not advertised even though it "ever worked".
        assert!(pm.active_pool().is_none());

        // b proves its connection → now b is the active entry on display.
        pm.report_success(1);
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://b".to_string())
        );
    }

    #[test]
    fn replace_prunes_removed_pools_from_the_display() {
        // A hot-reload that removes an entry from the chain must also stop
        // advertising it — its connection cannot be live if it no longer
        // exists. Entries still in the chain keep their status (an unrelated
        // edit must not make a healthy session display as "none").
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a working and advertised

        // Reload: b removed, a stays primary.
        let _ = pm.replace(vec![TestEntry::node("http://a")]);
        assert_eq!(
            pm.active_pool().map(|p| p.url().to_string()),
            Some("http://a".to_string())
        );

        // Reload: a demoted behind new primary c (unreachable). a's status is
        // kept (still in the chain) but it is no longer active — and c was
        // never connected, so nothing may be advertised.
        let _ = pm.replace(vec![
            TestEntry::node("http://c"),
            TestEntry::node("http://a"),
        ]);
        assert!(pm.active_pool().is_none());
    }

    #[test]
    fn verified_pool_failure_still_uses_threshold() {
        // An entry that HAS worked takes the full FAIL_THRESHOLD before
        // failing over — a working source blipping is normal and must not
        // instantly abandon the primary (that would flap on every transient
        // error).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a verified
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(1)); // threshold trips → b
    }

    #[test]
    fn fresh_chain_walks_candidates_without_revert() {
        // Nothing has ever connected: every candidate is equally unproven, so
        // the normal threshold applies (no instant revert to a "working"
        // entry that doesn't exist yet).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(0));
        assert_eq!(pm.report_failure(0), Some(1)); // threshold → b, as before
    }

    #[test]
    fn replace_returns_previous_active_index() {
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(1); // active is now b (index 1)
        let prev = pm.replace(vec![TestEntry::node("http://c")]);
        assert_eq!(prev, Some(1));
    }

    #[test]
    fn selection_carries_entry() {
        let s = TestEntry::stratum("xtm-rx:7038", "WALLET");
        let pm = chain(vec![TestEntry::node("http://a"), s]);
        // Force selection of the stratum entry.
        for _ in 0..FAIL_THRESHOLD {
            pm.report_failure(0);
        }
        let sel = pm.select().unwrap();
        assert_eq!(sel.pool.url(), "xtm-rx:7038");
        assert_eq!(
            sel.pool.identity(),
            format!("xtm-rx:7038\u{0}stratum\u{0}WALLET")
        );
    }

    #[test]
    fn share_rejections_fail_over_at_threshold() {
        // The broken-validator case: the source serves jobs fine (connection
        // alive) but rejects every share. Rejections must fail over after
        // REJECT_THRESHOLD — not FAIL_THRESHOLD, and not never.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0)); // stay put
        }
        // Threshold hit → fail over to b.
        assert_eq!(pm.report_share_rejection(0), Some(1));
        assert_eq!(pm.failover_count(), 1);
        let sel = pm.select().unwrap();
        assert_eq!(sel.index, 1);
    }

    #[test]
    fn accepted_share_clears_the_rejection_streak() {
        // Sporadic rejections (stale jobs after a reorg) must not accumulate:
        // an acceptance resets the streak.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        pm.clear_share_rejections(0); // one accepted share
                                      // The streak restarted — the same number of rejections no longer trips.
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        assert_eq!(pm.failover_count(), 0);
    }

    #[test]
    fn work_fetch_success_does_not_clear_the_rejection_streak() {
        // Deliberate: a successful work fetch / stratum login must NOT reset
        // the rejection counter. In the daemon path every iteration's healthy
        // work fetch would otherwise zero it before the submit rejection
        // could accumulate — and "serves work but cannot validate" is exactly
        // the condition this counter exists to catch.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        pm.report_success(0); // healthy work fetch on the same entry
                              // The streak survived — one more rejection trips it.
        assert_eq!(pm.report_share_rejection(0), Some(1));
    }

    #[test]
    fn replace_resets_the_rejection_streak() {
        // A config reload gives a reloaded chain fresh strikes (the source
        // may have fixed its validator in the meantime).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        let _ = pm.replace(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        // Streak reset — the same number of rejections no longer trips.
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        assert_eq!(pm.failover_count(), 0);
    }

    #[test]
    fn verified_status_carries_over_when_credentials_unchanged() {
        // An unrelated reload (no field edits) must keep was_verified — the
        // revert rule depends on remembering which entry actually served
        // work.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a verified + connected
        let _ = pm.replace(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert!(pm.last_verified_index() == Some(0));
    }

    #[test]
    fn edited_login_keys_lose_verified_status() {
        // Editing wallet (or any login key) on an entry makes its credentials
        // unproven: verified status does NOT carry across the reload, and the
        // revert rule must not treat the edited entry as known-good.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0); // a verified with the OLD wallet
        let mut a = TestEntry::node("http://a");
        a.wallet = "renamed".to_string();
        let _ = pm.replace(vec![a, TestEntry::node("http://b")]);
        assert_eq!(pm.last_verified_index(), None);
        // Selection still picks a (it is the enabled primary, just unproven).
        assert_eq!(pm.select().unwrap().index, 0);
    }

    #[test]
    fn edited_url_loses_verified_status() {
        // A URL change is a different entry — nothing carries over by URL
        // match.
        let pm = chain(vec![TestEntry::node("http://a")]);
        pm.report_success(0);
        let _ = pm.replace(vec![TestEntry::node("http://a2")]);
        assert_eq!(pm.last_verified_index(), None);
    }

    #[test]
    fn enabled_toggle_keeps_verified_status() {
        // Standby surgery: disabling then re-enabling an entry does NOT wipe
        // its verified history — the credentials are unchanged, so the revert
        // rule still knows this entry served work.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0);
        let mut off = TestEntry::node("http://a");
        off.enabled = false;
        let _ = pm.replace(vec![off, TestEntry::node("http://b")]);
        assert_eq!(pm.select().unwrap().index, 1); // mining moved to b
        let _ = pm.replace(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert_eq!(pm.last_verified_index(), Some(0)); // history survived
    }

    #[test]
    fn disabling_the_active_pool_moves_selection() {
        // enabled=false on the entry we are mining on must be a trigger: the
        // next select() points elsewhere, so the selection watcher fires and
        // the in-flight job window aborts (same path as a pool switch).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        pm.report_success(0);
        assert_eq!(pm.select().unwrap().index, 0);
        let mut off = TestEntry::node("http://a");
        off.enabled = false;
        let _ = pm.replace(vec![off, TestEntry::node("http://b")]);
        assert_eq!(pm.select().unwrap().index, 1);
    }

    #[test]
    fn banned_pool_stays_out_of_rotation_and_rebans_on_one_probe() {
        // Anti-ping-pong (the live-smoke finding): once an entry is banned
        // for share rejections, its cooldown must be LONGER than the normal
        // failover RETRY_AFTER — otherwise selection would re-prefer this
        // highest-priority entry every minute and the miner would bounce
        // dead→failover→dead, losing REJECT_THRESHOLD shares per cycle. The
        // working fallback keeps being selected for the whole window; a
        // config reload resets state at once (and the entry gets another
        // chance).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD {
            pm.report_share_rejection(0);
        }
        assert_eq!(pm.select().unwrap().index, 1); // on b now

        // The ban cooldown extends well past the normal failover window —
        // selection keeps landing on the working fallback.
        {
            let cands = pm.candidates.lock().unwrap();
            let until = cands[0].retry_at_ms.load(Ordering::Relaxed);
            assert!(
                until
                    >= PoolManager::<TestEntry>::now_ms() + REJECT_COOLDOWN.as_millis() as u64
                        - 1000,
                "rejection ban must use at least REJECT_COOLDOWN"
            );
        }
        assert_eq!(pm.select().unwrap().index, 1);

        // The streak PERSISTS across the ban: when the cooldown expires and
        // selection probes a again, ONE rejected submission re-bans it at
        // once (one lost share) instead of burning another full threshold.
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0].retry_at_ms.store(
                PoolManager::<TestEntry>::now_ms().saturating_sub(1000),
                Ordering::Relaxed,
            );
        }
        assert_eq!(pm.select().unwrap().index, 0); // probe: a is selected again
        assert_eq!(pm.report_share_rejection(0), Some(1)); // one strike → re-ban

        // A config reload resets all state — a becomes selectable again (and
        // will be banned once more if it still rejects).
        let _ = pm.replace(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        assert_eq!(pm.select().unwrap().index, 0);
    }

    #[test]
    fn accepted_share_unbans_a_rejected_pool() {
        // Failback: an ACCEPTED share on a banned entry proves its validator
        // works again — the streak resets and the entry is eligible for
        // selection as soon as its cooldown expires (no further strikes
        // owed).
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::node("http://b"),
        ]);
        for _ in 0..REJECT_THRESHOLD {
            pm.report_share_rejection(0);
        }
        assert_eq!(pm.select().unwrap().index, 1);

        // The validator recovers: one accepted share clears the ban.
        pm.clear_share_rejections(0);
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0].retry_at_ms.store(
                PoolManager::<TestEntry>::now_ms().saturating_sub(1000),
                Ordering::Relaxed,
            );
        }
        // Selection returns to the primary — and a fresh streak means it
        // takes another full REJECT_THRESHOLD of rejections to ban it again.
        assert_eq!(pm.select().unwrap().index, 0);
        for _ in 0..REJECT_THRESHOLD - 1 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
    }

    #[test]
    fn single_pool_rejections_never_fail_over() {
        // One candidate: rejections accumulate but there is nowhere to go —
        // the miner stays put (same as work failures on a single-entry
        // chain).
        let pm = chain(vec![TestEntry::node("http://a")]);
        for _ in 0..REJECT_THRESHOLD * 2 {
            assert_eq!(pm.report_share_rejection(0), Some(0));
        }
        assert_eq!(pm.failover_count(), 0);
    }

    #[test]
    fn fail_immediately_rotates_past_the_failed_entry() {
        // Login rejection is one strike: even with the threshold untripped,
        // selection moves to the next candidate at once.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::stratum("p:1", "W"),
        ]);
        pm.report_success(0); // a verified and active
        assert_eq!(pm.fail_immediately(0), Some(1));
        assert_eq!(pm.failover_count(), 1);
        assert_eq!(pm.select().unwrap().index, 1);
        // a is on cooldown (RETRY_AFTER); a success on it clears that.
        {
            let cands = pm.candidates.lock().unwrap();
            cands[0].retry_at_ms.store(
                PoolManager::<TestEntry>::now_ms().saturating_sub(1000),
                Ordering::Relaxed,
            );
        }
        assert_eq!(pm.select().unwrap().index, 0);
        pm.report_success(0);
        assert_eq!(pm.select().unwrap().index, 0);
    }

    #[test]
    fn pool_identities_lists_every_entry_in_config_order() {
        // Disabled entries keep their index — identity diffing must see them
        // so a hot-reload that flips `enabled` is detected by position.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::stratum("p:1", "W"),
            TestEntry::node("http://c"),
        ]);
        let ids = pm.pool_identities();
        assert_eq!(ids.len(), 3);
        assert!(ids[0].contains("http://a"));
        assert!(ids[1].contains("p:1"));
        assert!(ids[2].contains("http://c"));
        // Distinct login keys → distinct identities (the whole point).
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn pool_identity_enabled_carries_the_flag() {
        // The (identity, enabled) pairs let consumers detect an
        // enabled-only toggle that identity() alone cannot represent.
        let pm = chain(vec![
            TestEntry::node("http://a"),
            TestEntry::stratum("p:1", "W"),
        ]);
        let pairs = pm.pool_identity_enabled();
        assert_eq!(pairs.len(), 2);
        assert!(pairs[0].1, "node default enabled");
        assert!(pairs[1].1, "stratum default enabled");

        let pm2 = chain(vec![TestEntry::stratum_disabled("p:1", "W")]);
        let pairs2 = pm2.pool_identity_enabled();
        assert_eq!(pairs2.len(), 1);
        assert!(!pairs2[0].1, "disabled entry carries enabled=false");
        assert_eq!(pairs2[0].0, pairs[1].0, "same identity, different flag");
    }

    #[test]
    fn concurrent_replace_and_fail_paths_do_not_deadlock() {
        // Regression for the inverted lock order: replace() holds
        // pools→candidates while the failure paths (report_failure /
        // fail_immediately / report_share_rejection) previously locked
        // candidates→pools. Under contention that deadlocked. Hammer both
        // directions from separate threads; a watchdog panics on hang.
        let pm = std::sync::Arc::new(chain(vec![
            TestEntry::stratum("ws://a", "w1"),
            TestEntry::stratum("ws://b", "w2"),
            TestEntry::stratum("ws://c", "w3"),
        ]));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        // Hot-reload style replace() churn.
        let pm1 = pm.clone();
        let stop1 = stop.clone();
        handles.push(std::thread::spawn(move || {
            while !stop1.load(Ordering::Relaxed) {
                pm1.replace(vec![
                    TestEntry::stratum("ws://a", "w1"),
                    TestEntry::stratum("ws://b", "w2"),
                ]);
            }
        }));

        // Failure-accounting churn (the reverse-order side).
        let pm2 = pm.clone();
        drop(pm);
        let stop2 = stop.clone();
        handles.push(std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                let _ = pm2.report_failure(0);
                let _ = pm2.report_share_rejection(1);
                pm2.fail_immediately(2);
                pm2.report_success(0);
                pm2.clear_share_rejections(1);
            }
        }));

        // Watchdog thread: if the churn threads deadlock (a lock never released),
        // done is never set and this aborts the process — CI sees a failure
        // instead of a silent hang.
        let done = Arc::new(AtomicBool::new(false));
        let watchdog_done = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(15));
            if !watchdog_done.load(Ordering::Relaxed) {
                eprintln!("DEADLOCK: concurrent replace/fail churn did not finish");
                std::process::abort();
            }
        });
        // Let both churn threads run for a bounded number of iterations.
        std::thread::sleep(std::time::Duration::from_millis(500));
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    }
}
