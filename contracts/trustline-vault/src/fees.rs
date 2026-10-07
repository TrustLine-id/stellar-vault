// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Fees for this SEP-56 vault (inlined module — not a separate crate).
//!
//! Management (yearly, not compounded, paid in shares), per-holder performance (assets at
//! sale), early-exit penalty. Spec / formulas / integration notes:
//! [`docs/VAULT_FEES_SPEC.md`](../../../docs/VAULT_FEES_SPEC.md),
//! [`docs/VAULT_FEES_FORMULAS.md`](../../../docs/VAULT_FEES_FORMULAS.md),
//! [`docs/VAULT_FEES_INTEGRATION.md`](../../../docs/VAULT_FEES_INTEGRATION.md).
//!
//! Where each calculation comes from:
//! - [Math]: 256-bit `mul_div_with_rounding` from OpenZeppelin `stellar-contract-utils`
//! - [Std]: standard vault management-fee math (yearly rate, fee shares, previews)
//! - [Ours]: per-holder cost / performance fee, tiered penalty

use soroban_sdk::{
    contracterror, contractevent, contracttrait, contracttype, panic_with_error, Address, Env,
    Vec,
};
// [Math] x × y / d in 256 bits, rounded as requested.
use stellar_contract_utils::math::{i128_fixed_point::mul_div_with_rounding as mul_div, Rounding};

const BPS: i128 = 10_000;
const YEAR: i128 = 365 * 86_400; // [Std] 365-day year for management fee
const MAX_MANAGEMENT_BPS: u32 = 500; // [Std] 5% a year
const MAX_PERFORMANCE_BPS: u32 = 5_000; // [Std] 50%
const MAX_PENALTY_BPS: u32 = 2_000; // [Ours] 20% of the sale
const MAX_TIERS: u32 = 8; // [Ours]
/// ~7 days of ledgers (5s). Cost TTL matches vault share balances (`max_ttl`).
const WEEK_OF_LEDGERS: u32 = 120_960;

/// What the vault gives the library access to. Implement it on a type of the vault's crate
/// (not on the contract type itself, to keep its function names apart from the vault's).
pub trait FeeHost {
    /// Assets the vault holds or manages, as its `total_assets` (without any virtual asset).
    fn total_assets(e: &Env) -> i128;
    /// Shares in circulation.
    fn total_supply(e: &Env) -> i128;
    /// Virtual shares used by the vault against donation attacks.
    fn virtual_shares(e: &Env) -> i128;
    /// Shares held by `holder`.
    fn balance(e: &Env, holder: &Address) -> i128;
    /// Mint new shares to `to` (the management fee).
    fn mint_shares(e: &Env, to: &Address, shares: i128);
    /// Pay `assets` out of the vault to `to` (the performance fee).
    fn pay_assets(e: &Env, to: &Address, assets: i128);
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fees {
    /// Receives the fees: management fee shares, performance fees in assets. Never the vault.
    pub receiver: Address,
    /// Management fee, per year, of the total assets, not compounded.
    pub management_bps: u32,
    /// Performance fee, of the gain a holder realizes when selling.
    pub performance_bps: u32,
    /// `(shares, bps)` by increasing shares: selling MORE than `shares` pays `bps` of the
    /// assets; the highest tier reached applies. Rates never decrease from tier to tier.
    pub penalties: Vec<(i128, u32)>,
}

/// Storage keys, named apart from the vault's own.
#[contracttype]
enum FeeKey {
    FeeParams,
    FeeLastAccrual,   // time of the last management accrual
    FeeCost(Address), // what the holder paid for the shares it holds, in assets
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FeeError {
    InvalidFees = 900,
}

#[contractevent(topics = ["fees", "changed"], data_format = "vec")]
pub struct FeesChanged {
    pub fees: Fees,
}

#[contractevent(topics = ["fees", "accrued"], data_format = "vec")]
pub struct FeesAccrued {
    #[topic]
    pub receiver: Address,
    pub management_shares: i128,
    /// Their value, added to the receiver's cost.
    pub management_assets: i128,
}

/// A holder's cost changed (purchase, sale, transfer, management fee, withdrawal request):
/// its new total, so that an indexer never needs to read `cost_basis`.
#[contractevent(topics = ["cost"], data_format = "single-value")]
pub struct CostChanged {
    #[topic]
    pub holder: Address,
    pub cost: i128,
}

/// Shares transferred with their part of the sender's cost.
#[contractevent(topics = ["cost", "moved"], data_format = "vec")]
pub struct CostMoved {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub shares: i128,
    pub cost: i128,
}

#[contractevent(topics = ["performance"], data_format = "vec")]
pub struct PerformanceFeePaid {
    #[topic]
    pub owner: Address,
    pub shares: i128,
    pub assets: i128,
}

#[contractevent(topics = ["penalty"], data_format = "vec")]
pub struct PenaltyPaid {
    #[topic]
    pub owner: Address,
    pub shares: i128,
    pub assets: i128,
}

/// The management fee due now.
pub struct Due {
    /// Shares to mint to the receiver.
    pub shares: i128,
    /// Their value, in assets.
    pub fee: i128,
    last: u64,
    /// Effective values once fee shares are minted: supply + virtual shares, assets + 1.
    pub supply: i128,
    pub assets: i128,
}

/// The amounts of a sale of shares.
#[derive(Clone, Debug)]
pub struct Sale {
    /// Value of the shares.
    pub gross: i128,
    /// Early-exit penalty: stays in the vault.
    pub penalty: i128,
    /// Performance fee: paid to the receiver, in assets.
    pub performance: i128,
    /// Paid to the seller's receiver.
    pub paid: i128,
    /// The part of the owner's cost that leaves with the shares.
    pub cost: i128,
}

/// The library's functions. `H` is the vault's [`FeeHost`].
pub struct FeeLib;

impl FeeLib {
    // ---- Setup and parameters ----

