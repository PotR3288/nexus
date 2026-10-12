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
            msg_builder = msg_builder.to(
                addr.parse()
                    .map_err(|e| format!("Invalid to address '{addr}': {e}"))?,
            );
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
        let this = Self {
            host: self.host.clone(),
            port: self.port,
            from_address: self.from_address.clone(),
            to_addresses: self.to_addresses.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            ssl_tls: self.ssl_tls,
            product: self.product.clone(),
        };
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
        let this = Self {
            host: self.host.clone(),
            port: self.port,
            from_address: self.from_address.clone(),
            to_addresses: self.to_addresses.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            ssl_tls: self.ssl_tls,
            product: self.product.clone(),
        };
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

    #[test]
    fn dispatcher_with_no_channels_is_quiet() {
        let d = Dispatcher::new("m".into(), vec![]);
        assert!(!d.is_enabled());
        // Must not panic.
        d.send(&EmailEvent {
            subject: "s".into(),
            body: "b".into(),
        });
        d.send_blocking(&EmailEvent {
            subject: "s".into(),
            body: "b".into(),
        });
    }
}
