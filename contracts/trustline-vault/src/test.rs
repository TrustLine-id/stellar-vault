#![cfg(test)]

// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Integration tests with Trustline enforcement mocked to pass (`MockValidationEngine`).
//! Covers investor flows, roles, timelock, caps, adapters, fees, and the withdrawal queue.

use crate::caps::WAD;
use crate::fees::Fees;
use crate::timelock::{PendingOp, TimelockOpKind};
use crate::{TrustlineVault, TrustlineVaultClient, VIRTUAL_SHARES};
use dummy_adapter::{DummyAdapter, DummyAdapterClient};
use mock_validation_engine::{MockValidationEngine, MockValidationEngineClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    vec, Address, Bytes, Env, String, Vec,
};

// ─── Review harness (`Ctx`) ────────────────────────────────────────────────

pub(crate) const ONE: i128 = 10_000_000;
pub(crate) const YEAR: u64 = 365 * 86_400;

pub(crate) struct Ctx<'a> {
    pub env: Env,
    pub vault: TrustlineVaultClient<'a>,
    /// A registered dummy adapter with caps at the maximum.
    pub adapter: DummyAdapterClient<'a>,
    pub token: TokenClient<'a>,
    pub sac: StellarAssetClient<'a>,
    pub ve: Address,
    pub owner: Address,
    pub curator: Address,
    pub allocator: Address,
    pub sentinel: Address,
    pub alice: Address,
    pub bob: Address,
}

pub(crate) fn fees(env: &Env, receiver: &Address, management_bps: u32, performance_bps: u32) -> Fees {
    Fees { receiver: receiver.clone(), management_bps, performance_bps, penalties: Vec::new(env) }
}

impl<'a> Ctx<'a> {
    /// Roles given, one adapter registered with full caps; not the liquidity adapter.
    pub fn new() -> Self {
        Self::with(0, |env, owner| fees(env, owner, 0, 0))
    }

    /// The same, the adapter being the liquidity adapter.
    pub fn with_liquidity_adapter() -> Self {
        let c = Self::new();
        c.vault.set_liquidity_adapter(&c.allocator, &Some(c.adapter.address.clone()), &Bytes::new(&c.env));
        c
    }

    pub fn with(timelock: u64, make_fees: impl Fn(&Env, &Address) -> Fees) -> Self {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().with_mut(|l| l.timestamp = 1_800_000_000);
        let who = || Address::generate(&env);
        let (owner, curator, allocator, sentinel, alice, bob) = (who(), who(), who(), who(), who(), who());
        let ve = env.register(MockValidationEngine, (true,));
        let asset = env.register_stellar_asset_contract_v2(who()).address();
        let vault = env.register(
            TrustlineVault,
            (&ve, &asset, &owner, 7u32, String::from_str(&env, "Vault"), String::from_str(&env, "V"), make_fees(&env, &owner), timelock),
        );
        let vault = TrustlineVaultClient::new(&env, &vault);
        let adapter = DummyAdapterClient::new(&env, &env.register(DummyAdapter, (&ve, &vault.address, &asset, &owner)));
        let c = Ctx {
            token: TokenClient::new(&env, &asset),
            sac: StellarAssetClient::new(&env, &asset),
            env, vault, adapter, ve, owner, curator, allocator, sentinel, alice, bob,
        };
        c.sac.mint(&c.alice, &(1_000_000 * ONE));
        c.sac.mint(&c.bob, &(1_000_000 * ONE));
        c.vault.set_curator(&c.curator);
        c.vault.set_is_sentinel(&c.sentinel, &true);
        c.run(PendingOp::SetIsAllocator(c.allocator.clone(), true));
        c.add_adapter(&c.adapter.address);
        c
    }

    pub fn wait(&self, secs: u64) {
        self.env.ledger().with_mut(|l| l.timestamp += secs);
    }

    /// Submit a timelocked operation, wait its delay, execute it.
    pub fn run(&self, op: PendingOp) {
        let v = &self.vault;
        v.submit(&op);
        let kind = match &op {
            PendingOp::AddAdapter(_) => TimelockOpKind::AddAdapter,
            PendingOp::RemoveAdapter(_) => TimelockOpKind::RemoveAdapter,
            PendingOp::SetIsAllocator(_, _) => TimelockOpKind::SetIsAllocator,
            PendingOp::IncreaseTimelock(_, _) => TimelockOpKind::IncreaseTimelock,
            PendingOp::DecreaseTimelock(k, _) => k.clone(),
            PendingOp::Abdicate(_) => TimelockOpKind::Abdicate,
            PendingOp::IncreaseAbsoluteCap(_, _) => TimelockOpKind::IncreaseAbsoluteCap,
            PendingOp::IncreaseRelativeCap(_, _) => TimelockOpKind::IncreaseRelativeCap,
            PendingOp::SetFees(_, _, _, _) => TimelockOpKind::SetFees,
        };
        self.wait(v.timelock(&kind));
        match op {
            PendingOp::AddAdapter(a) => v.add_adapter(&a),
            PendingOp::RemoveAdapter(a) => v.remove_adapter(&a),
            PendingOp::SetIsAllocator(w, f) => v.set_is_allocator(&w, &f),
            PendingOp::IncreaseTimelock(k, d) => v.increase_timelock(&k, &d),
            PendingOp::DecreaseTimelock(k, d) => v.decrease_timelock(&k, &d),
            PendingOp::Abdicate(k) => v.abdicate(&k),
            PendingOp::IncreaseAbsoluteCap(a, n) => v.increase_absolute_cap(&a, &n),
            PendingOp::IncreaseRelativeCap(a, n) => v.increase_relative_cap(&a, &n),
            PendingOp::SetFees(r, m, p, t) => v.set_fees(&r, &m, &p, &t),
        }
    }

    /// Register an adapter with both caps at the maximum.
    pub fn add_adapter(&self, adapter: &Address) {
        self.run(PendingOp::AddAdapter(adapter.clone()));
        self.run(PendingOp::IncreaseAbsoluteCap(adapter.clone(), i128::MAX / 4));
        self.run(PendingOp::IncreaseRelativeCap(adapter.clone(), WAD));
    }

    pub fn new_adapter(&self) -> DummyAdapterClient<'a> {
        let id = self.env.register(DummyAdapter, (&self.ve, &self.vault.address, &self.token.address, &self.owner));
        DummyAdapterClient::new(&self.env, &id)
    }

    pub fn deposit(&self, who: &Address, assets: i128) -> i128 {
        self.vault.deposit(&assets, who, who, who)
    }

    pub fn redeem(&self, who: &Address, shares: i128) -> i128 {
        self.vault.redeem(&shares, who, who, who)
    }

    /// Exit `assets` of underlying (share count follows virtual offset). Returns shares moved.
    pub fn withdraw(&self, who: &Address, assets: i128) -> i128 {
        self.vault.withdraw(&assets, who, who, who)
    }

    pub fn redeem_all(&self, who: &Address) -> i128 {
        self.redeem(who, self.vault.balance(who))
    }

    pub fn shares(&self, who: &Address) -> i128 {
        self.vault.balance(who)
    }

    pub fn allocate(&self, adapter: &Address, assets: i128) {
        self.vault.allocate(&self.allocator, adapter, &Bytes::new(&self.env), &assets);
    }

    pub fn deallocate(&self, adapter: &Address, assets: i128) {
        self.vault.deallocate(&self.allocator, adapter, &Bytes::new(&self.env), &assets);
    }

    /// A gain (or, negative, a loss) of the adapter, as a strategy's result would show.
    pub fn adapter_result(&self, assets: i128) {
        if assets >= 0 {
            self.sac.mint(&self.adapter.address, &assets);
        } else {
            self.adapter.skim(&Address::generate(&self.env), &-assets);
        }
    }

    pub fn value_of(&self, who: &Address) -> i128 {
        self.vault.convert_to_assets(&self.vault.balance(who))
    }
}

fn close(a: i128, b: i128, tolerance: i128) -> bool {
    (a - b).abs() <= tolerance
}

// ─── Current harness (`VaultCtx`) ──────────────────────────────────────────

struct VaultCtx {
    env: Env,
    ve: Address,
    asset: Address,
    vault: Address,
    adapter: Address,
    owner: Address,
    curator: Address,
    allocator: Address,
    sentinel: Address,
    investor: Address,
    fee_receiver: Address,
}

