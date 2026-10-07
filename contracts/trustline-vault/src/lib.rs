#![no_std]

// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Trustline-protected SEP-56 tokenized vault with yield adapters
//! and curator fees ([`fees`] module).
//!
//! - Investor entrypoints are Trustline-gated.
//! - Roles: Owner / Curator / Allocator / Sentinel (no inheritance).
//! - Curator timelock ([`timelock`]): per-op delays, always
//!   `submit` (Trustline) then execute; `default_timelock` at deploy.
//! - Fees ([`fees`]): accounting module; parameter changes via curator timelock
//!   `SetFees` (no dedicated fee delay).
//! - Caps ([`caps`]): absolute / relative per **adapter**.
//! - Withdrawals ([`queue`]): optional FIFO layer — plug sites marked `QUEUE` in this
//!   file (`QueueHost` impl, init, entrypoints, `withdraw`/`redeem`, deposit gate).
//! - Allocator ops and Sentinel `deallocate` / `revoke` / cap decreases are immediate.

mod adapter;
mod caps;
mod fees;
mod queue;
mod timelock;

use adapter::YieldAdapterClient;
use fees::{FeeHost, FeeLib, Fees, Sale, VaultFees};
use queue::{QueueHost, WithdrawRequest};
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contractevent, contractimpl, contracttype, symbol_short, token, vec, Address, Bytes,
    Env, IntoVal, String, Vec,
};
use stellar_contract_utils::math::Rounding;
use timelock::{PendingOp, TimelockOpKind};
use trustline_sdk::{
    require_trustline, require_trustline_addrs, set_validation_engine,
    validation_engine as read_validation_engine,
};

/// Virtual shares offset used to mitigate donation attacks.
/// Tests mirror this constant — keep in sync when changing.
pub(crate) const VIRTUAL_SHARES: i128 = 1_000_000;

/// ~7 days of ledgers (5s). Threshold / extend margin for instance + balance TTL.
const WEEK_OF_LEDGERS: u32 = 120_960;

#[contract]
pub struct TrustlineVault;

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Asset,
    Owner,
    Curator,
    Allocators,
    IsAllocator(Address),
    Decimals,
    Name,
    Symbol,
    TotalSupply,
    Balance(Address),
    Allowance(Address, Address),
    Adapters,
    IsAdapter(Address),
    LiquidityAdapter,
    LiquidityData,
}

#[contractevent]
pub struct Deposit {
    #[topic]
    pub operator: Address,
    #[topic]
    pub from: Address,
    #[topic]
    pub receiver: Address,
    pub assets: i128,
    pub shares: i128,
}

#[contractevent]
pub struct Withdraw {
    #[topic]
    pub operator: Address,
    #[topic]
    pub receiver: Address,
    #[topic]
    pub owner: Address,
    pub assets: i128,
    pub shares: i128,
}

/// Fee library host (not on the contract type — keeps names apart).
pub struct Host;

impl FeeHost for Host {
    fn total_assets(e: &Env) -> i128 {
        TrustlineVault::total_assets(e.clone())
    }
    fn total_supply(e: &Env) -> i128 {
        TrustlineVault::total_supply(e.clone())
    }
    fn virtual_shares(_e: &Env) -> i128 {
        VIRTUAL_SHARES
    }
    fn balance(e: &Env, holder: &Address) -> i128 {
        TrustlineVault::balance(e.clone(), holder.clone())
    }
    fn mint_shares(e: &Env, to: &Address, shares: i128) {
        TrustlineVault::mint_shares(e, to, shares);
    }
    fn pay_assets(e: &Env, to: &Address, assets: i128) {
        let asset = TrustlineVault::query_asset(e.clone());
        let self_addr = e.current_contract_address();
        token::Client::new(e, &asset).transfer(&self_addr, to, &assets);
    }
}

// ── QUEUE plug (comment this impl + queue call sites below to disable) ───────
impl QueueHost for Host {
    fn authorize_exit(e: &Env, owner: &Address, operator: &Address, shares: i128) {
        TrustlineVault::authorize_exit(e, owner, operator, shares);
    }
    fn escrow_shares(e: &Env, owner: &Address, shares: i128) {
        let self_addr = e.current_contract_address();
        TrustlineVault::move_shares(e, owner, &self_addr, shares);
    }
    fn release_escrow_shares(e: &Env, owner: &Address, shares: i128) {
        let self_addr = e.current_contract_address();
        TrustlineVault::move_shares(e, &self_addr, owner, shares);
    }
    fn available_liquidity(e: &Env) -> i128 {
        TrustlineVault::available_liquidity_inner(e)
    }
    fn settle_escrowed_exit(
        e: &Env,
        owner: &Address,
        receiver: &Address,
        shares: i128,
        sale: &Sale,
    ) {
        // Shares already escrowed on the vault — burn from self, no re-auth.
        TrustlineVault::pay_exit(
            e,
            owner,
            receiver,
            owner,
            shares,
            sale,
            &e.current_contract_address(),
        );
    }
}
// ── end QUEUE plug ─────────────────────────────────────────────────────────

#[contractimpl(contracttrait)]
impl VaultFees for TrustlineVault {
    type Host = Host;

