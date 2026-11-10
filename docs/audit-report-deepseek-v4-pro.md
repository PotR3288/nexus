# Security Audit Report — nexus

**Repository**: PotR3288/nexus (formerly "miner-common")
**Commit**: `e664f49` ("docs: add full-repo security-audit prompt")
**Date**: 2026-11-09
**Auditor**: deepseek/deepseek-v4-pro (OpenRouter)
**Test status**: 93/93 passed, 0 failures

---

## Findings by section

### 1. Line/protocol I/O (hostile-input core)

**No findings in `read_line_capped`.** The function enforces the cap at every call site, clears the buffer before reading (no partial-line leakage), errors on cap-exceeded without newline, correctly handles EOF, and rejects invalid UTF-8. All five unit tests (`read_line_capped_delivers_lines_and_eof`, `read_line_capped_errors_past_cap_without_newline`, `read_line_capped_accepts_line_at_exact_cap`, `read_line_capped_rejects_invalid_utf8`, `login_handshake_flood_is_rejected`) verify these guarantees. The `login` function uses `read_line_capped` with `MAX_LINE_BYTES` (1 MiB), and the reader loop (`run_reader_loop`) likewise. No path bypasses the cap.

**No findings in the handshake/login state machine.** The flood test (`login_handshake_flood_is_rejected`) verifies that a 64 MiB newline-less flood terminates the login with a transport error (either "cap" or "closed"). The reader loop (`run_reader_loop:574-576`) checks `reader_shutdown` before every read tick. The login loop (`login:209-248`) tolerates interim notifications (id ≠ LOGIN_REQUEST_ID → continue) and rejects garbage JSON lines (parse failure → continue). No ordering of hostile frames can desync the parser: every line is independently parsed, and the state machine has exactly two states (awaiting login reply, running reader loop).

**LOW — `read_line_capped` buffer can transiently exceed `cap` by one `fill_buf` chunk** (`stratum.rs:132-137`). The cap check happens AFTER consuming the chunk without a newline. On a `BufReader<TcpStream>`, `fill_buf()` typically returns up to 8 KiB, so the buffer can reach `cap + ~8K` before the error fires. The connection is then killed and the buffer is cleared on the next call. An attacker can cause a ~1 MiB + 8 KiB allocation instead of a clean 1 MiB cap. Minimal practical impact — the connection is dropped at the cap boundary either way, and the allocation is bounded (O(cap + chunk), not O(attacker input)).

**LOW — `next_submit_id` wraps at `u64::MAX`** (`stratum.rs:572,596`). The correlating submit ID starts at `FIRST_SUBMIT_ID=100` and increments per submit without bound. After `u64::MAX` submits, the counter wraps. A wrapped ID could collide with a stale entry in `pending_submits: HashSet<u64>`, causing a share reply to be mis-correlated as a submit reply. At 1 million submits/second, wraparound requires ~585 million years. Practically unreachable, but violates the "never" guarantee in the id-correlation design intent (`stratum.rs:58-59`).

**Panic-site audit (reachable from network input):**

- `stratum.rs:266` — `bytes.try_into().unwrap()` inside `target_to_difficulty`. Safe: guarded by `bytes.len() != 4` check on line 260. The `unwrap` cannot fire.
- `stratum.rs:399` — `job_slot.lock().unwrap()` on a `Mutex`. Poison-tolerant in practice (no `panic!` in any code path that holds this lock), but a poisoned lock on a consumer-side panicking hook would propagate the poison here. All other `lock().unwrap()` sites in `stratum.rs` follow the same pattern, matching the `Mutex::lock` convention used throughout the crate.
- `stratum.rs:633` — `msg.get("params").cloned().unwrap_or(Value::Null)`. Cannot panic (uses `.unwrap_or`).
- `stratum.rs:654` — `.unwrap_or(false)`. Cannot panic.

**No `unsafe` blocks in protocol I/O paths.**

### 2. Shared-state and threading invariants

