//! Shared host infrastructure for xtminer and edgecase.
//!
//! Everything in this crate is host plumbing both miners need identically:
//! config file machinery, the xmrig-compatible HTTP status API core, the
//! watcher, the notification dispatcher, work-source failover, and stratum
//! framing. Miner-specific logic (config schemas, PoW algorithms, solver
//! dispatch, mining loops) stays in the miner repos; here it is expressed as
//! small traits the miners implement.
//!
//! License: BSD-3-Clause. This crate must stay free of GPL-derived code
//! (edgecase's CUDA solver is GPL-3 via GGM and lives only in that repo) so
//! both miners can link it.

pub mod api;
pub mod config_util;
pub mod notify;
pub mod stratum;
pub mod watch;
pub mod work_source;
