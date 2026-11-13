# Code-quality audit — nexus @ `9a4b3fd`

Independent re-audit of the shared host-infrastructure library crate (consumed by
xtminer and edgecase). Fresh from source; no prior audit reports consulted.

- **Commit:** `9a4b3fd392304db1ee7deb9c612da17d867a73ef` (`chore: bump version 0.3.0 -> 0.3.8`), master, remote PotR3288/nexus.
- **Consumers pinned at this exact rev:** xtminer `Cargo.toml` pins `9a4b3fd…` (full); edgecase pins `9a4b3fd` (short). HEAD has NOT moved past the pins — every finding below is built by both consumers today.
- **Tool state:** `cargo test --all-targets` → **92/92 pass** (10.99 s). `cargo test --all-targets --features tokio` → **97/97 pass** (11.70 s; the 5 extra are `stratum_async`). `cargo clippy --all-targets` → **0 warnings** (default and `--features tokio` both clean).
- **Assumptions (out of scope, per spec):** secret-masking correctness, transport encryption, authn (completed security audits); CentCom plaintext LAN transport (accepted design); miner hot-path tuning.

---

## 1. Completeness — shared-plumbing contract vs reality

Per-module usage (grep of `~/xtminer/src`, `~/edgecase/src`, read-only):

| Module | Consumers | Note |
|---|---|---|
| `api` | both | both implement `ApiHooks` (all 6 methods + `events`) |
| `centcom` | both | both drive `Client::{fetch_config,post_heartbeat,post_events}` |
| `config_util` | both | `load_toml_file`, `mask_secrets`, `deep_merge_toml`, `json_to_toml_value`, null helpers |
| `donation` | both | via `builtin_config`+`resolve` (see C2) |
| `logging` | both | `init_file_only`, `stderr_colored`, `console_line` |
| `notify` | both | `Event`/`EventKind`/`Rate`/`EventLog` live; `Channel`/`Dispatcher`/`EmailEvent` dead (C1) |
| `onboard` | both | `Check`/`Report`/`endpoint_check`/`ask_with`/`ask_required`/`write_config_atomic` |
| `spinner` | both | `start`/`stop` |
| `stratum` | **one** (xtminer) | full sync `Session` machinery; edgecase uses only `login_string` |
| `stratum_async` | **one** (edgecase) | `spawn_session`/`read_line_capped`; xtminer has no tokio |
| `watch` | both | `spawn_watcher`, `shared`, `apply_external_reload`, `diff_line` |

**C1 — MEDIUM — the `notify` transport layer is dead at runtime in both consumers.**
`Channel` trait (`src/notify.rs:55-73`), `EmailEvent` (`:43-49`), and `Dispatcher::{send,send_blocking,is_enabled}` (`:96-134`) form a fan-out transport with **zero** concrete `Channel` implementations anywhere in either miner. Both consumers construct the dispatcher with a hardcoded empty list — xtminer `src/notifications.rs:383` and edgecase `src/notifications.rs:307` both read `let channels: Vec<Box<dyn Channel>> = Vec::new();` — and both `channels.rs` files are doc-only ("No local channel module today"). So `send()`/`send_blocking()` iterate zero channels and no-op; the `EmailEvent` rendered by each miner's `email_event()` is delivered nowhere. The *actual* delivery path is the shared ring → CentCom: `EventLog::drain()` → `centcom::Client::post_events` (xtminer `src/centcom.rs:484-488`, edgecase `src/centcom.rs:486-490`). The tell is in the source itself: `Channel::name` (`:57`) and `Dispatcher::is_enabled` (`:96`) both carry `#[allow(dead_code)]`. The debt is doubled: both miners `pub use nexus::notify::{Channel, EmailEvent}` (xtminer `src/notifications.rs:254`, edgecase `src/notifications.rs:205`), exporting a carrier interface that no carrier implements.
*Fix:* delete `Channel`/`Dispatcher`/`EmailEvent` (keep `Event`/`EventKind`/`Rate`/`RateUnit`/`EventLog`), or gate them behind a `carriers` feature until a real carrier lands. In a library crate, an entire public transport sub-layer with no implementor is accidental contract.

**C2 — MEDIUM — `DonationStrategy::Window` (cron-window strategy) is unreachable; its `cron`+`chrono` deps are paid by both miners.**
`window_is_open` (`src/donation.rs:180-200`) and the `cron`/`chrono` crates that only it imports are compiled into both consumers but never executed. Both miners reach the engine exclusively through `builtin_config(fraction)`, which hard-codes `DonationStrategy::Cycle` (`src/donation.rs:105-123`) — xtminer `src/config.rs:219-231` (`donation_wallet_for`), edgecase `src/config.rs:592` (`effective_donations`). Neither deserializes a `DonationConfig` nor constructs `Window`, so the timed-window path (and its two dependencies in `Cargo.toml`, used nowhere else in `src/`) is unit-tested dead weight carried by both consumers.
*Fix:* drop the `Window` variant + `cron`/`chrono` (or move them behind a feature) until a consumer opts into time-window donations.

