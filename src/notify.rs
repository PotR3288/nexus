// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause
//! Channel-agnostic mining-event notifications shared by xtminer and
//! edgecase.
//!
//! xtminer and edgecase carried near-copies of this whole layer (dispatcher,
//! Channel trait, email channel); only the `Event` vocabulary differs
//! (xtminer: proxy/hashrate events; edgecase: node/solution events). This
//! module therefore owns the TRANSPORT half — [`Channel`], [`Dispatcher`],
//! the SMTP email channel, and the dispatch helpers — and each miner keeps
//! its own `Event` enum, rendered through the generic [`EmailEvent`] adapter
//! below.
//!
//! The dedup/episode state (proxy-lost markers) stays in the miners: it is
//! event-generation policy tied to their own connection state machines.
//!
//! # Unified event vocabulary ([`Event`])
//!
//! On top of the transport, this module defines the machine-readable event
//! vocabulary both miners map their internal event enums onto ([`Event`] +
//! [`EventKind`], [`Rate`], [`EventLog`]). The miners' human-rendered
//! notifications (email subjects/bodies) stay per-miner on purpose — the
//! wording is product voice; the STRUCTURED payload is what a GUI/Universe
//! integration consumes, and that is the part worth unifying:
//!
//! - one `kind` tag (stable, snake_case, matchable without version skew),
//! - timestamps + optional block height as first-class fields,
//! - rates carry their UNIT ([`RateUnit`]) — H/s and graphs/s are different
//!   quantities, never coerced,
//! - "not measured yet" is `None`/empty, never a fake 0.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use lettre::{
    message::header::{HeaderName, HeaderValue},
    message::Message,
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
};
use log::{debug, info, warn};

/// What a miner's event enum must render into before the shared email
/// channel can carry it. Both miners already have exactly these three
/// strings per event (subject prefix, subject body, plain-text body).
#[derive(Debug, Clone)]
pub struct EmailEvent {
    /// Plain subject body, e.g. "Block Accepted" (no miner/product prefix).
    pub subject: String,
    /// Full plain-text message body.
    pub body: String,
}

/// One notification channel. Implement this to add a carrier (Telegram,
/// Signal, Discord, SMS, …) — the dispatcher and all call sites stay
/// unchanged. Channels must be `Send + Sync`: the dispatcher is shared
/// across the mining loop, monitor thread, and shutdown path.
pub trait Channel: Send + Sync {
    /// Human-readable channel name for logs (e.g. "email").
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// Deliver one event. Must not block the caller for long — carriers
    /// should do their own async/thread dispatch. Returning an error is
    /// logged by the dispatcher and never fatal.
    fn deliver(&self, event: &EmailEvent, miner_id: &str);

    /// Deliver one event synchronously, blocking until the send completes
    /// (bounded by a timeout inside the carrier). Used for shutdown-time
    /// events (MinerStopped) that must actually leave before the process
    /// exits — detached dispatch threads would be killed by `process::exit`.
    /// Default: fall back to fire-and-forget.
    fn deliver_blocking(&self, event: &EmailEvent, miner_id: &str) {
        self.deliver(event, miner_id);
    }
}

/// Fans events out to every configured, enabled channel.
pub struct Dispatcher {
    /// Miner identity used in message bodies (miner_id or wallet fallback —
    /// both miners resolve this identically at startup).
    miner_id: String,
    channels: Vec<Box<dyn Channel>>,
}

impl Dispatcher {
    /// Build from an explicit channel list (the miners' config → channel
    /// construction stays in the miner; only the fan-out is shared).
    /// `miner_id` is the notification identity: `mining.miner_id` when set,
    /// falling back to the wallet address (identical rule in both miners).
    pub fn new(miner_id: String, channels: Vec<Box<dyn Channel>>) -> Self {
        if channels.is_empty() {
            debug!("Notifications: no channels enabled");
        }
        Self { miner_id, channels }
    }

