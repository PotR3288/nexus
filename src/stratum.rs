//! Monero-dialect stratum framing core shared by xtminer and edgecase.
//!
//! Both miners speak the Monero pool protocol (the dialect unmodified xmrig
//! speaks — pools like Kryptex's `xtm-rx-*` / C29 `xtm-*` servers) and both
//! carried a near-copy of the transport layer: connect, login with interim-
//! notification tolerance, a reader loop doing job pushes + id-correlated
//! submit replies + keepalives (with disable-on-unknown-method, learned from
//! LuckyPool/snipanet), and xmrig's `target`→difficulty mapping.
//!
//! What is shared here is the PROTOCOL layer. What stays in each miner is
//! the mining integration (job execution, dataset management, failover
//! accounting, notifications) — supplied as callbacks:
//!
//! - [`JobParser`]: raw job JSON → the miner's prepared job type.
//! - [`ReaderHooks`]: share accept/reject side effects (share counters are
//!   owned here; pool-failover accounting and notifications are the miner's).
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
//! (`tmp`); xmrig's share target is `u64::MAX / (u32::MAX / tmp)`, which
//! under the `hash < u64::MAX / difficulty` check equals pool difficulty
//! `u32::MAX / tmp`.

use log::{debug, info, warn};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Idle cap between keepalives while waiting for a job push.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Socket read timeout while polling for the next line (also the slice
/// between keepalives — keep it well under KEEPALIVE_INTERVAL).
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause before reconnecting after a dropped connection or login failure.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Login response deadline.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum bytes for a single stratum line (login replies AND session
/// pushes/replies). A malicious or broken pool must not be able to grow an
/// unbounded `String` in the reader loop — a newline-less flood would OOM
/// the miner. Any line longer than this (no newline within the cap) is a
/// transport error: the session disconnects, takes a strike, reconnects.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// How long a submit waits for transport confirmation before the connection
/// is considered lost.
pub const SUBMIT_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect to a pool: `host:port` (plain TCP; TLS on these fleets speaks
/// badly — plain TCP is the working path). Sets nodelay and the shared
/// read timeout (the reader loop's keepalive cadence depends on reads
/// timing out between lines).
pub fn connect(host_port: &str) -> Result<(TcpStream, BufReader<TcpStream>), String> {
    let stream = TcpStream::connect(host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    stream
        .set_nodelay(true)
        .map_err(|e| format!("set_nodelay: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    let reader = BufReader::new(stream.try_clone().map_err(|e| format!("clone: {e}"))?);
    Ok((stream, reader))
}

/// Send one JSON-RPC line.
pub fn send_message(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("send failed: {e}"))
}

/// Build the stratum login string: `WALLET[SEPARATOR]WORKER`. An empty
/// worker id sends the bare wallet — some pools (e.g. snipanet) parse the
/// whole field as an address and reject any suffix, even a trailing
/// separator.
/// Read one `\n`-terminated line into `line`, consuming at most `cap` bytes.
///
/// The bounded replacement for `BufRead::read_line`: a pool sending an
/// unterminated flood can no longer grow the buffer without limit — past
/// `cap` (without a newline) this returns a transport error and the caller
/// drops the connection. The buffer is cleared before reading, so a failed
/// call cannot leak a partial line into a later one. Cancel-safe: only
/// bytes through the newline (or the cap) are consumed, matching what gets
/// buffered into `line`.
pub fn read_line_capped<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    cap: usize,
) -> std::io::Result<usize> {
    line.clear();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break; // EOF
        }
        match memchr(available, b'\n') {
            Some(nl) => {
                buf.extend_from_slice(&available[..=nl]);
                reader.consume(nl + 1);
                break;
            }
            None => {
                let consumed = available.len();
                buf.extend_from_slice(available);
                reader.consume(consumed);
                if buf.len() > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("stratum line exceeds {cap}-byte cap"),
                    ));
                }
            }
        }
    }
    if buf.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("stratum line exceeds {cap}-byte cap"),
        ));
    }
    if buf.is_empty() {
        return Ok(0);
    }
    // Match BufRead::read_line semantics: invalid UTF-8 is a transport error.
    let text = std::str::from_utf8(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push_str(text);
    Ok(buf.len())
}

