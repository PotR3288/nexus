# nexus

Shared host infrastructure for [xtminer](https://github.com/PotR3288/xtminer)
and [edgecase](https://github/PotR3288/edgecase).

**License: BSD-3-Clause.** This crate contains only host-infrastructure code. It deliberately contains no PoW
algorithm logic, no config schemas, and no mining loops.

## What lives here

| module | contents |
|---|---|
| `config_util` | `ConfigLoad` tri-state loading, TOML/JSON deep-merge with mask-token semantics, RFC 7386 null-deletion handling, atomic validate-then-write config writes, `mask_secrets` |
| `api` | xmrig-compatible HTTP API server skeleton (thread-per-connection hand-rolled responder), auth, routing, `/json_rpc` control (pause/resume/stop/restart), `/log` tail parameterized by the [`ApiHooks`] trait so each miner renders its own summary/backends |
| `watch` | mtime config hot-reload watcher + `SharedConfig` arc-swap cell, with live/restart key tiers expressed via the [`ConfigTier`] trait |
| `notify` | notification `Dispatcher` (sync + blocking send), `Channel` trait, SMTP email channel (lettre) |
| `stratum` | Monero-dialect stratum framing core: login, keepalive loop (with disable-on-unknown-method), id-correlated submit replies, reconnect backoff — job parsing behind the [`JobParser`] trait |

