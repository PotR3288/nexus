# Code-quality audit report — nexus

- **Audited commit:** `a9a873d57b05792d6126630ded8504c5acc61442` (HEAD, 2026-11-10)
- **Audit model:** deepseek/deepseek-v4-pro (via OpenRouter)
- **Consumer pins:** xtminer → `0109979`, edgecase → `0109979` (same commit)
- **Tests:** 94/94 pass, clippy reports 3 warnings (all pre-existing in `donation.rs`)
- **Scope:** Four lenses — completeness, efficiency, clean/idiomatic, style
- **GPL-freedom sweep:** clean (documented invariant only; no GPL-derived code or deps)

---

## Assumptions

- Security properties (secret masking correctness, transport encryption, authn) are covered by completed security audits. This report flags quality defects only.
- CentCom plaintext transport is accepted as fleet design (LAN-only).
- Config masker coverage is verified working (both consumers call `mask_secrets` in their `config_json()` path).
- Consumers pin the same nexus rev (`0109979`); HEAD (`a9a873d`) is ahead of the pin. Findings apply to HEAD and will reach consumers on their next pin bump.

---

## 1. Completeness: shared-plumbing contract vs reality

### Module usage summary

| Module | xtminer | edgecase | Verdict |
|--------|---------|----------|---------|
| api | ✓ | ✓ | Both |
| centcom | ✓ | ✓ | Both |
| config_util | ✓ | ✓ | Both |
| donation | ✓ | ✓ | Both |
| logging | ✓ | ✓ | Both |
| notify | ✓ | ✓ | Both |
| onboard | ✓ | ✓ | Both |
| spinner | ✓ | ✓ | Both |
| stratum | ✓ | ✓ | Both |
| watch | ✓ | ✓ | Both |
| work_source | ✓ | ✓ | Both |

All 12 modules are used by both consumers. However, several public items within those modules are dead.

### Trait surface completeness

| Trait | Methods | xtminer impl | edgecase impl |
|-------|---------|-------------|---------------|
| `ApiHooks` | `summary`, `backends`, `config_json`, `put_config`, `json_rpc`, `log_tail`, `events` | All 7 | All 7 |
| `Channel` | `name`, `deliver`, `deliver_blocking` | Email only (via nexus `EmailChannel`) | Email only (via nexus `EmailChannel`) |
| `JobParser<J>` | `parse` | **NOT IMPL** | **NOT IMPL** |
| `WorkSourceEntry` | (multiple) | ✓ | ✓ |

### Findings

**C-1 (Medium) — `JobParser` trait is dead public API**
- File: `src/stratum.rs:293-295`
- Neither consumer implements or references this trait. Both use `ParseFn<J>` (the `Arc<dyn Fn>` form) via `ReaderHooks` instead. `JobParser` was the earlier design; `ParseFn` superseded it but the trait was never removed.
- Fix: delete the `JobParser` trait and `ParseFn` type alias (the alias doc-comment now serves only to reference a dead trait).

**C-2 (Medium) — `ConfigLoad<T>` and `load_toml_config` are dead public API**
- File: `src/config_util.rs:28-69`
- Both consumers define their OWN `ConfigLoad` enum (xtminer: `config.rs:148`, edgecase: `config.rs:855`) with identical shape, and their own `Config::load()` method. Neither imports or calls `nexus::config_util::load_toml_config`. The nexus `ConfigLoad` is used only by nexus's own watch tests (`watch.rs:291`).
- Fix: either delete these items (if consumers can't converge), or make the consumers converge on the shared versions. The duplication in config loading (each consumer's `Config::load` is a near-copy of `load_toml_config`) is itself a finding (see C-6).

**C-3 (Medium) — `validate_and_write` is dead public API**
- File: `src/config_util.rs:246-284`
- Neither consumer calls it. Both implement their own PUT /config handling with inline merge+validate+write logic (xtminer: `api.rs:420-520`, edgecase: `api.rs:478-495`).
- Fix: delete, or lift the consumers' put_config flows to use it.

**C-4 (Medium) — `parse_put_body` is dead public API**
- File: `src/config_util.rs:221-236`
- Neither consumer calls it. Both parse PUT bodies inline in their own `put_config` implementations.
- Fix: delete, or converge consumers onto it.