/// First index of `needle` in `buf`, if present (tiny stdlib-only memchr).
fn memchr(buf: &[u8], needle: u8) -> Option<usize> {
    buf.iter().position(|&b| b == needle)
}

pub fn login_string(wallet: &str, worker_id: &str, separator: &str) -> String {
    if worker_id.is_empty() {
        wallet.to_string()
    } else {
        format!("{wallet}{separator}{worker_id}")
    }
}

/// Outcome of one login attempt.
#[derive(Debug)]
pub enum LoginOutcome {
    /// Session id + first job (raw JSON).
    Ok { session_id: String, job: Value },
    /// Pool answered with an error (bad wallet etc.) — fatal for this pool.
    Rejected(String),
}

/// Login to the pool and read the login response (tolerating interim
/// notifications some pools push before the reply). `agent` is the miner's
/// agent string (e.g. `XTMiner/1.2.3`, `edgecase/0.9.0`).
pub fn login(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    login_str: &str,
    password: &str,
    agent: &str,
) -> Result<LoginOutcome, String> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "login",
        "params": {
            "login": login_str,
            "pass": password,
            "agent": agent,
        }
    });
    send_message(stream, &req)?;

    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        let mut line = String::new();
        let n = read_line_capped(reader, &mut line, MAX_LINE_BYTES)
            .map_err(|e| format!("read login reply: {e}"))?;
        if n == 0 {
            return Err("connection closed during login".to_string());
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for login response".to_string());
        }
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue, // tolerate garbage lines
        };
        if msg.get("id") != Some(&json!(1)) {
            continue;
        }
        if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
            return Ok(LoginOutcome::Rejected(format!("login rejected: {err}")));
        }
        let result = match msg.get("result").filter(|r| !r.is_null()) {
            Some(r) => r.clone(),
            None => {
                return Ok(LoginOutcome::Rejected(
                    "login response had no result".to_string(),
                ))
            }
        };
        let session_id = result
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "login response missing session id".to_string())?
            .to_string();
        let job = result
            .get("job")
            .cloned()
            .ok_or_else(|| "login response missing job".to_string())?;
        return Ok(LoginOutcome::Ok { session_id, job });
    }
}

/// xmrig's `setTarget` for an 8-hex-char target: the 4 bytes are read as a
/// little-endian u32 (`tmp`), and xmrig's share target becomes
/// `u64::MAX / (u32::MAX / tmp)` ≈ `tmp * 2^32`. Since both miners' share
/// check is `hash < u64::MAX / difficulty`, the equivalent pool difficulty
/// is `u32::MAX / tmp` (verified against live xtm-rx targets:
/// f1290000 → tmp 10737 → diff 400015; e2530000 → tmp 21474 → diff 200007).
pub fn target_to_difficulty(target_hex: &str) -> Result<u64, String> {
    let bytes = hex_decode(target_hex)?;
    if bytes.len() != 4 {
        return Err(format!(
            "unsupported target length {} (expected 4 bytes / 8 hex chars)",
            bytes.len()
        ));
    }
    let tmp = u32::from_le_bytes(bytes.try_into().unwrap());
    if tmp == 0 {
        return Err("target is zero".to_string());
    }
    let difficulty = u32::MAX as u64 / tmp as u64;
    if difficulty == 0 {
        return Err("target above u32::MAX — zero difficulty".to_string());
    }
    Ok(difficulty)
}

/// hex decode with a clean error (both miners use the `hex` crate; this
/// keeps nexus dependency-light).
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex string: {s}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex decode: {e}")))
        .collect()
}

/// Parse a raw job into the miner's prepared-job type. Implemented by each
/// miner (blob validation, nonce layout and difficulty semantics live with
/// the algorithm); malformed jobs are skipped by the reader loop, never
/// fatal.
pub trait JobParser<J>: Send + Sync {
    fn parse(&self, raw: &Value, nonce_width: u8) -> Result<J, String>;
}

