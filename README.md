# nexus

Shared host infrastructure for [xtminer](https://github.com/PotR3288/xtminer)
and [edgecase](https://github/PotR3288/edgecase): the code both miners were
carrying as near-copies, extracted so it exists once.

**License: BSD-3-Clause.** This crate contains only host-infrastructure code
original to the two miner repos. It deliberately contains **no** solver code
(edgecase's CUDA kernels are GPL-3 via GGM and stay in that repo), no PoW
algorithm logic, no config schemas, and no mining loops — those are the
products. The BSD-3 license is what lets the GPL-3 edgecase consume it while
xtminer stays BSD-3.

## What lives here

| module | contents |
|---|---|
| `config_util` | `ConfigLoad` tri-state loading, TOML/JSON deep-merge with mask-token semantics, RFC 7386 null-deletion handling, atomic validate-then-write config writes, `mask_secrets` |
| `api` | xmrig-compatible HTTP API server skeleton (thread-per-connection hand-rolled responder), auth, routing, `/json_rpc` control (pause/resume/stop/restart), `/log` tail — parameterized by the [`ApiHooks`] trait so each miner renders its own summary/backends |
| `watch` | mtime config hot-reload watcher + `SharedConfig` arc-swap cell, with live/restart key tiers expressed via the [`ConfigTier`] trait |
| `notify` | notification `Dispatcher` (sync + blocking send), `Channel` trait, SMTP email channel (lettre) |
| `stratum` | Monero-dialect stratum framing core: login, keepalive loop (with disable-on-unknown-method), id-correlated submit replies, reconnect backoff — job parsing behind the [`JobParser`] trait |

Each module's docs carry the design rationale inherited from the original
implementations.