**C3 — MEDIUM — `stratum` transport is single-consumer per variant; "both need it identically" holds only for the small protocol core.**
The sync `Session`/`login`/`ReaderHooks` (`src/stratum.rs`) is used by xtminer only; the async `spawn_session`/`StratumDialect` (`src/stratum_async.rs`) by edgecase only. Genuinely shared: `login_string` (both), `read_line_capped` (both, one port each), `target_to_difficulty` (xtminer only). The lib.rs contract ("everything in this crate is host plumbing both miners need identically") is stretched here: the crate hosts two parallel single-consumer session implementations whose shared value is the protocol *understanding*, not the transport. The module doc (`src/stratum.rs:17-21`) is candid about this, so it is a defensible consolidation of former near-copies rather than an outright misplacement — but it is a real divergence from the stated contract and worth naming.
*Fix:* no move required; either soften the lib.rs wording to "protocol layer + per-transport variants" or document the two-variant design as deliberate.

**C4 — LOW — dead public items with no consumer in either miner (library-crate debt).**
Verified by grep across both repos (0 references each):
- `stratum::JobParser<J>` trait (`src/stratum.rs:307-309`) — zero implementors; its own doc says xtminer passes an equivalent closure via `ParseFn` instead. Dead extension point.
- `stratum::SubmitMsg` (`src/stratum.rs:315-318`) — public but only flows through a private mpsc channel inside `Session`; no consumer names it. Should be `pub(crate)`.
- `watch::f64_changed` (`src/watch.rs:249-251`) — 0 references; the doc claim (below) is stale.
- `onboard::confirm_with` (`src/onboard.rs:287-300`) — 0 references; both miners use `ask_with`/`ask_required` only.
- `onboard::endpoint_host_port` (`src/onboard.rs:154-184`) and `onboard::tcp_reachable` (`:192-203`) — public, called only internally by `endpoint_check`; no direct consumer use.
- `logging::miner_log_dir` (`src/logging.rs:26-30`) — public, called only by `init_file_only`; both consumers re-roll the path instead (I2).

---

## 2. Efficiency — paths that run for days inside two miners

**E1 — MEDIUM — `api::log_tail_of_file` reads the entire unbounded log on every `/log` poll.**
`src/api.rs:497` does `std::fs::read_to_string(path)` — the whole file — then `.lines().rev().take(N)`. Because the file layer is `tracing_appender::rolling::never` (`src/logging.rs:46`), `logs/miner.log` grows for the whole process lifetime (multi-day mining runs). Each poll therefore costs O(total file size) in memory + line-splitting, not O(N); the doc's justification ("the GUI polls at most once a second and N is small", `src/api.rs:486-488`) is wrong — it is the total size that is read, and it grows without bound. Both consumers route `/log` through this exact helper (xtminer `src/api.rs:416-421`, edgecase `src/api.rs:492-497`), so the cost is paid identically by both.
*Fix:* seek to end and read backward (or cap the read to `N × max_line_len` bytes from the tail).

**Verified non-findings (checked, not assumed):**
- *Stratum read loop* (`src/stratum.rs:121-170`, `:628-643`): each line allocates a fresh `Vec` (in `read_line_capped`), a fresh `String` (caller), and a JSON `Value` — but the loop reads one line per `READ_TIMEOUT` slice (5 s) per connection, with real lines (job pushes, submit replies) a handful per second at most. Not a hot path; per-line allocation is not a finding under the spec's line-rate rule.
- *Config watcher* (`src/watch.rs`): mtime-triggered, 2 s poll, 300 ms debounce, re-parses a small TOML only on a settled change. Not unthrottled.
- *Notification dispatch*: no unbounded queue; the event ring is capped at 256 (`src/notify.rs:358`) and `drain()` is non-blocking. CentCom posts run on a dedicated thread with 5 s/10 s timeouts (`src/centcom.rs:211-214`). Loss on outage is documented, not a growth mechanism.
- *API core* (`src/api.rs`): `route` holds no lock and does no config re-read; per-request thread spawn is the documented, deliberate replacement for tiny_http's lost-wakeup hang (`:351-354`). Status polls are once-per-second per worker, so thread-per-connection is acceptable; noted, not flagged.

---

## 3. Idiomatic — API surface, duplication, error consistency

**I2 — LOW — both consumers duplicate the `logs/miner.log` path that `nexus::logging::miner_log_dir()` already provides.**
`miner_log_dir()` (`src/logging.rs:26-30`) exists precisely to own this path, but xtminer `src/api.rs:417-419` and edgecase `src/api.rs:493-495` each re-roll `std::env::current_dir().map(|d| d.join("logs").join("miner.log"))` inline. The shared helper is public and unused — dead API on one side, copy-paste on the other.
*Fix:* consumers call `nexus::logging::miner_log_dir()`.

