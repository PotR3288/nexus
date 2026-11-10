// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause

//! Donation policy engine: decides, per unit of work (mined template or
//! login session), whether the donation wallet replaces the operator's
//! primary wallet, and which donation address is active.
//!
//! Strategy: time-share. Donations happen by donating a portion of mining
//! time (blocks or sessions), yielding the configured portion of rewards in
//! expectation — no protocol change, no per-block coinbase split.
//!
//! - [`DonationStrategy::Window`]: cron-like timed windows evaluated in UTC.
//!   While a window is open the active donation address rotates deterministically.
//! - [`DonationStrategy::Cycle`]: wall-clock-free rotation; a fixed fraction
//!   of units (blocks/sessions) is donated, `cycle_len = ceil(1/fraction)`.
//!
//! Weighted rotation: with weights {3, 2} the donation slots 0-2 resolve to
//! address 0 and slots 3-4 to address 1, repeating every `total_weight` units.
//!
//! Determinism: same config + same [`ResolveInput`] ⇒ same answer, so an
//! operator can replay which address received which unit from config and
//! timestamps/heights alone.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// One donation address with its share of the rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeightedAddress {
    pub address: String,
    pub weight: u32,
}

/// When donations are active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DonationStrategy {
    /// Cron-scheduled timed windows, evaluated in UTC.
    Window {
        schedule: String,
        duration_minutes: u32,
    },
    /// Deterministic fraction-of-units cycle, no wall clock.
    Cycle { fraction: f64 },
}

impl Default for DonationStrategy {
    fn default() -> Self {
        DonationStrategy::Window {
            schedule: String::new(),
            duration_minutes: 0,
        }
    }
}

/// Full `[donations]` config section, shared verbatim by both miners.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DonationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub strategy: DonationStrategy,
    #[serde(default)]
    pub addresses: Vec<WeightedAddress>,
}

/// Input to [`resolve`]: wall clock (window openness) plus the rotation
/// index (template height, or login-session counter).
#[derive(Debug, Clone, Copy)]
pub struct ResolveInput {
    pub now: SystemTime,
    pub rotation: u64,
}

/// Maximum rotation cycle length: a tiny fraction clamps to 1 unit in 10k.
const MAX_CYCLE_LEN: u64 = 10_000;

/// Donation recipients, hard-coded by design: the miner's operator chooses
/// *how much* to donate (`fraction`, 0 = opt-out) but never *where it goes*,
/// so a downstream fork cannot silently redirect the funds.
///
/// Split is always 50/50:
/// - half of every donated unit goes to the xtminer/edgecase developers,
/// - half goes to the Tari development community.
pub const DEVELOPERS_ADDRESS: &str =
    "12PV8Y7kBKbTkLQcZuMGZxuP2fMBJ9UQFytBXYDKPMWyUVjrtuzEu2EFCXizgwhSC3XoXGb3RYHgcL56o2yPEhUfG2y";
pub const COMMUNITY_ADDRESS: &str =
    "12JYKMCdCPm1xZcDaWbcJCeGvQKFmiucPYn9YB6JBwFoEEVTamK3cQaZ8jDrde9nVKGvsA6KPYRzFja35n7RvrNTNEB";

/// Human-readable donation state for startup banners and hot-reload diffs:
/// "donating N%" when the share is effective, "off" when 0 (or out of range).
pub fn describe_percent(percent: f64) -> String {
    if percent > 0.0 && percent <= 100.0 {
        format!("donating {percent}%")
    } else {
        "off".to_string()
    }
}

/// The donation policy built from the hard-coded recipients and the
/// operator-chosen fraction. `fraction` 0 disables donations entirely
/// (the opt-out); values outside (0, 1] also disable — there is no
/// fail-loud here because the addresses are compile-time constants
/// validated by unit tests, and the operator's only knob is the share.
pub fn builtin_config(fraction: f64) -> DonationConfig {
    let enabled = fraction > 0.0 && fraction <= 1.0;
    DonationConfig {
        enabled,
        strategy: DonationStrategy::Cycle {
            fraction: fraction.clamp(0.0, 1.0),
        },
        addresses: vec![
            WeightedAddress {
                address: DEVELOPERS_ADDRESS.to_string(),
                weight: 1,
            },
            WeightedAddress {
                address: COMMUNITY_ADDRESS.to_string(),
                weight: 1,
            },
        ],
    }
}