**No findings.** Lock discipline is consistent (`Mutex<Vec<E>>` before `Mutex<Vec<Candidate>>` — documented at `work_source.rs:210-212` and verified by the deadlock hammer test `concurrent_replace_and_fail_paths_do_not_deadlock`). The stratum session uses `ArcSwap` for config, `AtomicBool` for shutdown, and `mpsc` channels for submit queuing — all standard, well-tested patterns. The reader loop (`stratum.rs:527-544`) drains queued submits on exit (lines 537-541), preventing stuck ack receivers. The `CONSOLE_LOCK` (`spinner.rs:42`) serializes all curated stderr writes and tolerates poison (`spinner.rs:51`), so a panic in one console write cannot wedge all later output.

### 3. Rate tracking / hashrate honesty

**No findings.** Rate tracking logic lives in the consumers (xtminer's EMA/window logic, edgecase's graphs/s). The `notify::Rate` struct (`notify.rs:326-336`) carries explicit unit tagging (`RateUnit::HashesPerSecond` vs `GraphsPerSecond`) so consumers can never coerce or compare across unit types. The `notify::Event` struct omits `None` fields from serialization (`skip_serializing_if = "Option::is_none"`), so "not measured yet" is absent, never a fake 0. The `EventLog` ring (`notify.rs:466-535`) is bounded at 256 entries and drops oldest entries when full — explicitly not an audit log.

### 4. CentCom client surface

**MEDIUM — `ManagedConfig` holds plaintext credentials from CentCom** (`centcom.rs:39-128`). The `ManagedConfig` struct includes `pool_password`, `email_password`, and `api_token` as plaintext `Option<String>` fields. These are deserialized from a CentCom `GET /v1/miners/{id}` response and applied to the miner's config. If CentCom is compromised, all miner credentials fleet-wide are exposed. Mitigation: the CentCom service runs on a trusted LAN over plain HTTP by design (line 19-22). However, within the declared "semi-trusted CentCom" threat model, having plaintext credentials in the wire protocol is a weakness. **Fix in nexus**: the `api_token` field in `ManagedConfig` is the miner's own API token — this should never come from CentCom (the miner generates its own). The `pool_password` and `email_password` are legitimate managed fields but their plaintext transport over HTTP is a documented trade-off.

> **Maintainer decision (2026-11-09):** CentCom stays LAN-only for now — plain HTTP on a trusted LAN is an accepted design choice, not a bug to fix at this time. No TLS work scheduled; revisit if CentCom ever leaves the LAN. The `api_token` observation stands as an open question (drop the field or document why it exists).

**MEDIUM — `EmailChannel` stores password as a plaintext `Clone` field** (`notify.rs:173-184`). The `password: String` is cloned in `deliver()` (line 252) on every notification dispatch. If any code path logs or serializes the `EmailChannel` struct, the password leaks. The struct also derives `Clone` but not `Debug` (avoiding accidental debug-print leakage). Mitigation: `lettre::Credentials` at line 228-231 consumes clones, not references. **Fix in nexus**: wrap `password` in a newtype that implements `Clone` but gates access behind an explicit method, or use `secrecy::Secret<String>`.

> **FIXED (2026-11-09, commit `6d14452`):** `EmailChannel.password` is now a `SecretString` newtype (`notify.rs`) — redacted in `Debug`, value reachable only via `expose()`, and backing bytes zeroized on drop. The per-dispatch channel clone still copies the credential (the dispatch design is unchanged), but each transient copy wipes its own bytes when dropped, and no formatting path can print it. Verified: 94/94 tests pass including a new redaction test; clippy clean for this change.

**No findings in auth/token handling.** The token is sent via `Authorization: Bearer <token>` header (`centcom.rs:249`). The `Client` struct holds the token in a plain `String` (line 222), which is acceptable since it's loaded from the operator's config file, not from network input. The token is never logged — `fetch_config`, `post_heartbeat`, and `post_events` do not log the token or the Authorization header.

**No findings in CentCom response parsing.** `fetch_config` (`centcom.rs:246-261`) deserializes the response body into `ConfigResponse` via `ureq::into_json`. A malformed response returns `Error::Parse`. The `ManagedConfig` uses `#[serde(default)]` on all fields, so missing fields default to `None` rather than causing parse failures — a hostile or buggy CentCom cannot inject partial configs that break deserialization.

