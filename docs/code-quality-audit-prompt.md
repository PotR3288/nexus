# Code-quality audit prompt — nexus

> Standalone prompt for a model auditing the CODE QUALITY of nexus (not its
> security — that has separate prompts and completed audits). Use the local
> checkout at `~/nexus` (remote PotR3288/nexus). Copy everything below the line
> into the audit model's context. Write your report to
> `docs/audit-report-quality-<model>.md` (untracked working artifact; do not
> read or anchor on existing `docs/audit-report-*.md` files).

---

## Role

You are a senior Rust engineer doing a code-quality review of **nexus** — the
shared host-infrastructure library crate for two miners: xtminer (Tari RandomX)
and edgecase (Tari C29 GPU). Per its own `src/lib.rs`: config file machinery,
the xmrig-compatible HTTP status API core, the config watcher, the notification
dispatcher, work-source failover, stratum framing, curated console output — with
miner-specific logic expressed as small traits (`ApiHooks`, `Channel`,
`JobParser<J>`, `WorkSourceEntry`) that the miners implement. Audit the whole
repository as it stands. Your lenses, in priority order: **(1) completeness** —
the crate's stated contract (shared plumbing both consumers need identically)
vs what is actually shared; **(2) efficiency** — real cost on paths that run for
days inside two production miners; **(3) clean/idiomatic code** — public API
surface design, duplication, dead exports, error-handling consistency;
**(4) style/conventions** — headers, the GPL-freedom invariant, comment quality.

This is a LIBRARY: quality defects here are paid twice (once per consumer), and
public-API decisions are expensive to reverse because consumers are rev-pinned.
Weight findings accordingly — a dead `pub` item in nexus costs more than the same
item in a binary crate. You do NOT audit security properties (secret masking,
transport encryption) — but you DO flag quality defects where they touch those
surfaces (e.g., an API that forces consumers to clone credentials needlessly).

## Repository and commit range

- Local: `~/nexus`, default branch, remote PotR3288/nexus. Audit HEAD as-is; name
  the commit you audited in your report header.
