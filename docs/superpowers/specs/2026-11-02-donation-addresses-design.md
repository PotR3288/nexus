# Design: Configurable Donation Addresses for xtminer and edgecase

- Date: 2026-11-02
- Status: draft for review
- Scope: `nexus` (shared crate), `xtminer`, `edgecase`
- Decisions made (owner: PotR3288):
  1. **Time-share only.** No changes to Tari core, no per-block coinbase split. Donation happens by donating a portion of *mining time* (blocks / sessions), which yields the configured portion of rewards in expectation.
  2. **Weighted, deterministic rotation** across one or more donation addresses, over a height/session-based cycle.
  3. **Donation unit = timed window** (predictable %, e.g. donate N minutes every hour) as the primary strategy for both miners; deterministic height-cycle rotation picks *which* address is active while a window is open.

## Intent and why

Miners should be able to give a configurable slice of their mining to one or
more addresses (community pool, developer fund, charity) without changing
protocol code and without trusting any third party to forward funds.

Key verified constraints from the codebase (tari @ fa80d5d, edgecase, xtminer):
- Tari consensus allows up to 1000 coinbase outputs per block
  (`max_block_coinbase_count`, consensus_constants.rs) but the shipped
  `generate_coinbase` API builds exactly one output + one kernel
  (`tari_transaction_components/src/coinbase_builder.rs`), so a per-block
  split would require new core-side transaction construction. Out of scope
  by decision.
- `edgecase` builds its own coinbase per template and already hot-reloads
  `mining.wallet_address` from the shared ArcSwap config per template
  (`edgecase/src/work.rs`, "Live-config integration" header). Swapping the
  address per template is therefore free — no wasted work, no dataset cost.
- `xtminer` is a stratum client; the wallet is only the login string
  `WALLET[SEP]WORKER` (`xtminer/src/stratum.rs`). Its datasets do not depend
  on the wallet, so switching = re-login, ~zero hashrate loss.
- Miners never hold funds (edgecase's coinbase is a one-sided payment to the
  configured address), so post-hoc "donate later" is impossible by design.
  The only lever is *which address receives which unit of work*.

## Goals

- One shared policy engine in `nexus`, consumed by both miners (and any
  future miner), so the logic exists once.
- One or more donation addresses, weighted.
- Configurable, predictable donation share via timed windows.
- Off by default; zero behavior change when disabled.
- Deterministic and auditable: given the config, a timestamp/height, the
  policy's answer is reproducible.

## Non-goals

- No changes to tari transaction/coinbase APIs (no `generate_coinbase_multi`).
- No per-block reward splitting.
- No pool-side or proxy-side payout changes (xtminer still only changes which
  login string it uses).
- No automatic donation estimation/reporting beyond what events already
  provide.

## Anti-goals (failure even if it "works")

- A miner silently donating when the operator did not enable it.
- Donation windows that overlap or drift so the effective share diverges
  meaningfully from the configured fraction without that being visible.
- Any change that degrades hashrate (dataset rebuilds, session churn) when a
  window opens/closes.

## Constraints

- Config conventions: both miners use `config.toml` (+ `config-sample.toml`
  kept in sync); `xtminer` resolves `-c/--config PATH` else `./config.toml`.
- xtminer and edgecase are separate repos with rev-pinned git dependencies on
  `nexus` and `randomx-rs`; new nexus features land in nexus first, then the
  miners bump the pin.
- Features off by default (standing convention).
- Live-config: edgecase values must take effect on the next template without
  restart; xtminer config changes apply per (re)connection.

## The design

### 1. Shared policy in nexus: `nexus::donation`

```rust
pub struct DonationConfig {
    pub enabled: bool,           // default false
    pub strategy: DonationStrategy,
    pub addresses: Vec<WeightedAddress>, // 1..=N, weights > 0
}

pub enum DonationStrategy {
    /// Primary: cron-like timed windows.
    Window { schedule: String /* cron expr */, duration_minutes: u32 },
    /// Alternative: deterministic height/session cycle, no wall clock.
    Cycle { fraction: f64 /* 0.0..=1.0 */ },
}

pub struct WeightedAddress {
    pub address: String,
    pub weight: u32,
}

/// Returns the wallet to use for the current unit of work.
/// Some(address) = donation due; None = use primary wallet.
pub fn resolve(cfg: &DonationConfig, unit: DonationUnit) -> Option<&str>;
```

`DonationUnit` carries what the caller has:
- `Height(u64)` — edgecase templates (also the rotation index for Cycle and
  for weighted address selection inside a Window).
- `Instant(SystemTime)` / `InstantLocal` — xtminer login boundaries.

**Window strategy.** A window is open iff `now` falls inside any occurrence
of `schedule` within `duration_minutes`. While open, the *active donation
address* rotates over the cycle deterministically: address index =
weighted pick by `height (or session counter) % total_weight`. This makes a
multi-address donation reproducible and evenly apportioned over time.

**Cycle strategy.** No wall clock: `height % cycle_len < fraction*cycle_len`
(open), same weighted rotation for address choice. For xtminer (no height),
a monotonic session counter takes the place of height. `cycle_len` is
derived as `ceil(1/fraction)` (clamped to a sane maximum, e.g. 10_000); it
is not a config field. Granularity note goes in the config docs: 5% ⇒ a
1-unit slot in every 20.