**No findings in command-application boundary.** The audit prompt asks whether nexus carries validation or documents that the consumer must. The `ManagedConfig` docstring (`centcom.rs:34-38`) explicitly states: "miners only override fields that are present (see each miner's `centcom` module for the mapping onto its own config)". The `apply_external_reload` function (`watch.rs:170-192`) re-applies CLI overrides after every external reload (including CentCom), and the consumer's `ReloadHooks` perform miner-specific validation and tiering. This is adequate documentation — nexus provides the pipeline; the miner owns the validation.

**LOW — `ureq::get` with raw string interpolation in Authorization header** (`centcom.rs:249`). The header value is `format!("Bearer {}", self.token)`. If `self.token` were ever controlled by network input (it is not — it comes from the operator's config file), this would be an HTTP header injection vector. Since the token is operator-configured and never takes remote input, this is a code-quality concern only. The same pattern exists at lines 268 and 298.

### 5. API design hazards

**No findings in public API types.** The public API uses typed wrappers where appropriate: `Rate` carries `RateUnit`, `SubmitMsg` carries an optional `mpsc::Sender`, `LoginOutcome` is an enum (not a raw `Result`). `Session::submit` and `Session::submit_with_ack` take `serde_json::Value` for params (matching the protocol), which is appropriate — the miner constructs the params from its typed job type.

**LOW — `spawn_server` leaks per-connection thread handles** (`api.rs:384-397`). Each HTTP request spawns a thread via `std::thread::Builder::new().spawn()` whose `JoinHandle` is discarded. Threads are bounded by `Connection: close` (always set in the response) and `REQUEST_READ_TIMEOUT` (10s read timeout), so a slow client can hold a thread for at most 10 seconds. On a loopback-only binding (the documented contract, line 365), only local processes can launch such an attack. **Fix in nexus** (optional): collect handles in a `Vec<JoinHandle>` and join them on server shutdown, or use a thread pool.

**No findings in SemVer/documentation honesty.** All public functions have doc comments. The contract disconnect between `Session::start`'s "reader_shutdown" promise and the `drop(stream)` behavior (line 429-435) is explicitly documented: the comment explains why `stream` is dropped instead of shutdown'd, with a live-verified justification. The `spawn_reader_thread` comment (line 518-519) correctly states that the thread's lifetime is bounded by `reader_shutdown`.

### 6. Other observations

**INFORMATIONAL — Both consumers pin a slightly stale rev** (`xtminer` → `0109979`, `edgecase` → `0109979`). *(Corrected 2026-11-09 against git history:)* the pin is exactly **3 commits** behind HEAD (`e664f49`) — today's `docs/audit-prompt.md`, plus `comments: streamline per audit` and `tests: cull subsumed/vacuous tests per audit`. All donation-engine work (policy engine, hard-coded recipients + operator fraction, real recipient addresses, describe_percent helper, DonationSwitch event kind) landed **before** the pin — `src/donation.rs` exists at rev `0109979`, so consumers are already running the full donation feature. There is no functional gap; a rev-pin bump to HEAD would only pick up docs and cosmetic cleanup commits.

**INFORMATIONAL — 3 Clippy warnings** (pre-existing):
- `donation.rs:164` — `neg_cmp_op_on_partial_ord` (`!(fraction > 0.0)` → `fraction <= 0.0`)
- `donation.rs:189` — `while_let_on_iterator` (manual while-let instead of for loop)

### 7. Unsafe block audit

All four `unsafe` blocks are well-bounded:

1. **`api.rs:438`** — `libc::execv`. Unix-only, runs after a 250ms delay on a dedicated thread. The CString conversion (`api.rs:432-434`) uses `.unwrap()` which fails only on interior NUL bytes in argv values — impossible on Unix for any real path. The `execv` never returns on success; on failure it falls through to `process::exit(1)`.

2. **`logging.rs:138`** — `GetStdHandle` + `GetConsoleMode` + `SetConsoleMode`. Windows-only, called once via `Once`. Handles NULL and INVALID_HANDLE_VALUE fallbacks. Mode bits are OR'd with `ENABLE_VIRTUAL_TERMINAL_PROCESSING` only — never cleared.

3. **`onboard.rs:347`** — `libc::geteuid()`. Reads the effective UID, no mutation.

4. **`onboard.rs:355`** — `libc::getuid()`. Reads the real UID, used only as a fallback for `SUDO_UID` parse failure.

5. **`onboard.rs:369`** — `libc::chown`. Changes ownership of a newly-written config file from root back to the invoking user (sudo detection via `SUDO_UID`). Best-effort; failure is logged but non-fatal. Only reached when `geteuid() == 0` and `SUDO_UID` is set and non-zero.

**No CUDA kernels in nexus.** Confirmed — the crate depends only on Rust libraries (`serde`, `toml`, `libc`, `arc-swap`, `cron`, `chrono`, `tiny_http`, `ureq`, `log`, `tracing-subscriber`, `tracing-appender`, `lettre`, `tokio`).

---

## Findings summary

| Severity      | Count | IDs (section) |
|---------------|-------|---------------|
| Critical      | 0     | —             |
| High          | 0     | —             |
| Medium        | 2     | Credential plaintext in CentCom wire protocol (4); EmailChannel password Clone (4) |
| Low           | 4     | read_line_capped transient over-cap (1); next_submit_id wrap (1); spawn_server thread leak (5); ureq header interpolation (4) |
| Informational | 2     | Stale consumer rev pins (6); 3 pre-existing Clippy warnings (6) |

### Top 3 findings

1. **MEDIUM — CentCom `ManagedConfig` carries `pool_password`, `email_password`, and `api_token` as plaintext `Option<String>`** (`centcom.rs:39-128`). These fields survive deserialization from CentCom and ride into the miner's config. A CentCom compromise exposes fleet-wide credentials. `api_token` in particular should never come from CentCom (the miner owns its own API token).

2. **MEDIUM — `EmailChannel` password stored as `Clone`-able plaintext `String`** (`notify.rs:178`). Cloned on every email delivery (`notify.rs:252`). If any code path serializes or debug-prints the `EmailChannel`, the password leaks. Fix: wrap in a secrecy type. — **FIXED 2026-11-09, commit `6d14452`** (see section 4).

3. **LOW — `spawn_server` leaks per-connection thread handles** (`api.rs:384-397`). Threads are bounded by `REQUEST_READ_TIMEOUT` (10s) and `Connection: close`, but a local attacker on loopback can accumulate threads. Mitigated by loopback-only binding.

---

## Residual risk assessment

Nexus is a well-hardened shared library with demonstrably correct hostile-input handling. The `read_line_capped` primitive, the login-handshake flood defense, and the config mask-token round-trip (GET→PUT survival of secrets) have all been live-verified with TCP-level tests. The two medium-severity findings are credential-hygiene concerns in a semi-trusted LAN deployment model — not exploitable from outside the LAN boundary. The crate carries no `unsafe` in I/O paths, no unbounded allocations reachable from attacker input, and no panic sites that a hostile pool can trigger. The crate is fit for its stated threat model, with the CentCom credential transport being the most significant residual risk.

Clippy reports 3 pre-existing warnings (2 in `donation.rs`, both cosmetic). Test suite: 93 passed, 0 failed. No fuzz harnesses are present — adding a `read_line_capped` fuzz target would strengthen the I/O hardening claims.

---

## Post-audit actions (maintainer, 2026-11-09)

| Finding | Disposition |
|---------|-------------|
| MEDIUM — CentCom plaintext credentials (§4) | **Accepted as design** — CentCom is LAN-only by decision; plain HTTP on a trusted LAN stays. Revisit if it ever leaves the LAN. `api_token`-from-CentCom remains an open question. |
| MEDIUM — EmailChannel password Clone (§4) | **Fixed in commit `6d14452`** — `SecretString` newtype (redacted Debug, `expose()` gate, zeroize-on-drop). 94/94 tests pass post-fix. |

Note: at the time of this audit the miners no longer send email locally (CentCom owns fleet mail delivery); `EmailChannel` is consumed by CentCom via its nexus pin, which will pick up `6d14452` on its next rev bump.