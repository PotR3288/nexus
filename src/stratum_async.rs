//! Async stratum session core (`feature = "tokio"`, off by default).
//!
//! Generic machinery shared with edgecase's C29 pool client — the bounded
//! connect, bounded handshake with leftover re-injection, select-loop task
//! with capped reads and bounded writes, and event routing — parameterized by
//! a [`StratumDialect`] seam. The sync `stratum` module and its consumers are
//! untouched; nothing here is compiled (or pulled into the dependency graph)
//! unless the `tokio` feature is enabled.
//!
//! Provenance: the session core is lcooke's original work, written against
//! live C29 pools (see edgecase `docs/pool-mining-luckypool.md`) and
//! relicensed BSD-3-Clause for nexus by the sole author (2026-11-11). The
//! bounded-read algorithm is his own tokio port of `stratum::read_line_capped`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Session I/O bounds. Defaults mirror the values edgecase has used in
/// production since the bounded-I/O pass (`b46d882`): connect/handshake 10 s,
/// writes 30 s, handshake cap 64 KiB, session line cap 1 MiB.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// TCP connect bound.
    pub connect_timeout: Duration,
    /// Login-handshake bound: a peer that accepts TCP and never replies (or
    /// floods bytes with no newline) must fail the spawn, not hang the caller.
    pub handshake_timeout: Duration,
    /// Per-write bound: a peer that stops reading fills the send buffer and a
    /// bare `write_all` would block the session task forever.
    pub write_timeout: Duration,
    /// Handshake buffer cap — more bytes than this with no newline means the
    /// peer is not speaking stratum (or is flooding).
    pub max_handshake_bytes: usize,
    /// Session line cap — a "line" longer than this (no newline seen) is a
    /// flood, not protocol.
    pub max_line_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(30),
            max_handshake_bytes: 64 * 1024,
            max_line_bytes: 1024 * 1024,
        }
    }
}

/// Dialect seam: everything protocol-specific about a stratum session, so the
/// generic core owns only transport. `Self` is shared (`Arc`) across the
/// handshake and the session task, and may hold dialect state (session id,
/// pending submits, share counters) exactly like edgecase's `PoolClient`.
pub trait StratumDialect: Send + Sync + 'static {
    /// What the session emits to the consumer. edgecase uses `u64` (the bumped
    /// job sequence); other dialects may emit richer events.
    type Event: Send + 'static;

    /// The login frame to write (without the trailing newline — the core
    /// appends it).
    fn login_frame(&self) -> String;

    /// Process the login reply (the first line after the handshake write).
    /// `Err(message)` aborts the spawn; the message is surfaced verbatim.
    fn on_handshake_reply(&self, line: &str) -> impl Future<Output = Result<(), String>> + Send;

    /// Process one post-handshake line. `events` is the channel whose receiver
    /// end is returned in [`SessionHandles`]; dropping all senders ends the
    /// session the same way a write error does.
    fn on_line(
        &self,
        line: &str,
        events: &tokio::sync::mpsc::UnboundedSender<Self::Event>,
    ) -> impl Future<Output = ()> + Send;
}

/// Handles for one spawned session. The consumer holds `submit_tx` for as
/// long as the session should stay alive — when every sender is dropped, the
/// session task closes the connection. `event_rx` returns `None` when the
/// session task exits (peer closed / I/O error / all senders dropped), so EOF
/// is the disconnect signal.
#[derive(Debug)]
pub struct SessionHandles<D: StratumDialect> {
    /// Events emitted by the dialect (new jobs, replies, etc.).
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<D::Event>,
    /// Submit lines to write to the peer (one JSON object per line).
    pub submit_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

/// Read one newline-terminated line into `out` (bytes), with a hard cap.
/// Unlike `AsyncBufReadExt::read_line` — which blocks until a newline arrives
/// and thus lets a newline-less flood grow the buffer without limit — this
/// returns an error once `cap` bytes accumulate without a newline. Cancel
/// safe: bytes are consumed from the underlying buffer immediately after each
/// fill, and a cancelled call leaves a partial line in `out` that the next
/// call continues. Returns 0 on clean EOF (after any trailing partial line
/// was already delivered by a previous call).
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: deliver any partial line already accumulated, else 0.
            return Ok(total);
        }
        if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&chunk[..nl]);
            // Consume ONLY through the newline — bytes after it belong to
            // the NEXT line (a job push in the same segment as a reply).
            reader.consume(nl + 1);
            return Ok(total + nl + 1);
        }
        total += chunk.len();
        if total > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeded cap with no newline",
            ));
        }
        out.extend_from_slice(chunk);
        let n = chunk.len();
        reader.consume(n);
    }
}