    /// Call once from the vault constructor. Initial fees apply at once (investors see them
    /// before depositing). Fee *changes* go through the vault curator timelock,
    /// not a dedicated fee delay — see [`FeeLib::set_fees`].
    pub fn init(e: &Env, fees: &Fees) {
        check(e, fees);
        let s = e.storage().instance();
        s.set(&FeeKey::FeeParams, fees);
        s.set(&FeeKey::FeeLastAccrual, &e.ledger().timestamp());
    }

    pub fn fees(e: &Env) -> Fees {
        e.storage().instance().get(&FeeKey::FeeParams).unwrap()
    }

    /// Apply new fee parameters. Vault calls this after the curator timelock executes
    /// (`submit(SetFees)` → `set_fees`). Accrues management under the old rate first.
    pub fn set_fees<H: FeeHost>(e: &Env, fees: Fees) {
        check(e, &fees);
        Self::accrue::<H>(e);
        e.storage().instance().set(&FeeKey::FeeParams, &fees);
        FeesChanged { fees }.publish(e);
    }

    // ---- Management fee ----

    /// The management fee due now, without writing anything.
    pub fn due<H: FeeHost>(e: &Env) -> Due {
        let f = Self::fees(e);
        let mut last: u64 = e.storage().instance().get(&FeeKey::FeeLastAccrual).unwrap();
        let now = e.ledger().timestamp();
        let assets = H::total_assets(e) + 1;
        let supply = H::total_supply(e) + H::virtual_shares(e);
        if H::total_supply(e) == 0 {
            return Due { shares: 0, fee: 0, last: now, supply, assets }; // [Ours] no holder, no fee
        }
        // [Std] management fee in assets: rate × elapsed × total assets, not compounded;
        // rate is in bps a year.
        let rate = f.management_bps as i128 * (now - last) as i128;
        let fee = mul_div(e, assets, rate, BPS * YEAR, Rounding::Floor);
        // [Std] fee shares = fee × (supply + virtual) / (assets − fee + 1)
        let shares = mul_div(e, fee, supply, assets - fee, Rounding::Floor);
        if shares > 0 || f.management_bps == 0 {
            // [Ours] otherwise the clock stays, so the fee keeps accruing and frequent calls
            // cannot round it away.
            last = now;
        }
        Due { shares, fee, last, supply: supply + shares, assets }
    }