    /// Accrue the management fee due as new shares. Anyone may call this.
    fn accrue(e: &Env) {
        FeeLib::accrue::<Host>(e);
        bump_instance(e);
    }
}

#[contractimpl]
impl TrustlineVault {
    /// Deploy a vault bound to `validation_engine` and underlying `asset`.
    ///
    /// `owner` is the only initial role. `decimals` (at most 18), `name`, and `symbol` describe
    /// the share token; deployments conventionally use the asset's decimals plus six. Initial
    /// `fees` apply immediately, and `default_timelock` (at most 365 days) seeds every operation
    /// bucket, including `SetFees`.
    pub fn __constructor(
        env: Env,
        validation_engine: Address,
        asset: Address,
        owner: Address,
        decimals: u32,
        name: String,
        symbol: String,
        fees: Fees,
        default_timelock: u64,
    ) {
        assert!(decimals <= 18, "decimals");
        set_validation_engine(&env, &validation_engine);
        env.storage().instance().set(&DataKey::Asset, &asset);
        env.storage().instance().set(&DataKey::Owner, &owner);
        env.storage().instance().set(&DataKey::Decimals, &decimals);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage().instance().set(&DataKey::TotalSupply, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::Adapters, &Vec::<Address>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::Allocators, &Vec::<Address>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::LiquidityData, &Bytes::new(&env));
        FeeLib::init(&env, &fees);
        timelock::init(&env, default_timelock);
        queue::init(&env); // QUEUE: comment to disable
        bump_instance(&env);
    }

    // ─── reads ─────────────────────────────────────────────────────────────

    /// Return the Validation Engine used to check Trustline approvals.
    pub fn validation_engine(env: Env) -> Address {
        read_validation_engine(&env)
    }