impl VaultCtx {
    fn vault(&self) -> TrustlineVaultClient<'_> {
        TrustlineVaultClient::new(&self.env, &self.vault)
    }

    fn adapter(&self) -> DummyAdapterClient<'_> {
        DummyAdapterClient::new(&self.env, &self.adapter)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.asset)
    }

    fn sac(&self) -> StellarAssetClient<'_> {
        StellarAssetClient::new(&self.env, &self.asset)
    }

    fn mint_underlying(&self, to: &Address, amount: i128) {
        self.sac().mint(to, &amount);
    }

    fn submit_exec(&self, op: PendingOp) {
        let v = self.vault();
        v.submit(&op);
        match op {
            PendingOp::SetIsAllocator(who, flag) => {
                v.set_is_allocator(&who, &flag);
            }
            PendingOp::AddAdapter(a) => {
                v.add_adapter(&a);
            }
            PendingOp::RemoveAdapter(a) => {
                v.remove_adapter(&a);
            }
            PendingOp::IncreaseAbsoluteCap(a, cap) => {
                v.increase_absolute_cap(&a, &cap);
            }
            PendingOp::IncreaseRelativeCap(a, cap) => {
                v.increase_relative_cap(&a, &cap);
            }
            PendingOp::IncreaseTimelock(kind, d) => {
                v.increase_timelock(&kind, &d);
            }
            PendingOp::DecreaseTimelock(kind, d) => {
                v.decrease_timelock(&kind, &d);
            }
            PendingOp::Abdicate(kind) => {
                v.abdicate(&kind);
            }
            PendingOp::SetFees(r, m, p, pen) => {
                v.set_fees(&r, &m, &p, &pen);
            }
        }
    }

    /// Owner→curator→allocator→adapter→max caps→liquidity adapter.
    fn bootstrap_full(&self) {
        let v = self.vault();
        v.set_curator(&self.curator);
        v.set_is_sentinel(&self.sentinel, &true);
        self.submit_exec(PendingOp::SetIsAllocator(self.allocator.clone(), true));
        self.submit_exec(PendingOp::AddAdapter(self.adapter.clone()));
        self.submit_exec(PendingOp::IncreaseAbsoluteCap(
            self.adapter.clone(),
            i128::MAX / 4,
        ));
        self.submit_exec(PendingOp::IncreaseRelativeCap(self.adapter.clone(), WAD));
        v.set_liquidity_adapter(&self.allocator, &Some(self.adapter.clone()), &Bytes::new(&self.env));
    }
}

fn zero_fees(env: &Env, receiver: &Address) -> Fees {
    Fees {
        receiver: receiver.clone(),
        management_bps: 0,
        performance_bps: 0,
        penalties: Vec::new(env),
    }
}

fn deploy(default_timelock: u64, fees: Option<Fees>) -> VaultCtx {
    let env = Env::default();
    env.mock_all_auths_allowing_non_root_auth();

    let owner = Address::generate(&env);
    let curator = Address::generate(&env);
    let allocator = Address::generate(&env);
    let sentinel = Address::generate(&env);
    let investor = Address::generate(&env);
    let fee_receiver = Address::generate(&env);

    let ve = env.register(MockValidationEngine, (true,));
    let sac = env.register_stellar_asset_contract_v2(owner.clone());
    let asset = sac.address();

    let fees = fees.unwrap_or_else(|| zero_fees(&env, &fee_receiver));
    let vault = env.register(
        TrustlineVault,
        (
            &ve,
            &asset,
            &owner,
            7u32,
            String::from_str(&env, "Trustline Vault"),
            String::from_str(&env, "tVAULT"),
            fees,
            default_timelock,
        ),
    );

    let adapter = env.register(
        DummyAdapter,
        (&ve, &vault, &asset, &owner),
    );

    VaultCtx {
        env,
        ve,
        asset,
        vault,
        adapter,
        owner,
        curator,
        allocator,
        sentinel,
        investor,
        fee_receiver,
    }
}

fn deploy_ready() -> VaultCtx {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    ctx
}

// ═══════════════════════════════════════════════════════════════════════════
// Review tests
// ═══════════════════════════════════════════════════════════════════════════

// ─── investors: refusals ───────────────────────────────────────────────────

#[test]
fn zero_and_negative_amounts_are_refused() {
    let c = Ctx::new();
    let a = &c.alice;
    c.deposit(a, 100 * ONE);
    for n in [0, -ONE] {
        assert!(c.vault.try_deposit(&n, a, a, a).is_err());
        assert!(c.vault.try_mint(&n, a, a, a).is_err());
        assert!(c.vault.try_withdraw(&n, a, a, a).is_err());
        assert!(c.vault.try_redeem(&n, a, a, a).is_err());
    }
    assert_eq!(c.vault.total_assets(), 100 * ONE);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn nobody_takes_out_more_than_they_hold() {
    let c = Ctx::new();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 100 * ONE);
    c.deposit(b, 100 * ONE);
    let bal = c.shares(a);
    assert!(c.vault.try_redeem(&(bal + 1), a, a, a).is_err());
    assert!(c.vault.try_withdraw(&(100 * ONE + 1), a, a, a).is_err());
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
    assert!(close(c.redeem_all(a), 100 * ONE, 2));
    assert!(close(c.vault.total_assets(), 100 * ONE, 2));
}

#[test]
fn a_deposit_is_refused_when_trustline_refuses_and_nothing_moves() {
    let c = Ctx::new();
    let a = &c.alice;
    c.deposit(a, 100 * ONE);
    let (shares_before, assets_before) = (c.shares(a), c.vault.total_assets());
    MockValidationEngineClient::new(&c.env, &c.ve).set_should_pass(&false);
    let before = c.token.balance(a);
    assert!(c.vault.try_deposit(&ONE, a, a, a).is_err());
    assert!(c.vault.try_mint(&ONE, a, a, a).is_err());
    assert!(c.vault.try_withdraw(&ONE, a, a, a).is_err());
    assert!(c.vault.try_redeem(&ONE, a, a, a).is_err());
    assert!(c.vault.try_set_curator(&c.bob).is_err());
    assert!(c.vault.try_submit(&PendingOp::SetIsAllocator(c.bob.clone(), true)).is_err());
    assert!(c.vault.try_allocate(&c.allocator, &c.adapter.address, &Bytes::new(&c.env), &ONE).is_err());
    assert_eq!(
        (c.token.balance(a), c.shares(a), c.vault.total_assets()),
        (before, shares_before, assets_before)
    );
}

#[test]
fn an_operator_acts_for_an_owner_only_within_its_allowance() {
    let c = Ctx::new();
    let (a, op) = (&c.alice, &c.bob);
    c.deposit(a, 100 * ONE);
    let need = c.vault.preview_withdraw(&(40 * ONE));
    let exp = c.env.ledger().sequence() + 100;
    assert!(c.vault.try_withdraw(&(40 * ONE), op, a, op).is_err());
    c.vault.approve(a, op, &(need - 1), &exp);
    assert!(c.vault.try_withdraw(&(40 * ONE), op, a, op).is_err());
    c.vault.approve(a, op, &need, &exp);
    let before = c.token.balance(op);
    assert_eq!(c.vault.withdraw(&(40 * ONE), op, a, op), need);
    assert_eq!(c.token.balance(op) - before, 40 * ONE);
    assert_eq!(c.vault.allowance(a, op), 0);
    assert!(c.vault.try_transfer_from(op, a, op, &1).is_err());
}

#[test]
fn previews_match_what_the_operations_do_without_fees() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(b, 777 * ONE + 3);
    c.adapter_result(123 * ONE + 7); // a share is no longer worth one unit
    let expected = c.vault.preview_deposit(&(50 * ONE));
    assert_eq!(c.deposit(a, 50 * ONE), expected);
    let cost = c.vault.preview_mint(&(10 * ONE));
    let before = c.token.balance(a);
    assert_eq!(c.vault.mint(&(10 * ONE), a, a, a), cost);
    assert_eq!(before - c.token.balance(a), cost);
    let out = c.vault.preview_redeem(&(20 * ONE));
    assert_eq!(c.redeem(a, 20 * ONE), out);
    let burned = c.vault.preview_withdraw(&(5 * ONE));
    let shares = c.vault.balance(a);
    assert_eq!(c.vault.withdraw(&(5 * ONE), a, a, a), burned);
    assert_eq!(shares - c.vault.balance(a), burned);
}

// ─── invariants ────────────────────────────────────────────────────────────

/// Whatever the share price, entering and leaving at once never returns more than was put in,
/// and never costs another holder anything.
#[test]
fn a_round_trip_never_gains_and_never_costs_the_others() {
    for (first, result) in [(1_000 * ONE, 0), (1_000 * ONE, 333 * ONE + 1), (1_000 * ONE, -(250 * ONE)), (7, 5), (3 * ONE + 1, 1), (999_999, 123_456_789)] {
        let c = Ctx::with_liquidity_adapter();
        let (holder, trader) = (&c.alice, &c.bob);
        c.deposit(holder, first);
        if result != 0 {
            c.adapter_result(result);
        }
        let holder_before = c.vault.max_withdraw(holder);
        let trader_before = c.token.balance(trader);
        for amount in [1, 2, 3, 7, ONE - 1, ONE, 13 * ONE + 5, 100_000 * ONE] {
            if c.vault.preview_deposit(&amount) == 0 {
                continue; // refused: would mint nothing
            }
            c.deposit(trader, amount);
            c.redeem_all(trader);
            c.vault.mint(&c.vault.preview_deposit(&amount), trader, trader, trader);
            match c.vault.max_withdraw(trader) {
                0 => c.redeem_all(trader),
                max => c.vault.withdraw(&max, trader, trader, trader),
            };
            if c.vault.balance(trader) > 0 {
                c.redeem_all(trader); // dust: shares worth less than one unit
            }
        }
        assert!(c.token.balance(trader) <= trader_before, "first {first}, result {result}: the trader gained {}", c.token.balance(trader) - trader_before);
        assert!(c.vault.max_withdraw(holder) >= holder_before, "first {first}, result {result}: the holder lost {}", holder_before - c.vault.max_withdraw(holder));
    }
}