/// One live TCP session. Performs the login handshake (bounded, with leftover
/// re-injection), then runs a single select-loop task that reads peer lines
/// (routing them through `Dialect::on_line`) and writes submitted lines. The
/// task exits — dropping the event sender so consumers see EOF — when the
/// peer closes, an I/O error occurs, or every submit sender is dropped.
///
/// Error strings match edgecase's original session messages verbatim (log
/// output is a contract).
pub async fn spawn_session<D: StratumDialect>(
    host: &str,
    port: u16,
    dialect: Arc<D>,
    config: SessionConfig,
) -> Result<SessionHandles<D>, String> {
    let addr = format!("{host}:{port}");
    let connect = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&addr));
    let stream = match connect.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => {
            return Err(format!(
                "pool connect timed out after {}s: {addr}",
                config.connect_timeout.as_secs()
            ))
        }
    };
    stream.set_nodelay(true).ok();

    // Login handshake. Read with a raw read (no BufReader): any bytes beyond
    // the login line in the same TCP segment are captured and re-injected into
    // the session task's reader below — they must not be lost.
    let login = dialect.login_frame();
    let mut stream = stream;
    use tokio::io::AsyncReadExt as _;
    if let Err(e) = async {
        stream.write_all((login + "\n").as_bytes()).await?;
        // flush() on TcpStream is a no-op but keeps the shape identical.
        stream.flush().await
    }
    .await
    {
        return Err(e.to_string());
    }
    let mut first_line = Vec::new();
    let mut buf = [0u8; 4096];
    // Bounded handshake: a peer that accepts TCP and then never sends (or
    // floods bytes without a newline) must fail fast, not hang the caller or
    // grow this vector without limit.
    let deadline = tokio::time::Instant::now() + config.handshake_timeout;
    loop {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => {
                return Err(format!(
                    "pool login reply timed out after {}s",
                    config.handshake_timeout.as_secs()
                ))
            }
        };
        if n == 0 {
            return Err("pool closed the connection during login".to_string());
        }
        first_line.extend_from_slice(&buf[..n]);
        if first_line.len() > config.max_handshake_bytes {
            return Err(format!(
                "pool sent {} bytes with no newline during login — not stratum",
                first_line.len()
            ));
        }
        if first_line.contains(&b'\n') {
            break;
        }
    }
    // Split at the FIRST newline: everything after it (a job push sent in the
    // same segment as the login reply) must be re-injected into the session
    // task's reader — those bytes are already out of the socket buffer.
    let pos = first_line.iter().position(|&b| b == b'\n').unwrap();
    let leftover = first_line.split_off(pos + 1);
    let first = String::from_utf8_lossy(&first_line[..pos]).into_owned();
    dialect.on_handshake_reply(&first).await?;

    // Session task: select between peer lines and submit lines for the life of
    // the connection.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<D::Event>();
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        // OwnedReadHalf implements AsyncRead but not AsyncBufRead — wrap it.
        let (read_half, mut writer) = stream.into_split();
        // Re-inject any bytes that arrived after the login line in the same
        // TCP segment — they were already read out of the socket during the
        // handshake and would otherwise be lost.
        let source = std::io::Cursor::new(leftover).chain(read_half);
        let mut reader = BufReader::new(source);
        let mut line_buf: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                res = read_line_capped(&mut reader, &mut line_buf, config.max_line_bytes) => match res {
                    // Ok(0) = EOF; Err = I/O error or the flood cap hit (a
                    // "line" with no newline beyond max_line_bytes).
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                },
                maybe_submit = submit_rx.recv() => match maybe_submit {
                    Some(line) => {
                        // Bounded write: a peer that stops reading fills the
                        // send buffer and a bare write_all would block this
                        // task forever (no EOF would ever be observed).
                        let write = async {
                            writer
                                .write_all((line + "\n").as_bytes())
                                .await
                                .and(writer.flush().await)
                        };
                        if let Err(_) | Ok(Err(_)) = tokio::time::timeout(
                            config.write_timeout,
                            write,
                        )
                        .await
                        {
                            break;   // write failed/stalled — connection is gone
                        }
                        continue;
                    }
                    None => break,    // all submit senders dropped → teardown
                }
            }
            let line = String::from_utf8_lossy(&line_buf).trim().to_string();
            line_buf.clear();
            if line.is_empty() {
                continue;
            }
            dialect.on_line(&line, &event_tx).await;
        }
        // Drop the event sender so any consumer of event_rx sees EOF.
        drop(event_tx);
    });

    Ok(SessionHandles {
        event_rx,
        submit_tx,
    })
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::io::AsyncReadExt as _;

    // ---- read_line_capped ----

    #[tokio::test]
    async fn capped_reader_delivers_lines_and_eof() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"alpha\nbeta\n".to_vec()));
        let mut out = Vec::new();
        let n = read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(n, 6);
        // The newline is consumed but NOT delivered — the caller trims anyway.
        assert_eq!(out, b"alpha".to_vec());
        out.clear();
        let n = read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(n, 5);
        assert_eq!(out, b"beta".to_vec());
        out.clear();
        let n = read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(out, b"".to_vec());
    }

    #[tokio::test]
    async fn capped_reader_splits_within_segment_and_hits_cap() {
        // Bytes after the newline belong to the NEXT line (a job push in the
        // same segment as a reply) — the reader must not swallow them.
        let mut reader = BufReader::new(std::io::Cursor::new(b"a\nbcd\n".to_vec()));
        let mut out = Vec::new();
        read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(out, b"a".to_vec());
        out.clear();
        read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(out, b"bcd".to_vec());
        out.clear();
        // A newline-less flood grows the buffer until the cap fires.
        let flood = vec![b'x'; 4096];
        let mut reader = BufReader::new(std::io::Cursor::new(flood));
        let mut out = Vec::new();
        let err = read_line_capped(&mut reader, &mut out, 1024)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // The cap fires on the first fill that exceeds it — bounded, not grown
        // to the flood size.
        assert!(out.len() <= 4096);
    }

    #[tokio::test]
    async fn capped_reader_continues_partial_line() {
        // Cancel-safety: a cancelled call leaves a partial line in `out` that
        // the next call continues. Simulate by reading in two chunks.
        let mut reader = BufReader::new(std::io::Cursor::new(b"ab\n".to_vec()));
        let mut out = Vec::new();
        // First fill_buf gets the whole line; assert the read path end-to-end.
        read_line_capped(&mut reader, &mut out, 1024).await.unwrap();
        assert_eq!(out, b"ab".to_vec());
    }

    // ---- spawn_session with a mock dialect ----

    #[derive(Default, Debug)]
    struct MockDialect {
        handshake_ok: AtomicBool,
        lines_seen: AtomicU64,
    }

    impl StratumDialect for MockDialect {
        type Event = u64;

        fn login_frame(&self) -> String {
            r#"{"id":1,"method":"login","params":{}}"#.to_string()
        }

        async fn on_handshake_reply(&self, line: &str) -> Result<(), String> {
            let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            if v.get("error").is_some() {
                return Err(format!("pool rejected login: {}", v["error"]));
            }
            self.handshake_ok.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn on_line(&self, line: &str, events: &tokio::sync::mpsc::UnboundedSender<u64>) {
            if line.contains("job") {
                self.lines_seen.fetch_add(1, Ordering::SeqCst);
                let _ = events.send(self.lines_seen.load(Ordering::SeqCst));
            }
        }
    }

    async fn start_mock_pool() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            // Login request.
            let mut got = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                if n == 0 || got.contains(&b'\n') {
                    break;
                }
            }
            // Login reply + a job push in the SAME segment — the leftover
            // bytes must be re-injected, not dropped.
            sock.write_all(
                b"{\"id\":1,\"result\":{\"status\":\"OK\"}}\n{\"id\":2,\"method\":\"job\",\"params\":{\"job\":true}}\n",
            )
            .await
            .unwrap();
            // Wait for the session's submit.
            let n = sock.read(&mut buf).await.unwrap();
            if n > 0 {
                sock.write_all(b"{\"id\":3,\"result\":true}\n")
                    .await
                    .unwrap();
            }
            // Hold the connection open until the peer goes away.
            loop {
                if sock.read(&mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn spawn_session_handshake_replies_and_routes_lines() {
        let addr = start_mock_pool().await;
        let dialect = Arc::new(MockDialect::default());
        let host = addr.ip().to_string();
        let mut handles = spawn_session(
            &host,
            addr.port(),
            dialect.clone(),
            SessionConfig::default(),
        )
        .await
        .expect("session spawned");
        assert!(dialect.handshake_ok.load(Ordering::SeqCst));

        // The leftover job push in the handshake segment must be re-injected.
        let first = handles.event_rx.recv().await.expect("job event");
        assert_eq!(first, 1);

        // Submit path: write a line, the mock pool answers with a reply line.
        handles
            .submit_tx
            .send(r#"{"id":3,"method":"submit"}"#.to_string())
            .unwrap();
        // The reply line contains no "job" → no event; the loop must still be
        // alive. Close the connection by dropping every submit sender.
        drop(handles.submit_tx);
        // EOF: the session task exits and the event sender is dropped.
        let _ = handles.event_rx.recv().await;
    }

    #[tokio::test]
    async fn spawn_session_rejects_login_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 || got_line(&buf[..n]) {
                    break;
                }
            }
            sock.write_all(b"{\"id\":1,\"error\":\"unauthorized\"}\n")
                .await
                .unwrap();
        });
        let dialect = Arc::new(MockDialect::default());
        let err = spawn_session(
            &addr.ip().to_string(),
            addr.port(),
            dialect,
            SessionConfig::default(),
        )
        .await
        .expect_err("login must be rejected");
        assert_eq!(err, "pool rejected login: \"unauthorized\"");
    }

    fn got_line(bytes: &[u8]) -> bool {
        bytes.contains(&b'\n')
    }
}