/// A queued submit with an optional one-shot transport-ack channel. `params`
/// must already contain the pool fields; the reader loop adds the correlating
/// envelope id. The ack (when present) reports TRANSPORT status — acceptance
/// is accounted separately (shares_good/bad + hooks).
pub struct SubmitMsg {
    pub params: Value,
    pub ack: Option<std::sync::mpsc::Sender<Result<(), String>>>,
}

/// Reader-loop side effects. The session owns the share counters; these
/// hooks carry the miner-specific accounting (failover strikes, logs,
/// notifications).
pub struct ReaderHooks<J> {
    /// Parse a raw job (login job or `job` push) into the miner's type.
    pub parse: Arc<dyn Fn(&Value, u8) -> Result<J, String> + Send + Sync>,
    /// A share was ACCEPTED (counter already incremented). Reset rejection
    /// streaks here (an accepted share proves the pool's validation path
    /// works — sporadic rejections must not accumulate toward failover).
    pub on_share_accepted: Arc<dyn Fn() + Send + Sync>,
    /// A share was REJECTED (counter already incremented). Count
    /// consecutive-rejection failover strikes here.
    pub on_share_rejected: Arc<dyn Fn(&str) + Send + Sync>,
    /// An error reply that is NOT a share rejection (e.g. keepalive
    /// "unknown method"). Only fires until keepalive is disabled.
    pub on_nonsubmit_error: Arc<dyn Fn(&str) + Send + Sync>,
    /// A parsed job was installed into the job slot (fresh work available).
    pub on_job_installed: Arc<dyn Fn(&J) + Send + Sync>,
}

impl<J> ReaderHooks<J> {
    pub fn new(parse: Arc<dyn Fn(&Value, u8) -> Result<J, String> + Send + Sync>) -> Self {
        Self {
            parse,
            on_share_accepted: Arc::new(|| {}),
            on_share_rejected: Arc::new(|_| {}),
            on_nonsubmit_error: Arc::new(|_| {}),
            on_job_installed: Arc::new(|_| {}),
        }
    }
}

/// A live stratum session: the reader loop runs on its own thread and owns
/// the socket; the mining side reads jobs from the job slot and hands
/// submits back through the session. The single-writer rule (only the reader
/// thread writes to the socket) keeps frames intact.
pub struct Session<J: Send + 'static> {
    /// The pool's login session id (echoed in submit params).
    session_id: String,
    /// The newest parsed job (login job or `job` push); the mining side
    /// picks it up between job windows.
    job_slot: Arc<Mutex<Option<J>>>,
    submit_tx: Mutex<std::sync::mpsc::Sender<SubmitMsg>>,
    reader_shutdown: Arc<AtomicBool>,
    /// Shares accepted / rejected on this session (id-correlated — only
    /// replies to queued submits count; keepalive and other replies never
    /// do).
    pub shares_good: Arc<AtomicU64>,
    pub shares_bad: Arc<AtomicU64>,
}