    /// Mint the management fee due. HOOK: call before any operation reads a share price
    /// (deposit, mint, withdraw, redeem, queue settlement, allocation or deallocation).
    pub fn accrue<H: FeeHost>(e: &Env) {
        let d = Self::due::<H>(e);
        if d.shares > 0 {
            let receiver = Self::fees(e).receiver;
            add_cost(e, &receiver, d.fee); // [Ours] fee shares cost their value: no fee on a fee
            H::mint_shares(e, &receiver, d.shares);
            FeesAccrued { receiver, management_shares: d.shares, management_assets: d.fee }.publish(e);
        }
        e.storage().instance().set(&FeeKey::FeeLastAccrual, &d.last);
    }

    /// Convert assets to shares using the effective supply and assets after management fees.
    pub fn to_shares<H: FeeHost>(e: &Env, assets: i128, rounding: Rounding) -> i128 {
        let d = Self::due::<H>(e);
        mul_div(e, assets.max(0), d.supply, d.assets, rounding)
    }

    /// Convert shares to assets using the effective supply and assets after management fees.
    pub fn to_assets<H: FeeHost>(e: &Env, shares: i128, rounding: Rounding) -> i128 {
        let d = Self::due::<H>(e);
        mul_div(e, shares.max(0), d.assets, d.supply, rounding)
    }

    // ---- Cost of each holder's shares ----
    // [Ours] Each holder's shares have a cost: what was paid for them. Purchases add what they
    // pay, sales remove the sold shares' part, transfers move it with the shares.

    pub fn cost_basis(e: &Env, holder: &Address) -> i128 {
        e.storage().persistent().get(&FeeKey::FeeCost(holder.clone())).unwrap_or(0)
    }

    /// Renew cost TTL with the same policy as share balances. No-op if no entry.
    /// Call whenever the holder's balance is bumped so cost cannot expire first.
    pub fn bump_cost(e: &Env, holder: &Address) {
        let key = FeeKey::FeeCost(holder.clone());
        if e.storage().persistent().has(&key) {
            extend_cost_ttl(e, &key);
        }
    }

    /// HOOK: on each deposit or mint, with the assets paid for the new shares.
    pub fn record_purchase(e: &Env, receiver: &Address, assets: i128) {
        add_cost(e, receiver, assets);
    }

    /// HOOK: on each share transfer (`transfer`, `transfer_from`), BEFORE the balances change.
    /// The shares take their part of the sender's cost: the sender's average cost is unchanged,
    /// the receiver's becomes the weighted average of what it held and what it got.
    pub fn record_transfer<H: FeeHost>(e: &Env, from: &Address, to: &Address, shares: i128) {
        let balance = H::balance(e, from);
        if from == to || shares <= 0 || shares > balance {
            return; // nothing moves, or the token refuses the transfer
        }
        let c = Self::cost_basis(e, from);
        let moved = mul_div(e, c, shares, balance, Rounding::Floor);
        set_cost(e, from, c - moved);
        add_cost(e, to, moved);
        CostMoved { from: from.clone(), to: to.clone(), shares, cost: moved }.publish(e);
    }

    // ---- Sales: penalty and performance fee ----

    /// HOOK: the amounts of selling `shares` of `owner`, BEFORE its balance changes. The gross
    /// value rounds down; then the early-exit penalty and holder performance fee apply:
    ///   gross = shares × assets / supply;   after = gross × (1 − penalty rate)
    ///   performance = rate × max(0, after − cost of the shares);   paid = after − performance
    /// Without an owner (standard previews), no performance fee.
    pub fn sale<H: FeeHost>(e: &Env, owner: Option<&Address>, shares: i128) -> Sale {
        let f = Self::fees(e);
        let d = Self::due::<H>(e);
        Self::sale_at::<H>(e, &f, &d, owner, shares)
    }

    fn sale_at<H: FeeHost>(e: &Env, f: &Fees, d: &Due, owner: Option<&Address>, shares: i128) -> Sale {
        let cost = owner.and_then(|o| part_of_cost::<H>(e, o, shares));
        sale_amounts(e, f, d, shares, cost)
    }