#[test]
fn the_supply_is_the_sum_of_the_balances_and_the_vault_can_pay_everyone() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 200, 1_000));
    c.vault.set_liquidity_adapter(&c.allocator, &Some(c.adapter.address.clone()), &Bytes::new(&c.env));
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 1_000 * ONE);
    c.wait(YEAR / 3);
    c.deposit(b, 500 * ONE + 11);
    c.adapter_result(77 * ONE);
    c.vault.transfer(a, b, &(100 * ONE));
    c.wait(YEAR / 3);
    c.redeem(a, c.vault.balance(a) / 3);
    c.adapter_result(-(40 * ONE));
    c.vault.accrue();
    let holders = [a, b, &c.owner, &c.vault.address];
    let sum: i128 = holders.iter().map(|h| c.vault.balance(h)).sum();
    assert_eq!(sum, c.vault.total_supply());
    // Everyone leaves: the vault pays them all and keeps only dust.
    let total = c.vault.total_assets();
    let held = |c: &Ctx| c.token.balance(a) + c.token.balance(b) + c.token.balance(&c.owner);
    let before = held(&c);
    for h in [a, b, &c.owner] {
        c.redeem_all(h);
    }
    let paid = held(&c) - before; // the performance fees go to the owner, in assets
    assert_eq!(c.vault.total_supply(), 0);
    assert!(paid <= total, "paid {paid} of {total}");
    assert!(total - paid < 10, "left {}", total - paid);
}

#[test]
fn gains_and_losses_are_shared_in_proportion() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 300 * ONE);
    c.deposit(b, 100 * ONE);
    c.adapter_result(40 * ONE);
    assert!(close(c.value_of(a), 330 * ONE, 2) && close(c.value_of(b), 110 * ONE, 2));
    c.adapter_result(-(220 * ONE));
    assert!(close(c.value_of(a), 165 * ONE, 2) && close(c.value_of(b), 55 * ONE, 2));
    assert!(close(c.redeem_all(b), 55 * ONE, 2));
    assert!(close(c.redeem_all(a), 165 * ONE, 2));
}

// ─── roles ─────────────────────────────────────────────────────────────────

#[test]
fn roles_do_not_inherit() {
    let c = Ctx::new();
    c.deposit(&c.alice, 100 * ONE);
    let data = Bytes::new(&c.env);
    for who in [&c.owner, &c.curator, &c.sentinel, &c.alice] {
        assert!(c.vault.try_allocate(who, &c.adapter.address, &data, &ONE).is_err());
        assert!(c.vault.try_set_liquidity_adapter(who, &Some(c.adapter.address.clone()), &data).is_err());
    }
    c.allocate(&c.adapter.address, 50 * ONE);
    for who in [&c.owner, &c.curator, &c.alice] {
        assert!(c.vault.try_deallocate(who, &c.adapter.address, &data, &ONE).is_err());
        assert!(c.vault.try_decrease_absolute_cap(who, &c.adapter.address, &0).is_ok() == (who == &c.curator));
    }
    let op = PendingOp::SetIsAllocator(c.bob.clone(), true);
    c.vault.submit(&op);
    for who in [&c.owner, &c.allocator, &c.alice] {
        assert!(c.vault.try_revoke(who, &op).is_err());
    }
    c.vault.revoke(&c.sentinel, &op);
}

#[test]
fn the_calls_ask_the_right_role_to_authorize() {
    let c = Ctx::new();
    let signed = |who: &Address| c.env.auths().iter().any(|(a, _)| a == who);
    c.vault.set_is_sentinel(&c.bob, &true);
    assert!(signed(&c.owner));
    c.vault.submit(&PendingOp::SetIsAllocator(c.bob.clone(), true));
    assert!(signed(&c.curator));
    c.vault.set_is_allocator(&c.bob, &true);
    assert!(c.env.auths().is_empty(), "a due operation is executed by anyone");
    c.deposit(&c.alice, 10 * ONE);
    c.allocate(&c.adapter.address, ONE);
    assert!(signed(&c.allocator));
    c.vault.deallocate(&c.sentinel, &c.adapter.address, &Bytes::new(&c.env), &ONE);
    assert!(signed(&c.sentinel));
    c.vault.process_withdrawals(&0);
    assert!(c.env.auths().is_empty(), "anyone processes the queue");
}

#[test]
fn set_owner_transfers_governance() {
    let c = Ctx::new();
    let signed = |who: &Address| c.env.auths().iter().any(|(a, _)| a == who);
    let new_owner = c.bob.clone();
    c.vault.set_owner(&new_owner);
    assert!(signed(&c.owner));
    assert_eq!(c.vault.owner(), new_owner);
    c.vault.set_curator(&c.alice);
    assert!(signed(&new_owner));
    assert_eq!(c.vault.curator(), Some(c.alice.clone()));
}

#[test]
fn set_queue_manager_asks_the_curator() {
    let c = Ctx::new();
    let qm = Address::generate(&c.env);
    c.vault.set_queue_manager(&Some(qm.clone()));
    assert!(c.env.auths().iter().any(|(a, _)| a == &c.curator));
    assert_eq!(c.vault.queue_manager(), Some(qm));
}

// ─── timelock ──────────────────────────────────────────────────────────────

#[test]
fn nothing_executes_without_its_own_submission() {
    let c = Ctx::new();
    let v = &c.vault;
    let other = c.new_adapter();
    assert!(v.try_add_adapter(&other.address).is_err());
    assert!(v.try_remove_adapter(&c.adapter.address).is_err());
    assert!(v.try_set_is_allocator(&c.bob, &true).is_err());
    assert!(v.try_increase_absolute_cap(&c.adapter.address, &(i128::MAX / 2)).is_err());
    assert!(v.try_increase_relative_cap(&c.adapter.address, &WAD).is_err());
    assert!(v.try_increase_timelock(&TimelockOpKind::SetFees, &60).is_err());
    assert!(v.try_abdicate(&TimelockOpKind::SetFees).is_err());
    assert!(v.try_set_fees(&c.bob, &500, &5_000, &Vec::new(&c.env)).is_err());
    // A submission covers its exact arguments, once.
    v.submit(&PendingOp::SetIsAllocator(c.bob.clone(), true));
    assert!(v.try_set_is_allocator(&c.alice, &true).is_err());
    assert!(v.try_set_is_allocator(&c.bob, &false).is_err());
    v.set_is_allocator(&c.bob, &true);
    assert!(v.try_set_is_allocator(&c.bob, &true).is_err());
}

#[test]
fn an_abdicated_operation_is_gone_for_good() {
    let c = Ctx::new();
    let other = c.new_adapter();
    let pending = PendingOp::AddAdapter(other.address.clone());
    c.vault.submit(&pending);
    c.run(PendingOp::Abdicate(TimelockOpKind::AddAdapter));
    assert!(c.vault.abdicated(&TimelockOpKind::AddAdapter));
    assert!(c.vault.try_add_adapter(&other.address).is_err(), "what was pending cannot execute");
    assert!(c.vault.try_submit(&pending).is_err(), "nothing new can be submitted");
    assert!(!c.vault.abdicated(&TimelockOpKind::RemoveAdapter));
    c.run(PendingOp::RemoveAdapter(c.adapter.address.clone()));
}

#[test]
fn a_delay_is_lowered_only_after_the_delay_it_lowers() {
    let c = Ctx::new();
    let kind = TimelockOpKind::SetFees;
    c.run(PendingOp::IncreaseTimelock(kind.clone(), 7 * 86_400));
    assert_eq!(c.vault.timelock(&kind), 7 * 86_400);
    assert_eq!(c.vault.timelock(&TimelockOpKind::AddAdapter), 0, "the other delays are unchanged");
    let lower = PendingOp::DecreaseTimelock(kind.clone(), 0);
    c.vault.submit(&lower);
    c.wait(7 * 86_400 - 1);
    assert!(c.vault.try_decrease_timelock(&kind, &0).is_err());
    c.wait(1);
    c.vault.decrease_timelock(&kind, &0);
    assert_eq!(c.vault.timelock(&kind), 0);
    // Not above a year, and only upwards.
    c.vault.submit(&PendingOp::IncreaseTimelock(kind.clone(), YEAR + 1));
    assert!(c.vault.try_increase_timelock(&kind, &(YEAR + 1)).is_err());
}