**I3 — LOW — two parallel "load a config file" tri-state enums with different vocabularies.**
`config_util::LoadResult<T>` (`Loaded`/`NotFound`/`Error`, `src/config_util.rs:31-39`) and `watch::LoadError` (`NotFound`/`Parse`, `src/watch.rs:82-88`) both model the same "absent vs broken vs loaded" split for the same file. The startup path returns `LoadResult`, the hot-reload path returns `LoadError`, so a consumer reasons about two overlapping error shapes for one operation.
*Fix:* unify (have the watcher's loader return `LoadResult`/a shared error) or document why the two must differ.

**Verified non-findings:** `mask_secrets` is invoked by both consumers (xtminer `=5`, edgecase `=4` references) and nexus has no config-serving path of its own — `api.rs` delegates to the consumer's `ApiHooks::config_json`, which is where the masker runs. No config path in this crate bypasses the masker (the `[api].token` pass is unconditional in `mask_secrets`, `src/config_util.rs:102-108`). Trait surface is otherwise minimal and complete: `ApiHooks` (6+1 methods) and `WorkSourceEntry` (5 methods) are fully implemented by both consumers.

---

## 4. Style / conventions

**S1 — LOW — `src/stratum_async.rs` is missing the standard header.**
Every other `.rs` file starts with `// Copyright (c) 2026 PotR3288` + `// SPDX-License-Identifier: BSD-3-Clause`; `stratum_async.rs:1` begins directly with the module doc (it carries a provenance/relicense note at `:10-13` but not the header).
*Fix:* add the two-line header.

**S2 — LOW — stale module doc in `src/stratum.rs:17-21`.**
Says edgecase uses only `login_string` and "Migrating it requires an async (tokio-native) session variant of Session — filed as future work, not part of the current surface." The async variant has now landed (`stratum_async.rs`) and edgecase consumes `spawn_session`/`read_line_capped`; the migration is done.
*Fix:* update the doc to state the async core exists and is edgecase's path.

**S3 — LOW — stale `f64_changed` doc in `src/watch.rs:247-248`.**
Claims "used by both miners' describe functions for threshold-style fields"; verified 0 references in either consumer (dead API, see C4).
*Fix:* delete alongside the dead function.

**Verified clean:**
- **GPL-freedom** — no GPL-derived code or dependency. `grep` for gpl/tromp/cuckaroo/solver found only correct provenance/comment references (`src/lib.rs:13-14` notes edgecase's GPL solver lives *outside* nexus; `src/stratum_async.rs:10-13` is a BSD-3 relicense note). All `Cargo.toml` deps are permissive (MIT/Apache/BSD). No finding.
- **Headers** — 13/14 files correct; only `stratum_async.rs` deviates (S1).
- **Stale-comment sample** — reviewed the module/type doc-comments across all 14 modules (stratum, stratum_async, config_util, api, watch, notify, donation, centcom, onboard, spinner, logging, work_source, lib). Two hits: S2, S3. No other citation failed to resolve.

---

## Summary

| Category | High | Medium | Low | Info |
|---|---|---|---|---|
| Completeness | 0 | 3 (C1, C2, C3) | 1 (C4) | 0 |
| Efficiency | 0 | 1 (E1) | 0 | 0 |
| Idiomatic | 0 | 0 | 2 (I2, I3) | 0 |
| Style | 0 | 0 | 3 (S1, S2, S3) | 0 |
| **Total** | **0** | **4** | **6** | **0** |

**Overall state.** This is a genuinely well-built shared crate: 92/97 tests pass, clippy is clean in both feature configs, the failover state machine (`work_source.rs`) is the most carefully documented and tested code in the repo, error strings and lock ordering are disciplined, and the GPL-freedom invariant holds with no contamination. The real debt is **surface-area drift, not correctness**: the crate's stated contract is "shared plumbing both miners need identically," yet three things contradict it — (1) an entire notification *transport* (`Channel`/`Dispatcher`/`EmailEvent`) that no consumer exercises because delivery moved to CentCom, (2) a donation *window* strategy that no consumer can reach because both go through `builtin_config`'s cycle-only path, and (3) a stratum layer that is two single-consumer transports rather than one shared thing. All three are documented-extension-point or single-consumer code living in a rev-pinned shared crate, where the "paid twice" cost and the "hard to reverse" public-API cost are both maximal. Before the next consumer bump, the highest-value revisits are: **C1** (drop or feature-gate the dead transport — it is the largest dead surface and is re-exported by both miners), **C2** (drop the unreachable window strategy and its two deps), and **E1** (fix the whole-file `/log` read, the one real per-poll cost on a path that runs for days). The dead `pub` items in C4 are cheap to clean in the same pass.