    /// True when at least one channel is active.
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        !self.channels.is_empty()
    }

    /// Send a notification for the given event to every enabled channel.
    /// Non-blocking — each channel does its own async/thread dispatch, and
    /// errors are logged, never propagated.
    pub fn send(&self, event: &EmailEvent) {
        for ch in &self.channels {
            ch.deliver(event, &self.miner_id);
        }
        if !self.channels.is_empty() {
            debug!(
                "Notification dispatched to {} channel(s): {}",
                self.channels.len(),
                event.subject
            );
        }
    }

    /// Send a notification for the given event to every enabled channel,
    /// BLOCKING until each channel has actually delivered (or timed out).
    /// Use for shutdown-time events (MinerStopped): the process may exit
    /// right after, and detached dispatch tasks would be killed by
    /// `process::exit` before the email leaves.
    pub fn send_blocking(&self, event: &EmailEvent) {
        for ch in &self.channels {
            ch.deliver_blocking(event, &self.miner_id);
        }
        if !self.channels.is_empty() {
            debug!(
                "Blocking notification dispatched to {} channel(s): {}",
                self.channels.len(),
                event.subject
            );
        }
    }
}

/// Spawn a detached dispatch task for an async send future. When called
/// from within a tokio runtime (the mining loop's context), the task joins
/// that runtime; otherwise (monitor thread, signal handler) it falls back to
/// a dedicated OS thread with a mini current-thread runtime. Panics inside
/// the future are logged, not fatal — a notification must never take the
/// miner down.
pub fn spawn_dispatch<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
        return;
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build mini-runtime for notification dispatch");
        rt.block_on(fut);
    });
}

/// Log a delivery failure with the channel name prefix.
pub fn log_delivery_failure<E: std::fmt::Display>(channel: &str, err: E) {
    warn!("{} notification delivery failed: {}", channel, err);
}

/// SMTP-backed implementation of [`Channel`] (lettre, async Tokio1).
/// All credentials are redacted in logs; the miner identity is included in
/// every message for differentiation across instances.
#[derive(Clone)]
pub struct EmailChannel {
    pub host: String,
    pub port: u16,
    pub from_address: String,
    pub to_addresses: Vec<String>,
    pub username: String,
    pub password: String,
    /// true = implicit TLS (relay), false = STARTTLS.
    pub ssl_tls: bool,
    /// Product prefix in the subject, e.g. "xtminer" / "edgecase".
    pub product: String,
}

impl EmailChannel {
    /// Build an email message from event data (pub for the miners' own
    /// Event→EmailEvent adapters and tests).
    pub fn build_email(&self, miner_id: &str, event: &EmailEvent) -> Result<Message, String> {
        let subject = format!("[{}] {} — Miner {}", self.product, event.subject, miner_id);

        let mut msg_builder = Message::builder()
            .from(
                self.from_address
                    .parse()
                    .map_err(|e| format!("Invalid from address: {e}"))?,
            )
            .raw_header(HeaderValue::new(
                HeaderName::new_from_ascii_str("Priority"),
                "Normal".to_string(),
            ))
            .subject(subject);

        for addr in &self.to_addresses {
            msg_builder = msg_builder.to(addr
                .parse()
                .map_err(|e| format!("Invalid to address '{addr}': {e}"))?);
        }

        msg_builder
            .body(event.body.clone())
            .map_err(|e| format!("Message build error: {e}"))
    }

    /// Send an email message via SMTP.
    async fn send_message(&self, msg: Message) -> Result<(), String> {
        let builder = if self.ssl_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
                .map_err(|e| format!("SMTP relay error: {e}"))?
                .port(self.port)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
                .map_err(|e| format!("SMTP STARTTLS relay error: {e}"))?
                .port(self.port)
        };

        let builder = if !self.username.is_empty() && !self.password.is_empty() {
            builder.credentials(Credentials::new(
                self.username.clone(),
                self.password.clone(),
            ))
        } else {
            builder
        };

