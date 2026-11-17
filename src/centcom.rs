// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Miner-side client for the CentCom service
//! (`PotR3288/centcom`) — central config distribution + fleet status +
//! notification emission.
//!
//! A miner configured with a `[centcom]` section (address, its own
//! id, and its per-miner token) uses this module to:
//!
//! - [`Client::fetch_config`] — `GET /v1/miners/{id}` returns the profile's
//!   managed config document ([`ManagedConfig`], schema v1) plus an
//!   `updated_at` epoch so pollers can skip unchanged documents.
//! - [`Client::post_heartbeat`] — `POST /v1/miners/{id}/heartbeat` reports
//!   version, uptime, and hashrate for the fleet dashboard.
//! - [`Client::post_events`] — `POST /v1/miners/{id}/events` submits mining
//!   events for notification dispatch by CentCom (email, Telegram, etc.).
//!
//! All methods are BLOCKING (ureq) — call them from a dedicated thread,
//! never from the mining hot path. The service speaks plain HTTP on a
//! trusted LAN; this client deliberately enables no TLS features so it adds
//! nothing to consumers' dependency graphs beyond ureq's core. A
//! `https://` base URL will fail with a clear transport error — use an http
//! endpoint or tunnel.
//!
//! [`ManagedConfig`] field names are the wire contract shared with
//! centcom's `store.rs` (and its web UI) — keep them in lockstep when
//! extending schema v1, then bump this crate and re-pin it in both miners.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::time::Duration;

/// Managed config document served by the CentCom — schema v1, the
/// curated fleet-level subset common to xtminer and edgecase. Every field is
/// optional: a profile may manage any subset, and miners only override
/// fields that are present (see each miner's `centcom` module for the
/// mapping onto its own config).
///
/// `Debug` is implemented manually: credential fields (`pool_password`,
/// `api_token`, `email_password`) print as `Some("***")` so a `{:?}` log
/// line can never leak them (audit finding 4.1).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ManagedConfig {
    // ── Pool / work source ────────────────────────────────────────────────
    /// Primary stratum/proxy endpoint (e.g. "stratum+tcp://host:port").
    #[serde(default)]
    pub pool_url: Option<String>,
    /// Work source kind for the primary endpoint ("node" | "proxy" | "pool").
    #[serde(default)]
    pub work_source: Option<String>,
    /// Stratum worker/login name.
    #[serde(default)]
    pub worker_id: Option<String>,
    /// Stratum password (edgecase `pass` / xtminer `password`).
    #[serde(default)]
    pub pool_password: Option<String>,
    /// Ordered failover endpoints tried after the primary drops out.
    #[serde(default)]
    pub failover_urls: Vec<String>,

    // ── Payout & identity ─────────────────────────────────────────────────
    /// Payout wallet address (Tari mainnet/testnet).
    #[serde(default)]
    pub wallet_address: Option<String>,
    /// Extra data appended to the coinbase message.
    #[serde(default)]
    pub coinbase_extra: Option<String>,
    /// Notification "Miner ID" label (`mining.miner_id`).
    #[serde(default)]
    pub mining_miner_id: Option<String>,

    // ── Performance ───────────────────────────────────────────────────────
    /// Solver trim count (edgecase `solver.ntrims`; ignored by xtminer).
    #[serde(default)]
    pub ntrims: Option<i32>,

    // ── Reporting / display ───────────────────────────────────────────────
    /// Hashrate report interval in seconds.
    #[serde(default)]
    pub hashrate_report_secs: Option<u64>,
    /// Hashrate window specs (e.g. ["10s", "60s", "5m"]); None = miner default.
    #[serde(default)]
    pub hashrate_windows: Option<Vec<String>>,

    // ── Logging ───────────────────────────────────────────────────────────
    /// Log level filter ("info", "debug", ...).
    #[serde(default)]
    pub log_level: Option<String>,
    /// Hashrate-only console mode (edgecase; ignored by xtminer in v1).
    #[serde(default)]
    pub hashrate_only: Option<bool>,

    // ── Miner HTTP API ────────────────────────────────────────────────────
    #[serde(default)]
    pub api_enabled: Option<bool>,
    #[serde(default)]
    pub api_port: Option<u16>,
    /// Bearer token for the miner's own status API.
    #[serde(default)]
    pub api_token: Option<String>,

    // ── Node (edgecase [node]; ignored by xtminer, which mines via
    //    proxy/stratum only) ───────────────────────────────────────────────
    /// Base node URL (edgecase `node.base_node`, e.g. "http://127.0.0.1:18142").
    #[serde(default)]
    pub node_url: Option<String>,
    /// Tari network (edgecase `node.network`, e.g. "mainnet" | "esmeralda").
    #[serde(default)]
    pub node_network: Option<String>,

    // ── Email notifications ───────────────────────────────────────────────
    #[serde(default)]
    pub email_enabled: Option<bool>,
    #[serde(default)]
    pub email_host: Option<String>,
    #[serde(default)]
    pub email_port: Option<u16>,
    #[serde(default)]
    pub email_from: Option<String>,
    #[serde(default)]
    pub email_to: Vec<String>,
    #[serde(default)]
    pub email_username: Option<String>,
    #[serde(default)]
    pub email_password: Option<String>,
    #[serde(default)]
    pub email_ssl_tls: Option<bool>,
    /// Solution/hashrate report interval for email, seconds.
    #[serde(default)]
    pub email_report_interval_secs: Option<u64>,
}

