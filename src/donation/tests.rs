// Copyright (c) 2026 PotR3288
// SPDX-License-Identifier: BSD-3-Clause

use super::*;

fn cfg(
    enabled: bool,
    strategy: DonationStrategy,
    addresses: Vec<WeightedAddress>,
) -> DonationConfig {
    DonationConfig {
        enabled,
        strategy,
        addresses,
    }
}

fn weighted(address: &str, weight: u32) -> WeightedAddress {
    WeightedAddress {
        address: address.to_string(),
        weight,
    }
}

fn window_cfg(schedule: &str, minutes: u32, addresses: Vec<WeightedAddress>) -> DonationConfig {
    cfg(
        true,
        DonationStrategy::Window {
            schedule: schedule.to_string(),
            duration_minutes: minutes,
        },
        addresses,
    )
}

fn cycle_cfg(fraction: f64, addresses: Vec<WeightedAddress>) -> DonationConfig {
    cfg(true, DonationStrategy::Cycle { fraction }, addresses)
}

/// Midnight of 2026-11-02 UTC + h hours + m minutes.
fn utc(h: u32, m: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(core::time::Duration::from_secs(
            1793577600 + (h as u64) * 3600 + (m as u64) * 60,
        ))
        .unwrap()
}

#[test]
fn resolve_disabled_returns_none() {
    let c = cfg(
        false,
        DonationStrategy::Cycle { fraction: 0.05 },
        vec![weighted("A", 1)],
    );
    let input = ResolveInput {
        now: SystemTime::UNIX_EPOCH,
        rotation: 5,
    };
    assert!(resolve(&c, &input).is_none());
}

#[test]
fn resolve_empty_addresses_returns_none() {
    let c = cycle_cfg(0.05, vec![]);
    let input = ResolveInput {
        now: utc(0, 0),
        rotation: 0,
    };
    assert!(resolve(&c, &input).is_none());
}

#[test]
fn cycle_fraction_zero_returns_none() {
    let c = cycle_cfg(0.0, vec![weighted("A", 1)]);
    let input = ResolveInput {
        now: utc(0, 0),
        rotation: 0,
    };
    assert!(resolve(&c, &input).is_none());
}

#[test]
fn cycle_fraction_one_returns_always() {
    let c = cycle_cfg(1.0, vec![weighted("A", 1)]);
    for r in 0..10 {
        let input = ResolveInput {
            now: utc(0, 0),
            rotation: r,
        };
        assert_eq!(
            resolve(&c, &input).as_deref(),
            Some("A"),
            "fraction 1.0 must donate every unit, r={r}"
        );
    }
}

#[test]
fn cycle_five_percent_donates_one_in_twenty() {
    // cycle_len = ceil(1/0.05) = 20; open slots = round(0.05*20) = 1 -> rotation % 20 == 0.
    let c = cycle_cfg(0.05, vec![weighted("A", 1)]);
    let open: Vec<u64> = (0..40)
        .filter(|r| {
            resolve(
                &c,
                &ResolveInput {
                    now: utc(0, 0),
                    rotation: *r,
                },
            )
            .is_some()
        })
        .collect();
    assert_eq!(
        open,
        vec![0, 20],
        "5% must open exactly slots 0 and 20 in a 40-unit run"
    );
}

#[test]
fn weighted_rotation_respects_weights_across_cycle() {
    // weights [3, 2]: slots 0-2 -> A, 3-4 -> B, repeating every 5.
    let c = cycle_cfg(1.0, vec![weighted("A", 3), weighted("B", 2)]);
    let picks: Vec<String> = (0..5)
        .map(|r| {
            resolve(
                &c,
                &ResolveInput {
                    now: utc(0, 0),
                    rotation: r,
                },
            )
            .unwrap()
        })
        .collect();
    assert_eq!(picks, vec!["A", "A", "A", "B", "B"]);
}

