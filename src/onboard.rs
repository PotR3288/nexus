//! Onboarding helpers shared by xtminer and edgecase: a self-check report
//! framework and first-run wizard prompt helpers.
//!
//! Both miners expose `--check` (verify the machine is ready to mine, with
//! plain-language verdicts and fix hints) and `--wizard` (interactive first
//! config). The *specific* checks and questions differ per miner — the
//! framework here only standardizes the shape and rendering so a user sees
//! the same UX in both.
//!
//! Output contract (both tools):
//!   ✓ name — detail        (pass)
//!   ! name — detail        (warning: usable, but suboptimal; hint may follow)
//!   ✗ name — detail        (failure: mining will not work; hint follows)
//! Exit code: 0 when nothing failed, 1 when at least one check failed.
//! Warnings never change the exit code (an absent-but-optional feature must
//! not block a first run).

use std::io::{self, BufRead, Write};
use std::path::Path;

/// Outcome of a single check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Everything is in order.
    Pass,
    /// Usable, but below par (e.g. hugepages not raised → lower hashrate).
    Warn,
    /// Mining cannot work until this is fixed.
    Fail,
}

/// One checked fact about the machine/config.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    /// Human-readable one-line result (plain words, no jargon).
    pub detail: String,
    /// How to fix a Warn/Fail; empty when nothing to fix.
    pub hint: String,
}

impl Check {
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Pass,
            detail: detail.into(),
            hint: String::new(),
        }
    }

    pub fn warn(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
            hint: hint.into(),
        }
    }

    pub fn fail(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
            hint: hint.into(),
        }
    }
}

/// A full check run: header line, the checks, and the summary verdict.
pub struct Report {
    pub title: String,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            checks: Vec::new(),
        }
    }

    pub fn push(&mut self, c: Check) {
        self.checks.push(c);
    }

    /// Number of failed checks (drives the process exit code).
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count()
    }

    /// Plain-text rendering (no colors: installers and piped output must stay
    /// readable — same rule as the miners' log files).
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.title);
        out.push('\n');
        for c in &self.checks {
            let mark = match c.status {
                Status::Pass => "✓",
                Status::Warn => "!",
                Status::Fail => "✗",
            };
            out.push_str(&format!("{} {} — {}\n", mark, c.name, c.detail));
            if !c.hint.is_empty() && c.status != Status::Pass {
                out.push_str(&format!("    fix: {}\n", c.hint));
            }
        }
        let f = self.failures();
        let w = self.warnings();
        if f == 0 && w == 0 {
            out.push_str("\nAll checks passed — this machine is ready to mine.\n");
        } else if f == 0 {
            out.push_str(&format!(
                "\nReady to mine, with {w} warning(s) above. Warnings lower\nperformance but do not stop mining.\n"
            ));
        } else {
            out.push_str(&format!(
                "\nNOT ready: {f} problem(s) above must be fixed before mining.\nFix each ✗ item, then run --check again.\n"
            ));
        }
        out
    }
}

/// Parse "host[:port]" (no scheme) into (host, port), defaulting to the given
/// port when absent. Accepts `host`, `host:port`, `[v6]:port`; scheme
/// prefixes (stratum+tcp://, http://) are stripped so users can paste pool
/// URLs straight from a pool's website.
pub fn endpoint_host_port(endpoint: &str, default_port: u16) -> Option<(String, u16)> {
    let mut s = endpoint.trim();
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    // Drop any path suffix (/wallet.worker is common in pool URLs).
    let s = s.split('/').next().unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6 literal: [::1]:3333
        let close = rest.find(']')?;
        let host = &rest[..close];
        let rest = rest[close + 1..].strip_prefix(':')?;
        let port = rest.parse().ok()?;
        if host.is_empty() {
            return None;
        }
        return Some((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => Some((h.to_string(), port)),
            // A colon that isn't a port separator (rare, unbracketed v6) —
            // treat the whole string as a host with the default port.
            Err(_) => Some((s.to_string(), default_port)),
        },
        None => Some((s.to_string(), default_port)),
    }
}