/// Debug view of a credential field in [`ManagedConfig`]: preserves presence
/// (`Some`) without the value (audit finding 4.1).
struct Redacted<'a>(&'a Option<String>);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(_) => write!(f, "Some(\"***\")"),
            None => write!(f, "None"),
        }
    }
}

impl fmt::Debug for ManagedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Mirrors the derived Debug except credential fields are redacted —
        // a `{:?}` log line must never print them (audit finding 4.1).
        f.debug_struct("ManagedConfig")
            .field("pool_url", &self.pool_url)
            .field("work_source", &self.work_source)
            .field("worker_id", &self.worker_id)
            .field("pool_password", &Redacted(&self.pool_password))
            .field("failover_urls", &self.failover_urls)
            .field("wallet_address", &self.wallet_address)
            .field("coinbase_extra", &self.coinbase_extra)
            .field("mining_miner_id", &self.mining_miner_id)
            .field("ntrims", &self.ntrims)
            .field("hashrate_report_secs", &self.hashrate_report_secs)
            .field("hashrate_windows", &self.hashrate_windows)
            .field("log_level", &self.log_level)
            .field("hashrate_only", &self.hashrate_only)
            .field("api_enabled", &self.api_enabled)
            .field("api_port", &self.api_port)
            .field("api_token", &Redacted(&self.api_token))
            .field("node_url", &self.node_url)
            .field("node_network", &self.node_network)
            .field("email_enabled", &self.email_enabled)
            .field("email_host", &self.email_host)
            .field("email_port", &self.email_port)
            .field("email_from", &self.email_from)
            .field("email_to", &self.email_to)
            .field("email_username", &self.email_username)
            .field("email_password", &Redacted(&self.email_password))
            .field("email_ssl_tls", &self.email_ssl_tls)
            .field(
                "email_report_interval_secs",
                &self.email_report_interval_secs,
            )
            .finish()
    }
}

/// Hard caps on managed-config field sizes (audit finding 4.3). The response
/// byte cap bounds the whole document BEFORE deserialization; these bound
/// what a single field can hold downstream (config structs, log lines) even
/// within a size-legal document. Generous by design — no legitimate fleet
/// doc comes close.
const MAX_FIELD_LEN: usize = 1024;
/// Max entries in the list fields (`failover_urls`, `email_to`,
/// `hashrate_windows`).
const MAX_LIST_ITEMS: usize = 64;

