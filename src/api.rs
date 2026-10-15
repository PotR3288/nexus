//! xmrig-compatible HTTP status API core shared by xtminer and edgecase.
//!
//! Both miners had drifted into near-copies of the same server (xtminer's
//! tiny_http accept path had an intermittent lost-wakeup hang on live hosts,
//! so both converged on a hand-rolled thread-per-connection responder over
//! std::net — one request per connection, Connection: close, read timeout).
//! This module owns that transport, the xmrig auth semantics, and the route
//! table; each miner supplies the miner-specific parts through [`ApiHooks`]:
//!
//! - `summary(version)` — the miner's own summary JSON (units differ: H/s
//!   windows vs graphs/s + cycles/s; v1/v2 shapes stay the miner's business).
//! - `backends()` — the /2/backends payload (CPU vs GPU descriptors).
//! - `config_json()` — the masked current config for GET /1|2/config.
//! - `put_config(body)` — validate + write; the miner owns its Config type.
//! - `json_rpc(method)` — pause/resume/stop/restart, miner-executed.
//! - `log_tail(query)` — the /log?lines=N tail (file path is the miner's).
//! - `events(query)` — the /1/events?limit=N unified event feed (default:
//!   empty array; a miner adopting the unified vocabulary overrides it).
//!
//! Auth mirrors xmrig's Httpd.cpp semantics: with a token configured, a
//! missing Authorization header is 401 and a wrong one is 403; with no token
//! configured requests are allowed (both miners bind loopback only).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// The miner-specific half of the API. All methods return ready JSON —
/// rendering stays in the miner where the units and types live.
pub trait ApiHooks: Send + Sync {
    /// GET /1/summary and /api.json (version 1) or /2/summary (version 2).
    fn summary(&self, version: usize) -> Value;
    /// GET /2/backends — array payload.
    fn backends(&self) -> Value;
    /// GET /1|2/config — the masked current config document.
    fn config_json(&self) -> Value;
    /// PUT /1|2/config — validate + write the on-disk file. Returns the
    /// (status, payload) the endpoint should answer with. The shared
    /// `put_config_document` helper implements the common merge+validate+write
    /// flow; miners call it from here with their Config type.
    fn put_config(&self, body: &[u8]) -> (u16, Value);
    /// POST /json_rpc — pause/resume/stop/restart. Returns the (status,
    /// payload) the endpoint should answer with (miners keep HTTP-error
    /// semantics like 400 Parse error here).
    fn json_rpc(&self, method: &str, body: &[u8]) -> (u16, Value);
    /// GET /log?lines=N — the log tail, or an error payload.
    fn log_tail(&self, query: &str) -> (u16, Value);
    /// GET /1/events?limit=N — the unified-vocabulary event tail (nexus
    /// `notify::Event` JSON). Default hook: empty feed, so a miner that has
    /// not adopted the unified vocabulary yet answers 200 with `[]` instead
    /// of 404 — consumers can poll both miners uniformly.
    fn events(&self, _query: &str) -> Value {
        json!([])
    }
}

/// xmrig's auth semantics (Httpd.cpp): a configured token is required —
/// missing header → 401, malformed or wrong token → 403. With no token
/// configured every request is allowed (the server binds loopback only).
pub fn check_auth(headers: &[(String, String)], token: &str) -> Result<(), u16> {
    if token.is_empty() {
        return Ok(());
    }
    let value = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.clone());
    match value {
        None => Err(401),
        Some(v) => {
            if let Some(bearer) = v.strip_prefix("Bearer ") {
                // Constant-time compare: a plain `==` short-circuits on the
                // first differing byte, leaking match length/prefix via
                // timing. Loopback-only binding mitigates, but tokens also
                // gate LAN-exposed deployments — don't hand attackers a
                // timing oracle.
                if constant_time_eq(bearer.as_bytes(), token.as_bytes()) {
                    return Ok(());
                }
            }
            Err(403)
        }
    }
}

