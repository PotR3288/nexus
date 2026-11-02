# Donation Addresses Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a configurable donation mechanism to `xtminer` and `edgecase`: a weighted rotation over one or more donation addresses, active during timed windows (UTC), with forced re-login at window boundaries.

**Architecture:** A shared policy engine in `nexus::donation` decides, per unit of work, whether the donation wallet replaces the primary wallet. `edgecase` consumes it in both solo mode (per-template coinbase address) and pool mode (login string); `xtminer` consumes it in both stratum and proxy modes (login string). Window boundaries trigger a timer-driven re-login through the existing failover machinery.

**Tech Stack:** Rust; `cron` crate for schedule parsing (UTC only); `serde` for TOML config; existing `nexus::stratum` login plumbing.

**Spec:** `docs/superpowers/specs/2026-11-02-donation-addresses-design.md`

## Global Constraints

- Feature off by default: `enabled = false`.
- Cron schedules evaluated in **UTC only**.
- `cycle_len` derived as `ceil(1/fraction)`, clamped to `10_000` max.
- Malformed donation address → fail loud at config load: donations disabled for the run, error logged, miner keeps mining on the primary wallet.
- `worker_id` and `worker_separator` are never replaced by donation logic.
- Config files `config-sample.toml` and `config.toml` must be kept in sync in both miner repos.
- Commit order: nexus first, then xtminer and edgecase.
- Conventions: `Copyright (c) 2026 PotR3288` + SPDX headers on new/changed own-code files; commit with `git -c commit.gpgsign=false`.

## Review Focus

These inputs/conditions are the ones most likely to bite an operator, and no task's tests alone cover them — each line gets a test added to the owning task:

1. **Window spans midnight / DST irrelevant but schedule-crooked** — e.g. `schedule = "50 23 * * *"` with `duration_minutes = 20` crosses midnight; openness must be true from 23:50 to 00:10 UTC. Test: Task 2.
2. **Weighted rotation with `total_weight` not divisible by weight** — e.g. weights {3, 2}: slots 0-2 → addrA, 3-4 → addrB, then repeats. Test: Task 2.
3. **Zero-weight or zero-length address list** — config validation must reject empty `[[donations.addresses]]` or a `weight = 0` entry as a config error. Test: Task 3.
4. **Malformed donation address** — `[donations.addresses] address = "not-an-address"` must trigger the fail-loud path, not panic or silently donate. Test: Task 3.
5. **Forced re-login while a window is open, connection drops mid-window, reconnects — must re-login as donation, not primary.** Test: Task 5.

---

### Task 1: `nexus::donation` — types and config surface

**Files:**
- Create: `~/nexus/src/donation.rs`
- Modify: `~/nexus/src/lib.rs` (add `pub mod donation;`)
- Modify: `~/nexus/Cargo.toml` (add `cron = "0.12"` dependency)
- Test: `~/nexus/src/donation.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new; uses std + `serde` (already a nexus dep) + `cron`.
- Produces: `DonationConfig`, `DonationStrategy`, `WeightedAddress`, `ResolveInput`, `resolve()` — the exact API Tasks 4-7 consume.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn resolve_disabled_returns_none() {
    let cfg = DonationConfig { enabled: false, strategy: default_strategy(), addresses: default_addresses() };
    let input = ResolveInput { now: SystemTime::UNIX_EPOCH, rotation: 5 };
    assert!(resolve(&cfg, &input).is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p nexus donation:: --no-run`
Expected: FAIL with "unresolved import `crate::donation`"

- [ ] **Step 3: Implement `DonationConfig`, `DonationStrategy`, `WeightedAddress`, `ResolveInput`, `resolve()` in `~/nexus/src/donation.rs`**

```rust
pub struct DonationConfig {
    pub enabled: bool,
    pub strategy: DonationStrategy,
    pub addresses: Vec<WeightedAddress>,
}

pub enum DonationStrategy {
    Window { schedule: String, duration_minutes: u32 },
    Cycle { fraction: f64 },
}

pub struct WeightedAddress { pub address: String, pub weight: u32 }
pub struct ResolveInput { pub now: SystemTime, pub rotation: u64 }

/// Returns Some(donation_address) or None (use primary wallet).
pub fn resolve(cfg: &DonationConfig, input: &ResolveInput) -> Option<String>;
```