**Timezone.** Cron schedules are evaluated in **UTC**, always. Stated in the
config docs so third-party replay of the audit log is exact regardless of
the operator's local timezone or DST.

**API shape.** The resolve signature takes one input struct, not an enum:

```rust
pub struct ResolveInput {
    pub now: SystemTime,      // wall clock (Window openness)
    pub rotation: u64,        // template height / login-session counter
}
pub fn resolve(cfg: &DonationConfig, input: &ResolveInput) -> Option<&str>;
```

**Window-boundary re-login (xtminer & edgecase pool/proxy mode).** A stable
connection can otherwise ride across a window with zero logins, so window
boundaries force a re-login: when a window opens or closes, a timer-driven
reconnect fires (existing failover machinery, driven by a new timer). This
makes the donated share exact, at the cost of one reconnect per boundary.

**Determinism rule.** Same inputs ⇒ same answer. An operator can reconstruct
exactly which addresses received which windows/blocks.

### 2. edgecase integration

- `config.rs`: add `[donations]` section → `DonationConfig` (via nexus types,
  serialized inline; serde-compatible with nexus).
- **Address validation at config load (owner decision):** when
  `[donations].enabled = true`, every donation address is parsed/validated at
  config load (same parsing as `wallet_payment_address`). A malformed
  address disables donations entirely for the run and logs a loud error;
  the miner keeps mining on the primary wallet. It does not refuse to start.
- `work.rs::fetch_work` (solo mode): after reading
  `cfg_snapshot.mining.wallet_address`, ask
  `nexus::donation::resolve(&cfg_snapshot.donations, ResolveInput{ now, rotation: height })`.
  If `Some(addr)`, use that as the coinbase payment address for this template.
- **Pool mode (owner decision: in scope).** `work.rs::pool_driver` uses
  `mining.wallet_address` as the stratum login string, exactly like xtminer.
  The same resolve logic applies there; window boundaries force a re-login
  to the pool with the donation login string (shared with xtminer, see §3).
  The existing hot-reload path already makes this take effect on the next
  template with no restart.
- Logging: a `debug!`/`info!` line per donated template
  (`"donation active: height={h} address=..."`) so runs are auditable.
- No changes to submission logic — the coinbase is just built with a
  different (still valid, one-sided) address.

### 3. xtminer integration

- `config.rs`: same `[donations]` section, same types, same load-time
  validation rule as edgecase (§2).
- `pool_failover.rs` / stratum selection: when a (re)login is about to
  happen, call `resolve(&donations, ResolveInput{ now, rotation: session_counter })`;
  if donation is due, the login string uses the donation address instead of
  the pool's configured `wallet_address` (worker id/separator unchanged so
  pool-side accounting still distinguishes the worker).
- **Forced re-login at window boundaries (owner decision).** xtminer also
  has a proxy (daemon) mode; donation logic applies there identically. A
  new timer-driven reconnect fires at every window open/close even if the
  connection is healthy, so the donated share is exact. Reconnects reuse the
  ordinary failover/connection machinery — the timer just nudges it. No
  `max_session_minutes` config is needed for this; the window edges are the
  only forced re-login points.
- `Window` is the primary strategy (owner decision); `Cycle` with a session
  counter remains available for offline/pinned-time testing.

### 4. Config surface (both miners, identical section)

```toml
[donations]
enabled = false                # off by default
strategy = "window"            # "window" | "cycle"

[[donations.addresses]]        # one or more
address = "tari:addrA"
weight = 3
[[donations.addresses]]
address = "tari:addrB"
weight = 2

[donations.window]             # when strategy = "window"
schedule = "0 12 * * *"
duration_minutes = 36          # 36 min/hour ≈ 5% share

[donations.cycle]              # when strategy = "cycle"
fraction = 0.05                # 5% of blocks/sessions

# xtminer only:
max_session_minutes = 60       # optional re-login cadence
```

Defaults: everything above disabled/empty. `config-sample.toml` gains the
section commented-out; the tracked `config.toml` files are updated in sync
per repo convention.

### 5. What's left to the builder

- Cron parsing/scheduling: pick a crate (`cron` is small and pure; avoid a
  full tokio timer crate) and parse `schedule` at config load, fail loudly on
  an invalid expression.
- Where the donation decision surfaces in each miner's logs/API (`/log`,
  notifications events) — follow each repo's existing event style.
- Version bumps and the order of pushes (nexus first, then xtminer/edgecase),
  per standing convention.
- Tests: nexus policy unit tests are the core (window boundaries, weighted
  rotation, cycle math, disabled short-circuit); miner-side tests follow each
  repo's existing smoke-test pattern (mock proxy + config-smoke.toml for
  xtminer).

## Success criteria

- With `[donations].enabled = false`, both miners behave byte-for-byte as
  today.
- With a 5% window config, edgecase's mined-block addresses over any long
  stretch converge to ≈5% donation (weighted split across addresses per
  weights), and xtminer's login sessions spend ≈5% of wall-clock time logged
  in with donation addresses.
- The decision function is pure and unit-tested; a third party can replay
  which address got which unit from config + timestamps/heights alone.
