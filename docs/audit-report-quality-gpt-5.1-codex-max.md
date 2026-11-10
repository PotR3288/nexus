# Code-quality audit — nexus (gpt-5.1-codex-max)

- **Commit audited:** a9a873d57b05792d6126630ded8504c5acc61442 (HEAD)
- **Consumer pins:** xtminer → nexus@010997927a546e5ff5c46c88f6530d89092ed6e0; edgecase → nexus@0109979
- **Scope:** Full repository per docs/code-quality-audit-prompt.md. Cross-checked consumers read-only at ~/xtminer and ~/edgecase. Ignored existing docs/audit-report-*.md per instructions.
- **Tests:** `cargo test --all-targets` (pass, 94/94); `cargo clippy --all-targets` (3 warnings: unused import, negated partial-ord compare, while-let on iterator in src/donation.rs).

## Findings

### Completeness

1. **High — Stratum module is xtminer-only despite being in the shared crate.** `src/stratum.rs` exposes the full stratum session/reader stack, but edgecase only calls `login_string` (edgecase/src/pool.rs:303-309) and does not use `Session`/`ReaderHooks`/keepalive logic at all, while xtminer depends on the full module (xtminer/src/stratum.rs:48-60). This violates the crate contract (everything in nexus is supposed to be shared host plumbing) and forces edgecase to carry an unused public API surface. *Fix:* move the xtminer-only stratum client into xtminer (leave a tiny shared helper for `login_string`/`target_to_difficulty` if needed) or split the module so nexus exports only the pieces both miners use.

### Efficiency

2. **Medium — Email dispatch spins a fresh OS thread + Tokio runtime per send.** In `EmailChannel::deliver` and `deliver_blocking` (src/notify.rs:320-367), each notification builds a new thread and current-thread Tokio runtime to run one SMTP future. On hosts with email enabled and periodic alerts, this pays a thread + runtime construction per event for the life of the miner. *Fix:* share a single async runtime/executor per process (or reuse an existing one when present) and dispatch onto it, falling back to a long-lived worker thread instead of per-notification threads.

### Clean / Idiomatic

3. **Medium — Dead public API in config_util and stratum.** The following `pub` items are unused by either consumer and only referenced inside nexus tests: `config_util::ConfigLoad`, `load_toml_config`, `parse_put_body`, `validate_and_write` (src/config_util.rs:23-255), and `stratum::JobParser` (src/stratum.rs:289-295). Leaving these public bloats the crate’s contract and risks accidental consumer coupling. *Fix:* make them crate-private or remove them; if consumers should share the logic, update them to call the shared versions and delete their local copies first.

4. **Low — Clippy hygiene issues in donation.rs.** `src/donation.rs` still carries an unused `UNIX_EPOCH` import (line 25), a negated partial-ord comparison (`if !(fraction > 0.0)` at line 164), and a `while let` over an iterator that clippy flags (line 189). *Fix:* drop the unused import, use `partial_cmp`/`is_sign_positive` for readability, and rewrite the loop as `for start in upcoming`.

### Style / Conventions

5. **Medium — Missing copyright/SPDX header in logging.rs.** Every source file is supposed to carry `Copyright (c) 2026 PotR3288` + `SPDX-License-Identifier: BSD-3-Clause`; `src/logging.rs` is missing both (lines 1-3). *Fix:* add the standard header block.

### Stale-comment check

Sampled 15 doc-comments across stratum (constants, caps, keepalive intervals), watch (poll/debounce), donation (percent semantics), spinner (rendering), api (route ordering), notify (Event vocabulary) and work_source (failover thresholds); all matched current code behavior. No stale-comment findings.

## Summary table (findings)

| Category      | High | Medium | Low | Info |
|---------------|------|--------|-----|------|
| Completeness  | 1    | 0      | 0   | 0    |
| Efficiency    | 0    | 1      | 0   | 0    |
| Idiomatic     | 0    | 1      | 1   | 0    |
| Style         | 0    | 1      | 0   | 0    |

## Overall state

nexus largely matches its contract, with tests passing and only minor clippy noise, but the stratum module remains xtminer-specific in a crate meant to be strictly shared, and the public surface includes unused config/stratum helpers that inflate the API without consumer adoption. Email dispatch currently pays a thread/runtime per notification; tightening that and pruning the dead exports would keep the shared crate leaner. Style-wise, headers are consistent except for logging.rs. Next maintainer focus: split or relocate the stratum client, consolidate config helpers with the consumers or make them private, and tighten notification dispatch to avoid per-event runtime construction.

## Adjudication notes (added after independent verification)

**Finding #1 (stratum xtminer-only): CONFIRMED — and the deepseek report's usage table was wrong.**
Verified by grep: edgecase references exactly one stratum item, `login_string`, at a single call site (`edgecase/src/pool.rs:304`); it implements its own session logic in `src/pool.rs`. xtminer imports the full module — `stratum.rs:49-50`: `login_string`, `target_to_difficulty`, `ReaderHooks`, `Session`, `RECONNECT_DELAY` — and drives a real `Session` with `ReaderHooks` at `stratum.rs:503-509`. The deepseek report's module-usage table (`stratum | ✓ | ✓ | Both`) overstates edgecase's usage; the correct statement is "shared helpers only (login_string, target_to_difficulty); Session/ReaderHooks are xtminer-only." Severity High is defensible under the completeness lens (top priority) but this is dead-weight API surface, not a bug.

**Finding #2 (EmailChannel per-send runtime): mechanism partially confirmed, RECLASSIFIED as dead code.**
`spawn_dispatch` (`notify.rs:147-163`) does join an ambient tokio runtime when one exists — the deepseek report's description of that function was accurate. But in both miner processes there is no ambient runtime on the dispatch path, so every email event would spawn a thread + current_thread runtime; `deliver_blocking` always does (per call). However: **EmailChannel has zero references in either consumer** — grep across `edgecase/src/` and `xtminer/src/` finds no construction or use of any kind. xtminer moved email delivery to CentCom (its `Dispatcher::build` creates zero channels); edgecase never wired it up. So the per-send cost is moot as an efficiency finding; the real issue is that EmailChannel + its dispatch paths are public API with no consumers — a completeness/dead-code finding, and the fifth dead item in the same family as Finding #3 (which deepseek's C-1..C-4 missed).

**Findings #3–#5: CONFIRMED, full convergence with the verified deepseek report.**
Dead-API list matches deepseek C-1..C-4 exactly (JobParser, ConfigLoad/load_toml_config, parse_put_body, validate_and_write — all previously grep-verified as consumer-free). Clippy items match deepseek I-1..I-3 at identical lines (25/164/189 in donation.rs). logging.rs header matches deepseek S-1. No false positives from this report; the two models agree on every shared item, and where they disagreed (stratum usage, EmailChannel characterization) source evidence settled both in favor of this report's substance.