Implementation notes:
- `resolve()` returns `None` immediately if `!cfg.enabled`.
- `Window`: use `cron::Schedule::from_str` to parse `schedule`, evaluate occurrences against `now` (UTC), and check if `now` falls within `[occurrence_start, occurrence_start + duration_minutes)`.
- `Cycle`: `cycle_len = ceil(1.0/fraction).clamp(1, 10_000)` as integer; open iff `rotation % cycle_len < (fraction * cycle_len as f64).round()`.
- When open: pick address by weighted rotation — accumulate weights in order, target = `rotation % total_weight`, walk the prefix sums until `target` falls into a bucket.
- New file carries `Copyright (c) 2026 PotR3288` + `// SPDX-License-Identifier: BSD-3-Clause`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nexus donation::`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd ~/nexus
git -c commit.gpgsign=false add src/donation.rs src/lib.rs Cargo.toml
git -c commit.gpgsign=false commit -m "nexus: add donation module (DonationConfig, resolve)"
```

---

### Task 2: `nexus::donation` — window boundary and weighted rotation tests

**Files:**
- Modify: `~/nexus/src/donation.rs` (test module only, unless a bug surfaces)
- Test: `~/nexus/src/donation.rs` tests

**Interfaces:**
- Consumes: `DonationConfig`, `resolve()` from Task 1.
- Produces: pinned test cases proving the Review Focus conditions 1 and 2.

- [ ] **Step 1: Write the failing test for midnight-crossing window**

```rust
#[test]
fn window_crossing_midnight_is_open_on_both_sides() {
    // schedule "50 23 * * *", duration 20min. 23:55 and 00:05 must both be "open".
    let cfg = window_cfg("50 23 * * *", 20, vec![weighted("donA", 1)]);
    let late = resolve(&cfg, &ResolveInput { now: utc(23, 55), rotation: 0 });
    let early = resolve(&cfg, &ResolveInput { now: utc(0, 5), rotation: 0 });
    assert!(late.is_some(), "23:55 UTC should be inside the window");
    assert!(early.is_some(), "00:05 UTC (after midnight) should still be inside the window");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p nexus donation:: -- window_crossing_midnight`
Expected: FAIL — `early` returns `None` because naive cron check only considers the same-day occurrence.

- [ ] **Step 3: Write the failing test for weighted rotation**

```rust
#[test]
fn weighted_rotation_respects_weights_across_cycle() {
    // weights [3, 2]: slots 0,1,2 -> addrA; 3,4 -> addrB; 5 -> wraps back to addrA.
    let cfg = cycle_cfg(1.0/5.0, vec![weighted("A", 3), weighted("B", 2)]);
    let picks: Vec<&str> = (0..5).map(|r| resolve(&cfg, &ResolveInput { now: utc(0,0), rotation: r }).unwrap()).collect();
    assert_eq!(picks, vec!["A", "A", "A", "B", "B"]);
}
```

- [ ] **Step 4: Run test to verify it passes** (rotation logic should already be correct from Task 1; this pins it)

Run: `cargo test -p nexus donation:: -- weighted_rotation`
Expected: PASS

- [ ] **Step 5: Fix the midnight-crossing implementation and re-run**

Adjust the window-openness logic in `resolve()` to check both the same-day occurrence and the previous-day occurrence (or use `Schedule::after` scanning backwards across the day boundary).

