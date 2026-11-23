// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Monero-dialect stratum framing constants and helpers — the shared
//! protocol layer for all miners in this fleet.
//!
//! Both miners speak the Monero pool protocol (the dialect unmodified xmrig
//! speaks — pools like Kryptex's `xtm-rx-*` / C29 `xtm-*` servers). The
//! session machinery itself (transport, reader loop, keepalives, submit
//! correlation) now lives in the async core [`crate::stratum_async`]; what
//! remains here is the protocol vocabulary shared by every consumer: the
//! framing constants, the login-string rule, and xmrig's `target`→
//! difficulty mapping.
//!
//! Protocol summary (from xtminer's original header, verified live against
//! xtm-rx-us.kryptex.network:7038):
//!
//! - `login`  `{login, pass, agent}` → `{id, job, status}` — `id` is a
//!   session handle echoed in every later request.
//! - `job`    (server push, no id) — replaces the active job.
//! - `submit` `{id, job_id, nonce, result}` → `{status: "OK"}` or an error.
//! - `keepalive` `{id}` — send periodically; pools drop silent miners.
//!
//! xmrig's `target` field: the hex string is a 4-byte little-endian u32
//! (`target_u32`); xmrig's share target is `u64::MAX / (u32::MAX / target_u32)`,
//! which under the `hash < u64::MAX / difficulty` check equals pool difficulty
//! `u32::MAX / target_u32`.

/// Idle cap between keepalives while waiting for a job push.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Pause before reconnecting after a dropped connection or login failure.
pub const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// Login response deadline.
pub const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// JSON-RPC id of the login request (pools echo it in the reply; the reader
/// loop correlates by id and must not count the login reply as a share).
pub const LOGIN_REQUEST_ID: u64 = 1;
/// JSON-RPC id of every keepalive request.
pub const KEEPALIVE_REQUEST_ID: u64 = 2;
/// First correlating id handed to a queued share submit (submits count up
/// from here; 1/2 are reserved for login/keepalive so a pool echoing a
/// stale id can never be mistaken for a share reply).
pub const FIRST_SUBMIT_ID: u64 = 100;

/// Maximum bytes for a single stratum line (login replies AND session
/// pushes/replies). A malicious or broken pool must not be able to grow an
/// unbounded `String` in the reader loop — a newline-less flood would OOM
/// the miner. Any line longer than this (no newline within the cap) is a
/// transport error: the session disconnects, takes a strike, reconnects.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// How long a submit waits for transport confirmation before the connection
/// is considered lost.
pub const SUBMIT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Build the stratum login string: `WALLET[SEPARATOR]WORKER`. An empty
/// worker id sends the bare wallet — some pools (e.g. snipanet) parse the
/// whole field as an address and reject any suffix, even a trailing
/// separator.
pub fn login_string(wallet: &str, worker_id: &str, separator: &str) -> String {
    if worker_id.is_empty() {
        wallet.to_string()
    } else {
        format!("{wallet}{separator}{worker_id}")
    }
}

/// xmrig's `setTarget` for an 8-hex-char target: the 4 bytes are read as a
/// little-endian u32 (`target_u32`), and xmrig's share target becomes
/// `u64::MAX / (u32::MAX / target_u32)` ≈ `target_u32 * 2^32`. Since both
/// miners' share check is `hash < u64::MAX / difficulty`, the equivalent
/// pool difficulty is `u32::MAX / target_u32` (verified against live
/// xtm-rx targets: f1290000 → target_u32 10737 → diff 400015; e2530000 →
/// target_u32 21474 → diff 200007).
pub fn target_to_difficulty(target_hex: &str) -> Result<u64, String> {
    let bytes = hex_decode(target_hex)?;
    if bytes.len() != 4 {
        return Err(format!(
            "unsupported target length {} (expected 4 bytes / 8 hex chars)",
            bytes.len()
        ));
    }
    let target_u32 = u32::from_le_bytes(bytes.try_into().unwrap());
    if target_u32 == 0 {
        return Err("target is zero".to_string());
    }
    let difficulty = u32::MAX as u64 / target_u32 as u64;
    if difficulty == 0 {
        return Err("target above u32::MAX — zero difficulty".to_string());
    }
    Ok(difficulty)
}

/// hex decode with a clean error (both miners use the `hex` crate; this
/// keeps nexus dependency-light).
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex string: {s}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex decode: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_to_difficulty_matches_observed_pool_values() {
        // Observed live from xtm-rx-us.kryptex.network:7038 jobs:
        // f1290000 → target_u32 10737 → difficulty 400015; e2530000 → 21474 → 200007.
        assert_eq!(target_to_difficulty("f1290000").unwrap(), 400_015);
        assert_eq!(target_to_difficulty("e2530000").unwrap(), 200_007);
        assert!(target_to_difficulty("00000000").is_err());
        assert!(target_to_difficulty("f1").is_err());
        assert!(target_to_difficulty("zzzz0000").is_err());
    }

    #[test]
    fn login_string_uses_separator_and_bare_wallet_for_empty_worker() {
        assert_eq!(login_string("W", "rig1", "/"), "W/rig1");
        assert_eq!(login_string("W", "rig1", "."), "W.rig1");
        // Pools that parse the whole field as an address (snipanet) reject
        // any suffix — even a trailing separator.
        assert_eq!(login_string("W", "", "/"), "W");
    }
}