impl ManagedConfig {
    /// Validate field sizes after deserialization (audit finding 4.3): a
    /// compromised CentCom must not be able to push megabyte-scale strings
    /// into the miner's config. Returns the first offending field so callers
    /// can log which cap was hit; on error the caller should reject the whole
    /// document and fall back to its local/cached config.
    pub fn validate_sizes(&self) -> Result<(), String> {
        let check = |name: &str, v: &Option<String>| match v {
            Some(s) if s.len() > MAX_FIELD_LEN => {
                Err(format!("{name} exceeds the {MAX_FIELD_LEN}-byte cap"))
            }
            _ => Ok(()),
        };
        let list = |name: &str, v: &[String]| {
            if v.len() > MAX_LIST_ITEMS {
                return Err(format!("{name} has more than {MAX_LIST_ITEMS} entries"));
            }
            for (i, s) in v.iter().enumerate() {
                if s.len() > MAX_FIELD_LEN {
                    return Err(format!("{name}[{i}] exceeds the {MAX_FIELD_LEN}-byte cap"));
                }
            }
            Ok(())
        };
        check("pool_url", &self.pool_url)?;
        check("work_source", &self.work_source)?;
        check("worker_id", &self.worker_id)?;
        check("pool_password", &self.pool_password)?;
        list("failover_urls", &self.failover_urls)?;
        check("wallet_address", &self.wallet_address)?;
        check("coinbase_extra", &self.coinbase_extra)?;
        check("mining_miner_id", &self.mining_miner_id)?;
        check("log_level", &self.log_level)?;
        check("api_token", &self.api_token)?;
        check("node_url", &self.node_url)?;
        check("node_network", &self.node_network)?;
        check("email_host", &self.email_host)?;
        check("email_from", &self.email_from)?;
        list("email_to", &self.email_to)?;
        check("email_username", &self.email_username)?;
        check("email_password", &self.email_password)?;
        if let Some(w) = &self.hashrate_windows {
            list("hashrate_windows", w)?;
        }
        Ok(())
    }
}

/// Miner → CentCom status report (POST /v1/miners/{id}/heartbeat).
#[derive(Serialize, Deserialize, Clone)]
pub struct Heartbeat {
    /// Miner binary version string.
    pub version: String,
    /// Seconds since the miner started.
    pub uptime_secs: u64,
    /// Aggregate hashrate in `rate_unit`.
    pub hashrate: f64,
    /// Unit label ("graphs/s", "H/s", ...).
    #[serde(default)]
    pub rate_unit: String,
    /// Per-device breakdown (empty when the miner tracks no per-device rates).
    #[serde(default)]
    pub devices: Vec<DeviceStat>,
    /// The work source the miner is currently mining to, as the miner
    /// renders it (e.g. "proxy http://host:18086", "stratum pool:3110",
    /// "node http://…"). Optional for wire compatibility with older
    /// miners; the fleet dashboard surfaces it next to the hashrate.
    #[serde(default)]
    pub active_endpoint: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DeviceStat {
    /// Device index as the miner numbers it (0-based).
    pub id: i32,
    /// Human-readable name if the miner knows one.
    #[serde(default)]
    pub name: Option<String>,
    /// Per-device rate in the heartbeat's `rate_unit`.
    pub rate: f64,
}

/// Payload for POST /v1/miners/{id}/events — a batch of unified mining events.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EventPayload {
    /// The miner id this batch belongs to (matches the path param).
    pub miner_id: String,
    /// Events in chronological order (newest last).
    #[serde(default)]
    pub events: Vec<crate::notify::Event>,
}

/// Response to GET /v1/miners/{id}.
#[derive(Debug, Deserialize)]
pub struct ConfigResponse {
    /// The managed config document (all-None when nothing is managed).
    pub config: ManagedConfig,
    /// Epoch seconds of the last admin-side change — compare across polls.
    #[serde(default)]
    pub updated_at: u64,
}

/// Client failure. Transport errors are expected on flaky LANs (the miner
/// falls back to its local/cached config); status errors mean auth or an
/// unknown id and should be logged loudly once.
#[derive(Debug)]
pub enum Error {
    /// DNS/connect/read/timeout — the service was unreachable.
    Http(String),
    /// The service answered with a non-2xx status (401 bad token, 403 token
    /// for another id, 404 unknown miner).
    Status { code: u16, body: String },
    /// A 2xx response that did not parse as the expected JSON shape.
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "transport error: {e}"),
            Error::Status { code, body } => write!(f, "HTTP {code}: {body}"),
            Error::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Fetch timeout: startup fetches may cross a slow LAN hop; keep it bounded.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Heartbeat/event-batch timeout: status and event POSTs must never hold up