Run: `cargo test -p nexus donation::`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cd ~/nexus
git -c commit.gpgsign=false add src/donation.rs
git -c commit.gpgsign=false commit -m "nexus: pin donation window boundaries and weighted rotation"
```

---

### Task 3: Donation config validation and parsing (shared)

**Files:**
- Modify: `~/nexus/src/donation.rs` (add `validate()` method)
- Modify: `~/xtminer/src/config.rs` (add `[donations]` section → `DonationConfig` struct)
- Modify: `~/edgecase/src/config.rs` (add `[donations]` section → `DonationConfig` struct)
- Modify: `~/xtminer/config-sample.toml` + `~/xtminer/config.toml` (add commented-out `[donations]` block)
- Modify: `~/edgecase/config-sample.toml` + `~/edgecase/config.toml` (same)
- Test: `~/nexus/src/donation.rs` (validation tests); `~/xtminer/src/config.rs` (parse tests); `~/edgecase/src/config.rs` (parse tests)

**Interfaces:**
- Consumes: `DonationConfig` from Task 1; each miner's existing config parse flow.
- Produces: `nexus::donation::validate(&DonationConfig) -> Result<(), DonationError>` used by both miners at load; serde-compat `[donations]` TOML surface in both miners.

- [ ] **Step 1: Write the failing validation tests in `~/nexus/src/donation.rs`**

```rust
#[test]
fn validate_rejects_empty_addresses() {
    let cfg = DonationConfig { enabled: true, strategy: default_strategy(), addresses: vec![] };
    assert!(validate(&cfg).is_err());
}
#[test]
fn validate_rejects_zero_weight() {
    let cfg = DonationConfig { enabled: true, strategy: default_strategy(), addresses: vec![WeightedAddress { address: "x".into(), weight: 0 }] };
    assert!(validate(&cfg).is_err());
}
#[test]
fn validate_rejects_malformed_address() {
    let cfg = DonationConfig { enabled: true, strategy: default_strategy(), addresses: vec![WeightedAddress { address: "not-an-address".into(), weight: 1 }] };
    assert!(validate(&cfg).is_err());
}
#[test]
fn validate_allows_disabled_with_garbage() {
    let cfg = DonationConfig { enabled: false, strategy: default_strategy(), addresses: vec![WeightedAddress { address: "not-an-address".into(), weight: 1 }] };
    assert!(validate(&cfg).is_ok(), "disabled donations must not block config load");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p nexus donation:: -- validate`
Expected: FAIL with "cannot find function `validate`"

- [ ] **Step 3: Implement `validate()` in `~/nexus/src/donation.rs`**

```rust
pub fn validate(cfg: &DonationConfig) -> Result<(), DonationError>;
pub enum DonationError { EmptyAddressList, ZeroWeight, MalformedAddress(String), InvalidSchedule(String), InvalidFraction(f64) }
```

- Disabled configs always pass.
- Enabled configs require ≥1 address, every weight > 0, every address parseable by the caller's address parser (nexus owns the call signature; each miner injects its parse fn or the parse happens in Task 4/5 — see Interfaces below for which).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p nexus donation:: -- validate`
Expected: PASS

- [ ] **Step 5: Wire `[donations]` into `~/xtminer/src/config.rs` (struct + serde defaults + parse test)**

Add `#[serde(default)] pub donations: nexus::donation::DonationConfig` to the top-level config struct. Add a test that parses a TOML with `[donations]` and asserts the struct is populated.

- [ ] **Step 6: Wire `[donations]` into `~/edgecase/src/config.rs` (struct + serde defaults + parse test)**

Same pattern as Step 5.

- [ ] **Step 7: Update `config-sample.toml` and `config.toml` in both repos**

Add a commented-out `[donations]` block with all fields shown, per the repo's existing config-sample conventions (all keys present, most commented out, off-by-default values).

- [ ] **Step 8: Run both miners' full test suites**

Run: `cargo test -p xtminer && cargo test -p edgecase`
Expected: PASS (no regression from new config field)

- [ ] **Step 9: Commit**

```bash
cd ~/nexus
git -c commit.gpgsign=false add src/donation.rs
git -c commit.gpgsign=false commit -m "nexus: donation config validation"
cd ~/xtminer
git -c commit.gpgsign=false add src/config.rs config-sample.toml config.toml Cargo.toml Cargo.lock
git -c commit.gpgsign=false commit -m "xtminer: wire [donations] config section"
cd ~/edgecase
git -c commit.gpgsign=false add src/config.rs config-sample.toml config.toml Cargo.toml Cargo.lock
git -c commit.gpgsign=false commit -m "edgecase: wire [donations] config section"
```

---

### Task 4: edgecase solo-mode integration (per-template coinbase)

**Files:**
- Modify: `~/edgecase/src/work.rs` (`fetch_work`)
- Test: `~/edgecase/src/work.rs` (existing test module, if present) or a new integration test

**Interfaces:**
- Consumes: `nexus::donation::{DonationConfig, resolve, ResolveInput}` from Tasks 1-3.
- Produces: solo-mode behavior change only; pool mode handled in Task 5.

- [ ] **Step 1: Write the failing test**

Test that `fetch_work` uses the donation address when a window is open. Because `fetch_work` requires a live gRPC client, test the resolution helper instead: extract the wallet-selection logic into a pure function `select_wallet(cfg: &Config, height: u64, now: SystemTime) -> String` and test that directly.

```rust
#[test]
fn select_wallet_returns_donation_address_when_window_open() {
    let cfg = test_config_with_donations_window_open(); // enabled, window open at `now`
    let wallet = select_wallet(&cfg, /* height */ 42, /* now */ utc(12, 30));
    assert_eq!(wallet, "donA");
}
#[test]
fn select_wallet_returns_primary_when_window_closed() {
    let cfg = test_config_with_donations_window_closed();
    let wallet = select_wallet(&cfg, 42, utc(12, 30));
    assert_eq!(wallet, "primaryA");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgecase -- select_wallet`
Expected: FAIL with "cannot find function `select_wallet`"

- [ ] **Step 3: Implement `select_wallet()` in `~/edgecase/src/work.rs`**

```rust
fn select_wallet(cfg: &Config, height: u64, now: SystemTime) -> String {
    match nexus::donation::resolve(&cfg.donations, &ResolveInput { now, rotation: height as u64 }) {
        Some(addr) => addr,
        None => cfg.mining.wallet_address.clone(),
    }
}
```

Call it from `fetch_work` where `wallet_address` is currently derived from `cfg_snapshot.mining.wallet_address`, replacing the direct read. Add an `info!` log line when the donation wallet is selected, per the spec's audit requirement.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p edgecase -- select_wallet`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd ~/edgecase
git -c commit.gpgsign=false add src/work.rs
git -c commit.gpgsign=false commit -m "edgecase: solo-mode donation wallet selection"
```

---

### Task 5: xtminer stratum-mode integration (login-string swap + forced re-login)

**Files:**
- Modify: `~/xtminer/src/stratum.rs` (login-string construction)
- Modify: `~/xtminer/src/pool_failover.rs` (window-boundary timer driving reconnect)
- Test: `~/xtminer/src/stratum.rs` test module; `~/xtminer/src/pool_failover.rs` test module

**Interfaces:**
- Consumes: `nexus::donation::{DonationConfig, resolve, ResolveInput}` from Tasks 1-3.
- Produces: xtminer's donation-aware login string and boundary-triggered re-login. edgecase pool mode (Task 6) reuses this pattern.

- [ ] **Step 1: Write the failing login-string test**

```rust
#[test]
fn login_string_uses_donation_wallet_when_window_open() {
    // Existing login_string builds from cfg.wallet_address.
    // New behavior: a helper `effective_wallet(cfg, now, session_counter)` returns
    // donation address if resolve() says so, else the primary wallet_address.
    let cfg = test_cfg_with_donations_window_open();
    let w = effective_wallet(&cfg, utc(12, 30), /* session */ 0);
    assert_eq!(w, "donA");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xtminer -- effective_wallet`
Expected: FAIL with "cannot find function `effective_wallet`"

- [ ] **Step 3: Implement `effective_wallet()` in `~/xtminer/src/stratum.rs`**

```rust
fn effective_wallet(cfg: &Config, now: SystemTime, session_counter: u64) -> String {
    match nexus::donation::resolve(&cfg.donations, &ResolveInput { now, rotation: session_counter }) {
        Some(addr) => addr,
        None => cfg.pools[/* active */].wallet_address.clone(),
    }
}
```

Thread this into the existing `login_string()` call site so the donation address replaces the wallet but `worker_id` and `worker_separator` are preserved unchanged.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xtminer -- effective_wallet`
Expected: PASS

- [ ] **Step 4b: Wire `effective_wallet()` into the proxy-mode call site**

`http_client.rs:137` builds `getblocktemplate_with_wallet(id, addr, extra_nonce)`. The `addr` argument must come from `effective_wallet()` when a `[pool]` entry has `source = "proxy"`. Add a test:

```rust
#[test]
fn proxy_getblocktemplate_uses_donation_wallet_when_window_open() {
    // The proxy path builds a JSON-RPC getblocktemplate request with the
    // wallet in params; verify the wallet passed is the donation address
    // when resolve() is due, and the primary wallet when not.
}
```

The `pool_failover.rs` boundary tick (Steps 5-8 below) covers both proxy and stratum reconnects, since the manager is protocol-agnostic (`WorkSource::Proxy` and `WorkSource::Stratum` entries share one failover chain).

- [ ] **Step 5: Write the failing forced-re-login test**

```rust
#[test]
fn window_boundary_triggers_relogin() {
    // Simulate: session started at T; window opens at T+delta; manager must
    // issue a reconnect signal at exactly the boundary.
    let mgr = PoolManager::new(test_pools(), test_donation_cfg());
    mgr.tick_at(utc(12, 29));  // no boundary yet
    assert!(!mgr.reconnect_requested());
    mgr.tick_at(utc(12, 30));  // boundary crossed
    assert!(mgr.reconnect_requested(), "window open must force a re-login");
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test -p xtminer -- window_boundary_triggers_relogin`
Expected: FAIL with "no method named `tick_at`"

- [ ] **Step 7: Implement boundary-driven re-login in `~/xtminer/src/pool_failover.rs`**

Add a periodic tick (e.g. every 30 s) to the manager's existing event loop. On each tick, call `nexus::donation::resolve()` with the current wall clock. If the resolve output (Some/None) has *changed* since the last tick, trigger the ordinary reconnect path (`PoolManager::fail_immediately` or the equivalent reconnect signal). No new connection machinery — this only nudges the existing one.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p xtminer -- window_boundary`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
cd ~/xtminer
git -c commit.gpgsign=false add src/stratum.rs src/pool_failover.rs
git -c commit.gpgsign=false commit -m "xtminer: donation login swap + window-boundary re-login"
```

---

### Task 6: edgecase pool-mode integration (mirror of Task 5)

**Files:**
- Modify: `~/edgecase/src/pool.rs` (login-string construction + boundary re-login)
- Test: `~/edgecase/src/pool.rs` test module

**Interfaces:**
- Consumes: the same `nexus::donation` API; mirrors xtminer's Task 5 pattern.
- Produces: pool-mode donation behavior for edgecase.

- [ ] **Step 1: Write the failing login-string test**

Same shape as Task 5 Step 1, adapted to `edgecase/src/pool.rs`'s existing login construction (the stratum login string is built from the same `wallet_address` field the solo path reads).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgecase -- effective_wallet`
Expected: FAIL

- [ ] **Step 3: Implement the login-string swap**

Mirror Task 5 Step 3: a local `effective_wallet()` helper, then thread it into `pool.rs`'s login-string construction. Preserve `worker_id`/`worker_separator`.

- [ ] **Step 4: Write the failing forced-re-login test**

Same shape as Task 5 Step 5, adapted to `edgecase/src/pool.rs`'s connection manager.

- [ ] **Step 5: Implement boundary-driven re-login**

Same pattern as Task 5 Step 7: periodic tick, detect resolve-output change, trigger reconnect via the existing machinery.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p edgecase -- effective_wallet window_boundary`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cd ~/edgecase
git -c commit.gpgsign=false add src/pool.rs
git -c commit.gpgsign=false commit -m "edgecase: pool-mode donation login swap + boundary re-login"
```

---

### Task 7: End-to-end smoke verification

**Files:**
- Modify: `~/xtminer/tests/config-smoke.toml` (add `[donations]` section)
- Modify: `~/edgecase/config-smoke.toml` or equivalent (add `[donations]`)
- Test: manual live verification (existing conventions: mock proxy + API probes)

**Interfaces:**
- Consumes: all prior tasks.
- Produces: live evidence that donation logic works end-to-end without breaking normal mining.

- [ ] **Step 1: Add a `[donations]` block to `~/xtminer/tests/config-smoke.toml`**

Enabled, `strategy = "window"`, a short schedule that fires during the smoke run, one donation address. Record the exact block used.

- [ ] **Step 2: Start the mock proxy and run xtminer against it**

Run: `python3 ~/xtminer/tests/mock_proxy.py` (background), then start xtminer with the smoke config.

- [ ] **Step 3: Verify via API probe**

Probe `http://127.0.0.1:18088` (or the repo's configured API port) and inspect `logs/miner.log`:
- During the window: log shows the donation wallet in the login attempt; API reports the donation address.
- After the window: log shows reversion to the primary wallet; API reports the primary address.

- [ ] **Step 4: Same live check for edgecase (solo + pool mode if reachable)**

If a local base node / mock pool is available in this session's environment, run the same probe sequence. Otherwise, record what was verified live vs. what remains covered only by unit tests.

- [ ] **Step 5: Confirm disabled-path byte-for-byte behavior**

Run the smoke config with `[donations].enabled = false` (default) and diff the API output/log lines against the pre-change baseline. Must be identical.

- [ ] **Step 6: Commit any test-fixture changes**

```bash
cd ~/xtminer
git -c commit.gpgsign=false add tests/config-smoke.toml
git -c commit.gpgsign=false commit -m "xtminer: donations smoke config"
cd ~/edgecase
git -c commit.gpgsign=false add config-smoke.toml
git -c commit.gpgsign=false commit -m "edgecase: donations smoke config"
```

---

### Task 8: Push (nexus first, then miners)

**Files:** no new changes — just the push step.

- [ ] **Step 1: Push nexus**

```bash
cd ~/nexus && git -c commit.gpgsign=false push origin master
```

- [ ] **Step 2: Push xtminer and edgecase**

```bash
cd ~/xtminer && git -c commit.gpgsign=false push origin master
cd ~/edgecase && git -c commit.gpgsign=false push origin master
```