/// Returns `Some(donation_address)` when a donation is due for this unit of
/// work, `None` to use the primary wallet. Pure: same inputs, same answer.
pub fn resolve(cfg: &DonationConfig, input: &ResolveInput) -> Option<String> {
    if !cfg.enabled || cfg.addresses.is_empty() {
        return None;
    }
    let open = match &cfg.strategy {
        DonationStrategy::Cycle { fraction } => cycle_is_open(*fraction, input.rotation),
        DonationStrategy::Window {
            schedule,
            duration_minutes,
        } => window_is_open(schedule, *duration_minutes, input.now)?,
    };
    if !open {
        return None;
    }
    // Pick the address by DONATION-SLOT index, not raw rotation: with a Cycle
    // strategy, donation slots recur every `cycle_len` rotations, so a raw
    // rotation % total_weight can land on the same slot parity every time
    // (e.g. cycle_len 10, two addresses → every slot is even → the second
    // address never gets picked). Counting elapsed donation slots makes the
    // weighted split exact regardless of cycle length.
    let pick_index = match &cfg.strategy {
        DonationStrategy::Cycle { fraction } => {
            let cycle_len = (1.0 / fraction).ceil().clamp(1.0, MAX_CYCLE_LEN as f64) as u64;
            let open_slots = (fraction * cycle_len as f64)
                .round()
                .clamp(1.0, cycle_len as f64) as u64;
            (input.rotation / cycle_len) * open_slots + (input.rotation % cycle_len).min(open_slots)
        }
        DonationStrategy::Window { .. } => input.rotation,
    };
    Some(pick_address(&cfg.addresses, pick_index))
}

/// Cycle openness: `cycle_len = ceil(1/fraction)` (clamped to
/// [`MAX_CYCLE_LEN`]); the first `round(fraction * cycle_len)` slots of every
/// cycle are donation slots. fraction 0 donates nothing.
fn cycle_is_open(fraction: f64, rotation: u64) -> bool {
    // Explicit (not `!(fraction > 0.0)`): NaN must also close the cycle —
    // clippy's partial_cmp rewrite would let NaN through to a div-by-zero.
    if fraction <= 0.0 || fraction.is_nan() {
        return false;
    }
    let cycle_len = (1.0 / fraction).ceil().clamp(1.0, MAX_CYCLE_LEN as f64) as u64;
    let open_slots = (fraction * cycle_len as f64)
        .round()
        .clamp(1.0, cycle_len as f64) as u64;
    rotation % cycle_len < open_slots
}

/// Window openness: true iff `now` falls inside an occurrence of `schedule`
/// plus `duration_minutes`. Occurrences whose start is BEFORE now are scanned
/// until one whose end is >= now, so a window that crosses midnight stays
/// open on the morning side. UTC only.
fn window_is_open(schedule: &str, duration_minutes: u32, now: SystemTime) -> Option<bool> {
    use cron::Schedule;
    use std::str::FromStr;
    // Configs use the standard 5-field cron syntax (`min hour dom month dow`);
    // the cron crate expects seconds first, so pad the expression.
    let padded = format!("0 {schedule}");
    let sched = Schedule::from_str(&padded).ok()?;
    let now_utc: chrono::DateTime<chrono::Utc> = now.into();
    // Scan the last 25h of occurrences (24h + slack) for one covering now.
    let horizon = now_utc - chrono::Duration::hours(25);
    for start in sched.after(&horizon) {
        let end = start + chrono::Duration::minutes(duration_minutes as i64);
        if start <= now_utc && now_utc < end {
            return Some(true);
        }
        if start > now_utc {
            return Some(false);
        }
    }
    Some(false)
}

/// Deterministic weighted pick: target slot = rotation % total_weight,
/// walked across prefix sums in configured order.
fn pick_address(addresses: &[WeightedAddress], rotation: u64) -> String {
    let total: u64 = addresses.iter().map(|a| a.weight as u64).sum();
    let target = if total == 0 { 0 } else { rotation % total };
    let mut acc = 0u64;
    for a in addresses {
        acc += a.weight as u64;
        if target < acc {
            return a.address.clone();
        }
    }
    addresses[0].address.clone()
}

#[cfg(test)]
mod tests;