/// TCP-connect to host:port with a bounded timeout. `Ok(true)` = reachable.
/// Used by `--check` in both miners for pool/stratum endpoints and for the
/// edgecase base-node gRPC address (proves routing + a listener exists; it
/// cannot prove protocol correctness, and the detail text must say so).
pub fn tcp_reachable(host: &str, port: u16, timeout: std::time::Duration) -> Result<bool, String> {
    use std::net::ToSocketAddrs;
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?;
    let mut last_err: Option<String> = None;
    for addr in addrs {
        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => return Ok(true),
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    Err(last_err.unwrap_or_else(|| format!("no addresses resolved for {host}:{port}")))
}

/// Run a TCP reachability check and fold it into a [`Check`] (shared by both
/// miners' `--check` for pool endpoints / base-node addresses). A success
/// proves routing + a listening socket, NOT protocol correctness.
pub fn endpoint_check(
    name: &str,
    endpoint: &str,
    default_port: u16,
    protocol_note: &str,
) -> Check {
    match endpoint_host_port(endpoint, default_port) {
        None => Check::fail(
            name,
            format!("endpoint '{endpoint}' is not a host:port address"),
            "Set a host:port endpoint (e.g. pool.example.com:3333).",
        ),
        Some((host, port)) => match tcp_reachable(&host, port, std::time::Duration::from_secs(5)) {
            Ok(true) => Check::pass(
                name,
                format!("{endpoint} is reachable ({protocol_note} not verified — the first real connection decides that)"),
            ),
            Ok(false) => Check::fail(
                name,
                format!("{endpoint} did not accept a connection"),
                "Check the endpoint address, and whether a firewall blocks outbound connections.",
            ),
            Err(e) => Check::fail(
                name,
                format!("{endpoint}: {e}"),
                "Check DNS and your internet connection.",
            ),
        },
    }
}

// ── Wizard prompt helpers ────────────────────────────────────────────────────
// Interactive first-run questions. stdin is read line-by-line (a wizard is
// inherently interactive; non-tty callers are expected to gate on --wizard
// being explicit). Every helper is unit-testable through `ask_with` which
// takes the reader explicitly.

/// Ask `prompt` and read a line; empty input returns `default` (shown to the
/// user as `(default: X)` when non-empty). Trailing whitespace trimmed.
pub fn ask_with(reader: &mut dyn BufRead, prompt: &str, default: &str) -> String {
    print_question(prompt, default);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return default.to_string();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn print_question(prompt: &str, default: &str) {
    let mut out = io::stdout();
    if default.is_empty() {
        let _ = write!(out, "{prompt}: ");
    } else {
        let _ = write!(out, "{prompt} (default: {default}): ");
    }
    let _ = out.flush();
}

/// Like [`ask_with`] but keeps asking until non-empty (no default allowed —
/// used for wallet addresses where "empty" would mine to nobody).
pub fn ask_required(reader: &mut dyn BufRead, prompt: &str) -> String {
    loop {
        print_question(prompt, "");
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            println!("A value is required here.");
            continue;
        }
        return line.trim().to_string();
    }
}

/// Yes/no question. Empty input takes `default`; y/yes (case-insensitive) →
/// true, anything else → false.
pub fn confirm_with(reader: &mut dyn BufRead, prompt: &str, default: bool) -> bool {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {}: ", prompt, suffix);
    let _ = io::stdout().flush();
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return default;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Write config content atomically (temp file + rename in the same directory,
/// same durability rule as the API's PUT /config path). Refuses to overwrite
/// an existing file — the caller must check-and-confirm first; this is the
/// last-resort guard so a wizard can never clobber a live config.
pub fn write_config_atomic(path: &Path, content: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        ));
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.wizard-tmp",
        path.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".to_string())
    ));
    std::fs::write(&tmp, content).map_err(|e| format!("write failed: {e}"))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("rename failed: {e}")
        })
        .map(|_| {
            // Best-effort chmod 600: a wizard-written config holds a wallet
            // address and possibly pool credentials.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_render_and_exit_codes() {
        let mut r = Report::new("Test");
        r.push(Check::pass("config", "loaded"));
        assert_eq!(r.failures(), 0);
        assert!(r.render().contains("All checks passed"));

        r.push(Check::warn("hugepages", "not raised", "echo vm.nr_hugepages=..."));
        assert_eq!(r.failures(), 0);
        assert!(r.render().contains("warning(s)"));
        assert!(r.render().contains("fix:"));

        r.push(Check::fail("wallet", "empty", "run --wizard"));
        assert_eq!(r.failures(), 1);
        assert!(r.render().contains("NOT ready"));
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            endpoint_host_port("stratum+tcp://pool.example:3333", 5555),
            Some(("pool.example".into(), 3333))
        );
        assert_eq!(
            endpoint_host_port("pool.example", 5555),
            Some(("pool.example".into(), 5555))
        );
        assert_eq!(
            endpoint_host_port("http://127.0.0.1:18081/json_rpc", 80),
            Some(("127.0.0.1".into(), 18081))
        );
        assert_eq!(
            endpoint_host_port("[::1]:18081", 80),
            Some(("::1".into(), 18081))
        );
        assert_eq!(endpoint_host_port("", 80), None);
    }

    #[test]
    fn wizard_prompts_take_defaults_and_required() {
        let mut r = "line one\n\n".as_bytes();
        assert_eq!(ask_with(&mut r, "Q?", "def"), "line one"); // non-empty input wins
        assert_eq!(ask_with(&mut r, "Q?", "def"), "def"); // empty → default
        let mut r2 = "  padded  \n".as_bytes();
        assert_eq!(ask_with(&mut r2, "Q?", "def"), "padded");
        let mut r3 = "\nwallet\n".as_bytes();
        assert_eq!(ask_required(&mut r3, "wallet"), "wallet");
        let mut r4 = "y\n\nno\n".as_bytes();
        assert!(confirm_with(&mut r4, "ok?", true)); // "y"
        assert!(!confirm_with(&mut r4, "ok?", false)); // "" → default false
        assert!(!confirm_with(&mut r4, "ok?", true)); // "no" overrides default
    }

    #[test]
    fn atomic_write_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "original").unwrap();
        assert!(write_config_atomic(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        // Fresh write succeeds and parses as written.
        let fresh = dir.path().join("fresh.toml");
        write_config_atomic(&fresh, "a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "a = 1\n");
        // No temp litter left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("wizard-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