// ─── caps ──────────────────────────────────────────────────────────────────

#[test]
fn a_new_adapter_receives_nothing_until_both_caps_are_raised() {
    let c = Ctx::new();
    let data = Bytes::new(&c.env);
    c.deposit(&c.alice, 100 * ONE);

    // Absolute only is not enough.
    let abs_only = c.new_adapter();
    c.run(PendingOp::AddAdapter(abs_only.address.clone()));
    assert!(c.vault.try_allocate(&c.allocator, &abs_only.address, &data, &ONE).is_err());
    c.run(PendingOp::IncreaseAbsoluteCap(abs_only.address.clone(), 50 * ONE));
    assert!(c.vault.try_allocate(&c.allocator, &abs_only.address, &data, &ONE).is_err(), "relative cap still 0");

    // Relative only is not enough either.
    let rel_only = c.new_adapter();
    c.run(PendingOp::AddAdapter(rel_only.address.clone()));
    c.run(PendingOp::IncreaseRelativeCap(rel_only.address.clone(), WAD));
    assert!(c.vault.try_allocate(&c.allocator, &rel_only.address, &data, &ONE).is_err(), "absolute cap still 0");

    // Both caps: allocation goes through.
    c.run(PendingOp::IncreaseRelativeCap(abs_only.address.clone(), WAD / 4));
    c.allocate(&abs_only.address, 25 * ONE);
    assert_eq!(c.vault.allocation(&abs_only.address), 25 * ONE);
}

#[test]
fn the_absolute_and_the_relative_cap_both_bound_an_allocation() {
    let c = Ctx::new();
    let other = c.new_adapter();
    c.run(PendingOp::AddAdapter(other.address.clone()));
    c.run(PendingOp::IncreaseAbsoluteCap(other.address.clone(), 30 * ONE));
    c.run(PendingOp::IncreaseRelativeCap(other.address.clone(), WAD / 4)); // 25% of the total
    c.deposit(&c.alice, 100 * ONE);
    let data = Bytes::new(&c.env);
    assert!(c.vault.try_allocate(&c.allocator, &other.address, &data, &(25 * ONE + 1)).is_err(), "above 25% of 100");
    c.allocate(&other.address, 25 * ONE);
    c.deposit(&c.alice, 100 * ONE); // total 200: 25% is 50, the absolute cap is 30
    assert!(c.vault.try_allocate(&c.allocator, &other.address, &data, &(5 * ONE + 1)).is_err(), "above the absolute cap");
    c.allocate(&other.address, 5 * ONE);
    assert_eq!(c.vault.allocation(&other.address), 30 * ONE);
    assert_eq!(other.real_assets(), 30 * ONE);
    // Taking back frees room.
    c.deallocate(&other.address, 10 * ONE);
    assert_eq!(c.vault.allocation(&other.address), 20 * ONE);
    c.allocate(&other.address, 10 * ONE);
}

#[test]
fn caps_go_up_through_the_timelock_and_down_at_once() {
    let c = Ctx::with(3_600, |env, owner| fees(env, owner, 0, 0));
    let ad = &c.adapter.address;
    // Up: submitted, then waits.
    let up = PendingOp::IncreaseAbsoluteCap(ad.clone(), i128::MAX / 2);
    c.vault.submit(&up);
    assert!(c.vault.try_increase_absolute_cap(ad, &(i128::MAX / 2)).is_err());
    // Down: the curator or a sentinel, at once.
    c.vault.decrease_absolute_cap(&c.sentinel, ad, &(10 * ONE));
    c.vault.decrease_relative_cap(&c.curator, ad, &(WAD / 2));
    assert_eq!((c.vault.absolute_cap(ad), c.vault.relative_cap(ad)), (10 * ONE, WAD / 2));
    // "Decrease" cannot raise, "increase" cannot lower, a relative cap cannot pass 100%.
    assert!(c.vault.try_decrease_absolute_cap(&c.curator, ad, &(11 * ONE)).is_err());
    assert!(c.vault.try_decrease_relative_cap(&c.curator, ad, &WAD).is_err());
    c.vault.revoke(&c.curator, &up);
    for bad in [PendingOp::IncreaseAbsoluteCap(ad.clone(), ONE), PendingOp::IncreaseRelativeCap(ad.clone(), WAD / 4), PendingOp::IncreaseRelativeCap(ad.clone(), WAD + 1)] {
        c.vault.submit(&bad);
        c.wait(3_600);
        let r = match bad {
            PendingOp::IncreaseAbsoluteCap(a, n) => c.vault.try_increase_absolute_cap(&a, &n).is_err(),
            PendingOp::IncreaseRelativeCap(a, n) => c.vault.try_increase_relative_cap(&a, &n).is_err(),
            _ => unreachable!(),
        };
        assert!(r);
    }
    assert!(c.vault.try_decrease_absolute_cap(&c.curator, ad, &-1).is_err());
}

#[test]
fn a_cap_lowered_under_the_allocation_stops_new_allocations_only() {
    let c = Ctx::new();
    let ad = &c.adapter.address;
    c.deposit(&c.alice, 100 * ONE);
    c.allocate(ad, 60 * ONE);
    c.vault.decrease_absolute_cap(&c.sentinel, ad, &(10 * ONE));
    assert!(c.vault.try_allocate(&c.allocator, ad, &Bytes::new(&c.env), &1).is_err());
    c.deallocate(ad, 60 * ONE);
    assert_eq!(c.vault.idle_assets(), 100 * ONE);
    c.allocate(ad, 10 * ONE);
}

// ─── adapters ──────────────────────────────────────────────────────────────

#[test]
fn an_adapter_is_registered_for_this_vault_and_this_asset_only() {
    let c = Ctx::new();
    let other_asset = c.env.register_stellar_asset_contract_v2(Address::generate(&c.env)).address();
    let for_other_vault = c.env.register(DummyAdapter, (&c.ve, &Address::generate(&c.env), &c.token.address, &c.owner));
    let for_other_asset = c.env.register(DummyAdapter, (&c.ve, &c.vault.address, &other_asset, &c.owner));
    for bad in [for_other_vault, for_other_asset] {
        c.vault.submit(&PendingOp::AddAdapter(bad.clone()));
        assert!(c.vault.try_add_adapter(&bad).is_err());
        assert!(!c.vault.is_adapter(&bad));
    }
    assert_eq!(c.vault.adapters().len(), 1);
}

#[test]
fn nothing_moves_beyond_what_is_there_or_to_an_unknown_adapter() {
    let c = Ctx::new();
    let unknown = c.new_adapter();
    c.deposit(&c.alice, 100 * ONE);
    c.allocate(&c.adapter.address, 60 * ONE);
    let data = Bytes::new(&c.env);
    let al = &c.allocator;
    assert!(c.vault.try_allocate(al, &c.adapter.address, &data, &(40 * ONE + 1)).is_err(), "more than the idle assets");
    assert!(c.vault.try_deallocate(al, &c.adapter.address, &data, &(60 * ONE + 1)).is_err(), "more than the adapter holds");
    assert!(c.vault.try_allocate(al, &unknown.address, &data, &ONE).is_err(), "not registered");
    assert!(c.vault.try_set_liquidity_adapter(al, &Some(unknown.address.clone()), &data).is_err());
    assert!(c.vault.try_allocate(al, &c.adapter.address, &data, &0).is_err());
    assert!(c.vault.try_deallocate(al, &c.adapter.address, &data, &-ONE).is_err());
    assert_eq!((c.vault.idle_assets(), c.adapter.real_assets(), c.vault.total_assets()), (40 * ONE, 60 * ONE, 100 * ONE));
    assert_eq!(c.value_of(&c.alice), 100 * ONE);
}

#[test]
fn locked_assets_cannot_be_taken_back() {
    let c = Ctx::new();
    c.deposit(&c.alice, 100 * ONE);
    c.allocate(&c.adapter.address, 60 * ONE);
    c.adapter.lock(&(50 * ONE));
    assert_eq!(c.adapter.liquid_assets(), 10 * ONE);
    assert!(c.vault.try_deallocate(&c.allocator, &c.adapter.address, &Bytes::new(&c.env), &(10 * ONE + 1)).is_err());
    c.deallocate(&c.adapter.address, 10 * ONE);
    assert_eq!(c.vault.total_assets(), 100 * ONE, "locked assets still count in the total");
}

// ─── queue ─────────────────────────────────────────────────────────────────