    /// QUEUES: when `shares` of `owner` enter a withdrawal request, BEFORE its balance changes,
    /// take their part of its cost and keep it with the request. The request then settles on
    /// its own cost, whatever the owner does meanwhile.
    pub fn detach_cost<H: FeeHost>(e: &Env, owner: &Address, shares: i128) -> i128 {
        let part = part_of_cost::<H>(e, owner, shares).unwrap_or(0);
        set_cost(e, owner, Self::cost_basis(e, owner) - part);
        part
    }

    /// QUEUES: restore cost previously taken by [`detach_cost`] (e.g. cancelled request).
    pub fn attach_cost(e: &Env, owner: &Address, cost: i128) {
        if cost > 0 {
            add_cost(e, owner, cost);
        }
    }

    /// QUEUES: the amounts of settling a request of `shares` whose cost was detached; then
    /// burn, pay `paid`, and call `settle_sale` (which then removes no further cost).
    pub fn sale_with_cost<H: FeeHost>(e: &Env, shares: i128, cost: i128) -> Sale {
        let mut sale = sale_amounts(e, &Self::fees(e), &Self::due::<H>(e), shares, Some(cost));
        sale.cost = 0; // already detached from the owner
        sale
    }

    /// For a withdrawal of exactly `assets`: the fewest shares whose sale pays at least
    /// `assets`, rounded in the vault's favour, or None beyond the owner's balance. Within a
    /// penalty tier the payout grows with the shares, but it
    /// drops when a tier starts: search each tier's range, lowest first, by bisection. Without
    /// an owner (a preview), no performance fee and no balance limit.
    pub fn shares_for<H: FeeHost>(e: &Env, owner: Option<&Address>, assets: i128) -> Option<i128> {
        let f = Self::fees(e);
        let d = Self::due::<H>(e);
        let limit = match owner {
            Some(o) => H::balance(e, o),
            None => mul_div(e, assets.max(0) * 2, d.supply, d.assets, Rounding::Ceil), // penalty ≤ 20%
        };
        let paid = |s: i128| Self::sale_at::<H>(e, &f, &d, owner, s).paid;
        let mut low = 0;
        for tier in 0..=f.penalties.len() {
            let high = if tier < f.penalties.len() { f.penalties.get(tier).unwrap().0.min(limit) } else { limit };
            if high >= low && paid(high) >= assets {
                let (mut a, mut b) = (low, high);
                while a < b {
                    let mid = a + (b - a) / 2;
                    if paid(mid) >= assets { b = mid } else { a = mid + 1 }
                }
                return Some(b);
            }
            low = high + 1;
        }
        None
    }

    /// For a withdrawal: the sale that pays exactly `assets`, the rounding surplus staying in
    /// the vault as penalty. Returns the shares to burn with it.
    pub fn withdrawal<H: FeeHost>(e: &Env, owner: &Address, assets: i128) -> Option<(i128, Sale)> {
        let shares = Self::shares_for::<H>(e, Some(owner), assets)?;
        let mut sale = Self::sale::<H>(e, Some(owner), shares);
        sale.penalty += sale.paid - assets;
        sale.paid = assets;
        Some((shares, sale))
    }

    /// HOOK: once the vault has burned the shares and paid `sale.paid`: remove the sold shares'
    /// cost, pay the performance fee to the receiver, and publish the events. [Ours] The penalty
    /// is never transferred: it stays in the vault and raises the value of the remaining shares,
    /// a gain for their holders like any other.
    pub fn settle_sale<H: FeeHost>(e: &Env, owner: &Address, shares: i128, sale: &Sale) {
        set_cost(e, owner, Self::cost_basis(e, owner) - sale.cost);
        if sale.performance > 0 {
            H::pay_assets(e, &Self::fees(e).receiver, sale.performance);
            PerformanceFeePaid { owner: owner.clone(), shares, assets: sale.performance }.publish(e);
        }
        if sale.penalty > 0 {
            PenaltyPaid { owner: owner.clone(), shares, assets: sale.penalty }.publish(e);
        }
    }
}

/// The fee functions a vault exposes, all with their implementation:
/// `#[contractimpl(contracttrait)] impl VaultFees for MyVault { type Host = MyHost; }`.
///
/// Fee *parameter changes* are not on this trait: the vault routes them through its curator
/// timelock (`submit(SetFees)` → `set_fees`).
#[contracttrait]
pub trait VaultFees {
    type Host: FeeHost;