impl<J: Send + Clone + 'static> Session<J> {
    /// Run one connection lifecycle: connect → login → spawn the reader
    /// loop. Returns the session plus the first job's parsed form. On
    /// login rejection or transport failure the socket is closed and the
    /// error returned (callers classify retryable vs fatal themselves via
    /// the error string, matching both miners' behavior).
    ///
    /// `pool_url` is `host:port`; `login_str` is the full login
    /// (`wallet[sep]worker` — see [`login_string`]).
    pub fn start(
        pool_url: &str,
        login_str: &str,
        password: &str,
        agent: &str,
        nonce_width: u8,
        hooks: ReaderHooks<J>,
    ) -> Result<(Self, Option<J>), String> {
        let (mut stream, mut reader) = connect(pool_url)?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| format!("set_read_timeout: {e}"))?;

        let (session_id, first_job) =
            match login(&mut reader, &mut stream, login_str, password, agent) {
                Ok(LoginOutcome::Ok { session_id, job }) => (session_id, job),
                Ok(LoginOutcome::Rejected(e)) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Err(e);
                }
                Err(e) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Err(e);
                }
            };
        info!("Stratum login OK (session {session_id})");

        let job_slot: Arc<Mutex<Option<J>>> = Arc::new(Mutex::new(None));
        let first_out = (hooks.parse)(&first_job, nonce_width).ok();
        if let Some(job) = &first_out {
            *job_slot.lock().unwrap() = Some(job.clone());
            (hooks.on_job_installed)(job);
        } else {
            // Login job malformed: not fatal — wait for a push (both miners
            // skip malformed jobs rather than dying).
            debug!("Login job failed to parse — waiting for a job push");
        }

        let reader_shutdown = Arc::new(AtomicBool::new(false));
        let (submit_tx, submit_rx) = std::sync::mpsc::channel::<SubmitMsg>();
        let shares_good = Arc::new(AtomicU64::new(0));
        let shares_bad = Arc::new(AtomicU64::new(0));

        let write_half = stream
            .try_clone()
            .map_err(|e| format!("clone write half: {e}"))?;
        let mut write_half = write_half;
        let mut reader = reader;
        let session_id_reader = session_id.clone();
        let job_slot_reader = Arc::clone(&job_slot);
        let hooks = Arc::new(hooks);
        let hooks_reader = Arc::clone(&hooks);
        let shutdown_reader = Arc::clone(&reader_shutdown);
        let shares_good_reader = Arc::clone(&shares_good);
        let shares_bad_reader = Arc::clone(&shares_bad);
        // The reader thread keeps draining submits until shutdown; drop the
        // join handle — its lifetime is bounded by shutdown().
        let _reader_thread = std::thread::Builder::new()
            .name("stratum-reader".to_string())
            .spawn(move || {
                let mut submit_rx = submit_rx;
                let result: Result<(), String> = run_reader_loop(
                    &mut reader,
                    &mut write_half,
                    &session_id_reader,
                    &job_slot_reader,
                    &shares_good_reader,
                    &shares_bad_reader,
                    hooks_reader.as_ref(),
                    nonce_width,
                    &mut submit_rx,
                    &shutdown_reader,
                    KEEPALIVE_INTERVAL,
                );
                result?;
                // Loop ended cleanly (shutdown flag). Drain queued submits so
                // any pending ack fails fast instead of waiting out the
                // caller's ack timeout on a session that is going away.
                while let Ok(msg) = submit_rx.try_recv() {
                    if let Some(ack) = msg.ack {
                        let _ = ack.send(Err("connection closed".to_string()));
                    }
                }
                Ok::<(), String>(())
            })
            .map_err(|e| format!("failed to spawn stratum reader thread: {e}"))?;

        // `stream` drops at scope end — the reader thread holds its own
        // clones, so the connection stays open. Do NOT call
        // stream.shutdown() here: shutdown() tears down the TCP connection
        // for ALL fds referencing the socket (clones included), which would
        // kill the reader thread immediately after login (live-verified via
        // the mock-pool smoke: login OK then instant EOF, every cycle).
        drop(stream);

        Ok((
            Self {
                session_id,
                job_slot,
                submit_tx: Mutex::new(submit_tx),
                reader_shutdown,
                shares_good,
                shares_bad,
            },
            first_out,
        ))
    }

    /// The pool session id (rides in every submit's `params.id`).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The newest parsed job, cloned out of the slot.
    pub fn take_job(&self) -> Option<J> {
        self.job_slot.lock().unwrap().as_ref().cloned()
    }

    /// Read-only access to the job slot (for external selection watchers).
    pub fn job_slot(&self) -> Arc<Mutex<Option<J>>> {
        Arc::clone(&self.job_slot)
    }

    /// Queue a share submit (fire-and-forget transport). `params` must
    /// already contain the pool fields (`id`/`job_id`/`nonce`/`result`); the
    /// session adds the correlating envelope id. Acceptance is accounted by
    /// the reader loop (shares_good/shares_bad + hooks).
    pub fn submit(&self, params: Value) -> Result<(), String> {
        self.submit_with_ack(params)
            .map(|_: std::sync::mpsc::Receiver<Result<(), String>>| ())
    }

    /// Queue a share submit and get a one-shot transport-ack receiver.
    /// `recv()` on it yields `Ok(())` once the frame reached the socket, or
    /// `Err(reason)` when the write failed / the reader thread died before
    /// acking. Acceptance itself is still asynchronous (accounted by the
    /// reader loop).
    pub fn submit_with_ack(
        &self,
        params: Value,
    ) -> Result<std::sync::mpsc::Receiver<Result<(), String>>, String> {
        let req = json!({"jsonrpc": "2.0", "id": 3, "method": "submit", "params": params});
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        self.submit_tx
            .lock()
            .unwrap()
            .send(SubmitMsg {
                params: req,
                ack: Some(ack_tx),
            })
            .map_err(|_| "reader thread gone — connection lost".to_string())?;
        Ok(ack_rx)
    }

    /// Stop the reader loop and close the session down.
    pub fn shutdown(&self) {
        self.reader_shutdown.store(true, Ordering::Relaxed);
    }
}