#[test]
fn a_queued_request_holds_its_shares_and_is_paid_in_order() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 400 * ONE);
    c.deposit(b, 600 * ONE);
    let (sa0, sb0) = (c.shares(a), c.shares(b));
    let supply0 = c.vault.total_supply();
    c.adapter.lock(&(1_000 * ONE));
    let (a0, b0) = (c.token.balance(a), c.token.balance(b));
    let s1 = c.withdraw(a, 100 * ONE);
    let s2 = c.withdraw(b, 200 * ONE);
    let s3 = c.withdraw(a, 50 * ONE);
    assert_eq!(c.vault.withdrawal_queue_len(), 3);
    assert_eq!(c.vault.balance(&c.vault.address), s1 + s2 + s3, "the shares wait in the vault");
    assert_eq!(c.vault.total_supply(), supply0, "and still count in the supply");
    assert_eq!((c.shares(a), c.shares(b)), (sa0 - s1 - s3, sb0 - s2));
    let q = c.vault.withdrawal_queue();
    assert_eq!((q.get(0).unwrap().id, q.get(1).unwrap().id, q.get(2).unwrap().id), (0, 1, 2));
    // Liquidity for the first two only.
    c.adapter.lock(&(700 * ONE));
    assert_eq!(c.vault.process_withdrawals(&0), 2);
    assert_eq!((c.token.balance(a) - a0, c.token.balance(b) - b0), (100 * ONE, 200 * ONE));
    assert_eq!(c.vault.withdrawal_queue().get(0).unwrap().id, 2);
    assert_eq!(c.vault.total_supply(), supply0 - s1 - s2);
    c.adapter.lock(&0);
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.token.balance(a) - a0, 150 * ONE);
    assert_eq!(c.vault.balance(&c.vault.address), 0);
    assert_eq!(c.vault.process_withdrawals(&0), 0);
}

#[test]
fn a_request_pays_the_receiver_it_named() {
    let c = Ctx::with_liquidity_adapter();
    let (a, receiver) = (&c.alice, &c.bob);
    c.deposit(a, 100 * ONE);
    c.adapter.lock(&(100 * ONE));
    let (a0, r0) = (c.token.balance(a), c.token.balance(receiver));
    assert!(c.vault.withdraw(&(40 * ONE), receiver, a, a) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    c.adapter.lock(&0);
    c.vault.process_withdrawals(&0);
    assert_eq!((c.token.balance(a) - a0, c.token.balance(receiver) - r0), (0, 40 * ONE));
}

#[test]
fn a_queued_request_follows_the_share_price_until_it_is_paid() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 500 * ONE);
    c.deposit(b, 500 * ONE);
    c.adapter.lock(&(1_000 * ONE));
    let a0 = c.token.balance(a);
    let half = c.shares(a);
    assert_eq!(c.redeem(a, half), 0);
    c.adapter.lock(&0);
    c.adapter_result(100 * ONE); // +10% while it waits
    c.vault.process_withdrawals(&0);
    assert!(close(c.token.balance(a) - a0, 550 * ONE, 2), "paid {}", c.token.balance(a) - a0);
    assert!(close(c.value_of(b), 550 * ONE, 2));
}

#[test]
fn the_performance_fee_of_a_queued_request_is_on_its_own_gain() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 0, 2_000)); // 20%
    c.vault.set_liquidity_adapter(&c.allocator, &Some(c.adapter.address.clone()), &Bytes::new(&c.env));
    let a = &c.alice;
    c.deposit(a, 1_000 * ONE);
    c.adapter_result(100 * ONE);
    c.adapter.lock(&c.adapter.real_assets());
    let (a0, r0) = (c.token.balance(a), c.token.balance(&c.owner));
    let half = c.shares(a) / 2;
    assert_eq!(c.redeem(a, half), 0); // half: cost 500, worth 550
    assert!(close(c.vault.cost_basis(a), 500 * ONE, 1), "the request took its part of the cost");
    assert!(close(c.vault.withdrawal_queue().get(0).unwrap().cost, 500 * ONE, 1));
    c.adapter.lock(&0);
    c.vault.process_withdrawals(&0);
    assert!(close(c.token.balance(&c.owner) - r0, 10 * ONE, 2), "fee {}", c.token.balance(&c.owner) - r0);
    assert!(close(c.token.balance(a) - a0, 540 * ONE, 2), "paid {}", c.token.balance(a) - a0);
    assert!(close(c.vault.cost_basis(a), 500 * ONE, 1), "the rest keeps its own cost");
}

#[test]
fn deposits_stay_idle_while_requests_wait_and_can_pay_them() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 100 * ONE);
    c.adapter.lock(&(100 * ONE));
    let a0 = c.token.balance(a);
    assert!(c.withdraw(a, 60 * ONE) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    c.deposit(b, 60 * ONE);
    assert_eq!((c.vault.idle_assets(), c.adapter.real_assets()), (60 * ONE, 100 * ONE));
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.token.balance(a) - a0, 60 * ONE);
    // The queue is empty again: the next deposit is invested.
    c.deposit(b, 10 * ONE);
    assert_eq!(c.adapter.real_assets(), 110 * ONE);
}

#[test]
fn a_redeem_does_not_flush_an_existing_queue_even_when_liquid() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 400 * ONE);
    c.deposit(b, 600 * ONE);
    c.adapter.lock(&(1_000 * ONE));
    let (a0, b0) = (c.token.balance(a), c.token.balance(b));
    assert!(c.withdraw(a, 100 * ONE) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    // Liquidity returns, but a later exit must not drain the backlog.
    c.adapter.lock(&0);
    assert!(c.withdraw(b, 200 * ONE) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 2);
    assert_eq!((c.token.balance(a) - a0, c.token.balance(b) - b0), (0, 0));
    assert_eq!(c.vault.process_withdrawals(&0), 2);
    assert_eq!((c.token.balance(a) - a0, c.token.balance(b) - b0), (100 * ONE, 200 * ONE));
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn process_withdrawals_respects_max() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 300 * ONE);
    c.deposit(b, 300 * ONE);
    c.adapter.lock(&(600 * ONE));
    assert!(c.withdraw(a, 100 * ONE) > 0);
    assert!(c.withdraw(b, 100 * ONE) > 0);
    assert!(c.withdraw(a, 100 * ONE) > 0);
    c.adapter.lock(&0);
    assert_eq!(c.vault.process_withdrawals(&1), 1);
    assert_eq!(c.vault.withdrawal_queue_len(), 2);
    assert_eq!(c.vault.process_withdrawals(&2), 2);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn below_min_withdrawal_shares_is_refused() {
    let c = Ctx::with_liquidity_adapter();
    c.deposit(&c.alice, 100 * ONE);
    assert!(c.vault.try_redeem(&999, &c.alice, &c.alice, &c.alice).is_err());
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
    // At the minimum with liquidity locked → stays queued (must be worth > 0 so settle does not no-op).
    c.adapter.lock(&(100 * ONE));
    let shares = c.withdraw(&c.alice, ONE);
    assert!(shares >= 1_000);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    assert_eq!(c.vault.withdrawal_queue().get(0).unwrap().shares, shares);
}

#[test]
fn full_balance_below_min_withdrawal_shares_is_allowed() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, ONE);
    // Hand bob a dust lot (< MIN) so a full-balance exit is still allowed.
    c.vault.transfer(a, b, &500);
    assert_eq!(c.vault.balance(b), 500);
    // Under virtual shares a 500-share lot may round to 0 assets; the point is it is not refused.
    assert!(c.vault.try_redeem(&500, b, b, b).is_ok());
    assert_eq!(c.vault.balance(b), 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn dust_balance_below_min_can_still_exit() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, ONE);
    c.vault.transfer(a, b, &1_500);
    assert!(c.vault.try_redeem(&1_000, b, b, b).is_ok());
    assert_eq!(c.vault.balance(b), 500);
    assert!(c.vault.balance(b) < 1_000);
    // Remaining dust (< MIN) may still exit; a non-dust holder cannot redeem 500 alone.
    assert!(c.vault.try_redeem(&500, b, b, b).is_ok());
    assert_eq!(c.vault.balance(b), 0);
    assert!(c.vault.try_redeem(&500, a, a, a).is_err());
}