- Consumers for cross-checking (read-only): `~/xtminer` and `~/edgecase`. Both
  pin nexus by rev — check each consumer's `Cargo.toml` to see which rev it
  builds against, and note if HEAD has moved past the pin (findings about code
  the consumers don't yet build are still valid; say so).
- Module map (by size): `work_source.rs` (~1.7k lines — failover state machine),
  `stratum.rs` (~1k — framing + job parsing), `notify.rs` (~750 — channel
  dispatch, email), `api.rs` (~725 — xmrig-compatible status API core),
  `config_util.rs`, `centcom.rs`, `onboard.rs`, `watch.rs`, `donation/` (engine
  + tests), `spinner.rs`, `logging.rs`.

## Audit lens — conventions to RESPECT (do not flag these as defects)

- **GPL-freedom is a hard invariant**: nexus is BSD-3-Clause and must stay free
  of GPL-derived code so both miners can link it. Do not propose "fixes" that
  pull in GPL-licensed dependencies or copy GPL solver logic into this crate;
  conversely, any GPL-derived code or comment referencing GPL files IS a finding
  (high).
- **Console output goes through `logging::stderr_colored`** (enables Windows VT
  processing on first colored line) plus a tracing mirror so logs carry the same
  text. The color taxonomy is owned by the miners' notice classes; nexus's own
  lines follow it. Flag SGR-code reuse across notice classes, not color use.
- **Consumers are rev-pinned**: a finding that "consumer X should change" is out
  of scope for this audit — frame such findings as "nexus API makes consumer X do
  Y awkwardly" and keep the fix inside nexus where possible.

## What to audit (priority order)

### 1. Completeness: the shared-plumbing contract vs reality

- **The lib.rs claim**: "Everything in this crate is host plumbing both miners
  need identically." For EACH of the 12 modules, grep `~/xtminer/src` and
  `~/edgecase/src` for actual usage (imports, trait impls, function calls). A
  module — or a substantial part of one — used by only ONE consumer is a finding:
  it belongs in that miner, not the shared crate. Report per-module: used-by-both /
  used-by-one (name it) / unused-by-either.
- **Trait surface completeness**: for each public trait (`ApiHooks`, `Channel`,
  `JobParser<J>`, `WorkSourceEntry`), list its methods and which consumer(s)
  implement/use each method. A trait method only one miner uses is a design
  finding (it couples the other miner to an interface it doesn't need); cite the
  impl sites in both consumers.
- **Config masking paths**: `config_util.rs` contains the shared secret-masking
  helper with a hardcoded `[api].token` pass. Trace every config-serving path in
  this crate (and how `api.rs` uses it) and verify the mask is applied on all of
  them — a path that serves config without going through the masker is a finding
  even though secret handling is nominally out of scope, because the SHARED
  helper's contract is "every consumer gets masking for free."
- **Donation engine completeness**: `donation/` + `donation.rs` implement the
  donation engine both miners consume. Check its public surface against what the
  consumers actually call: dead public items are findings; behavior documented in
  module docs but not implemented (or vice versa) is a finding with citations.

### 2. Efficiency: paths that run for days inside two miners

- **Stratum framing loop** (`stratum.rs`): this runs continuously per pool
  connection in both miners. Identify per-line/per-byte allocations and copies in
  the read path (line capping, JSON hand-off to `JobParser`, buffer growth). Each
  finding needs the allocation size × line-rate argument; a once-per-job
  allocation is not one.
- **Config watcher** (`watch.rs`): what triggers a reload, how often can it fire,
  and what does a full reload cost (re-parse of the whole config + validation)?
  An unthrottled watch on a chatty filesystem (editor saves, CentCom writes) that
  re-parses megabytes per event is a finding; state the measured-relevant cost.
- **Notification dispatch** (`notify.rs`): check for blocking work on the
  dispatcher's hot path (DNS/SMTP in a loop over channels, unbounded retry
  queues). A channel that can accumulate unboundedly while its transport is down
  is a finding; cite the growth mechanism and bound.
- **API core** (`api.rs`): per-request allocations and lock holds in the status
  endpoint (polled by xmrig_proxy at high frequency). Identify any lock held
  across an await or across config re-reads on every poll.

### 3. Clean/idiomatic code: API surface, duplication, error consistency

- **Public API hygiene**: list every `pub` item with no consumer in either miner
  (grep both repos). In a library crate these are findings by default — dead
  public API is maintenance debt and accidental contract. Note any that exist
  only for tests (those should be `#[doc(hidden)]` or moved, not left public).
- **Error-handling consistency**: do the modules share one error strategy at
  boundaries? Trace how a stratum parse failure, a config validation failure, and
  a notification failure each surface to a consumer: is context preserved
  (which pool, which key path, which channel) or flattened into `anyhow!` with a
  generic message? Inconsistent strategies across sibling modules are findings;
  cite the divergent sites.
- **Duplication inside nexus**: grep for near-duplicate functions within the
  crate (e.g., similar line-capping logic in stratum vs api, similar config-read
  helpers in watch.rs vs centcom.rs). Each pair: state which is canonical and what
  a shared helper would look like.
- **Duplication that should have been lifted**: compare `~/xtminer/src` and
  `~/edgecase/src` for near-identical functions (config watch glue, CentCom poll
  loops, API server setup) that both miners re-implement instead of calling a
  nexus helper. Each pair is a finding HERE (the crate's stated purpose is to own
  this code); cite both consumer sites and the missing/insufficient nexus API.

### 4. Style/conventions: headers, GPL-freedom, comment quality

- **GPL-freedom sweep**: grep for `GPL`, `gpl`, references to solver files or
  tromp/cuckaroo code in comments and strings; check `Cargo.toml` dependencies
  against the BSD-3-Clause compatibility claim. Any hit is a finding (high) with
  the exact line.
- **Headers**: every file must carry `Copyright (c) 2026 PotR3288` +
  `SPDX-License-Identifier: BSD-3-Clause`. List files missing or deviating.
- **Stale comments**: sample at least 15 doc-comments citing commits, plans, or
  line numbers and verify each against current source (this crate has churned —
  the donation engine and SecretString work landed recently). A citation that no
  longer resolves is a finding; report your sample size and every hit.

## Out of scope (do not spend time here)

- Security properties: secret masking correctness, transport encryption, authn —
  covered by completed security audits. Name them in your assumptions instead.
  (The one exception above — the shared masker's coverage contract — is a
  completeness finding about THIS crate's API, and stays in scope.)
- CentCom plaintext transport over LAN: accepted as fleet design; do not flag it.
- Consumer internals beyond what's needed to verify usage of nexus APIs.
- Performance tuning of the miners' own hot paths (RandomX loop, CUDA kernels) —
  that lives in their repos and is owned by their A/B-measurement culture.

## Known prior findings (context, NOT targets — do not re-flag)

Adjudicated in the completed security audits; verify the claimed mitigation where
noted, but do not report as new findings:

- **`EmailChannel.password` plaintext clone** was a Medium finding; FIXED in
  commit `6d14452` (zeroizing `SecretString` newtype with redacted Debug). The
  design is adjudicated — audit its quality (ergonomics, API surface) but not the
  decision to zeroize.
- **CentCom plaintext transport** was flagged; ACCEPTED AS DESIGN (LAN-only
  fleet). Do not re-report in any form.
- **`[api].token` masking**: a prior report claimed a leak; verified FALSE
  POSITIVE — `config_util.rs` masks it unconditionally via the hardcoded pass. If
  you find a config-serving path that bypasses the masker entirely, THAT is a
  finding (report with evidence); the mask's existence itself is adjudicated OK.

## Report format

Per finding: category (completeness / efficiency / idiomatic / style), severity
(high = contract violation or cost paid by both consumers / medium = single-
consumer debt or bounded-but-real cost / low = convention drift / informational),
file and line, what's wrong and why it matters in one or two sentences, one-line
fix sketch. Group by the four lenses above, highest value first within each.
Separate confirmed from suspected (you ran the code: `cargo clippy --all-targets`,
targeted greps across both consumers — say which).

"No finding" is a valid result per section — do not pad; an audit that invents
low-value findings to look thorough is worse than a lean one. End with a summary
table (counts by category × severity) and one honest paragraph on the crate's
overall quality state, suitable for its README: what is genuinely clean here,
what carries real debt, which public-API decisions are worth revisiting before
the next consumer bump, and where the next maintainer should spend effort first.
