# Code-quality audit — nexus @9a4b3fd392304db1ee7deb9c612da17d867a73ef (gpt-5.1-codex-max rerun)

## Test/Clippy matrix
- `cargo test --all-targets`: 92/92 passed (11.12s)
- `cargo test --all-targets --features tokio`: 97/97 passed (11.26s)
- `cargo clippy --all-targets`: clean
- `cargo clippy --all-targets --features tokio`: clean

## Findings
### Completeness
1. **High – stratum split is single-consumer on both sides**: the sync stratum core (`src/stratum.rs`) is only consumed by xtminer; edgecase uses just `login_string` (edgecase/src/pool.rs:302) and otherwise runs the tokio version, while the async core (`src/stratum_async.rs`) is only consumed by edgecase (edgecase/src/pool.rs:525-533,542-613; no references in xtminer). This violates the crate’s “shared plumbing both miners need identically” contract and leaves two divergent transports paid twice. Suggested fix: move one of the two into its consumer or collapse to a single shared transport (tokio or sync) with a compatibility shim, then drop the unused surface from this crate.
2. **Medium – dead public trait**: `JobParser` is exposed as the official job codec extension point (`src/stratum.rs:300-309`) but is unused in both consumers (`rg "JobParser" ~/xtminer ~/edgecase` → none). Public-but-unimplemented API in a rev-pinned library is contract debt. Suggested fix: remove or hide the trait (use `ParseFn` only) until a second consumer needs it, or gate it behind a private/internal module.

### Efficiency
3. **Medium – per-line realloc + double copy in hot stratum paths**: the sync reader allocates a fresh `Vec` on every `read_line_capped` call and then clones into a `String` (`src/stratum.rs:121-170`), and the tokio version does the same (`src/stratum_async.rs:95-133`). These run on every line for every pool connection; they burn two allocations and two copies per line. Suggested fix: reuse the caller’s output buffer (fill the provided `String`/`Vec` in place) or stash a reusable scratch buffer alongside the reader to avoid per-line heap churn.

### Style/Conventions
4. **Low – missing header**: `src/stratum_async.rs` is the only source file missing the required `Copyright (c) 2026 PotR3288` + `SPDX-License-Identifier: BSD-3-Clause` header (lines 1-2). Add the standard header to keep the license sweep consistent.

## Verified non-findings
- **GPL-freedom**: `rg -n "GPL" src` only returns the license invariant in `src/lib.rs:13-14`; no GPL-derived code or deps present.
- **Config masking coverage**: the API surface routes masked configs supplied by the consumers; both miners call `nexus::config_util::mask_secrets` in their `ApiHooks::config_json` paths (xtminer/src/api.rs:55-68, edgecase/src/api.rs:55-68) so the shared masker is consistently applied.
- **Doc comments freshness**: sampled 15 doc comments across `api.rs` (10-21), `stratum.rs` (13-21), `stratum_async.rs` (22-41), `watch.rs` (5-15), and `work_source.rs` (93-124); all matched current behavior and line references.

## Summary table
| Category      | High | Medium | Low | Info |
|---------------|------|--------|-----|------|
| Completeness  | 1    | 1      | 0   | 0    |
| Efficiency    | 0    | 1      | 0   | 0    |
| Idiomatic     | 0    | 0      | 0   | 0    |
| Style         | 0    | 0      | 1   | 0    |

## Overall assessment
Core surfaces are well-tested (both feature configs green, clippy clean) and the GPL-freedom and masking invariants hold, but the shared/one-surface contract is drifting: the stratum layer is effectively two single-consumer transports and `JobParser` is public but unused. Tightening the shared surface (merge or relocate the stratum halves, prune dead API) and fixing the hot-path per-line allocation would bring the crate back in line with its "shared plumbing" charter; otherwise the code is stable and documented.

## Adjudication (two-model cross-check, 2026-11-12)
Cross-checked against the independent deepseek-v4-pro re-run report (`audit-report-quality-deepseek-v4-pro-rerun.md`). Every finding below was verified against source at HEAD `9a4b3fd`.

**Convergent — both models flagged (high confidence):**
- **Stratum single-consumer split (HIGH, top item).** CONFIRMED precisely: xtminer imports the sync `Session`/`ReaderHooks` (+login_string, target_to_difficulty, RECONNECT_DELAY) from `nexus::stratum` (xtminer/src/stratum.rs:59–62); edgecase pulls ONLY `login_string` from that module and runs its entire session on `nexus::stratum_async` (edgecase/src/pool.rs:524–589). So the crate now carries two full parallel stratum transports, each consumed by exactly one miner — a direct violation of the lib.rs:5 "host plumbing both miners need identically" charter. Both models flagged it (deepseek C3 Med; gpt #1 High); I rate HIGH. This is a design decision, not a mechanical fix: either migrate xtminer onto the async core (unify on tokio) or accept the split deliberately — document it, soften the charter wording, and prune the sync surface to just what xtminer uses.
- **JobParser dead public trait (LOW–MED).** CONFIRMED 0 refs in both consumers. Public-but-unimplemented extension point = contract debt in a rev-pinned lib. Prune or make private until a second consumer needs it.
- **stratum_async.rs missing header (LOW).** CONFIRMED — the only file lacking the copyright/SPDX header. Trivial.

**Deepseek-only (verified real; gpt missed):**
- **notify transport layer dead at runtime (MED).** No production `Channel` impl in either miner (only a test double); both construct empty channel lists. Delivery actually goes via CentCom EventLog::drain → post_events. The Channel/Dispatcher/EmailEvent sub-layer is re-exported by both consumers but never instantiated with a real carrier — dead surface.
- **DonationStrategy::Window unreachable + unused cron/chrono deps (MED).** builtin_config hard-codes Cycle; zero Window refs in either consumer; yet `cron` and `chrono` are compiled into both for an unreachable path. Two deps paid by both consumers for nothing.
- **log_tail_of_file whole-file read (MED).** api.rs:497 does `read_to_string(path)` on the whole file then `.rev().take(N)`. With rolling::never that is O(file size) per /log poll on a multi-day log — real efficiency issue, not O(N).
- **More dead public items (LOW).** SubmitMsg, watch::f64_changed, onboard::confirm_with, endpoint_host_port/tcp_reachable, logging::miner_log_dir all 0 refs in both consumers — prune candidates alongside JobParser.

**GPT-only (partially valid; deepseek missed):**
- **Sync read_line_capped per-line Vec alloc (LOW).** PARTIALLY VALID: the sync reader does allocate a fresh `Vec::new()` per call then copy into the caller's String (confirmed). But gpt's "the tokio version does the same" is WRONG — the async reader takes `out: &mut Vec<u8>` from the caller and reuses it. And stratum traffic is low-frequency, so impact is Low, not Medium. Cheap micro-opt (reuse a scratch buffer) worth doing opportunistically; not a perf priority.

**Deepseek-only style/doc (LOW):** I2 both consumers duplicate logs/miner.log path that miner_log_dir() provides; I3 two parallel "load config" tri-state enums with different vocabularies (the known future-work config-loading extraction); S2 stale stratum.rs:17–21 module doc (says async migration is future work — it has landed for edgecase); S3 stale f64_changed doc in watch.rs.

**Net:** nexus @9a4b3fd is stable and well-tested (both feature configs green, clippy clean; GPL-freedom + masking invariants hold), but the re-run surfaced real surface-area debt that predates the original audit: the stratum split (HIGH, design decision) plus a cluster of dead-surface items (notify layer, donation window+deps, several dead public symbols) and one genuine /log efficiency issue. The mechanical prunes can ship independently of the stratum design call.