#[test]
fn cancel_head_of_queue_lets_the_next_settle() {
    let c = Ctx::with_liquidity_adapter();
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 400 * ONE);
    c.deposit(b, 400 * ONE);
    c.adapter.lock(&(800 * ONE));
    let b0 = c.token.balance(b);
    assert!(c.vault.withdraw(&(100 * ONE), a, a, a) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    assert!(c.vault.withdraw(&(100 * ONE), b, b, b) > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 2);
    let id_a = c.vault.withdrawal_queue().get(0).unwrap().id;
    c.vault.cancel_withdrawal(a, &id_a);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    assert_eq!(c.vault.withdrawal_queue().get(0).unwrap().owner, b.clone());
    c.adapter.lock(&0);
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.token.balance(b) - b0, 100 * ONE);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn a_queued_withdraw_caps_payout_at_its_target_assets() {
    let c = Ctx::with_liquidity_adapter();
    let a = &c.alice;
    c.deposit(a, 1_000 * ONE);
    c.adapter.lock(&(1_000 * ONE));
    let a0 = c.token.balance(a);
    let shares = c.vault.withdraw(&(500 * ONE), a, a, a);
    assert!(shares > 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    assert_eq!(
        c.vault.withdrawal_queue().get(0).unwrap().target_assets,
        Some(500 * ONE)
    );
    c.adapter.lock(&0);
    c.adapter_result(200 * ONE); // price up while queued
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.token.balance(a) - a0, 500 * ONE);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn process_stalls_when_liquidity_covers_payout_but_not_the_fee() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 0, 2_000)); // 20%
    c.vault
        .set_liquidity_adapter(&c.allocator, &Some(c.adapter.address.clone()), &Bytes::new(&c.env));
    let a = &c.alice;
    c.deposit(a, 1_000 * ONE);
    c.adapter_result(100 * ONE);
    c.adapter.lock(&c.adapter.real_assets());
    // Half the shares: cost ~500, worth ~550 → paid 540, performance 10 (need 550).
    let half = c.vault.balance(a) / 2;
    assert_eq!(c.redeem(a, half), 0);
    let real = c.adapter.real_assets();
    c.adapter.lock(&(real - 540 * ONE));
    assert!(c.vault.available_liquidity() < 550 * ONE);
    assert_eq!(c.vault.process_withdrawals(&0), 0);
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    c.adapter.lock(&(real - 550 * ONE));
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn without_a_liquidity_adapter_only_idle_pays_the_queue() {
    let c = Ctx::new();
    let a = &c.alice;
    c.deposit(a, 100 * ONE);
    c.allocate(&c.adapter.address, 100 * ONE);
    c.adapter.lock(&(100 * ONE));
    assert_eq!(c.vault.available_liquidity(), 0);
    assert_eq!(c.vault.withdraw(&(40 * ONE), a, a, a), c.vault.preview_withdraw(&(40 * ONE)));
    // Alone + illiquid → still queued (withdraw returns shares escrowed).
    assert_eq!(c.vault.withdrawal_queue_len(), 1);
    assert_eq!(c.vault.process_withdrawals(&0), 0);
    c.adapter.lock(&0);
    c.deallocate(&c.adapter.address, 40 * ONE);
    assert_eq!(c.vault.process_withdrawals(&0), 1);
    assert_eq!(c.vault.withdrawal_queue_len(), 0);
}

#[test]
fn max_exit_views_are_accounting_only_even_when_illiquid() {
    // Illiquidity is handled by the queue; max_* do not clip to available liquidity.
    let c = Ctx::with_liquidity_adapter();
    let a = &c.alice;
    c.deposit(a, 100 * ONE);
    let bal = c.vault.balance(a);
    c.adapter.lock(&(100 * ONE));
    assert_eq!(c.vault.available_liquidity(), 0);
    assert_eq!(c.vault.max_redeem(a), bal);
    assert_eq!(c.vault.max_withdraw(a), c.vault.preview_redeem(&bal));
}

// ─── fees ──────────────────────────────────────────────────────────────────

#[test]
fn fees_beyond_the_limits_are_refused_at_deployment_and_at_change() {
    let c = Ctx::new();
    let base = fees(&c.env, &c.owner, 0, 0);
    let bad = [
        Fees { management_bps: 501, ..base.clone() },
        Fees { performance_bps: 5_001, ..base.clone() },
        Fees { penalties: vec![&c.env, (ONE, 2_001u32)], ..base.clone() },
        Fees { penalties: vec![&c.env, (2 * ONE, 100u32), (ONE, 200u32)], ..base.clone() },
        Fees { penalties: vec![&c.env, (ONE, 200u32), (2 * ONE, 100u32)], ..base.clone() },
        Fees { receiver: c.vault.address.clone(), ..base.clone() },
    ];
    for f in bad {
        c.vault.submit(&PendingOp::SetFees(f.receiver.clone(), f.management_bps, f.performance_bps, f.penalties.clone()));
        assert!(c.vault.try_set_fees(&f.receiver, &f.management_bps, &f.performance_bps, &f.penalties).is_err());
    }
    assert_eq!(c.vault.fees(), base);
    c.run(PendingOp::SetFees(c.bob.clone(), 500, 5_000, vec![&c.env, (ONE, 2_000u32)]));
    assert_eq!(c.vault.fees().receiver, c.bob);
}

#[test]
fn the_management_fee_is_minted_for_the_time_elapsed_and_once() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 200, 0));
    c.deposit(&c.alice, 1_000 * ONE);
    c.wait(YEAR);
    assert!(close(c.value_of(&c.alice), 980 * ONE, ONE / 100), "previews already count the fee due");
    c.vault.accrue();
    assert!(close(c.value_of(&c.owner), 20 * ONE, ONE / 100), "receiver {}", c.value_of(&c.owner));
    let minted = c.vault.balance(&c.owner);
    c.vault.accrue();
    assert_eq!(c.vault.balance(&c.owner), minted);
    assert_eq!(c.vault.total_assets(), 1_000 * ONE, "paid in shares: no asset left");
}

#[test]
fn the_fee_due_under_the_old_rate_is_minted_before_a_new_rate_applies() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 100, 0));
    c.deposit(&c.alice, 1_000 * ONE);
    c.vault.submit(&PendingOp::SetFees(c.owner.clone(), 500, 0, Vec::new(&c.env)));
    c.wait(YEAR);
    c.vault.set_fees(&c.owner, &500, &0, &Vec::new(&c.env));
    assert!(close(c.value_of(&c.owner), 10 * ONE, ONE / 100), "one year at 1%, not 5%: {}", c.value_of(&c.owner));
}

#[test]
fn no_performance_fee_without_a_gain_and_none_on_a_loss() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 0, 2_000));
    c.vault.set_liquidity_adapter(&c.allocator, &Some(c.adapter.address.clone()), &Bytes::new(&c.env));
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 1_000 * ONE);
    c.adapter_result(100 * ONE);
    // Immediate sale after a gain: the fee is taken (mirrors the old performance_fee_on_sale_after_gain).
    let r0 = c.token.balance(&c.owner);
    let a0 = c.token.balance(a);
    assert!(close(c.redeem_all(a), 1_080 * ONE, 2));
    assert!(close(c.token.balance(&c.owner) - r0, 20 * ONE, 2));
    assert!(close(c.token.balance(a) - a0, 1_080 * ONE, 2));

    // Fresh depositor at the new price: no gain, no fee. Then a loss: still no fee.
    c.deposit(b, 1_100 * ONE);
    let r1 = c.token.balance(&c.owner);
    assert!(close(c.redeem_all(b), 1_100 * ONE, 2));
    assert_eq!(c.token.balance(&c.owner), r1);
    c.deposit(a, 1_000 * ONE);
    c.adapter_result(-(300 * ONE));
    let (gross, _, performance, paid) = c.vault.value_of(a, &c.vault.balance(a));
    assert_eq!((performance, paid), (0, gross));
    c.redeem_all(a);
    assert_eq!(c.token.balance(&c.owner), r1);
}

#[test]
fn the_early_exit_penalty_stays_in_the_vault() {
    // Penalty tiers are in **shares**. Scale thresholds by VIRTUAL_SHARES so they still
    // sit at ~50% / ~90% of a 1000*ONE first deposit under the inflation offset.
    let t1 = 500 * ONE * VIRTUAL_SHARES;
    let t2 = 900 * ONE * VIRTUAL_SHARES;
    let c = Ctx::with(0, |env, owner| Fees {
        receiver: owner.clone(),
        management_bps: 0,
        performance_bps: 0,
        penalties: vec![env, (t1, 100u32), (t2, 300u32)],
    });
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 1_000 * ONE);
    c.deposit(b, 1_000 * ONE);
    assert!(close(c.redeem(a, 100 * ONE * VIRTUAL_SHARES), 100 * ONE, 2), "under the first tier");
    assert!(close(c.redeem(a, 600 * ONE * VIRTUAL_SHARES), 594 * ONE, 2), "1% above 500 shares");
    assert!(c.value_of(b) > 1_000 * ONE, "the penalty went to the remaining holders");
    assert_eq!(c.token.balance(&c.owner), 0, "not to the fee receiver");
    let (gross, penalty, _, _) = c.vault.value_of(b, &c.vault.balance(b));
    assert!(close(penalty, gross * 3 / 100, 2), "3% above 900 shares");
}