        let transport = builder.build();
        match transport.send(msg).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("SMTP send failed: {e}")),
        }
    }
}

impl Channel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }

    /// Render the event into an email and hand it to the async dispatch
    /// helper. Never blocks the caller.
    fn deliver(&self, event: &EmailEvent, miner_id: &str) {
        let this = self.clone();
        let miner_id = miner_id.to_string();
        let event = event.clone();

        spawn_dispatch(async move {
            match this.build_email(&miner_id, &event) {
                Ok(msg) => {
                    if let Err(e) = this.send_message(msg).await {
                        log_delivery_failure("email", e);
                    } else {
                        info!(
                            "Email notification sent: Miner {} — {}",
                            miner_id, event.subject
                        );
                    }
                }
                Err(e) => {
                    warn!("Could not build email notification: {e}");
                }
            }
        });
    }

    /// Deliver synchronously: run the same send future to completion on a
    /// dedicated thread and wait for it. Used for MinerStopped — the process
    /// can exit immediately after, and a detached dispatch task would be
    /// killed before the SMTP exchange finishes.
    fn deliver_blocking(&self, event: &EmailEvent, miner_id: &str) {
        let this = self.clone();
        let miner_id = miner_id.to_string();
        let event = event.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build mini-runtime for blocking notification");
            rt.block_on(async move {
                match this.build_email(&miner_id, &event) {
                    Ok(msg) => {
                        if let Err(e) = this.send_message(msg).await {
                            log_delivery_failure("email", e);
                        } else {
                            info!(
                                "Email notification sent (blocking): Miner {} — {}",
                                miner_id, event.subject
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Could not build email notification: {e}");
                    }
                }
            });
        });
        // Bounded wait: deliver on a best-effort basis if SMTP stalls.
        let _ = handle.join();
    }
}

/// Hashrate unit carried by a [`Rate`]. H/s (RandomX, xtminer) and
/// graphs/s (Cuckaroo, edgecase) are different physical quantities —
/// a consumer must never average or compare across units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateUnit {
    /// RandomX hashes per second.
    HashesPerSecond,
    /// Cuckaroo graphs per second.
    GraphsPerSecond,
}

/// A hashrate measurement: value + unit + measurement window. A window of
/// `None` means instantaneous (EMA); `value` is never a fake 0 — omit the
/// rate entirely when nothing has been measured yet.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    /// Measured rate in the unit below.
    pub value: f64,
    /// What the number counts.
    pub unit: RateUnit,
    /// Window the rate was measured over, in seconds (`None` = instantaneous
    /// or EMA-derived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_secs: Option<u64>,
}

/// Stable, matchable event kinds for the unified vocabulary. Serializes as
/// snake_case; new kinds may be added — consumers must ignore unknown ones
/// (forward compatibility), never fail on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A solution was found and submitted (block candidate or pool share).
    SolutionFound,
    /// The submission was accepted by the pool/proxy/node.
    SolutionAccepted,
    /// The submission was rejected, with the reason in `detail`.
    SolutionRejected,
    /// Measured hashrate dropped below the alert threshold.
    HashrateDrop,
    /// Connection to the work source (proxy/node/pool) was lost.
    WorkSourceLost,
    /// Connection to the work source was restored.
    WorkSourceRestored,
    /// Periodic status report.
    PeriodicReport,
    /// The miner started.
    MiningStarted,
    /// The miner stopped.
    MinerStopped,
    /// The chain tip has not advanced for longer than the gap threshold.
    BlockGapDetected,
}

