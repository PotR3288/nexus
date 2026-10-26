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
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
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
/// Heartbeat timeout: status POSTs must never hold up their thread long —
/// the next tick retries.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// [`Error::Status`] with the service's error message.
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
        res.into_json::<ConfigResponse>()
            .map_err(|e| Error::Parse(e.to_string()))
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
        // 200 with {"ok":true} — drain the (tiny) body and finish.
        let mut sink = Vec::new();
        res.into_reader()
            .read_to_end(&mut sink)
            .map_err(|e| Error::Http(e.to_string()))?;
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
        let mut sink = Vec::new();
        res.into_reader()
            .read_to_end(&mut sink)
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(())
    }
}

/// Best-effort extraction of an error response body (ureq keeps the response
/// attached to Status errors).
fn response_body(resp: ureq::Response) -> String {
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf).unwrap_or_default();
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
}