#[test]
fn shares_transfer_with_their_cost() {
    let c = Ctx::with(0, |env, owner| fees(env, owner, 0, 2_000));
    let (a, b) = (&c.alice, &c.bob);
    c.deposit(a, 1_000 * ONE);
    c.sac.mint(&c.vault.address, &(100 * ONE)); // +10%
    let moved = c.shares(a) * 2 / 5;
    c.vault.transfer(a, b, &moved);
    assert!(close(c.vault.cost_basis(a), 600 * ONE, 1));
    assert!(close(c.vault.cost_basis(b), 400 * ONE, 1));
    // The gain the shares carried is taxed at the receiver's sale.
    let r0 = c.token.balance(&c.owner);
    c.redeem_all(b);
    assert!(close(c.token.balance(&c.owner) - r0, 8 * ONE, 2), "fee {}", c.token.balance(&c.owner) - r0);
    assert!(c.vault.try_transfer(a, b, &(c.vault.balance(a) + 1)).is_err());
    assert!(c.vault.try_transfer(a, b, &-1).is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// Unique current tests (VaultCtx)
// ═══════════════════════════════════════════════════════════════════════════

// ─── constructor / roles ───────────────────────────────────────────────────

#[test]
fn constructor_sets_owner_only() {
    let ctx = deploy(0, None);
    let v = ctx.vault();
    assert_eq!(v.owner(), ctx.owner);
    assert!(v.curator().is_none());
    assert_eq!(v.allocators().len(), 0);
    assert_eq!(v.default_timelock(), 0);
    assert_eq!(v.validation_engine(), ctx.ve);
    assert_eq!(v.total_supply(), 0);
}

#[test]
fn set_queue_manager_needs_a_curator() {
    let ctx = deploy(0, None);
    let qm = Address::generate(&ctx.env);
    assert!(ctx.vault().try_set_queue_manager(&Some(qm)).is_err());
}

#[test]
fn roles_bootstrap_and_sentinel() {
    let ctx = deploy(0, None);
    let v = ctx.vault();
    v.set_curator(&ctx.curator);
    assert_eq!(v.curator(), Some(ctx.curator.clone()));
    v.set_is_sentinel(&ctx.sentinel, &true);
    assert!(v.is_sentinel(&ctx.sentinel));
    ctx.submit_exec(PendingOp::SetIsAllocator(ctx.allocator.clone(), true));
    assert!(v.is_allocator(&ctx.allocator));
    ctx.submit_exec(PendingOp::SetIsAllocator(ctx.allocator.clone(), false));
    assert!(!v.is_allocator(&ctx.allocator));
}

// ─── invest happy path ─────────────────────────────────────────────────────

#[test]
fn deposit_auto_allocates_to_liquidity_adapter() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 10_000);
    let shares = ctx
        .vault()
        .deposit(&10_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(shares, ctx.vault().balance(&ctx.investor));
    assert!(shares > 10_000, "virtual shares inflate the share count");
    assert_eq!(ctx.vault().idle_assets(), 0);
    assert_eq!(ctx.adapter().real_assets(), 10_000);
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 10_000);
    assert_eq!(ctx.vault().total_assets(), 10_000);
    assert_eq!(ctx.vault().cost_basis(&ctx.investor), 10_000);
}

#[test]
fn mint_pulls_preview_assets() {
    let ctx = deploy_ready();
    // Mint a share count that maps to a meaningful asset pull under VIRTUAL_SHARES.
    let shares = 5_000 * VIRTUAL_SHARES;
    let preview = ctx.vault().preview_mint(&shares);
    ctx.mint_underlying(&ctx.investor, preview);
    let assets = ctx
        .vault()
        .mint(&shares, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(assets, preview);
    assert_eq!(ctx.vault().balance(&ctx.investor), shares);
}

#[test]
fn redeem_settles_immediately_when_liquid() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 8_000);
    ctx.vault()
        .deposit(&8_000, &ctx.investor, &ctx.investor, &ctx.investor);
    let paid = ctx.vault().redeem(
        &ctx.vault().balance(&ctx.investor),
        &ctx.investor,
        &ctx.investor,
        &ctx.investor,
    );
    assert!(close(paid, 8_000, 2));
    assert_eq!(ctx.vault().balance(&ctx.investor), 0);
    assert!(close(ctx.token().balance(&ctx.investor), 8_000, 2));
    assert_eq!(ctx.vault().withdrawal_queue_len(), 0);
}

#[test]
fn withdraw_returns_shares_and_pays_assets() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 6_000);
    ctx.vault()
        .deposit(&6_000, &ctx.investor, &ctx.investor, &ctx.investor);
    let before = ctx.vault().balance(&ctx.investor);
    let shares = ctx
        .vault()
        .withdraw(&3_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert!(shares > 0);
    assert_eq!(ctx.vault().balance(&ctx.investor), before - shares);
    assert_eq!(ctx.token().balance(&ctx.investor), 3_000);
}

// ─── cancel withdrawal / sentinel ──────────────────────────────────────────

#[test]
fn owner_can_cancel_queued_withdrawal() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 5_000);
    ctx.vault()
        .deposit(&5_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(ctx.vault().cost_basis(&ctx.investor), 5_000);
    let total = ctx.vault().balance(&ctx.investor);

    ctx.adapter().lock(&5_000);
    let burned = ctx
        .vault()
        .withdraw(&2_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 1);
    let id = ctx.vault().withdrawal_queue().get(0).unwrap().id;
    assert_eq!(ctx.vault().balance(&ctx.investor), total - burned);
    assert_eq!(ctx.vault().balance(&ctx.vault), burned);
    assert!(close(ctx.vault().cost_basis(&ctx.investor), 3_000, 1));

    ctx.vault().cancel_withdrawal(&ctx.investor, &id);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 0);
    assert_eq!(ctx.vault().balance(&ctx.investor), total);
    assert_eq!(ctx.vault().balance(&ctx.vault), 0);
    assert_eq!(ctx.vault().cost_basis(&ctx.investor), 5_000);
}

#[test]
fn cancel_middle_of_queue_preserves_fifo() {
    let ctx = deploy_ready();
    let a = ctx.investor.clone();
    let b = Address::generate(&ctx.env);
    ctx.mint_underlying(&a, 4_000);
    ctx.mint_underlying(&b, 4_000);
    ctx.vault().deposit(&4_000, &a, &a, &a);
    ctx.vault().deposit(&4_000, &b, &b, &b);
    let sb0 = ctx.vault().balance(&b);
    ctx.adapter().lock(&8_000);

    assert!(ctx.vault().withdraw(&2_000, &a, &a, &a) > 0);
    assert!(ctx.vault().withdraw(&2_000, &b, &b, &b) > 0);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 2);
    let id_a = ctx.vault().withdrawal_queue().get(0).unwrap().id;
    let id_b = ctx.vault().withdrawal_queue().get(1).unwrap().id;

    ctx.vault().cancel_withdrawal(&b, &id_b);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 1);
    assert_eq!(ctx.vault().withdrawal_queue().get(0).unwrap().id, id_a);
    assert_eq!(ctx.vault().balance(&b), sb0);

    ctx.adapter().lock(&0);
    assert_eq!(ctx.vault().process_withdrawals(&0), 1);
    assert_eq!(ctx.token().balance(&a), 2_000);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 0);
}

#[test]
#[should_panic(expected = "not authorized")]
fn cancel_by_non_owner_panics() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 3_000);
    ctx.vault()
        .deposit(&3_000, &ctx.investor, &ctx.investor, &ctx.investor);
    ctx.adapter().lock(&3_000);
    assert!(
        ctx.vault()
            .withdraw(&1_000, &ctx.investor, &ctx.investor, &ctx.investor)
            > 0
    );
    let id = ctx.vault().withdrawal_queue().get(0).unwrap().id;
    let other = Address::generate(&ctx.env);
    ctx.vault().cancel_withdrawal(&other, &id);
}

#[test]
fn queue_manager_can_cancel_like_request_owner() {
    let ctx = deploy_ready();
    let qm = Address::generate(&ctx.env);
    ctx.vault().set_queue_manager(&Some(qm.clone()));
    assert_eq!(ctx.vault().queue_manager(), Some(qm.clone()));

    ctx.mint_underlying(&ctx.investor, 5_000);
    ctx.vault()
        .deposit(&5_000, &ctx.investor, &ctx.investor, &ctx.investor);
    let total = ctx.vault().balance(&ctx.investor);
    ctx.adapter().lock(&5_000);
    let burned = ctx
        .vault()
        .withdraw(&2_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 1);
    let id = ctx.vault().withdrawal_queue().get(0).unwrap().id;
    assert_eq!(ctx.vault().balance(&ctx.investor), total - burned);
    assert!(close(ctx.vault().cost_basis(&ctx.investor), 3_000, 1));

    ctx.vault().cancel_withdrawal(&qm, &id);
    assert_eq!(ctx.vault().withdrawal_queue_len(), 0);
    assert_eq!(ctx.vault().balance(&ctx.investor), total);
    assert_eq!(ctx.vault().balance(&ctx.vault), 0);
    assert_eq!(ctx.vault().cost_basis(&ctx.investor), 5_000);

    ctx.vault().set_queue_manager(&None);
    assert_eq!(ctx.vault().queue_manager(), None);
}

#[test]
fn sentinel_deallocate_helps_queue() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 4_000);
    ctx.vault()
        .deposit(&4_000, &ctx.investor, &ctx.investor, &ctx.investor);
    ctx.adapter().lock(&4_000);
    assert!(
        ctx.vault()
            .withdraw(&4_000, &ctx.investor, &ctx.investor, &ctx.investor)
            > 0
    );
    assert_eq!(ctx.vault().withdrawal_queue_len(), 1);
    // Sentinel can deallocate once unlocked to free liquidity.
    ctx.adapter().lock(&0);
    ctx.vault()
        .deallocate(&ctx.sentinel, &ctx.adapter, &Bytes::new(&ctx.env), &4_000);
    assert_eq!(ctx.vault().idle_assets(), 4_000);
    assert_eq!(ctx.vault().process_withdrawals(&0), 1);
    assert_eq!(ctx.token().balance(&ctx.investor), 4_000);
}

