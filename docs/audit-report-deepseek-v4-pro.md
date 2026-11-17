# Security Audit Report: nexus crate

**Target**: `nexus` (shared mining-core library, rev at HEAD)  
**Consumers**: xtminer, edgecase (pin a nexus rev; pin versions not verified in this audit — see informational finding)  
**Threat model**: hostile pools, semi-trusted consumers, semi-trusted CentCom service  
**Audit scope**: full repository, priority order as specified  
**Consolidation note**: v2 run of deepseek/deepseek-v4-pro (2026-11-17), consolidated with an independent same-model first run and spot-verified against source. Finding 1.1 corrected per verification; finding 5.2 restored from the first run (omitted here, verified in source). First-run findings dropped as overstated or already mitigated: sync capped-reader gap (xtminer enforces its own caps via shared-client config), constant-time length leak (`constant_time_eq` already burns comparable time on mismatch by design).

---

## 1. Line/protocol I/O (hostile-input core)

### Confirmed Findings

#### 1.1 Quadratic I/O amplification in `tail_lines` (log-tail DoS) — FIXED in c150f33
- **Severity**: Critical *(corrected from Low after source verification; first independent run rated this correctly)*
- **File**: `src/api.rs`, function `tail_lines` (line 512) — verified against source
- **Trigger sketch**: Each loop iteration re-seeks to `start` and re-reads the ENTIRE `[start, file_end)` range (`read_exact` into a buffer sized `file_end - start`) instead of only the newly expanded 8 KiB chunk, then expands `start` backward by CHUNK. Total I/O is O(L²/CHUNK): for a 1 GiB log with few newlines that is tens of terabytes of reads per single `/log` poll. A local attacker who can write the log file — or simply long-running log growth plus GUI polling — saturates disk I/O and stalls the miner. Peak memory is also ~file size for newline-free files (secondary effect).
- **Fix**: Read only the newly expanded chunk each iteration (seek to `start`, read CHUNK bytes, prepend), or cap total scanned bytes and return a partial result on overflow. Fix in nexus.

#### 1.2 Thread creation panic in `spawn_server` — FIXED in 07fe760
- **Severity**: Low
- **File**: `src/api.rs`, line `std::thread::Builder::new().name("api-server").spawn(...).expect("failed to spawn API server thread")`
- **Trigger sketch**: If the operating system cannot spawn a new thread (e.g., due to resource limits), the library panics, crashing the entire miner process.
- **Fix**: Return a `Result` or log the error and return `None` instead of panicking. Fix in nexus.

#### 1.3 Potential panic on null byte in command-line arguments during restart — FIXED in 07fe760
- **Severity**: Low
- **File**: `src/api.rs`, function `schedule_restart_with_failure_prefix`, line `CString::new(a.clone()).unwrap()`
- **Trigger sketch**: If a command-line argument contains a null byte (extremely unlikely in normal operation but possible if the binary path is crafted), the `unwrap()` panics.
- **Fix**: Handle the `CString::new` error gracefully (e.g., log and skip restart). Fix in nexus.

#### 1.4 No shutdown mechanism for the API server
- **Severity**: Informational
- **File**: `src/api.rs`, function `spawn_server`
- **Trigger sketch**: The API server thread runs until process exit; there is no way for the consumer to stop it gracefully (e.g., during a controlled shutdown sequence).
- **Fix**: Provide a shutdown signal (e.g., `Arc<AtomicBool>`) to allow the consumer to stop the accept loop. Fix in nexus (or document that the server is intended to run for the process lifetime).

---

## 2. Shared-state and threading invariants

### Confirmed Findings

No findings. Lock discipline in `work_source::PoolManager` follows a consistent order (`pools` → `candidates` → `connected_urls`). The `ArcSwap` in `watch` is used correctly. The console spinner lock (`CONSOLE_LOCK`) is properly scoped and poison-tolerant. No deadlocks or data races were identified.

---

## 3. Rate tracking / hashrate honesty

### Confirmed Findings

No findings. The nexus crate does not contain rate-tracking logic; that responsibility lies with the consumers. The library provides no path to fabricate or inflate hashrate.

---

## 4. CentCom client surface

### Confirmed Findings

#### 4.1 Sensitive fields exposed via `Debug` on `ManagedConfig` — FIXED in 07fe760
- **Severity**: Medium
- **File**: `src/centcom.rs`, `#[derive(Debug)]` on `ManagedConfig`
- **Trigger sketch**: If a consumer logs a `ManagedConfig` instance with `{:?}` (e.g., during debugging or error reporting), secrets such as `api_token`, `email_password`, `pool_password` are printed in plaintext.
- **Fix**: Implement a custom `Debug` that redacts secret fields (replace with `"***"`) or remove the `Debug` derive and provide a manual implementation that masks them. Fix in nexus.

#### 4.2 Unbounded response size from CentCom server — FIXED in 07fe760
- **Severity**: Medium
- **File**: `src/centcom.rs`, all `ureq` request methods (`fetch_config`, `post_heartbeat`, `post_events`)
- **Trigger sketch**: A malicious or compromised CentCom server could send an arbitrarily large response body (e.g., a multi-gigabyte JSON). The `ureq` client reads the entire response into memory, potentially causing the miner to run out of memory and crash.
- **Fix**: Set a maximum response size limit (e.g., a few megabytes) using `ureq`’s configuration or by reading with a capped reader. Fix in nexus.

#### 4.3 No input size limits on `ManagedConfig` fields — FIXED in 07fe760
- **Severity**: Low
- **File**: `src/centcom.rs`, `ManagedConfig` struct
- **Trigger sketch**: A CentCom server (or an attacker impersonating it) could send extremely long strings for fields like `pool_url`, `wallet_address`, etc., causing excessive memory allocation when the config is deserialized.
- **Fix**: Add maximum length validations for string fields in `ManagedConfig` (or during deserialization) to bound memory usage. Fix in nexus.

---

## 5. API design hazards

### Confirmed Findings

#### 5.1 Stale-pin drift between consumers (unverified)
- **Severity**: Informational
- **File**: N/A (repository-level observation)
- **Trigger sketch**: xtminer and edgecase pin a nexus revision. If they pin different revisions, an API change in nexus could break one consumer while the other works, or cause silent contract drift when one is updated.
- **Fix**: Verify that both consumers pin the same nexus rev and update them in lockstep. Document the required pinning policy in the nexus README.

#### 5.2 No per-header count limit in `read_request` — FIXED in 07fe760
- **Severity**: Low *(restored from first independent run; verified in source)*
- **File**: `src/api.rs`, function `read_request`
- **Trigger sketch**: Request headers are bounded only by the aggregate 64 KiB byte cap — there is no per-header-count limit. An attacker can send ~13K minimal headers within that budget, forcing proportional String allocations and parsing work per request. Bounded (memory stays O(cap)) but wasteful; a slow-loris-style CPU drain on the API thread.
- **Fix**: Cap header count (e.g., 100) before the byte cap is reached. Fix in nexus.

---

## Unconfirmed Suspicions

No unconfirmed suspicions remain after review. All potential issues were either confirmed or ruled out.

---

## Residual Risk

The nexus crate is well-structured for its role as a shared mining library. The most significant residual risks stem from the CentCom client’s handling of external data: unbounded response sizes and the exposure of secrets via `Debug`. These can be mitigated with the fixes above. The API server’s lack of a shutdown mechanism and the potential for thread-creation panics are low-severity but should be addressed to improve robustness. Overall, the library’s core I/O parsing is resistant to hostile pool input, and the threading model is sound. Consumers must ensure they apply the same pinning discipline to avoid drift.
