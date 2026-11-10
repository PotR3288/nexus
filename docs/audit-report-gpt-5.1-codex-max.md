# Security audit — nexus (gpt-5.1-codex-max)

## Confirmed findings

- **Medium — CentCom managed credentials travel in cleartext over HTTP**  
  - **Where:** `src/centcom.rs`: ManagedConfig fields (`pool_password`, `email_password`, `api_token`) lines 41-123; client fetch/POST paths use plain `http://` only (comments + `Client::new`/`fetch_config`/`post_*`, lines ~18-24, 225-314).  
  - **Trigger:** Any CentCom deployment over a non-trusted LAN (or future WAN) sends pool/email/API credentials in the response body and bearer token in headers without TLS; attackers on-path can read/modify managed config and heartbeats/events.  
  - **Fix:** Add TLS support (or documented HTTPS proxy) and refuse `http://` when secrets are present; alternatively tunnel via mutual-TLS SSH/VPN and document that hard requirement.  
  - **Owner:** CentCom client (nexus) + CentCom deployment guidance.
  - **Disposition (maintainer, 2026-11-09):** Accepted as design — CentCom is LAN-only by decision; plain HTTP on a trusted LAN stays for now. Revisit if it ever leaves the LAN.

- **Medium — Email channel password held as freely clonable `String`**  
  - **Where:** `src/notify.rs` EmailChannel fields lines ~173-182 and async send path lines ~215-305 clone `password` into dispatched tasks.  
  - **Trigger:** Process-memory capture (core dump/support bundle) or accidental logging of structs exposes SMTP credentials in plaintext; no secrecy wrapper or zeroization.  
  - **Fix:** Wrap `password` (and `username`) in a secrecy/newtype that skips Debug/Clone by default and zeroizes on drop; only copy into lettre Credentials at send time.  
  - **Owner:** nexus.
  - **Disposition (maintainer, 2026-11-09):** FIXED in commit `6d14452` — `EmailChannel.password` is now a `SecretString` newtype: redacted in Debug, value reachable only via `expose()`, bytes zeroized on drop. Per-dispatch channel clones still copy the credential (dispatch design unchanged) but each transient copy wipes its own bytes on drop; no formatting path can print it. 94/94 tests pass post-fix including a new redaction test.

## Unconfirmed findings

- None.

## Residual risk

Hostile-input core (stratum reader, capped lines, login flood handling) and API surface (auth gating, header/body caps) matched the spec and tests; no unsafe blocks or network-reachable panics were found. Remaining risk comes from operators running CentCom on unsecured networks and from plaintext credential storage in process memory; both are bounded by deployment discipline and can be reduced with the fixes above.