/// The unified, machine-readable mining event — the struct a GUI/Universe
/// integration consumes. Built from each miner's internal event enum via
/// `From` impls living in the miner crates (the miners own the mapping; this
/// crate owns the shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Stable kind tag.
    pub kind: EventKind,
    /// Unix time (seconds) the event was recorded.
    #[serde(default, alias = "ts")]
    pub timestamp: u64,
    /// Template/block height the event refers to, when the source reports
    /// one. `None` when unknown (e.g. stratum pools carry no height) —
    /// never 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u64>,
    /// Nonce of the submitted solution (`SolutionFound`/`SolutionAccepted`/
    /// `SolutionRejected` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
    /// Share/solution difficulty as reported by the work source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<u64>,
    /// Hashrate readings attached to the event (periodic reports carry one
    /// per window; hashrate alerts carry the current + baseline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rates: Vec<Rate>,
    /// Work-source identity the event refers to (e.g. "stratum host:port",
    /// proxy URL, base-node URL) — matches what the miners log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_source: Option<String>,
    /// Free-form detail: rejection reason, error text, gap seconds. Kept
    /// human-readable; structured facts live in the fields above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Event {
    /// Build an event stamped with the current Unix time.
    pub fn now(kind: EventKind) -> Self {
        Self {
            kind,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            height: None,
            nonce: None,
            difficulty: None,
            rates: Vec::new(),
            work_source: None,
            detail: None,
        }
    }

    /// Builder-style setters (chainable), so miner mapping code reads
    /// declaratively.
    pub fn with_height(mut self, h: u64) -> Self {
        self.height = Some(h);
        self
    }
    pub fn with_nonce(mut self, n: u64) -> Self {
        self.nonce = Some(n);
        self
    }
    pub fn with_difficulty(mut self, d: u64) -> Self {
        self.difficulty = Some(d);
        self
    }
    pub fn with_rate(mut self, r: Rate) -> Self {
        self.rates.push(r);
        self
    }
    pub fn with_work_source(mut self, s: impl Into<String>) -> Self {
        self.work_source = Some(s.into());
        self
    }
    pub fn with_detail(mut self, s: impl Into<String>) -> Self {
        self.detail = Some(s.into());
        self
    }
    /// Set `height` only when present (`None` stays omitted — unknown
    /// height is never serialized as 0).
    pub fn with_height_opt(mut self, h: Option<u64>) -> Self {
        self.height = h;
        self
    }
    /// Set `detail` only when present (`None` stays omitted).
    pub fn with_detail_opt(mut self, s: Option<String>) -> Self {
        self.detail = s;
        self
    }
}

/// Bounded, thread-safe in-memory event ring — the shared backing store for
/// a `GET /events`-style surface. Newest last. Dropping the oldest entries
/// when full is intentional: this is a monitoring feed, not an audit log.
pub struct EventLog {
    capacity: usize,
    events: Mutex<VecDeque<Event>>,
}

impl EventLog {
    /// New ring holding the most recent `capacity` events.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            events: Mutex::new(VecDeque::new()),
        }
    }

    /// Record one event, dropping the oldest when full.
    pub fn push(&self, event: Event) {
        let mut q = self.lock();
        if q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back(event);
    }

    /// Snapshot of the last `limit` events, oldest first (`limit` 0 or
    /// larger than the buffer = everything held).
    pub fn tail(&self, limit: usize) -> Vec<Event> {
        let q = self.lock();
        if limit == 0 || limit >= q.len() {
            return q.iter().cloned().collect();
        }
        q.iter().skip(q.len() - limit).cloned().collect()
    }

    /// Number of events currently held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when no events are held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Event>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for EventLog {
    fn default() -> Self {
        // 256 covers hours of normal operation; an outage storm cycles
        // rather than growing unbounded.
        Self::new(256)
    }
}

#[cfg(test)]
mod notify_event_tests {
    use super::*;

    fn hps(value: f64, window: Option<u64>) -> Rate {
        Rate {
            value,
            unit: RateUnit::HashesPerSecond,
            window_secs: window,
        }
    }