/// The reader loop: parse pushes, answer submits, send keepalives.
/// Returns Err with the connection-failure reason. Share accounting is
/// id-correlated (xmrig-style): only replies matching a queued submit id
/// count as share accept/reject — keepalive and other request replies are
/// never counted, and a pool that answers keepalive with "unknown method"
/// (LuckyPool, snipanet) has keepalives disabled after the first such reply
/// instead of polluting share stats.
#[allow(clippy::too_many_arguments)]
pub fn run_reader_loop<J: Clone>(
    reader: &mut BufReader<TcpStream>,
    write_half: &mut TcpStream,
    session_id: &str,
    job_slot: &Arc<Mutex<Option<J>>>,
    shares_good: &Arc<AtomicU64>,
    shares_bad: &Arc<AtomicU64>,
    hooks: &ReaderHooks<J>,
    nonce_width: u8,
    submit_rx: &mut std::sync::mpsc::Receiver<SubmitMsg>,
    reader_shutdown: &Arc<AtomicBool>,
    keepalive_interval: Duration,
) -> Result<(), String> {
    // The keepalive cadence only advances when reads return; without a read
    // timeout a silent pool would block read_line forever and no keepalive
    // would ever fire. Set it here so direct callers (tests) get the same
    // behavior Session::start provides.
    reader
        .get_ref()
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    let mut last_keepalive = Instant::now();
    // Id-correlated reply accounting: only errors/results matching a queued
    // submit id count as share accept/reject.
    let mut pending_submits: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut next_submit_id: u64 = 100;
    let mut keepalive_disabled = false;
    loop {
        if reader_shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Periodic keepalive — pools drop silent miners.
        if !keepalive_disabled && last_keepalive.elapsed() >= keepalive_interval {
            let ka = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "keepalive",
                "params": {"id": session_id}
            });
            send_message(write_half, &ka)?;
            debug!("Stratum keepalive sent");
            last_keepalive = Instant::now();
        }

        // Non-blocking check for queued submits (shares to send). Each gets
        // a unique id so its reply can be correlated.
        while let Ok(mut msg) = submit_rx.try_recv() {
            let id = next_submit_id;
            next_submit_id += 1;
            pending_submits.insert(id);
            if let Some(obj) = msg.params.as_object_mut() {
                obj.insert("id".to_string(), json!(id));
            }
            let res = send_message(write_half, &msg.params);
            if let Some(ack) = msg.ack {
                // The reply arrives as a regular line below; acceptance
                // accounting happens there. The ack reports transport status
                // only.
                let _ = ack.send(res.map_err(|e| e.to_string()));
            }
        }

        // Wait for the next line (with a short timeout slice so keepalives
        // and shutdown stay responsive). Capped: a newline-less flood from a
        // broken/malicious pool errors out instead of growing the buffer.
        let mut line = String::new();
        match read_line_capped(reader, &mut line, MAX_LINE_BYTES) {
            Ok(0) => return Err("connection closed by pool".to_string()),
            Ok(_) => {}
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(format!("read error: {e}")),
        }
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            match method {
                "job" => {
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    match (hooks.parse)(&params, nonce_width) {
                        Ok(job) => {
                            *job_slot.lock().unwrap() = Some(job.clone());
                            // Wake the mining window so fresh work is picked
                            // up immediately (no stale hashing).
                            (hooks.on_job_installed)(&job);
                        }
                        Err(e) => warn!("Malformed job push skipped: {e}"),
                    }
                }
                other => debug!("Unhandled stratum push: {other}"),
            }
            continue;
        }

        // A response to one of our requests. Correlate by id (xmrig-style):
        // only replies to queued submits count toward share stats.
        let msg_id = msg.get("id").and_then(Value::as_u64);
        let is_submit_reply = msg_id
            .map(|id| pending_submits.remove(&id))
            .unwrap_or(false);

        if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
            let text = match err.get("message").and_then(Value::as_str) {
                Some(m) => match err.get("code").and_then(Value::as_i64) {
                    Some(c) => format!("{c}: {m}"),
                    None => m.to_string(),
                },
                None => err.to_string(),
            };
            if is_submit_reply {
                info!("Share rejected: {text}");
                shares_bad.fetch_add(1, Ordering::Relaxed);
                (hooks.on_share_rejected)(&text);
            } else {
                (hooks.on_nonsubmit_error)(&text);
                let unknown_method = text.to_ascii_lowercase().contains("unknown method");
                if unknown_method && !keepalive_disabled {
                    info!("Pool does not support keepalive — disabling it (errors were not share rejections)");
                    keepalive_disabled = true;
                }
            }
            continue;
        }
        if is_submit_reply {
            debug!("Share accepted: {msg}");
            shares_good.fetch_add(1, Ordering::Relaxed);
            (hooks.on_share_accepted)();
        } else {
            debug!("Stratum response: {msg}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A loopback socket pair (client end, server end as BufReader+stream).
    fn socket_pair() -> (
        (TcpStream, BufReader<TcpStream>),
        (TcpStream, BufReader<TcpStream>),
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.set_nodelay(true).unwrap();
        server.set_nodelay(true).unwrap();
        let client_reader = BufReader::new(client.try_clone().unwrap());
        let server_reader = BufReader::new(server.try_clone().unwrap());
        ((client, client_reader), (server, server_reader))
    }

    fn read_line(reader: &mut BufReader<TcpStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[test]
    fn target_to_difficulty_matches_observed_pool_values() {
        // Observed live from xtm-rx-us.kryptex.network:7038 jobs:
        // f1290000 → tmp 10737 → difficulty 400015; e2530000 → 21474 → 200007.
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

    #[test]
    fn login_tolerates_interim_notifications_and_rejects_errors() {
        let ((mut c_stream, mut c_reader), (mut s_stream, mut s_reader)) = socket_pair();

        // Drive the login from the "pool" side.
        let pool = std::thread::spawn(move || {
            // An interim notification before the reply.
            serde_json::to_writer(
                &mut s_stream,
                &json!({"jsonrpc":"2.0","method":"job","params":{}}),
            )
            .unwrap();
            s_stream.write_all(b"\n").unwrap();
            // Then the login reply.
            serde_json::to_writer(
                &mut s_stream,
                &json!({"jsonrpc":"2.0","id":1,"result":{"id":"sess-1","job":{"job_id":"j"}},"status":"OK"}),
            )
            .unwrap();
            s_stream.write_all(b"\n").unwrap();
            // Read the login request for assertion.
            read_line(&mut s_reader)
        });

        let outcome = login(&mut c_reader, &mut c_stream, "W/rig", "x", "TestMiner/1.0").unwrap();
        let req = pool.join().unwrap();
        assert_eq!(req["method"], "login");
        assert_eq!(req["params"]["login"], "W/rig");
        assert_eq!(req["params"]["agent"], "TestMiner/1.0");
        match outcome {
            LoginOutcome::Ok { session_id, job } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(job["job_id"], "j");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn login_error_reply_is_rejected() {
        let ((mut c_stream, mut c_reader), (mut s_stream, _s_reader)) = socket_pair();
        let pool = std::thread::spawn(move || {
            serde_json::to_writer(
                &mut s_stream,
                &json!({"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"Invalid login"}}),
            )
            .unwrap();
            s_stream.write_all(b"\n").unwrap();
        });
        pool.join().unwrap();
        match login(&mut c_reader, &mut c_stream, "W", "x", "a/1") {
            Ok(LoginOutcome::Rejected(e)) => assert!(e.contains("Invalid login"), "{e}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn reader_loop_correlates_submits_and_disables_keepalive() {
        let ((mut c_stream, mut c_reader), (mut s_stream, mut s_reader)) = socket_pair();

        let shares_good = Arc::new(AtomicU64::new(0));
        let shares_bad = Arc::new(AtomicU64::new(0));
        let accepted = Arc::new(AtomicU64::new(0));
        let rejected = Arc::new(AtomicU64::new(0));
        let nonsubmit_errors = Arc::new(AtomicU64::new(0));
        let installed = Arc::new(AtomicU64::new(0));

        let hooks = ReaderHooks {
            parse: Arc::new(|raw: &Value, _nw: u8| {
                // Trivial "job" type for the test.
                Ok(raw
                    .get("job_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string())
            }),
            on_share_accepted: {
                let a = Arc::clone(&accepted);
                let _ = &shares_good;
                Arc::new(move || {
                    a.fetch_add(1, Ordering::Relaxed);
                })
            },
            on_share_rejected: {
                let r = Arc::clone(&rejected);
                Arc::new(move |_| {
                    r.fetch_add(1, Ordering::Relaxed);
                })
            },
            on_nonsubmit_error: {
                let n = Arc::clone(&nonsubmit_errors);
                Arc::new(move |_| {
                    n.fetch_add(1, Ordering::Relaxed);
                })
            },
            on_job_installed: {
                let i = Arc::clone(&installed);
                Arc::new(move |_| {
                    i.fetch_add(1, Ordering::Relaxed);
                })
            },
        };

        let reader_task = {
            let shares_good = Arc::clone(&shares_good);
            let shares_bad = Arc::clone(&shares_bad);
            let job_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let job_slot_c = Arc::clone(&job_slot);
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_c = Arc::clone(&shutdown);
            let (tx, rx) = std::sync::mpsc::channel::<SubmitMsg>();
            let handle = std::thread::spawn(move || {
                let mut rx = rx;
                run_reader_loop(
                    &mut c_reader,
                    &mut c_stream,
                    "sess-1",
                    &job_slot_c,
                    &shares_good,
                    &shares_bad,
                    &hooks,
                    64,
                    &mut rx,
                    &shutdown_c,
                    Duration::from_millis(150), // fast keepalive for the test
                )
            });
            (handle, shutdown, tx, job_slot)
        };
        let (handle, shutdown, tx, job_slot) = reader_task;

        // Give the reader a moment to start (first keepalive fires at 150ms).
        std::thread::sleep(Duration::from_millis(250));
        // Pool side: observe the keepalive, answer "unknown method".
        let ka = read_line(&mut s_reader);
        assert_eq!(ka["method"], "keepalive", "{ka}");
        serde_json::to_writer(
            &mut s_stream,
            &json!({"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found: unknown method"}}),
        )
        .unwrap();
        s_stream.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // No further keepalive may be sent: sleep past the interval again and
        // assert the pool side sees nothing.
        s_stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut probe = String::new();
        let n = s_reader.read_line(&mut probe).unwrap_or(0);
        assert_eq!(
            n, 0,
            "keepalive must be disabled after unknown-method reply"
        );
        s_stream.set_read_timeout(None).unwrap();

        // Push a job.
        serde_json::to_writer(
            &mut s_stream,
            &json!({"jsonrpc":"2.0","method":"job","params":{"job_id":"job-9"}}),
        )
        .unwrap();
        s_stream.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(job_slot.lock().unwrap().as_deref(), Some("job-9"));
        assert_eq!(installed.load(Ordering::Relaxed), 1);

        // Queue two submits via the channel; reply accept to one, reject to
        // the other, in REVERSE id order — correlation must be by id.
        let mk_msg = || SubmitMsg {
            params: json!({"jsonrpc":"2.0","method":"submit","params":{}}),
            ack: None,
        };
        tx.send(mk_msg()).unwrap();
        tx.send(mk_msg()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let s1 = read_line(&mut s_reader);
        let s2 = read_line(&mut s_reader);
        let id1 = s1["id"].as_u64().unwrap();
        let id2 = s2["id"].as_u64().unwrap();
        assert_ne!(id1, id2);
        assert_eq!(s1["method"], "submit");
        // Reply to the SECOND submit first (out of order).
        serde_json::to_writer(
            &mut s_stream,
            &json!({"jsonrpc":"2.0","id":id2,"result":{"status":"OK"}}),
        )
        .unwrap();
        s_stream.write_all(b"\n").unwrap();
        serde_json::to_writer(
            &mut s_stream,
            &json!({"jsonrpc":"2.0","id":id1,"error":{"code":24,"message":"Duplicate share"}}),
        )
        .unwrap();
        s_stream.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(shares_good.load(Ordering::Relaxed), 1);
        assert_eq!(shares_bad.load(Ordering::Relaxed), 1);
        assert_eq!(accepted.load(Ordering::Relaxed), 1);
        assert_eq!(rejected.load(Ordering::Relaxed), 1);
        // The keepalive "unknown method" error must NOT be a share rejection.
        assert_eq!(nonsubmit_errors.load(Ordering::Relaxed), 1);

        // Connection close ends the loop with an error. BOTH server-side fds
        // must be dropped — the BufReader holds a cloned fd that would keep
        // the socket half-open and starve the reader's EOF.
        drop((s_stream, s_reader));
        let result = handle.join().unwrap();
        shutdown.store(true, Ordering::Relaxed);
        assert!(result.is_err(), "closed connection must end the loop");
    }

    #[test]
    fn read_line_capped_delivers_lines_and_eof() {
        let mut data = std::io::Cursor::new(b"{\"a\":1}\n{\"b\":2}\ntrailing".to_vec());
        let mut line = String::new();
        assert_eq!(read_line_capped(&mut data, &mut line, 1024).unwrap(), 8);
        assert_eq!(line, "{\"a\":1}\n");
        assert_eq!(read_line_capped(&mut data, &mut line, 1024).unwrap(), 8);
        assert_eq!(line, "{\"b\":2}\n");
        let n = read_line_capped(&mut data, &mut line, 1024).unwrap();
        assert_eq!(line, "trailing"); // partial tail without newline (EOF)
        assert_eq!(n, 8);
        assert_eq!(read_line_capped(&mut data, &mut line, 1024).unwrap(), 0);
        assert!(line.is_empty());
    }

    #[test]
    fn read_line_capped_errors_past_cap_without_newline() {
        let mut data = std::io::Cursor::new(vec![b'x'; 4096]);
        let mut line = String::new();
        let err = read_line_capped(&mut data, &mut line, 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_capped_accepts_line_at_exact_cap() {
        let mut data = std::io::Cursor::new(vec![b'x'; 1023].to_vec());
        data.get_mut().push(b'\n');
        let mut line = String::new();
        read_line_capped(&mut data, &mut line, 1024).unwrap();
        assert_eq!(line.len(), 1024); // 1023 x's + newline (read_line semantics)
    }

    #[test]
    fn read_line_capped_rejects_invalid_utf8() {
        let mut data = std::io::Cursor::new(vec![0xFF, b'\n']);
        let mut line = String::new();
        let err = read_line_capped(&mut data, &mut line, 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn login_handshake_flood_is_rejected() {
        // Live TCP: a pool that floods bytes with no newline (no framing) must
        // terminate the login with a transport error, not grow memory forever.
        let ((client, mut reader), (server, _)) = socket_pair();
        std::thread::spawn(move || {
            // 64 MB so the server blocks in write before finishing (a 2 MB
            // flood fits in loopback socket buffers, and closing with the
            // client's login request unread sends RST — the race we test past).
            std::io::Write::write_all(&mut &server, &vec![b'x'; 64 * 1024 * 1024]).ok();
        });
        let mut stream = client;
        let err = login(&mut reader, &mut stream, "w", "p", "test/1.0");
        assert!(err.is_err(), "flood must abort login");
        let msg = err.unwrap_err();
        assert!(
            msg.contains("cap") || msg.contains("closed"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn oversized_session_line_disconnects() {
        let ((client, mut reader), (server, _)) = socket_pair();
        std::thread::spawn(move || {
            std::io::Write::write_all(&mut &server, &vec![b'y'; 2 * 1024 * 1024]).ok();
        });
        drop(client);
        let mut line = String::new();
        let err = read_line_capped(&mut reader, &mut line, MAX_LINE_BYTES);
        assert!(err.is_err(), "oversized session line must be an error");
    }
}