/// their thread long — the next tick retries.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on CentCom response bodies (audit finding 4.2): managed config
/// documents are a few KiB at most, so a multi-gigabyte body from a
/// compromised service is malformed, not data. The read itself is bounded —
/// `into_json`/`read_to_end` would otherwise buffer the whole body into
/// memory before anything could reject it.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Miner-side client for one CentCom profile. Cheap to clone (three
/// Strings).
#[derive(Clone)]
pub struct Client {
    base_url: String,
    miner_id: String,
    token: String,
}

impl Client {
    /// `base_url` like "http://172.31.34.185:8090" (trailing slash tolerated).
    pub fn new(
        base_url: impl Into<String>,
        miner_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            miner_id: miner_id.into(),
            token: token.into(),
        }
    }

    /// The profile id this client queries as (miners log it in banners).
    pub fn miner_id(&self) -> &str {
        &self.miner_id
    }

    /// Fetch the managed config document. 401/403/404 surface as
    /// [`Error::Status`] with the service's error message. Response bodies
    /// are read under a hard byte cap and field sizes validated before the
    /// document is returned (audit findings 4.2 / 4.3).
    pub fn fetch_config(&self) -> Result<ConfigResponse, Error> {
        let url = format!("{}/v1/miners/{}", self.base_url, self.miner_id);
        let res = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(FETCH_TIMEOUT)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => Error::Status {
                    code,
                    body: response_body(resp),
                },
                other => Error::Http(other.to_string()),
            })?;
        let doc = capped_json::<ConfigResponse>(res)?;
        // Reject documents with absurd field values before any consumer maps
        // them onto its config (audit finding 4.3).
        doc.config.validate_sizes().map_err(Error::Parse)?;
        Ok(doc)
    }

    /// Post a status report. Failures are non-fatal by design (the next tick
    /// retries); the fleet dashboard simply shows the miner as stale.
    pub fn post_heartbeat(&self, hb: &Heartbeat) -> Result<(), Error> {
        let url = format!("{}/v1/miners/{}/heartbeat", self.base_url, self.miner_id);
        let res = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HEARTBEAT_TIMEOUT)
            .send_json(hb)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => Error::Status {
                    code,
                    body: response_body(resp),
                },
                other => Error::Http(other.to_string()),
            })?;
        // 200 with {"ok":true} — drain the (tiny) body under the byte cap
        // and finish (audit finding 4.2).
        drain_capped(res)?;
        Ok(())
    }

    /// Submit a batch of mining events for notification dispatch by CentCom.
    /// Events are serialized as the unified nexus vocabulary (snake_case
    /// kinds, typed rates, etc.). Failures are non-fatal — if CentCom is
    /// unreachable the miner keeps running; events are lost but the local
    /// event ring still records them for the HTTP API.
    pub fn post_events(&self, events: &[crate::notify::Event]) -> Result<(), Error> {
        let url = format!("{}/v1/miners/{}/events", self.base_url, self.miner_id);
        let payload = EventPayload {
            miner_id: self.miner_id.clone(),
            events: events.to_vec(),
        };
        let res = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HEARTBEAT_TIMEOUT)
            .send_json(&payload)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => Error::Status {
                    code,
                    body: response_body(resp),
                },
                other => Error::Http(other.to_string()),
            })?;
        // 200 with {"ok":true} — drain under the byte cap (audit finding 4.2).
        drain_capped(res)?;
        Ok(())
    }
}