**C-5 (Medium) — CentCom poll/cache infrastructure duplicated in both consumers (~1257 lines)**
- Consumer files: `~/xtminer/src/centcom.rs` (655 lines), `~/edgecase/src/centcom.rs` (602 lines)
- Both implement identical function sets: `save_cache`, `load_cache`, `sleep_interruptible`, `spawn_poller`, `spawn_heartbeat`, `spawn_event_poster`, `startup`, `apply_managed`. The cache-file struct, poll-loop structure, heartbeat ticker, and event-posting loop are near-copies. This is exactly the code nexus's stated purpose says it should own.
- Fix: lift the common CentCom poll/heartbeat/event/cache machinery into a `nexus::centcom::Poller` that takes miner-specific callbacks for config application. The ~80% common code becomes one shared implementation.

**C-6 (Medium) — Config-watch glue duplicated in both consumers (~785 lines)**
- Consumer files: `~/xtminer/src/config_watch.rs` (428 lines), `~/edgecase/src/config_watch.rs` (357 lines)
- Both wrap `nexus::watch::spawn_watcher` with identical loader construction (mapping their `ConfigLoad` → `nexus::watch::LoadError`) and identical CLI-override application. edgecase's module additionally contains reload-diff functions that are largely common.
- Fix: lift the shared ConfigLoader construction and override-application into nexus with a generic helper. The per-miner diffing stays in the miners.

---

## 2. Efficiency: paths that run for days

After tracing the stratum framing loop, config watcher, notification dispatcher, and API core:

- **Stratum `read_line_capped`**: allocates a new `Vec<u8>` per call (line 116). The cap is `MAX_LINE_BYTES` (1 MiB), and typical stratum lines are under 10 KiB arriving every few seconds. Cost argument: ~4-10 KiB allocation a few times per minute per pool connection — not meaningful at mining scale; the line-rate argument does not hold.
- **Stratum `send_message`**: serializes JSON to a new `String` per message (line 94). One per share submit (a few/second per GPU) + one keepalive per 30s. The JSON payloads are under 200 bytes. Cost is negligible.
- **Config watcher** (`watch.rs`): polls mtime every 2s with a 300ms debounce. On change, re-parses the full config (a few KiB). Polling on a chatty editor produces at most one reload per debounce window. Re-parse cost is microseconds for a small TOML file — not meaningful.
- **Notification dispatcher** (`notify.rs`): `spawn_dispatch` joins the existing tokio runtime when available, otherwise spawns a dedicated thread with a mini-runtime. No blocking on the mining hot path. EmailChannel clones itself per dispatch (cheap — three Strings + one SecretString clone with its own zeroizing). No unbounded queues; error accumulation is logged and discarded.
- **API core** (`api.rs`): thread-per-connection, fully synchronous. No locks held across awaits (there are no awaits). Per-request allocations are the parsed `Request` struct and response JSON string — both small and short-lived. The xmrig_proxy polls at ~1 Hz.

**No efficiency findings that meet the bar.** The crate's hot-path design is clean — allocations are bounded and come at transport rates (stratum lines, API polls), not at computation rates (hash iterations, graph generations).

---

## 3. Clean / idiomatic code

**I-1 (Low) — Unused import `UNIX_EPOCH` in donation.rs**
- File: `src/donation.rs:25`
- `use std::time::{SystemTime, UNIX_EPOCH}` — `SystemTime` is used; `UNIX_EPOCH` is not. Leftover from an earlier iteration.
- Fix: remove `UNIX_EPOCH` from the import.

**I-2 (Low) — Negated comparison on partial-ord type**
- File: `src/donation.rs:164` — `if !(fraction > 0.0)`
- Clippy `neg_cmp_op_on_partial_ord`: harder to read than `if fraction <= 0.0` (though `<=` on floats has NaN considerations — the explicit `!(> 0.0)` is arguably correct here).
- Fix: use `fraction.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)` for clarity, or suppress the lint with a comment explaining the NaN exclusion.

**I-3 (Low) — `while let` on iterator**
- File: `src/donation.rs:189` — `while let Some(start) = upcoming.next()`
- Clippy `while_let_on_iterator`: should be `for start in upcoming`.
- Fix: convert to `for start in upcoming.by_ref()` and remove the manual `.next()` calls.

**No error-handling inconsistency finding:** The crate has a consistent pattern — `Result<T, String>` at module boundaries for stratum, config, and API; `anyhow`-style flattened strings with contextual prefixes. Both consumers follow the same pattern. No divergence detected.