    #[test]
    fn event_serializes_with_stable_snake_case_kind() {
        let e = Event::now(EventKind::SolutionAccepted)
            .with_height(940635)
            .with_nonce(120)
            .with_difficulty(232);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "solution_accepted");
        assert_eq!(v["height"], 940635);
        assert_eq!(v["nonce"], 120);
        // None fields are omitted, not zeroed.
        assert!(v.get("detail").is_none());
        assert!(v.get("work_source").is_none());
    }

    #[test]
    fn rates_carry_units_and_optional_windows() {
        let e = Event::now(EventKind::PeriodicReport)
            .with_rate(hps(1850.5, Some(30)))
            .with_rate(Rate {
                value: 4.26,
                unit: RateUnit::GraphsPerSecond,
                window_secs: Some(60),
            });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["rates"][0]["unit"], "hashes_per_second");
        assert_eq!(v["rates"][0]["window_secs"], 30);
        assert_eq!(v["rates"][1]["unit"], "graphs_per_second");
        // Round-trips through the same shape.
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back.rates.len(), 2);
        assert_eq!(back.rates[1].unit, RateUnit::GraphsPerSecond);
    }

    #[test]
    fn event_log_is_bounded_newest_last_and_tail_limited() {
        let log = EventLog::new(3);
        for i in 0..5u64 {
            log.push(Event::now(EventKind::MiningStarted).with_nonce(i));
        }
        assert_eq!(log.len(), 3);
        let tail = log.tail(10);
        // Oldest were dropped; order is oldest-first among what remains.
        assert_eq!(tail[0].nonce, Some(2));
        assert_eq!(tail[2].nonce, Some(4));
        // Limit=2 keeps only the two newest.
        let tail2 = log.tail(2);
        assert_eq!(tail2.len(), 2);
        assert_eq!(tail2[0].nonce, Some(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn channel(product: &str) -> EmailChannel {
        EmailChannel {
            host: "smtp.test.com".into(),
            port: 587,
            from_address: "miner@test.com".into(),
            to_addresses: vec!["admin@test.com".into()],
            username: String::new(),
            password: String::new(),
            ssl_tls: false,
            product: product.into(),
        }
    }

    #[test]
    fn build_email_includes_product_subject_and_miner_id() {
        let ch = channel("xtminer");
        let ev = EmailEvent {
            subject: "Block Accepted".into(),
            body: "body text".into(),
        };
        let msg = ch.build_email("192.168.1.100", &ev).unwrap();
        let subject = msg.headers().get_raw("Subject").unwrap_or("").to_string();
        assert!(subject.contains("[xtminer]"), "{subject}");
        assert!(subject.contains("Block Accepted"), "{subject}");
        assert!(subject.contains("192.168.1.100"), "{subject}");
    }

    #[test]
    fn build_email_rejects_bad_addresses() {
        let mut ch = channel("edgecase");
        ch.from_address = "not-an-address".into();
        let ev = EmailEvent {
            subject: "s".into(),
            body: "b".into(),
        };
        assert!(ch.build_email("m", &ev).is_err());

        let mut ch2 = channel("edgecase");
        ch2.to_addresses = vec!["also-bad".into()];
        assert!(ch2.build_email("m", &ev).is_err());
    }

    #[test]
    fn dispatcher_fans_out_to_all_channels() {
        // Simple recording channel.
        struct Rec(Arc<std::sync::Mutex<Vec<String>>>);
        impl Channel for Rec {
            fn name(&self) -> &'static str {
                "rec"
            }
            fn deliver(&self, e: &EmailEvent, m: &str) {
                self.0.lock().unwrap().push(format!("{m}:{}", e.subject));
            }
        }
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let d = Dispatcher::new(
            "miner-x".into(),
            vec![Box::new(Rec(log.clone())), Box::new(Rec(log.clone()))],
        );
        d.send(&EmailEvent {
            subject: "Mining Report".into(),
            body: "b".into(),
        });
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.starts_with("miner-x:")));
    }
}