/// Read a JSON response body under the hard byte cap, then deserialize
/// (audit finding 4.2). Bounding the READ is what keeps memory bounded —
/// `into_json` would buffer an arbitrarily large body before parsing could
/// reject it. Reading one byte past the cap is enough to detect overflow
/// without allocating for it.
fn capped_json<T: serde::de::DeserializeOwned>(res: ureq::Response) -> Result<T, Error> {
    let mut buf = Vec::new();
    res.into_reader()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Http(e.to_string()))?;
    if buf.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Parse(format!(
            "response body exceeds the {MAX_RESPONSE_BYTES}-byte cap"
        )));
    }
    serde_json::from_slice(&buf).map_err(|e| Error::Parse(e.to_string()))
}

/// Drain a 2xx response body under the same byte cap as [`capped_json`] —
/// the POST endpoints' bodies are tiny; we only need to confirm they arrived.
fn drain_capped(res: ureq::Response) -> Result<(), Error> {
    let mut sink = Vec::new();
    res.into_reader()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut sink)
        .map_err(|e| Error::Http(e.to_string()))?;
    if sink.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Parse(format!(
            "response body exceeds the {MAX_RESPONSE_BYTES}-byte cap"
        )));
    }
    Ok(())
}

/// Best-effort extraction of an error response body (ureq keeps the response
/// attached to Status errors). The read is capped — only 200 chars are kept,
/// so a hostile service can't make us buffer a huge error body either.
fn response_body(resp: ureq::Response) -> String {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(4 * 1024)
        .read_to_end(&mut buf)
        .unwrap_or_default();
    // Keep log lines short; the service's error JSON is small anyway.
    String::from_utf8_lossy(&buf).chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// Minimal single-purpose mock of the CentCom miner API: answers
    /// GET /v1/miners/{id} with a canned config doc and records POST bodies.
    fn mock_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut buf = [0u8; 4096];
                let n = match s.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let line = req.lines().next().unwrap_or("");
                let mut body_start = 0;
                if let Some(pos) = req.find("\r\n\r\n") {
                    body_start = pos + 4;
                }
                let body = req[body_start..].to_string();
                seen_thread.lock().unwrap().push(format!("{line} | {body}"));

                let (status, payload): (&str, String) = if line.starts_with("GET") {
                    (
                        "200 OK",
                        r#"{"config":{"pool_url":"stratum+tcp://p:32765","ntrims":48,"email_to":["a@b.c"]},"updated_at":123}"#
                            .to_string(),
                    )
                } else if line.starts_with("POST") {
                    ("200 OK", r#"{"ok":true}"#.to_string())
                } else {
                    ("405 Method Not Allowed", String::new())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (addr, seen)
    }

    #[test]
    fn fetch_config_parses_document() {
        let (addr, _seen) = mock_server();
        let c = Client::new(&addr, "rig-a", "tok");
        let doc = c.fetch_config().unwrap();
        assert_eq!(doc.updated_at, 123);
        assert_eq!(
            doc.config.pool_url.as_deref(),
            Some("stratum+tcp://p:32765")
        );
        assert_eq!(doc.config.ntrims, Some(48));
        assert_eq!(doc.config.email_to, vec!["a@b.c".to_string()]);
        // Unmanaged fields stay None.
        assert!(doc.config.wallet_address.is_none());
    }

    #[test]
    fn heartbeat_posts_json_body() {
        let (addr, seen) = mock_server();
        let c = Client::new(&addr, "rig-a", "tok");
        c.post_heartbeat(&Heartbeat {
            version: "0.3".into(),
            uptime_secs: 60,
            hashrate: 4.2,
            rate_unit: "graphs/s".into(),
            active_endpoint: Some("proxy http://127.0.0.1:18086".into()),
            devices: vec![DeviceStat {
                id: 0,
                name: None,
                rate: 4.2,
            }],
        })
        .unwrap();
        let log = seen.lock().unwrap().clone();
        assert!(
            log.iter()
                .any(|l| l.contains("POST /v1/miners/rig-a/heartbeat")),
            "{log:?}"
        );
    }

    #[test]
    fn post_events_submits_batched_unified_events() {
        let (addr, seen) = mock_server();
        let c = Client::new(&addr, "rig-b", "tok2");
        use crate::notify::{Event, EventKind};
        let events = vec![
            Event::now(EventKind::MiningStarted),
            Event::now(EventKind::SolutionAccepted)
                .with_height(940635)
                .with_nonce(120),
        ];
        c.post_events(&events).unwrap();
        let log = seen.lock().unwrap().clone();
        assert!(
            log.iter()
                .any(|l| l.contains("POST /v1/miners/rig-b/events")),
            "{log:?}"
        );
    }

    #[test]
    fn event_payload_serializes_correctly() {
        use crate::notify::{Event, EventKind};
        let events = vec![
            Event::now(EventKind::MiningStarted),
            Event::now(EventKind::SolutionAccepted)
                .with_height(940635)
                .with_nonce(120),
        ];
        let payload = EventPayload {
            miner_id: "rig-b".to_string(),
            events,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""miner_id":"rig-b""#));
        assert!(json.contains("mining_started"));
        assert!(json.contains("solution_accepted"));
        assert!(json.contains("940635"));
        assert!(json.contains("120"));
    }

    #[test]
    fn unreachable_service_is_transport_error() {
        // Port 1 on loopback: nothing listens there.
        let c = Client::new("http://127.0.0.1:1", "rig-a", "tok");
        match c.fetch_config() {
            Err(Error::Http(_)) => {}
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    #[test]
    fn managed_config_partial_json_round_trips() {
        // The wire contract: partial documents (a profile managing a subset)
        // must deserialize with defaults for the rest.
        let cfg: ManagedConfig = serde_json::from_str(r#"{"ntrims":60}"#).unwrap();
        assert_eq!(cfg.ntrims, Some(60));
        assert!(cfg.pool_url.is_none());
        assert!(cfg.failover_urls.is_empty());
    }

    #[test]
    fn debug_redacts_credential_fields() {
        // A `{:?}` log line must never print credentials (audit finding 4.1):
        // presence is preserved as Some("***"), values are not.
        let cfg = ManagedConfig {
            pool_url: Some("stratum+tcp://p:32765".into()),
            pool_password: Some("hunter2-secret".into()),
            api_token: Some("bearer-token-xyz".into()),
            email_password: Some("email-pass-abc".into()),
            ..Default::default()
        };
        let out = format!("{cfg:?}");
        assert!(!out.contains("hunter2-secret"));
        assert!(!out.contains("bearer-token-xyz"));
        assert!(!out.contains("email-pass-abc"));
        assert_eq!(out.matches(r#"Some("***")"#).count(), 3);
        // Non-secret fields still print in full.
        assert!(out.contains("stratum+tcp://p:32765"));
    }

    #[test]
    fn validate_sizes_rejects_oversized_fields() {
        // A compromised CentCom must not push megabyte-scale strings into the
        // miner's config (audit finding 4.3).
        let cfg = ManagedConfig {
            pool_url: Some("x".repeat(MAX_FIELD_LEN + 1)),
            ..Default::default()
        };
        assert!(cfg.validate_sizes().unwrap_err().contains("pool_url"));

        let cfg = ManagedConfig {
            failover_urls: vec!["ok".to_string(); MAX_LIST_ITEMS + 1],
            ..Default::default()
        };
        assert!(cfg.validate_sizes().unwrap_err().contains("failover_urls"));

        // A size-legal document passes.
        let ok: ManagedConfig = serde_json::from_str(
            r#"{"pool_url":"stratum+tcp://p:32765","email_to":["a@b.c"],"ntrims":48}"#,
        )
        .unwrap();
        assert!(ok.validate_sizes().is_ok());
    }

    #[test]
    fn oversized_response_body_is_rejected() {
        // A hostile service answering with a body past the cap must be
        // rejected before deserialization allocates for it (audit 4.2).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // consume the request (ignored)
                let payload = "x".repeat(MAX_RESPONSE_BYTES + 1);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let c = Client::new(&addr, "rig-a", "tok");
        match c.fetch_config() {
            Err(Error::Parse(e)) => assert!(e.contains("cap"), "{e}"),
            other => panic!("expected capped Parse error, got {other:?}"),
        }
    }
}