    /// Return the owner, who appoints the curator and sentinels.
    pub fn owner(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Owner).unwrap()
    }

    /// Return the curator, or `None` until the owner appoints one.
    pub fn curator(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Curator)
    }

    /// Return all addresses currently allowed to allocate vault assets.
    pub fn allocators(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Allocators)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Return whether `who` is an allocator.
    pub fn is_allocator(env: Env, who: Address) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsAllocator(who))
            .unwrap_or(false)
    }

    /// Return all sentinels, who may revoke operations, lower caps, and deallocate.
    pub fn sentinels(env: Env) -> Vec<Address> {
        timelock::sentinels(&env)
    }

    /// Return whether `who` is a sentinel.
    pub fn is_sentinel(env: Env, who: Address) -> bool {
        timelock::is_sentinel(&env, &who)
    }

    /// Return the initial delay, in seconds, assigned to each timelock bucket.
    pub fn default_timelock(env: Env) -> u64 {
        timelock::default_delay(&env)
    }

    /// Return the delay for operations of `kind`, in seconds.
    pub fn timelock(env: Env, kind: TimelockOpKind) -> u64 {
        timelock::op_delay(&env, &kind)
    }

    /// Return whether operations of `kind` have been permanently disabled.
    pub fn abdicated(env: Env, kind: TimelockOpKind) -> bool {
        timelock::is_abdicated(&env, &kind)
    }

    /// Return the Unix timestamp when `op` becomes executable, or `None` if not pending.
    pub fn executable_at(env: Env, op: PendingOp) -> Option<u64> {
        timelock::executable_at(&env, &op)
    }

    /// Return assets allocated to `adapter`, in underlying asset units.
    pub fn allocation(env: Env, adapter: Address) -> i128 {
        caps::allocation(&env, &adapter)
    }

    /// Return the adapter's absolute allocation cap, in underlying asset units.
    pub fn absolute_cap(env: Env, adapter: Address) -> i128 {
        caps::absolute_cap(&env, &adapter)
    }

    /// Return the adapter's relative cap in WAD units (`1e18` = 100%).
    pub fn relative_cap(env: Env, adapter: Address) -> i128 {
        caps::relative_cap(&env, &adapter)
    }

    /// Return the underlying SEP-41 asset contract.
    pub fn query_asset(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Asset).unwrap()
    }

    /// Return the number of decimals used by the vault share token.
    pub fn decimals(env: Env) -> u32 {
        env.storage().instance().get(&DataKey::Decimals).unwrap()
    }

    /// Return the vault share token name.
    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    /// Return the vault share token symbol.
    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    /// Return the issued share supply, in share units.
    pub fn total_supply(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    /// Return `id`'s share balance, in share units.
    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(id))
            .unwrap_or(0)
    }

    /// Return the shares `spender` may transfer from `from`.
    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        env.storage()
            .temporary()
            .get(&DataKey::Allowance(from, spender))
            .unwrap_or(0)
    }

    /// Return all registered yield adapters.
    pub fn adapters(env: Env) -> Vec<Address> {
        env.storage().instance().get(&DataKey::Adapters).unwrap()
    }

    /// Return whether `adapter` is registered with this vault.
    pub fn is_adapter(env: Env, adapter: Address) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsAdapter(adapter))
            .unwrap_or(false)
    }

    /// Return the adapter used for automatic deposits and withdrawal liquidity.
    pub fn liquidity_adapter(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::LiquidityAdapter)
    }

    /// Return the opaque data passed to the liquidity adapter.
    pub fn liquidity_data(env: Env) -> Bytes {
        env.storage()
            .instance()
            .get(&DataKey::LiquidityData)
            .unwrap_or_else(|| Bytes::new(&env))
    }

    /// Return idle underlying held by the vault, in asset units.
    pub fn idle_assets(env: Env) -> i128 {
        let asset = Self::query_asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).balance(&self_addr)
    }

    /// Return idle assets plus every adapter's `real_assets`, in asset units.
    pub fn total_assets(env: Env) -> i128 {
        let mut total = Self::idle_assets(env.clone());
        let adapters: Vec<Address> = Self::adapters(env.clone());
        for i in 0..adapters.len() {
            let a = adapters.get(i).unwrap();
            total = total
                .checked_add(YieldAdapterClient::new(&env, &a).real_assets())
                .expect("overflow");
        }
        total
    }

    /// Idle + liquidity-adapter `liquid_assets` (what `ensure_idle` / the queue can pull).
    /// Uses liquid — not `real_assets` — so locked / working capital does not fake solvency.
    pub fn available_liquidity(env: Env) -> i128 {
        Self::available_liquidity_inner(&env)
    }

    // ── QUEUE plug (comment these entrypoints out with the QueueHost impl) ──
    /// Return queued withdrawals in FIFO order.
    pub fn withdrawal_queue(env: Env) -> Vec<WithdrawRequest> {
        queue::queue(&env)
    }

    /// Return the number of queued withdrawals.
    pub fn withdrawal_queue_len(env: Env) -> u32 {
        queue::len(&env)
    }

    /// Return the optional manager allowed to cancel any queued withdrawal.
    pub fn queue_manager(env: Env) -> Option<Address> {
        queue::manager(&env)
    }

    /// Curator: set or clear the queue manager. Requires Trustline approval.
    pub fn set_queue_manager(env: Env, manager: Option<Address>) {
        let curator = Self::require_curator(&env);
        require_trustline!(env, curator, 0, "set_queue_manager"(manager));
        queue::set_manager(&env, manager);
        bump_instance(&env);
    }

    /// Request owner **or** queue manager: leave the withdrawal queue.
    /// Restores escrowed shares and cost basis to the **request owner**.
    /// No Trustline (undo of a prior gated exit). Immediate.
    pub fn cancel_withdrawal(env: Env, caller: Address, id: u64) {
        caller.require_auth();
        queue::cancel::<Host>(&env, &caller, id);
        bump_instance(&env);
    }
    // ── end QUEUE plug ─────────────────────────────────────────────────────

    // ─── Conversions / previews (after management fee due) ─────────────────

    /// Convert `assets` to shares at the current price, rounded down.
    pub fn convert_to_shares(env: Env, assets: i128) -> i128 {
        FeeLib::to_shares::<Host>(&env, assets, Rounding::Floor)
    }

    /// Convert `shares` to underlying assets at the current price, rounded down.
    pub fn convert_to_assets(env: Env, shares: i128) -> i128 {
        FeeLib::to_assets::<Host>(&env, shares, Rounding::Floor)
    }

    /// Return the maximum assets `receiver` may deposit; currently unlimited.
    pub fn max_deposit(_env: Env, _receiver: Address) -> i128 {
        i128::MAX
    }

    /// Preview shares minted for an asset deposit, rounded down.
    pub fn preview_deposit(env: Env, assets: i128) -> i128 {
        FeeLib::to_shares::<Host>(&env, assets, Rounding::Floor)
    }

    /// Return the maximum shares `receiver` may mint; currently unlimited.
    pub fn max_mint(_env: Env, _receiver: Address) -> i128 {
        i128::MAX
    }

    /// Preview assets required to mint `shares`, rounded up.
    pub fn preview_mint(env: Env, shares: i128) -> i128 {
        FeeLib::to_assets::<Host>(&env, shares, Rounding::Ceil)
    }

    /// Return the assets `owner` can receive by selling all shares, net of fees.
    pub fn max_withdraw(env: Env, owner: Address) -> i128 {
        let bal = Self::balance(env.clone(), owner.clone());
        FeeLib::sale::<Host>(&env, Some(&owner), bal).paid
    }

    /// Preview shares needed for `assets`; holder-specific performance fees are excluded.
    pub fn preview_withdraw(env: Env, assets: i128) -> i128 {
        FeeLib::shares_for::<Host>(&env, None, assets).expect("preview_withdraw")
    }

    /// Return the shares `owner` may redeem; liquidity shortages queue the request.
    pub fn max_redeem(env: Env, owner: Address) -> i128 {
        Self::balance(env, owner)
    }

    /// Preview assets paid for `shares`; holder-specific performance fees are excluded.
    pub fn preview_redeem(env: Env, shares: i128) -> i128 {
        FeeLib::sale::<Host>(&env, None, shares).paid
    }

    // ─── SEP-41 share token ────────────────────────────────────────────────

    /// Authorize `spender` to transfer `amount` shares from `from`.
    /// The allowance uses temporary storage; `expiration_ledger` is currently ignored.
    pub fn approve(env: Env, from: Address, spender: Address, amount: i128, _expiration_ledger: u32) {
        from.require_auth();
        assert!(amount >= 0, "amount");
        env.storage()
            .temporary()
            .set(&DataKey::Allowance(from.clone(), spender.clone()), &amount);
        let max = env.storage().max_ttl();
        env.storage().temporary().extend_ttl(
            &DataKey::Allowance(from, spender),
            max.saturating_sub(WEEK_OF_LEDGERS),
            max,
        );
        bump_instance(&env);
    }

    /// Transfer `amount` shares from `from` to `to`. Requires `from` authorization.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        FeeLib::record_transfer::<Host>(&env, &from, &to, amount);
        Self::move_shares(&env, &from, &to, amount);
        bump_instance(&env);
    }

    /// Transfer `amount` approved shares from `from` to `to`. Requires `spender` authorization.
    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        let key = DataKey::Allowance(from.clone(), spender.clone());
        let allowed: i128 = env.storage().temporary().get(&key).unwrap_or(0);
        assert!(allowed >= amount, "allowance");
        env.storage()
            .temporary()
            .set(&key, &(allowed - amount));
        FeeLib::record_transfer::<Host>(&env, &from, &to, amount);
        Self::move_shares(&env, &from, &to, amount);
        bump_instance(&env);
    }

    // ─── SEP-56 investor entrypoints (Trustline + fee hooks) ───────────────

    /// Deposit `assets` from `from` and mint shares to `receiver`.
    /// Requires `operator` authorization and Trustline approval for the asset amount.
    pub fn deposit(
        env: Env,
        assets: i128,
        receiver: Address,
        from: Address,
        operator: Address,
    ) -> i128 {
        operator.require_auth();
        assert!(assets > 0, "assets");
        let asset = Self::query_asset(env.clone());
        require_trustline_addrs!(
            env,
            operator,
            assets,
            "deposit"(assets, receiver, from, operator),
            [receiver, from, asset],
        );
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);
        let shares = Self::preview_deposit(env.clone(), assets);
        assert!(shares > 0, "zero shares");
        FeeLib::record_purchase(&env, &receiver, assets);
        Self::deposit_internal(&env, assets, shares, &receiver, &from, &operator);
        shares
    }

    /// Mint exactly `shares` to `receiver`, pulling the required assets from `from`.
    /// Requires `operator` authorization and Trustline approval for the share amount.
    pub fn mint(
        env: Env,
        shares: i128,
        receiver: Address,
        from: Address,
        operator: Address,
    ) -> i128 {
        operator.require_auth();
        assert!(shares > 0, "shares");
        let asset = Self::query_asset(env.clone());
        require_trustline_addrs!(
            env,
            operator,
            shares,
            "mint"(shares, receiver, from, operator),
            [receiver, from, asset],
        );
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);
        let assets = Self::preview_mint(env.clone(), shares);
        assert!(assets > 0, "zero assets");
        FeeLib::record_purchase(&env, &receiver, assets);
        Self::deposit_internal(&env, assets, shares, &receiver, &from, &operator);
        assets
    }

    /// Withdraw exactly `assets`; settle when liquid or enqueue in FIFO order.
    /// Requires `operator` authorization and Trustline approval; returns shares consumed or escrowed.
    pub fn withdraw(
        env: Env,
        assets: i128,
        receiver: Address,
        owner: Address,
        operator: Address,
    ) -> i128 {
        operator.require_auth();
        assert!(assets > 0, "assets");
        let asset = Self::query_asset(env.clone());
        require_trustline_addrs!(
            env,
            operator,
            assets,
            "withdraw"(assets, receiver, owner, operator),
            [receiver, owner, asset],
        );
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);

        // QUEUE ON — to disable: comment the next line and uncomment the DIRECT block.
        return queue::withdraw_assets::<Host>(&env, assets, &receiver, &owner, &operator);
        /*
        // DIRECT (queue off):
        let (shares, sale) = FeeLib::withdrawal::<Host>(&env, &owner, assets)
            .expect("exceeded max withdraw");
        Self::exit(&env, shares, &sale, &receiver, &owner, &operator);
        shares
        */
    }

    /// Redeem exactly `shares`; settle when liquid or enqueue in FIFO order.
    /// Requires `operator` authorization and Trustline approval; returns assets paid immediately.
    pub fn redeem(
        env: Env,
        shares: i128,
        receiver: Address,
        owner: Address,
        operator: Address,
    ) -> i128 {
        operator.require_auth();
        assert!(shares > 0, "shares");
        let asset = Self::query_asset(env.clone());
        require_trustline_addrs!(
            env,
            operator,
            shares,
            "redeem"(shares, receiver, owner, operator),
            [receiver, owner, asset],
        );
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);

        // QUEUE ON — to disable: comment the next line and uncomment the DIRECT block.
        return queue::redeem_shares::<Host>(&env, shares, &receiver, &owner, &operator);
        /*
        // DIRECT (queue off):
        assert!(
            shares <= Self::balance(env.clone(), owner.clone()),
            "exceeded max redeem"
        );
        let sale = FeeLib::sale::<Host>(&env, Some(&owner), shares);
        Self::exit(&env, shares, &sale, &receiver, &owner, &operator);
        sale.paid
        */
    }

    // QUEUE: comment this entrypoint out when disabling the queue layer.
    /// Settle up to `max` queued withdrawals FIFO (`0` means no limit). Anyone may call.
    /// Stops at the first request that cannot be paid and returns the number settled.
    pub fn process_withdrawals(env: Env, max: u32) -> u32 {
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);
        queue::process::<Host>(&env, None, max).0
    }

    // ─── Roles / timelock (Trustline on Owner/Curator paths) ───────────────

    /// Owner: transfer vault ownership. Requires Trustline approval.
    pub fn set_owner(env: Env, new_owner: Address) {
        let owner = Self::require_owner(&env);
        require_trustline!(env, owner, 0, "set_owner"(new_owner));
        env.storage().instance().set(&DataKey::Owner, &new_owner);
        bump_instance(&env);
    }

    /// Owner: appoint the curator. Requires Trustline approval.
    pub fn set_curator(env: Env, new_curator: Address) {
        let owner = Self::require_owner(&env);
        require_trustline!(env, owner, 0, "set_curator"(new_curator));
        env.storage().instance().set(&DataKey::Curator, &new_curator);
        bump_instance(&env);
    }

    /// Owner: add or remove a sentinel immediately. Requires Trustline approval.
    pub fn set_is_sentinel(env: Env, who: Address, is_sentinel: bool) {
        let owner = Self::require_owner(&env);
        require_trustline!(
            env,
            owner,
            0,
            "set_is_sentinel"(who, is_sentinel)
        );
        timelock::set_is_sentinel(&env, who, is_sentinel);
        bump_instance(&env);
    }

    /// Curator: queue a timelocked operation. Requires Trustline approval.
    /// Submission is required even when the operation's delay is zero.
    pub fn submit(env: Env, op: PendingOp) {
        let curator = Self::require_curator(&env);
        require_trustline!(env, curator, 0, "submit"(op));
        timelock::submit(&env, op);
        bump_instance(&env);
    }

    /// Curator or Sentinel: revoke a pending operation immediately. No Trustline check.
    pub fn revoke(env: Env, actor: Address, op: PendingOp) {
        actor.require_auth();
        let ok = Self::curator(env.clone()) == Some(actor.clone())
            || timelock::is_sentinel(&env, &actor);
        assert!(ok, "not curator/sentinel");
        timelock::revoke(&env, op);
        bump_instance(&env);
    }

    /// Execute a submitted allocator-role change once due. Anyone may call; no new Trustline check.
    pub fn set_is_allocator(env: Env, who: Address, is_allocator: bool) {
        let op = PendingOp::SetIsAllocator(who.clone(), is_allocator);
        timelock::begin_execute(&env, &op);
        Self::set_is_allocator_inner(&env, who, is_allocator);
        bump_instance(&env);
    }

    /// Execute a submitted `AddAdapter` operation once due.
    /// The adapter must target this vault and asset and report zero assets.
    pub fn add_adapter(env: Env, adapter: Address) {
        let op = PendingOp::AddAdapter(adapter.clone());
        timelock::begin_execute(&env, &op);
        Self::add_adapter_inner(&env, adapter);
        bump_instance(&env);
    }

    /// Execute a submitted `RemoveAdapter` operation once due.
    /// The adapter must hold no assets or recorded allocation.
    pub fn remove_adapter(env: Env, adapter: Address) {
        let op = PendingOp::RemoveAdapter(adapter.clone());
        timelock::begin_execute(&env, &op);
        Self::remove_adapter_inner(&env, adapter);
        bump_instance(&env);
    }

    /// Execute a submitted increase of `kind`'s delay, in seconds.
    pub fn increase_timelock(env: Env, kind: TimelockOpKind, new_duration: u64) {
        let op = PendingOp::IncreaseTimelock(kind.clone(), new_duration);
        timelock::begin_execute(&env, &op);
        timelock::apply_increase(&env, kind, new_duration);
        bump_instance(&env);
    }

    /// Execute a submitted decrease of `kind`'s delay, in seconds.
    pub fn decrease_timelock(env: Env, kind: TimelockOpKind, new_duration: u64) {
        let op = PendingOp::DecreaseTimelock(kind.clone(), new_duration);
        timelock::begin_execute(&env, &op);
        timelock::apply_decrease(&env, kind, new_duration);
        bump_instance(&env);
    }

    /// Execute a submitted abdication, permanently disabling operations of `kind`.
    pub fn abdicate(env: Env, kind: TimelockOpKind) {
        let op = PendingOp::Abdicate(kind.clone());
        timelock::begin_execute(&env, &op);
        timelock::apply_abdicate(&env, kind);
        bump_instance(&env);
    }

    /// Execute a submitted absolute-cap increase, in underlying asset units.
    pub fn increase_absolute_cap(env: Env, adapter: Address, new_absolute_cap: i128) {
        let op = PendingOp::IncreaseAbsoluteCap(adapter.clone(), new_absolute_cap);
        timelock::begin_execute(&env, &op);
        caps::increase_absolute(&env, adapter, new_absolute_cap);
        bump_instance(&env);
    }

    /// Execute a submitted relative-cap increase in WAD units (`1e18` = 100%).
    pub fn increase_relative_cap(env: Env, adapter: Address, new_relative_cap: i128) {
        let op = PendingOp::IncreaseRelativeCap(adapter.clone(), new_relative_cap);
        timelock::begin_execute(&env, &op);
        caps::increase_relative(&env, adapter, new_relative_cap);
        bump_instance(&env);
    }

    /// Curator or Sentinel: lower an absolute cap immediately, in asset units. No Trustline check.
    pub fn decrease_absolute_cap(
        env: Env,
        actor: Address,
        adapter: Address,
        new_absolute_cap: i128,
    ) {
        actor.require_auth();
        let ok = Self::curator(env.clone()) == Some(actor.clone())
            || timelock::is_sentinel(&env, &actor);
        assert!(ok, "not curator/sentinel");
        caps::decrease_absolute(&env, adapter, new_absolute_cap);
        bump_instance(&env);
    }

    /// Curator or Sentinel: lower a relative cap immediately in WAD units. No Trustline check.
    pub fn decrease_relative_cap(
        env: Env,
        actor: Address,
        adapter: Address,
        new_relative_cap: i128,
    ) {
        actor.require_auth();
        let ok = Self::curator(env.clone()) == Some(actor.clone())
            || timelock::is_sentinel(&env, &actor);
        assert!(ok, "not curator/sentinel");
        caps::decrease_relative(&env, adapter, new_relative_cap);
        bump_instance(&env);
    }

    /// Execute a submitted fee change once due. Anyone may call; rates are basis points.
    pub fn set_fees(
        env: Env,
        receiver: Address,
        management_bps: u32,
        performance_bps: u32,
        penalties: Vec<(i128, u32)>,
    ) {
        let op = PendingOp::SetFees(
            receiver.clone(),
            management_bps,
            performance_bps,
            penalties.clone(),
        );
        timelock::begin_execute(&env, &op);
        FeeLib::set_fees::<Host>(
            &env,
            Fees {
                receiver,
                management_bps,
                performance_bps,
                penalties,
            },
        );
        bump_instance(&env);
    }

    /// Allocator: select or clear the adapter used for deposits and withdrawal liquidity.
    /// Requires Trustline approval; `data` is passed unchanged to the adapter.
    pub fn set_liquidity_adapter(
        env: Env,
        allocator: Address,
        adapter: Option<Address>,
        data: Bytes,
    ) {
        Self::require_allocator(&env, &allocator);
        require_trustline!(
            env,
            allocator,
            0,
            "set_liquidity_adapter"(allocator, adapter, data)
        );
        match adapter {
            Some(a) => {
                assert!(Self::is_adapter(env.clone(), a.clone()), "not registered");
                env.storage().instance().set(&DataKey::LiquidityAdapter, &a);
                env.storage().instance().set(&DataKey::LiquidityData, &data);
            }
            None => {
                env.storage().instance().remove(&DataKey::LiquidityAdapter);
                env.storage()
                    .instance()
                    .set(&DataKey::LiquidityData, &Bytes::new(&env));
            }
        }
        bump_instance(&env);
    }

    /// Allocator: move `assets` from the vault into a registered adapter.
    /// Requires Trustline approval and enforces the adapter's absolute and relative caps.
    pub fn allocate(
        env: Env,
        allocator: Address,
        adapter: Address,
        data: Bytes,
        assets: i128,
    ) {
        Self::require_allocator(&env, &allocator);
        require_trustline!(
            env,
            allocator,
            assets,
            "allocate"(allocator, adapter, data, assets)
        );
        assert!(assets > 0, "assets");
        assert!(Self::is_adapter(env.clone(), adapter.clone()), "not registered");
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);
        Self::allocate_internal(&env, &adapter, &data, assets);
    }

    /// Allocator or Sentinel: return `assets` from a registered adapter to the vault.
    /// Requires Trustline approval; Sentinels may call it to reduce risk.
    pub fn deallocate(
        env: Env,
        actor: Address,
        adapter: Address,
        data: Bytes,
        assets: i128,
    ) {
        actor.require_auth();
        let is_alloc = Self::is_allocator(env.clone(), actor.clone());
        let is_sent = timelock::is_sentinel(&env, &actor);
        assert!(is_alloc || is_sent, "not allocator/sentinel");
        require_trustline!(
            env,
            actor,
            assets,
            "deallocate"(actor, adapter, data, assets)
        );
        assert!(assets > 0, "assets");
        assert!(Self::is_adapter(env.clone(), adapter.clone()), "not registered");
        FeeLib::accrue::<Host>(&env);
        bump_instance(&env);
        Self::deallocate_internal(&env, &adapter, &data, assets);
    }

    // ─── internals ─────────────────────────────────────────────────────────

    fn require_owner(env: &Env) -> Address {
        let owner: Address = env.storage().instance().get(&DataKey::Owner).unwrap();
        owner.require_auth();
        owner
    }

    fn require_curator(env: &Env) -> Address {
        let curator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Curator)
            .expect("no curator");
        curator.require_auth();
        curator
    }

    fn require_allocator(env: &Env, who: &Address) {
        who.require_auth();
        assert!(
            Self::is_allocator(env.clone(), who.clone()),
            "not allocator"
        );
    }

    fn set_is_allocator_inner(env: &Env, who: Address, is_allocator: bool) {
        let currently = Self::is_allocator(env.clone(), who.clone());
        if currently == is_allocator {
            return;
        }
        env.storage()
            .instance()
            .set(&DataKey::IsAllocator(who.clone()), &is_allocator);
        let list: Vec<Address> = Self::allocators(env.clone());
        if is_allocator {
            let mut next = list;
            next.push_back(who);
            env.storage().instance().set(&DataKey::Allocators, &next);
        } else {
            let mut next = Vec::<Address>::new(env);
            for i in 0..list.len() {
                let a = list.get(i).unwrap();
                if a != who {
                    next.push_back(a);
                }
            }
            env.storage().instance().set(&DataKey::Allocators, &next);
        }
    }

    fn add_adapter_inner(env: &Env, adapter: Address) {
        if Self::is_adapter(env.clone(), adapter.clone()) {
            return;
        }
        let client = YieldAdapterClient::new(env, &adapter);
        assert!(client.vault() == env.current_contract_address(), "vault mismatch");
        assert!(client.asset() == Self::query_asset(env.clone()), "asset mismatch");
        // Prevent share-price inflation: a funded adapter must not enter `total_assets`.
        assert!(client.real_assets() == 0, "adapter not empty");
        let mut adapters: Vec<Address> = Self::adapters(env.clone());
        adapters.push_back(adapter.clone());
        env.storage().instance().set(&DataKey::Adapters, &adapters);
        env.storage()
            .instance()
            .set(&DataKey::IsAdapter(adapter), &true);
    }

    fn remove_adapter_inner(env: &Env, adapter: Address) {
        if !Self::is_adapter(env.clone(), adapter.clone()) {
            return;
        }
        let client = YieldAdapterClient::new(env, &adapter);
        // Prevent share-price collapse: assets would drop out of `total_assets` while
        // still sitting in the adapter.
        assert!(client.real_assets() == 0, "adapter not empty");
        assert!(
            caps::allocation(env, &adapter) == 0,
            "adapter has allocation"
        );
        if Self::liquidity_adapter(env.clone()) == Some(adapter.clone()) {
            env.storage().instance().remove(&DataKey::LiquidityAdapter);
            env.storage()
                .instance()
                .set(&DataKey::LiquidityData, &Bytes::new(env));
        }
        let adapters: Vec<Address> = Self::adapters(env.clone());
        let mut next = Vec::<Address>::new(env);
        for i in 0..adapters.len() {
            let a = adapters.get(i).unwrap();
            if a != adapter {
                next.push_back(a);
            }
        }
        env.storage().instance().set(&DataKey::Adapters, &next);
        env.storage()
            .instance()
            .set(&DataKey::IsAdapter(adapter), &false);
    }

    fn deposit_internal(
        env: &Env,
        assets: i128,
        shares: i128,
        receiver: &Address,
        from: &Address,
        operator: &Address,
    ) {
        let asset = Self::query_asset(env.clone());
        let self_addr = env.current_contract_address();
        // Entrypoint already required `operator`. When `from` is distinct it must
        // also authorize this vault call (cannot pull third-party funds with only
        // operator auth). SEP-41 `transfer` still checks `from` in the auth tree.
        if from != operator {
            from.require_auth();
        }
        token::Client::new(env, &asset).transfer(from, &self_addr, &assets);

        if let Some(liq) = Self::liquidity_adapter(env.clone()) {
            // QUEUE gate — comment the `if queue::is_empty` line (keep the body) to always
            // auto-allocate when the queue layer is off.
            if queue::is_empty(env) {
                let data = Self::liquidity_data(env.clone());
                Self::allocate_internal(env, &liq, &data, assets);
            }
        }

        Self::mint_shares(env, receiver, shares);
        Deposit {
            operator: operator.clone(),
            from: from.clone(),
            receiver: receiver.clone(),
            assets,
            shares,
        }
        .publish(env);
    }

    fn authorize_exit(env: &Env, owner: &Address, operator: &Address, shares: i128) {
        if operator != owner {
            let key = DataKey::Allowance(owner.clone(), operator.clone());
            let allowed: i128 = env.storage().temporary().get(&key).unwrap_or(0);
            assert!(allowed >= shares, "allowance");
            env.storage().temporary().set(&key, &(allowed - shares));
            return;
        }
        // `operator == owner`: entrypoint already did `operator.require_auth()`.
        // Do not call `owner.require_auth()` again (Auth ExistingValue under mocks;
        // redundant on-chain). Share escrow is internal — no SEP-41 auth needed.
    }

    /// Direct exit (queue off): auth, burn from owner, pay, settle fees.
    /// For the commented DIRECT path in `withdraw` / `redeem`.
    /*
    fn exit(
        env: &Env,
        shares: i128,
        sale: &Sale,
        receiver: &Address,
        owner: &Address,
        operator: &Address,
    ) {
        Self::authorize_exit(env, owner, operator, shares);
        Self::pay_exit(env, owner, receiver, operator, shares, sale, owner);
    }
        */

    /// Shared payout path: ensure liquidity, burn `shares` from `burn_from`, pay, settle.
    fn pay_exit(
        env: &Env,
        owner: &Address,
        receiver: &Address,
        operator: &Address,
        shares: i128,
        sale: &Sale,
        burn_from: &Address,
    ) {
        let need = sale
            .paid
            .checked_add(sale.performance)
            .expect("overflow");
        Self::ensure_idle(env, need);

        Self::burn_shares(env, burn_from, shares);

        let asset = Self::query_asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(env, &asset).transfer(&self_addr, receiver, &sale.paid);

        FeeLib::settle_sale::<Host>(env, owner, shares, sale);

        Withdraw {
            operator: operator.clone(),
            receiver: receiver.clone(),
            owner: owner.clone(),
            assets: sale.paid,
            shares,
        }
        .publish(env);
    }

    fn available_liquidity_inner(env: &Env) -> i128 {
        let mut liq = Self::idle_assets(env.clone());
        if let Some(adapter) = Self::liquidity_adapter(env.clone()) {
            liq = liq
                .checked_add(YieldAdapterClient::new(env, &adapter).liquid_assets())
                .expect("overflow");
        }
        liq
    }

    fn ensure_idle(env: &Env, assets: i128) {
        let idle = Self::idle_assets(env.clone());
        if assets > idle {
            let liq = Self::liquidity_adapter(env.clone()).expect("insufficient liquidity");
            let data = Self::liquidity_data(env.clone());
            let shortfall = assets - idle;
            Self::deallocate_internal(env, &liq, &data, shortfall);
        }
    }

    fn allocate_internal(env: &Env, adapter: &Address, data: &Bytes, assets: i128) {
        let asset = Self::query_asset(env.clone());
        let self_addr = env.current_contract_address();
        // Relative cap uses total assets before this allocate.
        // Must run *before* authorize_as_current_contract — that auth only covers the
        // next cross-contract call (adapter.allocate → token.transfer).
        let total_before = Self::total_assets(env.clone());

        env.authorize_as_current_contract(vec![
            env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: asset,
                    fn_name: symbol_short!("transfer"),
                    args: vec![
                        env,
                        self_addr.into_val(env),
                        adapter.clone().into_val(env),
                        assets.into_val(env),
                    ],
                },
                sub_invocations: vec![env],
            }),
        ]);

        let client = YieldAdapterClient::new(env, adapter);
        let got = client.allocate(data, &assets);
        assert!(got == assets, "partial allocate");
        caps::on_allocate(env, adapter, assets, total_before);
    }

    fn deallocate_internal(env: &Env, adapter: &Address, data: &Bytes, assets: i128) {
        let client = YieldAdapterClient::new(env, adapter);
        let got = client.deallocate(data, &assets);
        assert!(got == assets, "partial deallocate");
        caps::on_deallocate(env, adapter, assets);
    }

    fn mint_shares(env: &Env, to: &Address, amount: i128) {
        let bal = Self::balance(env.clone(), to.clone())
            .checked_add(amount)
            .expect("overflow");
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &bal);
        let supply = Self::total_supply(env.clone())
            .checked_add(amount)
            .expect("overflow");
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &supply);
        bump_balance(env, to);
    }

    fn burn_shares(env: &Env, from: &Address, amount: i128) {
        let bal = Self::balance(env.clone(), from.clone());
        assert!(bal >= amount, "balance");
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(bal - amount));
        let supply = Self::total_supply(env.clone()) - amount;
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &supply);
        bump_balance(env, from);
    }

    fn move_shares(env: &Env, from: &Address, to: &Address, amount: i128) {
        assert!(amount >= 0, "amount");
        if amount == 0 || from == to {
            return;
        }
        let from_bal = Self::balance(env.clone(), from.clone());
        assert!(from_bal >= amount, "balance");
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(from_bal - amount));
        let to_bal = Self::balance(env.clone(), to.clone())
            .checked_add(amount)
            .expect("overflow");
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &to_bal);
        bump_balance(env, from);
        bump_balance(env, to);
    }
}

fn bump_balance(env: &Env, id: &Address) {
    let max = env.storage().max_ttl();
    env.storage().persistent().extend_ttl(
        &DataKey::Balance(id.clone()),
        max.saturating_sub(WEEK_OF_LEDGERS),
        max,
    );
    // Spec: cost is kept alive with the balance (same TTL policy).
    FeeLib::bump_cost(env, id);
}

/// Keep the contract instance (roles, adapters, fees, queue meta, timelock) alive.
/// Same policy as VE / firewall: extend to `max_ttl` when remaining TTL &lt; ~7 days.
fn bump_instance(env: &Env) {
    let max = env.storage().max_ttl();
    env.storage()
        .instance()
        .extend_ttl(max.saturating_sub(WEEK_OF_LEDGERS), max);
}

#[cfg(test)]
mod test;