// ─── allocate / caps / yield ───────────────────────────────────────────────

#[test]
fn manual_allocate_and_deallocate() {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    // Clear auto-liq allocate by unsetting? Keep liq adapter but deposit with empty queue
    // allocates automatically — instead allocate manually after deposit without liq.
    let v = ctx.vault();
    v.set_liquidity_adapter(&ctx.allocator, &None, &Bytes::new(&ctx.env));

    ctx.mint_underlying(&ctx.investor, 2_000);
    ctx.vault()
        .deposit(&2_000, &ctx.investor, &ctx.investor, &ctx.investor);
    assert_eq!(ctx.vault().idle_assets(), 2_000);

    v.allocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &1_500,
    );
    assert_eq!(ctx.vault().idle_assets(), 500);
    assert_eq!(ctx.adapter().real_assets(), 1_500);
    assert_eq!(v.allocation(&ctx.adapter), 1_500);

    v.deallocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &500,
    );
    assert_eq!(v.allocation(&ctx.adapter), 1_000);
    assert_eq!(ctx.vault().idle_assets(), 1_000);
}

#[test]
fn adapter_yield_can_be_redeemed() {
    let ctx = deploy_ready();
    ctx.mint_underlying(&ctx.investor, 1_000);
    ctx.vault()
        .deposit(&1_000, &ctx.investor, &ctx.investor, &ctx.investor);
    // Simulate yield into adapter (no share mint).
    let donor = Address::generate(&ctx.env);
    ctx.mint_underlying(&donor, 200);
    ctx.adapter().inject(&donor, &200);
    assert_eq!(ctx.adapter().real_assets(), 1_200);
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 1_000);
    assert_eq!(ctx.vault().total_assets(), 1_200);

    let paid = ctx.vault().redeem(
        &ctx.vault().balance(&ctx.investor),
        &ctx.investor,
        &ctx.investor,
        &ctx.investor,
    );
    // Investor owns 100% of supply → gets ~all assets (virtual share dust).
    assert!(paid >= 1_199, "paid={paid}");
    assert_eq!(ctx.vault().withdrawal_queue_len(), 0);
    // Cap bookkeeping must be cleared after pulling principal + yield.
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 0);
}

#[test]
fn allocation_tracks_partial_and_over_deallocate() {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    ctx.vault()
        .set_liquidity_adapter(&ctx.allocator, &None, &Bytes::new(&ctx.env));

    ctx.mint_underlying(&ctx.investor, 1_000);
    ctx.vault()
        .deposit(&1_000, &ctx.investor, &ctx.investor, &ctx.investor);
    ctx.vault().allocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &1_000,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 1_000);

    // Partial principal pull.
    ctx.vault().deallocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &400,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 600);
    assert_eq!(ctx.adapter().real_assets(), 600);

    // Yield on remaining principal, then pull everything (assets > allocation).
    let donor = Address::generate(&ctx.env);
    ctx.mint_underlying(&donor, 150);
    ctx.adapter().inject(&donor, &150);
    assert_eq!(ctx.adapter().real_assets(), 750);
    ctx.vault().deallocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &750,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 0);
    assert_eq!(ctx.adapter().real_assets(), 0);

    // Fresh allocate still works after a zeroed book.
    ctx.vault().allocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &500,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 500);
}

#[test]
fn deallocate_yield_after_allocation_already_zero() {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    ctx.vault()
        .set_liquidity_adapter(&ctx.allocator, &None, &Bytes::new(&ctx.env));

    ctx.mint_underlying(&ctx.investor, 500);
    ctx.vault()
        .deposit(&500, &ctx.investor, &ctx.investor, &ctx.investor);
    ctx.vault().allocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &500,
    );
    ctx.vault().deallocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &500,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 0);

    // Inject with no allocation, then pull — must not panic (old ZeroAllocation).
    let donor = Address::generate(&ctx.env);
    ctx.mint_underlying(&donor, 80);
    ctx.adapter().inject(&donor, &80);
    ctx.vault().deallocate(
        &ctx.allocator,
        &ctx.adapter,
        &Bytes::new(&ctx.env),
        &80,
    );
    assert_eq!(ctx.vault().allocation(&ctx.adapter), 0);
    assert_eq!(ctx.adapter().real_assets(), 0);
}

// ─── fees / timelock (unique current) ──────────────────────────────────────

#[test]
fn set_fees_via_timelock() {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    let pens = Vec::new(&ctx.env);
    ctx.submit_exec(PendingOp::SetFees(
        ctx.fee_receiver.clone(),
        100,
        500,
        pens.clone(),
    ));
    let f = ctx.vault().fees();
    assert_eq!(f.management_bps, 100);
    assert_eq!(f.performance_bps, 500);
}

#[test]
fn revoke_pending_set_fees() {
    let ctx = deploy(0, None);
    ctx.bootstrap_full();
    // Raise SetFees delay, then submit and revoke before eta.
    ctx.submit_exec(PendingOp::IncreaseTimelock(TimelockOpKind::SetFees, 100));
    let pens = Vec::new(&ctx.env);
    let op = PendingOp::SetFees(ctx.fee_receiver.clone(), 100, 0, pens);
    ctx.vault().submit(&op);
    assert!(ctx.vault().executable_at(&op).is_some());
    ctx.vault().revoke(&ctx.sentinel, &op);
    assert!(ctx.vault().executable_at(&op).is_none());
}

#[test]
#[should_panic]
fn timelock_delay_blocks_execute_until_eta() {
    let ctx = deploy(1_000, None);
    let v = ctx.vault();
    v.set_curator(&ctx.curator);
    let op = PendingOp::SetIsAllocator(ctx.allocator.clone(), true);
    v.submit(&op);
    // Immediate execute should fail (NotReady).
    v.set_is_allocator(&ctx.allocator, &true);
}

#[test]
fn timelock_execute_after_eta() {
    let ctx = deploy(1_000, None);
    let v = ctx.vault();
    v.set_curator(&ctx.curator);
    let op = PendingOp::SetIsAllocator(ctx.allocator.clone(), true);
    v.submit(&op);
    let eta = v.executable_at(&op).unwrap();
    ctx.env.ledger().set_timestamp(eta);
    v.set_is_allocator(&ctx.allocator, &true);
    assert!(v.is_allocator(&ctx.allocator));
}

#[test]
fn deposit_from_distinct_funder() {
    let ctx = deploy_ready();
    let funder = Address::generate(&ctx.env);
    ctx.mint_underlying(&funder, 2_000);
    let shares = ctx
        .vault()
        .deposit(&2_000, &ctx.investor, &funder, &ctx.investor);
    assert_eq!(shares, ctx.vault().balance(&ctx.investor));
    assert!(shares > 2_000, "virtual shares inflate the share count");
    assert_eq!(ctx.token().balance(&funder), 0);
}

#[test]
fn remove_liquidity_adapter_clears_pointer() {
    let ctx = deploy_ready();
    assert_eq!(ctx.vault().liquidity_adapter(), Some(ctx.adapter.clone()));
    ctx.submit_exec(PendingOp::RemoveAdapter(ctx.adapter.clone()));
    assert!(!ctx.vault().is_adapter(&ctx.adapter));
    assert!(ctx.vault().liquidity_adapter().is_none());
}

#[test]
fn cannot_remove_adapter_while_it_holds_assets() {
    let c = Ctx::with_liquidity_adapter();
    c.deposit(&c.alice, 100 * ONE);
    assert!(c.adapter.real_assets() > 0);
    c.vault.submit(&PendingOp::RemoveAdapter(c.adapter.address.clone()));
    assert!(c.vault.try_remove_adapter(&c.adapter.address).is_err());
    c.deallocate(&c.adapter.address, c.adapter.real_assets());
    assert_eq!(c.adapter.real_assets(), 0);
    c.vault.remove_adapter(&c.adapter.address);
    assert!(!c.vault.is_adapter(&c.adapter.address));
}

#[test]
fn cannot_add_adapter_that_already_holds_assets() {
    let c = Ctx::new();
    let other = c.new_adapter();
    c.sac.mint(&c.alice, &(50 * ONE));
    other.inject(&c.alice, &(50 * ONE));
    assert!(other.real_assets() > 0);
    c.vault.submit(&PendingOp::AddAdapter(other.address.clone()));
    assert!(c.vault.try_add_adapter(&other.address).is_err());
    // Empty it, then add succeeds.
    other.skim(&c.bob, &other.real_assets());
    assert_eq!(other.real_assets(), 0);
    c.vault.add_adapter(&other.address);
    assert!(c.vault.is_adapter(&other.address));
}
