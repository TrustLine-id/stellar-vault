// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Yield adapter client interface.
//!
//! Adapters are separate Soroban contracts. The vault only talks to them through
//! this surface — see [`ADAPTERS.md`](../../../docs/ADAPTERS.md).

use soroban_sdk::{contractclient, Address, Bytes, Env, Vec};

/// Contract client for a yield adapter registered on a Trustline vault.
///
/// `allocate` / `deallocate` move underlying; `real_assets` is valuation;
/// `liquid_assets` is immediately deallocatable (may be lower when working capital
/// is locked — used by vault `available_liquidity` / the withdrawal queue).
///
/// Role surface (adapter-local, independent of vault roles):
/// - **Owner** — configures the adapter (`set_owner`, `set_is_allocator`)
/// - **Allocator** — reserved for future adapter-side execution (array + flag)
#[contractclient(name = "YieldAdapterClient")]
pub trait YieldAdapter {
    /// Pull `assets` of the underlying from the vault into this adapter.
    ///
    /// `data` is adapter-specific (market id, strategy params, …). The dummy
    /// adapter ignores it. Must only succeed when invoked by the bound vault.
    ///
    /// Returns the amount actually allocated (may equal `assets`).
    fn allocate(env: Env, data: Bytes, assets: i128) -> i128;

    /// Return `assets` of the underlying from this adapter to the vault.
    ///
    /// Must only succeed when invoked by the bound vault.
    /// Returns the amount actually deallocated. Must not exceed [`liquid_assets`].
    fn deallocate(env: Env, data: Bytes, assets: i128) -> i128;

    /// Current value of this adapter's investments, in underlying asset units.
    /// Must not panic under normal conditions.
    fn real_assets(env: Env) -> i128;

    /// Assets immediately available to `deallocate` back to the vault.
    /// Equal to [`real_assets`] when nothing is locked / illiquid.
    fn liquid_assets(env: Env) -> i128;

    /// Underlying SEP-41 asset this adapter invests.
    fn asset(env: Env) -> Address;

    /// Vault authorized to call `allocate` / `deallocate`.
    fn vault(env: Env) -> Address;

    /// Adapter owner (separate from vault owner).
    fn owner(env: Env) -> Address;

    /// Adapter allocators (reserved for future use).
    fn allocators(env: Env) -> Vec<Address>;

    /// Whether `who` is an adapter allocator.
    fn is_allocator(env: Env, who: Address) -> bool;
}