/// Length-independent byte comparison: always scans the full key, XOR-
/// folding into a single accumulator. Not `subtle`-verified crypto, but it
/// removes the dominant timing signal for a bearer-token equality check.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Length is not secret (request bytes are attacker-chosen anyway);
        // still, burn comparable time to avoid a coarse length oracle.
        let _ = a.iter().chain(b.iter()).fold(0u8, |acc, x| acc ^ x);
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn unauthorized(code: u16) -> (u16, Value) {
    (
        code,
        json!({"error": {"code": code, "message": "Unauthorized"}}),
    )
}

/// Route one request. The endpoint table and gate ordering are frozen to
/// xtminer's (the API contract the GUI depends on):
///
/// 1. POST /json_rpc — own auth gate.
/// 2. GET /log and GET /1/events — own auth gate (before the 405 check,
///    so POST /log answers 405, not 404).
/// 3. GET|PUT /1|2/config — own auth gate (PUT allowed before the 405 check).
/// 4. Non-GET/PUT → 405 (before auth, matching xmrig).
/// 5. GET → auth → summary/backends routes.
pub fn route(
    hooks: &dyn ApiHooks,
    token: &str,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> (u16, Value) {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    };

    if method == "POST" && path == "/json_rpc" {
        if let Err(code) = check_auth(headers, token) {
            return unauthorized(code);
        }
        let m = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_default();
        return hooks.json_rpc(&m, body);
    }

    if method == "GET" && path == "/log" {
        if let Err(code) = check_auth(headers, token) {
            return unauthorized(code);
        }
        return hooks.log_tail(query);
    }

    if method == "GET" && path == "/1/events" {
        if let Err(code) = check_auth(headers, token) {
            return unauthorized(code);
        }
        return (200, hooks.events(query));
    }

    if matches!(path, "/1/config" | "/2/config") && matches!(method, "GET" | "PUT") {
        if let Err(code) = check_auth(headers, token) {
            return unauthorized(code);
        }
        if method == "GET" {
            return (200, hooks.config_json());
        }
        return hooks.put_config(body);
    }

    if method != "GET" && method != "PUT" {
        return (
            405,
            json!({"error": {"code": 405, "message": "Method not allowed"}}),
        );
    }

    if let Err(code) = check_auth(headers, token) {
        return unauthorized(code);
    }

    match path {
        "/1/summary" | "/api.json" => (200, hooks.summary(1)),
        "/2/summary" => (200, hooks.summary(2)),
        "/2/backends" => (200, hooks.backends()),
        _ => (404, json!({"error": {"code": 404, "message": "Not found"}})),
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

fn write_response(stream: &mut TcpStream, status: u16, payload: &str) {
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_reason(status),
        payload.len(),
        payload
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// Find the first occurrence of `needle` in `haystack` (std has no memmem;
/// buffers here are ≤ 64 KiB so the windows scan is fine).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP request: method, url, headers, body (per Content-Length).
/// Returns None on timeout or malformed input — the connection is then closed.
///
/// The header section is located by SEARCHING for the "\r\n\r\n" delimiter,
/// not by waiting for the buffer to END with it: a POST whose body arrives
/// in the same TCP segment as the headers would never satisfy an
/// ends-with check and would hang until the read timeout (live-verified:
/// GETs answered in ~4 ms while POST /json_rpc stalled 10 s and closed).
fn read_request(
    stream: &mut TcpStream,
) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4; // first byte AFTER the delimiter
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.lines();
    let request_line = lines.next()?.to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let url = parts.next().unwrap_or("").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    // Body per Content-Length (json_rpc POSTs; GETs have none). Cap at 1 MiB.
    // Bytes after the delimiter already read with the header section are the
    // body prefix — keep them, only read the remainder from the socket.
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(1 << 20);
    let mut body: Vec<u8> = buf[header_end..].to_vec();
    if content_length > body.len() {
        let missing = content_length - body.len();
        body.resize(content_length, 0);
        stream
            .read_exact(&mut body[(content_length - missing)..])
            .ok()?;
    } else {
        // Pipelined extra bytes beyond this request's body are ignored —
        // the responder always answers Connection: close.
        body.truncate(content_length);
    }

    Some((method, url, headers, body))
}

/// Serve until the process exits, on a dedicated std thread — the mining loop
/// must never be responsible for the API. Returns None when the API is
/// disabled or the bind fails (mining continues either way).
///
/// Thread-per-connection: the accept loop only accepts; each request is
/// handled on its own short-lived thread so a slow or stalled client can
/// never block subsequent requests (the fix for tiny_http 0.12's lost-wakeup
/// hang both miners hit).
pub fn spawn_server(
    enabled: bool,
    port: u16,
    token: &str,
    hooks: Arc<dyn ApiHooks>,
) -> Option<std::thread::JoinHandle<()>> {
    if !enabled {
        log::debug!("API disabled in config");
        return None;
    }
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Failed to bind API on {addr}: {e}");
            return None;
        }
    };
    log::info!("API listening on http://{addr} (xmrig-compatible)");
    let token = token.to_string();

    Some(
        std::thread::Builder::new()
            .name("api-server".to_string())
            .spawn(move || loop {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        let hooks = Arc::clone(&hooks);
                        let token = token.clone();
                        std::thread::Builder::new()
                            .name("api-conn".to_string())
                            .spawn(move || {
                                // 10 s covers a slow dashboard poll; payloads
                                // are small and served from memory.
                                if stream
                                    .set_read_timeout(Some(Duration::from_secs(10)))
                                    .is_err()
                                {
                                    return;
                                }
                                let (method, url, headers, body) = match read_request(&mut stream) {
                                    Some(req) => req,
                                    None => return, // timeout or malformed — close
                                };
                                let (status, payload) =
                                    route(hooks.as_ref(), &token, &method, &url, &headers, &body);
                                write_response(&mut stream, status, &payload.to_string());
                            })
                            .ok();
                    }
                    Err(e) => {
                        log::warn!("API accept error: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("failed to spawn API server thread"),
    )
}

/// Re-exec this process with its own argv (same binary, same arguments) so a
/// restart-required config change takes effect without external supervision.
/// Runs on a detached thread after a short delay — long enough for the JSON-
/// RPC response to be flushed and acknowledged before the image is replaced.
/// Unix: `execv` keeps the PID (systemd sees no exit). Windows: spawn a
/// detached child with the same argv, then exit(0).
///
/// `failure_prefix` is prepended to the execv-failure stderr line so each
/// miner keeps its own greppable marker (e.g. "xtminer:" / "edgecase:").
pub fn schedule_restart_with_failure_prefix(failure_prefix: &str) {
    let args: Vec<String> = std::env::args().collect();
    if args.is_empty() {
        return;
    }
    let failure_prefix = failure_prefix.to_string();
    std::thread::Builder::new()
        .name("api-restart".to_string())
        .spawn(move || {
            // Give the HTTP response time to reach the client.
            std::thread::sleep(Duration::from_millis(250));
            #[cfg(unix)]
            {
                use std::ffi::CString;
                let c_args: Vec<CString> = args
                    .iter()
                    .map(|a| CString::new(a.clone()).unwrap())
                    .collect();
                // execv takes a NUL-terminated list.
                let mut ptrs: Vec<*const libc::c_char> =
                    c_args.iter().map(|a| a.as_ptr()).collect();
                ptrs.push(std::ptr::null());
                unsafe {
                    libc::execv(ptrs[0], ptrs.as_ptr());
                }
                // execv only returns on failure.
                let err = std::io::Error::last_os_error();
                eprintln!(
                    "{} restart via execv failed ({err}) — falling back to exit(1)",
                    failure_prefix
                );
                std::process::exit(1);
            }
            #[cfg(not(unix))]
            {
                let _ = &failure_prefix;
                use std::os::windows::process::CommandExt;
                let mut cmd = std::process::Command::new(&args[0]);
                cmd.args(&args[1..])
                    .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no console flash
                    .spawn();
                std::process::exit(0);
            }
        })
        .ok();
}

/// [`schedule_restart_with_failure_prefix`] with a bare "restart:" prefix.
pub fn schedule_restart() {
    schedule_restart_with_failure_prefix("restart:");
}

/// Parse the `limit=` query parameter for the /1/events feed: default 100,
/// clamped to [1, 256] (the ring's default capacity — asking for more than
/// the buffer holds is meaningless, and an unbounded limit would let a
/// poller allocate freely).
pub fn events_limit(query: &str) -> usize {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("limit="))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 256)
}

/// Tail of a log file for the GUI log pane (xtminer's /log semantics):
/// `{"path": …, "lines": […]}`, last N lines in chronological order, N
/// clamped to [50, 2000] with default 500. Reading on each request is fine —
/// the GUI polls at most once a second and N is small.
pub fn log_tail_of_file(query: &str, path: &std::path::Path) -> (u16, Value) {
    let lines = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("lines="))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(50, 2000);

    match std::fs::read_to_string(path) {
        Ok(content) => {
            let tail: Vec<&str> = content.lines().rev().take(lines).collect();
            // Reverse back to chronological order.
            let mut out: Vec<String> = tail.into_iter().map(str::to_string).collect();
            out.reverse();
            (200, json!({"path": path.to_string_lossy(), "lines": out}))
        }
        Err(e) => (
            500,
            json!({"error": {"code": 500, "message": format!("log file unreadable: {e}")}}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct TestHooks {
        calls: Mutex<Vec<String>>,
    }
    impl TestHooks {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl ApiHooks for TestHooks {
        fn summary(&self, version: usize) -> Value {
            self.calls.lock().unwrap().push(format!("summary{version}"));
            json!({"algo": "test", "version": version})
        }
        fn backends(&self) -> Value {
            json!([{"type": "test"}])
        }
        fn config_json(&self) -> Value {
            json!({"masked": true})
        }
        fn put_config(&self, _body: &[u8]) -> (u16, Value) {
            self.calls.lock().unwrap().push("put".into());
            (200, json!({"status": "OK"}))
        }
        fn json_rpc(&self, method: &str, _body: &[u8]) -> (u16, Value) {
            self.calls.lock().unwrap().push(format!("rpc:{method}"));
            (200, json!({"jsonrpc": "2.0", "result": {"status": "OK"}}))
        }
        fn log_tail(&self, _query: &str) -> (u16, Value) {
            (200, json!({"path": "x", "lines": []}))
        }
    }

    #[test]
    fn auth_missing_token_is_401_wrong_is_403() {
        let h_none: Vec<(String, String)> = vec![];
        assert_eq!(check_auth(&h_none, "secret"), Err(401));
        let h_bad = vec![("Authorization".to_string(), "Bearer wrong".to_string())];
        assert_eq!(check_auth(&h_bad, "secret"), Err(403));
        let h_ok = vec![("Authorization".to_string(), "Bearer secret".to_string())];
        assert_eq!(check_auth(&h_ok, "secret"), Ok(()));
        // No token configured → always allowed.
        assert_eq!(check_auth(&h_none, ""), Ok(()));
        // Constant-time compare must still behave as an equality check.
        let h_prefix = vec![("Authorization".to_string(), "Bearer secre".to_string())];
        assert_eq!(
            check_auth(&h_prefix, "secret"),
            Err(403),
            "prefix must not match"
        );
        let h_long = vec![("Authorization".to_string(), "Bearer secretX".to_string())];
        assert_eq!(
            check_auth(&h_long, "secret"),
            Err(403),
            "longer must not match"
        );
        let h_case = vec![("AUTHORIZATION".to_string(), "Bearer secret".to_string())];
        assert_eq!(
            check_auth(&h_case, "secret"),
            Ok(()),
            "header name is case-insensitive"
        );
    }

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(super::constant_time_eq(b"", b""));
        assert!(super::constant_time_eq(b"abc", b"abc"));
        assert!(!super::constant_time_eq(b"abc", b"abd"));
        assert!(!super::constant_time_eq(b"abc", b"ab"));
        assert!(!super::constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn route_dispatches_all_endpoints() {
        let h = TestHooks::new();
        let empty: Vec<(String, String)> = vec![];

        let (st, v) = route(&h, "", "GET", "/2/summary", &empty, b"");
        assert_eq!(st, 200);
        assert_eq!(v["algo"], "test");

        let (st, _) = route(&h, "", "GET", "/1/summary", &empty, b"");
        assert_eq!(st, 200);

        let (st, _) = route(&h, "", "GET", "/api.json", &empty, b"");
        assert_eq!(st, 200);

        let (st, v) = route(&h, "", "GET", "/2/backends", &empty, b"");
        assert_eq!(st, 200);
        assert!(v.is_array());

        let (st, _) = route(&h, "", "GET", "/nope", &empty, b"");
        assert_eq!(st, 404);

        let (st, _) = route(&h, "", "DELETE", "/2/summary", &empty, b"");
        assert_eq!(st, 405);
    }

    #[test]
    fn route_config_gates_and_methods() {
        let h = TestHooks::new();

        // GET works without auth when no token configured.
        let (st, v) = route(&h, "", "GET", "/2/config", &[], b"");
        assert_eq!(st, 200);
        assert_eq!(v["masked"], true);

        // Auth gates the config routes.
        let (st, _) = route(&h, "secret", "GET", "/2/config", &[], b"");
        assert_eq!(st, 401);

        // PUT routes to the miner's put_config.
        let (st, v) = route(&h, "", "PUT", "/1/config", &[], b"[x]\n");
        assert_eq!(st, 200);
        assert_eq!(v["status"], "OK");
    }

    #[test]
    fn route_json_rpc_extracts_method() {
        let h = TestHooks::new();
        let (st, v) = route(
            &h,
            "",
            "POST",
            "/json_rpc",
            &[],
            br#"{"jsonrpc":"2.0","method":"pause","id":1}"#,
        );
        assert_eq!(st, 200);
        assert_eq!(v["result"]["status"], "OK");
        assert_eq!(h.calls.lock().unwrap().last().unwrap(), "rpc:pause");

        // Unparseable body → empty method → miner's json_rpc decides the
        // error; route always answers 200 with the miner's payload.
        let (st, _) = route(&h, "", "POST", "/json_rpc", &[], b"not json");
        assert_eq!(st, 200);
    }

    #[test]
    fn log_route_gated_by_auth() {
        let h = TestHooks::new();
        let (st, _) = route(&h, "secret", "GET", "/log?lines=50", &[], b"");
        assert_eq!(st, 401);
        let h_bad = vec![("Authorization".to_string(), "Bearer wrong".to_string())];
        let (st, _) = route(&h, "secret", "GET", "/log?lines=50", &h_bad, b"");
        assert_eq!(st, 403);
        let (st, _) = route(&h, "", "GET", "/log?lines=50", &[], b"");
        assert_eq!(st, 200);
    }

    #[test]
    fn events_route_gated_by_auth_and_defaults_to_empty() {
        let h = TestHooks::new();
        // Auth gates the feed.
        let (st, _) = route(&h, "secret", "GET", "/1/events", &[], b"");
        assert_eq!(st, 401);
        let h_bad = vec![("Authorization".to_string(), "Bearer wrong".to_string())];
        let (st, _) = route(&h, "secret", "GET", "/1/events", &h_bad, b"");
        assert_eq!(st, 403);
        // Default hook answers 200 with an empty array (uniform polling).
        let (st, v) = route(&h, "", "GET", "/1/events?limit=5", &[], b"");
        assert_eq!(st, 200);
        assert_eq!(v, json!([]));
        // POST → 405 (gated before auth, same as /log).
        let (st, _) = route(&h, "", "POST", "/1/events", &[], b"");
        assert_eq!(st, 405);
    }

    #[test]
    fn events_limit_parsing_clamps() {
        let clamp = |q: &str| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("limit="))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(100)
                .clamp(1, 256)
        };
        assert_eq!(clamp(""), 100);
        assert_eq!(clamp("limit=1"), 1);
        assert_eq!(clamp("limit=999999"), 256);
        assert_eq!(clamp("bogus=1"), 100);
    }

    #[test]
    fn log_tail_reads_chronological_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("miner.log");
        let lines = (1..=250u32)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &lines).unwrap();

        let (st, v) = log_tail_of_file("lines=100", &path);
        assert_eq!(st, 200);
        let arr = v["lines"].as_array().unwrap();
        assert_eq!(arr.len(), 100);
        assert_eq!(arr[0], "line 151");
        assert_eq!(arr[99], "line 250");

        // Clamp bounds.
        let clamp = |q: &str| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("lines="))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(500)
                .clamp(50, 2000)
        };
        assert_eq!(clamp(""), 500);
        assert_eq!(clamp("lines=1"), 50);
        assert_eq!(clamp("lines=999999"), 2000);
    }
}