    /// Return the active management, performance, and early-exit fee configuration.
    fn fees(e: &Env) -> Fees {
        FeeLib::fees(e)
    }

    /// What `holder` paid for the shares it holds, in assets.
    fn cost_basis(e: &Env, holder: Address) -> i128 {
        FeeLib::cost_basis(e, &holder)
    }

    /// The value of `shares` held by `owner` if sold now: (gross value, early-exit penalty,
    /// performance fee on the owner's gain, net value paid). With the owner's whole balance,
    /// the real value of its holding, below the nominal value by the fees it would pay.
    fn value_of(e: &Env, owner: Address, shares: i128) -> (i128, i128, i128, i128) {
        let s = FeeLib::sale::<Self::Host>(e, Some(&owner), shares);
        (s.gross, s.penalty, s.performance, s.paid)
    }

    /// Mint the management fee due to the receiver. Anyone can call it (e.g. a daily keeper).
    fn accrue(e: &Env) {
        FeeLib::accrue::<Self::Host>(e);
    }
}

// ---- Internals ----

fn set_cost(e: &Env, holder: &Address, cost: i128) {
    let key = FeeKey::FeeCost(holder.clone());
    e.storage().persistent().set(&key, &cost);
    extend_cost_ttl(e, &key);
    CostChanged { holder: holder.clone(), cost }.publish(e);
}

fn extend_cost_ttl(e: &Env, key: &FeeKey) {
    let max = e.storage().max_ttl();
    e.storage()
        .persistent()
        .extend_ttl(key, max.saturating_sub(WEEK_OF_LEDGERS), max);
}

fn add_cost(e: &Env, holder: &Address, amount: i128) {
    set_cost(e, holder, FeeLib::cost_basis(e, holder) + amount);
}

/// The part of `owner`'s cost that goes with `shares` of its balance (rounded up), or None if it
/// holds none.
fn part_of_cost<H: FeeHost>(e: &Env, owner: &Address, shares: i128) -> Option<i128> {
    let balance = H::balance(e, owner);
    if balance <= 0 {
        return None;
    }
    Some(mul_div(e, FeeLib::cost_basis(e, owner), shares.clamp(0, balance), balance, Rounding::Ceil))
}

/// The value of the shares (rounded down), followed by the early-exit penalty and, when the
/// shares' cost is known, the performance fee on the remaining gain.
fn sale_amounts(e: &Env, f: &Fees, d: &Due, shares: i128, cost: Option<i128>) -> Sale {
    let gross = mul_div(e, shares.max(0), d.assets, d.supply, Rounding::Floor);
    let after = mul_div(e, gross, BPS - penalty_bps(f, shares), BPS, Rounding::Floor);
    let performance = match cost {
        Some(c) => mul_div(e, (after - c).max(0), f.performance_bps as i128, BPS, Rounding::Floor),
        None => 0,
    };
    Sale { gross, penalty: gross - after, performance, paid: after - performance, cost: cost.unwrap_or(0) }
}

/// [Ours] The penalty rate for selling `shares`: that of the highest tier exceeded.
fn penalty_bps(f: &Fees, shares: i128) -> i128 {
    f.penalties.iter().filter(|(above, _)| shares > *above).map(|(_, bps)| bps as i128).last().unwrap_or(0)
}

/// Bounds: [Std] the management and performance caps; [Ours] the rest.
fn check(e: &Env, f: &Fees) {
    let mut ok = f.receiver != e.current_contract_address() // its own shares would be lost
        && f.management_bps <= MAX_MANAGEMENT_BPS
        && f.performance_bps <= MAX_PERFORMANCE_BPS
        && f.penalties.len() <= MAX_TIERS;
    let mut last: (i128, u32) = (-1, 0);
    for (above, bps) in f.penalties.iter() {
        ok &= above > last.0 && bps >= last.1 && bps <= MAX_PENALTY_BPS;
        last = (above, bps);
    }
    if !ok {
        panic_with_error!(e, FeeError::InvalidFees);
    }
}