**No internal duplication finding:** Searched for near-duplicate functions within nexus (similar line-capping, config-read helpers) — none found. The modules are well-factored with clear boundaries.

---

## 4. Style / conventions

**S-1 (Medium) — Missing SPDX header in logging.rs**
- File: `src/logging.rs:1`
- The file opens with `//! Shared tracing subscriber setup for the miners.` — missing the `// Copyright (c) 2026 PotR3288` and `// SPDX-License-Identifier: BSD-3-Clause` header present in every other source file.
- Fix: add the standard two-line header before the module doc comment.

**GPL-freedom sweep — clean**
- `src/lib.rs:13-14`: documents the GPL-freedom invariant (not a violation — this is the crate's own license statement).
- `src/notify.rs:372,379`: references "Cuckaroo" and "GraphsPerSecond" as unit names for the `RateUnit` enum — descriptive, not GPL-derived.
- `Cargo.toml`: all dependencies are BSD-3-Clause / MIT / Apache-2.0 compatible. No copyleft dependencies.
- No references to solver files, tromp code, or GPL-licensed external sources in comments or strings.

**Stale comment sample** — 18 doc-comments sampled across all modules. Only minor observables:
- `src/stratum.rs:20`: references "xtminer's original header" — accurate (stratum was extracted from xtminer).
- `src/spinner.rs:34`: cadence comment (`~11 fps`) — verified: 90ms frame interval ≈ 11 fps.
- `src/api.rs:285`: comment about POST /json_rpc stalling 10s — describes the pre-fix behavior motivating the search-based header delimiter; the fix is in place and the comment accurately explains the rationale.
- `src/config_util.rs:270`: "watcher's mtime poll picks it up within ~2s" — verified: `POLL_INTERVAL` is 2s.
- No citations to commits, stale line numbers, or plans that no longer resolve.
- **No stale comment findings.**

---

## Summary

| Category | High | Medium | Low | Info |
|----------|------|--------|-----|------|
| Completeness | 0 | 6 | 0 | 0 |
| Efficiency | 0 | 0 | 0 | 0 |
| Idiomatic / Clean | 0 | 0 | 3 | 0 |
| Style | 0 | 1 | 0 | 1 |

### Top 3 findings

1. **C-5/C-6 — Consumer code duplication (~2000 lines)**: Both miners carry near-identical copies of CentCom poll/heartbeat/event/cache infrastructure (~1257 lines) and config-watch glue (~785 lines). This is the crate's stated purpose — "host plumbing both miners need identically" — and it's the highest-value cleanup: one fix removes two copies of the same code.

2. **C-1–C-4 — Four dead public API items**: `JobParser` trait, `ConfigLoad<T>`, `load_toml_config`, `validate_and_write`, and `parse_put_body` in the crate's public surface are all unused by either consumer. In a library crate where public-API decisions are expensive to reverse (consumers are rev-pinned), each dead `pub` item is a tax on every future reader and a hazard if a consumer accidentally grows a dependency on it.

3. **S-1 — Missing SPDX header in logging.rs**: The only source file without the standard `Copyright (c) 2026 PotR3288` / `SPDX-License-Identifier: BSD-3-Clause` header — a one-line fix that removes a license-ambiguity risk.

### Overall quality state

nexus is a clean, well-factored shared library. All 12 modules serve both consumers; the trait surface (`ApiHooks`, `Channel`, `WorkSourceEntry`) is minimal and correctly partitioned (the miners' domain logic stays in the miners). The stratum transport is battle-tested (live-verified against real pools), the config hot-reload pipeline is robust (debounced, atomic, tiered), and the notification dispatch never blocks mining. Tests are thorough (94/94 pass) and cover edge cases like line-flood rejection, keepalive disable, id-correlated share accounting, and config-merge round-trips.

The real debt is not in the code that IS here but in the code that ISN'T: the CentCom poll/cache layer and config-watch glue that both consumers re-implement should be lifted into nexus before the next consumer pin bump. The dead public API items (`JobParser`, `ConfigLoad`, `load_toml_config`, `validate_and_write`, `parse_put_body`) should be pruned at the same time — removing them is a non-breaking change since no consumer references them. The next maintainer should start there: lift the duplicated consumer code, prune the dead API, then bump both consumers' nexus pins and delete their copies.