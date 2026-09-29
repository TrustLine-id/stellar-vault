// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Absolute / relative caps, keyed by **adapter address**.
//!
//! - Absolute cap in underlying units; relative cap in WAD (`1e18` = 100%).
//! - Increases: Curator via timelock. Decreases: Curator|Sentinel, immediate.
//! - Caps checked on allocate only (relative is soft). Deallocate never blocked by caps.

use soroban_sdk::{
    contracterror, contractevent, contracttype, panic_with_error, Address, Env,
};
use stellar_contract_utils::math::{i128_fixed_point::mul_div_with_rounding as mul_div, Rounding};

/// WAD: 1e18 = 100% relative exposure.
pub const WAD: i128 = 1_000_000_000_000_000_000;

/// ~7 days of ledgers (5s). Cap TTL matches vault balances / cost (`max_ttl`).
const WEEK_OF_LEDGERS: u32 = 120_960;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CapError {
    AbsoluteCapNotIncreasing = 1,
    AbsoluteCapNotDecreasing = 2,
    RelativeCapNotIncreasing = 3,
    RelativeCapNotDecreasing = 4,
    RelativeCapAboveOne = 5,
    ZeroAbsoluteCap = 6,
    AbsoluteCapExceeded = 7,
    RelativeCapExceeded = 8,
    ZeroAllocation = 9, // reserved (deallocate may zero allocation when pulling yield)
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Caps {
    pub allocation: i128,
    pub absolute_cap: i128,
    pub relative_cap: i128,
}

impl Caps {
    pub fn zero() -> Self {
        Self {
            allocation: 0,
            absolute_cap: 0,
            relative_cap: 0,
        }
    }
}

#[contracttype]
#[derive(Clone)]
pub enum CapKey {
    Caps(Address),
}

#[contractevent(topics = ["caps", "abs_up"], data_format = "vec")]
pub struct AbsoluteCapIncreased {
    pub adapter: Address,
    pub new_absolute_cap: i128,
}

#[contractevent(topics = ["caps", "abs_down"], data_format = "vec")]
pub struct AbsoluteCapDecreased {
    pub adapter: Address,
    pub new_absolute_cap: i128,
}

#[contractevent(topics = ["caps", "rel_up"], data_format = "vec")]
pub struct RelativeCapIncreased {
    pub adapter: Address,
    pub new_relative_cap: i128,
}

#[contractevent(topics = ["caps", "rel_down"], data_format = "vec")]
pub struct RelativeCapDecreased {
    pub adapter: Address,
    pub new_relative_cap: i128,
}

pub fn get_caps(env: &Env, adapter: &Address) -> Caps {
    env.storage()
        .persistent()
        .get(&CapKey::Caps(adapter.clone()))
        .unwrap_or_else(Caps::zero)
}

fn set_caps(env: &Env, adapter: &Address, caps: &Caps) {
    let key = CapKey::Caps(adapter.clone());
    env.storage().persistent().set(&key, caps);
    let max = env.storage().max_ttl();
    env.storage()
        .persistent()
        .extend_ttl(&key, max.saturating_sub(WEEK_OF_LEDGERS), max);
}

pub fn allocation(env: &Env, adapter: &Address) -> i128 {
    get_caps(env, adapter).allocation
}

pub fn absolute_cap(env: &Env, adapter: &Address) -> i128 {
    get_caps(env, adapter).absolute_cap
}

pub fn relative_cap(env: &Env, adapter: &Address) -> i128 {
    get_caps(env, adapter).relative_cap
}

pub fn increase_absolute(env: &Env, adapter: Address, new_absolute_cap: i128) {
    assert!(new_absolute_cap >= 0, "cap");
    let mut c = get_caps(env, &adapter);
    if new_absolute_cap < c.absolute_cap {
        panic_with_error!(env, CapError::AbsoluteCapNotIncreasing);
    }
    c.absolute_cap = new_absolute_cap;
    set_caps(env, &adapter, &c);
    AbsoluteCapIncreased {
        adapter,
        new_absolute_cap,
    }
    .publish(env);
}

pub fn decrease_absolute(env: &Env, adapter: Address, new_absolute_cap: i128) {
    assert!(new_absolute_cap >= 0, "cap");
    let mut c = get_caps(env, &adapter);
    if new_absolute_cap > c.absolute_cap {
        panic_with_error!(env, CapError::AbsoluteCapNotDecreasing);
    }
    c.absolute_cap = new_absolute_cap;
    set_caps(env, &adapter, &c);
    AbsoluteCapDecreased {
        adapter,
        new_absolute_cap,
    }
    .publish(env);
}

pub fn increase_relative(env: &Env, adapter: Address, new_relative_cap: i128) {
    if new_relative_cap > WAD {
        panic_with_error!(env, CapError::RelativeCapAboveOne);
    }
    assert!(new_relative_cap >= 0, "cap");
    let mut c = get_caps(env, &adapter);
    if new_relative_cap < c.relative_cap {
        panic_with_error!(env, CapError::RelativeCapNotIncreasing);
    }
    c.relative_cap = new_relative_cap;
    set_caps(env, &adapter, &c);
    RelativeCapIncreased {
        adapter,
        new_relative_cap,
    }
    .publish(env);
}

pub fn decrease_relative(env: &Env, adapter: Address, new_relative_cap: i128) {
    assert!(new_relative_cap >= 0, "cap");
    let mut c = get_caps(env, &adapter);
    if new_relative_cap > c.relative_cap {
        panic_with_error!(env, CapError::RelativeCapNotDecreasing);
    }
    c.relative_cap = new_relative_cap;
    set_caps(env, &adapter, &c);
    RelativeCapDecreased {
        adapter,
        new_relative_cap,
    }
    .publish(env);
}

/// After a successful allocate of `assets` into `adapter`.
pub fn on_allocate(env: &Env, adapter: &Address, assets: i128, total_assets: i128) {
    let mut c = get_caps(env, adapter);
    if c.absolute_cap <= 0 {
        panic_with_error!(env, CapError::ZeroAbsoluteCap);
    }
    c.allocation = c
        .allocation
        .checked_add(assets)
        .expect("allocation overflow");
    if c.allocation > c.absolute_cap {
        panic_with_error!(env, CapError::AbsoluteCapExceeded);
    }
    if c.relative_cap != WAD {
        let max_rel = mul_div(env, total_assets.max(0), c.relative_cap, WAD, Rounding::Floor);
        if c.allocation > max_rel {
            panic_with_error!(env, CapError::RelativeCapExceeded);
        }
    }
    set_caps(env, adapter, &c);
}

/// After a successful deallocate of `assets` from `adapter`.
///
/// `allocation` tracks principal for cap accounting, not mark-to-market. Yield
/// (or other gains) can make the adapter hold more than `allocation`. Pulling
/// that excess must zero the book rather than revert — otherwise `ensure_idle`
/// / redemptions stall after gains.
///
/// Invariant: only called after a successful adapter `deallocate` of exactly
/// `assets` (> 0). Never decreases below zero; never touches absolute/relative caps.
pub fn on_deallocate(env: &Env, adapter: &Address, assets: i128) {
    assert!(assets > 0, "assets");
    let mut c = get_caps(env, adapter);
    if assets >= c.allocation {
        c.allocation = 0;
    } else {
        c.allocation -= assets;
    }
    set_caps(env, adapter, &c);
}