#[test]
fn weighted_rotation_wraps_over_multiple_cycles() {
    // weights [3, 2] over two full cycles (rotation 5..10) must repeat the pattern.
    let c = cycle_cfg(1.0, vec![weighted("A", 3), weighted("B", 2)]);
    let picks: Vec<String> = (5..10)
        .map(|r| {
            resolve(
                &c,
                &ResolveInput {
                    now: utc(0, 0),
                    rotation: r,
                },
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        picks,
        vec!["A", "A", "A", "B", "B"],
        "weighted rotation must repeat every total_weight units"
    );
}

#[test]
fn window_open_inside_and_closed_outside() {
    // "0 12 * * *" with 30 minutes: open 12:00..12:30 UTC only.
    let c = window_cfg("0 12 * * *", 30, vec![weighted("A", 1)]);
    assert!(
        resolve(
            &c,
            &ResolveInput {
                now: utc(12, 0),
                rotation: 0
            }
        )
        .is_some(),
        "12:00 must be inside"
    );
    assert!(
        resolve(
            &c,
            &ResolveInput {
                now: utc(12, 29),
                rotation: 0
            }
        )
        .is_some(),
        "12:29 must be inside"
    );
    assert!(
        resolve(
            &c,
            &ResolveInput {
                now: utc(12, 30),
                rotation: 0
            }
        )
        .is_none(),
        "12:30 must be outside (end-exclusive)"
    );
    assert!(
        resolve(
            &c,
            &ResolveInput {
                now: utc(11, 59),
                rotation: 0
            }
        )
        .is_none(),
        "11:59 must be outside"
    );
}

#[test]
fn window_crossing_midnight_is_open_on_both_sides() {
    // schedule "50 23 * * *", 20 minutes: open 23:50..00:10 UTC.
    let c = window_cfg("50 23 * * *", 20, vec![weighted("donA", 1)]);
    let late = resolve(
        &c,
        &ResolveInput {
            now: utc(23, 55),
            rotation: 0,
        },
    );
    let early = resolve(
        &c,
        &ResolveInput {
            now: utc(0, 5),
            rotation: 0,
        },
    );
    assert!(late.is_some(), "23:55 UTC should be inside the window");
    assert!(
        early.is_some(),
        "00:05 UTC (next day) should still be inside the window"
    );
}

#[test]
fn builtin_config_zero_fraction_disables() {
    let cfg = builtin_config(0.0);
    assert!(!cfg.enabled, "0% is the opt-out — nothing may donate");
    assert!(resolve(
        &cfg,
        &ResolveInput {
            now: utc(0, 0),
            rotation: 0
        }
    )
    .is_none());
}

#[test]
fn builtin_config_splits_evenly_between_two_addresses() {
    // The two hard-coded recipients get equal weight, so the weighted pick
    // alternates A/B across donation slots: an even 50/50 split of whatever
    // fraction the operator chose.
    let cfg = builtin_config(0.10);
    // fraction 0.1 → cycle_len 10, one donation slot per cycle: slots land at
    // rotations 0, 10, 20, … The slot-index pick alternates DEV/COMMUNITY.
    let picks: Vec<&str> = [0u64, 10, 20, 30, 40, 50]
        .iter()
        .filter_map(|r| {
            resolve(
                &cfg,
                &ResolveInput {
                    now: utc(0, 0),
                    rotation: *r,
                },
            )
        })
        .map(|s| {
            if s == DEVELOPERS_ADDRESS {
                "DEV"
            } else {
                "COMMUNITY"
            }
        })
        .collect();
    assert_eq!(
        picks,
        vec!["DEV", "COMMUNITY", "DEV", "COMMUNITY", "DEV", "COMMUNITY"]
    );
}

#[test]
fn builtin_config_fraction_bounds() {
    // fraction outside (0, 1] must not enable donations.
    assert!(!builtin_config(-0.01).enabled);
    assert!(!builtin_config(1.01).enabled);
    assert!(builtin_config(0.001).enabled);
    assert!(builtin_config(1.0).enabled);
}

#[test]
fn builtin_addresses_are_valid_looking() {
    // Compile-time-ish guard: the hard-coded constants must never be empty
    // or whitespace (full address parsing is the miner's job at startup).
    assert!(!DEVELOPERS_ADDRESS.trim().is_empty());
    assert!(!COMMUNITY_ADDRESS.trim().is_empty());
    assert_ne!(DEVELOPERS_ADDRESS, COMMUNITY_ADDRESS);
    // Real Tari dual-address form: no placeholders, base58 charset, plausible
    // length (85+ chars). Catches an accidental placeholder making it in.
    for a in [DEVELOPERS_ADDRESS, COMMUNITY_ADDRESS] {
        assert!(
            !a.contains("PLACEHOLDER"),
            "placeholder shipped as a real address"
        );
        assert!(a.len() >= 85, "address too short: {a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric()),
            "non-base58 char in {a}"
        );
    }
}
