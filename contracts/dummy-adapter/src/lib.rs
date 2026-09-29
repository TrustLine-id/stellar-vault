#![no_std]

// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Reference yield adapter for demos and local testing — **not for production**.
//!
//! Holds underlying idle in the adapter contract.
//!
//! - `allocate` / `deallocate` are only callable by the bound vault.
//! - Adapter roles are **local** (not the vault’s): **Owner** + **Allocator[]**.
//! - All role setters are Trustline-gated. Allocator role is stored for future
//!   adapter-side execution; vault↔adapter moves still require vault auth.
//! - **Demo / test helpers only** — do not copy into a production adapter:
//!   `inject` / `skim` (fake gain/loss), `lock` (illiquid working capital).

use soroban_sdk::{
    contract, contractimpl, contracttype, token, Address, Bytes, Env, Vec,
};
use trustline_sdk::{require_trustline, set_validation_engine};

/// ~7 days of ledgers (5s). Same instance TTL policy as the vault / VE.
const WEEK_OF_LEDGERS: u32 = 120_960;

#[contract]
pub struct DummyAdapter;

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Vault,
    Asset,
    Owner,
    Allocators,
    IsAllocator(Address),
    /// Underlying counted in [`real_assets`] but not in [`liquid_assets`].
    Locked,
}

#[contractimpl]
impl DummyAdapter {
    /// Bind this adapter to a vault + underlying asset. Bootstrap: **owner only**.
    /// Owner later sets allocators via `set_is_allocator`.
    pub fn __constructor(
        env: Env,
        validation_engine: Address,
        vault: Address,
        asset: Address,
        owner: Address,
    ) {
        set_validation_engine(&env, &validation_engine);
        env.storage().instance().set(&DataKey::Vault, &vault);
        env.storage().instance().set(&DataKey::Asset, &asset);
        env.storage().instance().set(&DataKey::Owner, &owner);
        env.storage()
            .instance()
            .set(&DataKey::Allocators, &Vec::<Address>::new(&env));
        env.storage().instance().set(&DataKey::Locked, &0i128);
        bump_instance(&env);
    }

    pub fn vault(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Vault).unwrap()
    }

    pub fn asset(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Asset).unwrap()
    }

    pub fn owner(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Owner).unwrap()
    }

    pub fn allocators(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Allocators)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn is_allocator(env: Env, who: Address) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsAllocator(who))
            .unwrap_or(false)
    }

    /// Transfer adapter ownership — Trustline-protected.
    pub fn set_owner(env: Env, new_owner: Address) {
        let owner = Self::require_owner(&env);
        require_trustline!(env, owner, 0, "set_owner"(new_owner));
        env.storage().instance().set(&DataKey::Owner, &new_owner);
        bump_instance(&env);
    }

    /// Add or remove an adapter allocator — Trustline-protected (owner-only).
    ///
    /// Reserved for future adapter-side calls; does not gate vault `allocate` today.
    pub fn set_is_allocator(env: Env, who: Address, is_allocator: bool) {
        let owner = Self::require_owner(&env);
        require_trustline!(
            env,
            owner,
            0,
            "set_is_allocator"(who, is_allocator)
        );
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
            let mut next = Vec::<Address>::new(&env);
            for i in 0..list.len() {
                let a = list.get(i).unwrap();
                if a != who {
                    next.push_back(a);
                }
            }
            env.storage().instance().set(&DataKey::Allocators, &next);
        }
        bump_instance(&env);
    }

    /// Pull `assets` from the vault into this adapter. Vault-only.
    ///
    /// `data` is ignored (reserved for strategy-specific adapters).
    pub fn allocate(env: Env, data: Bytes, assets: i128) -> i128 {
        let _ = data;
        assert!(assets > 0, "amount");
        let vault = Self::vault(env.clone());
        vault.require_auth();
        let asset = Self::asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).transfer(&vault, &self_addr, &assets);
        bump_instance(&env);
        assets
    }

    /// Push `assets` from this adapter back to the vault. Vault-only.
    /// Fails if `assets` exceeds unlocked balance ([`liquid_assets`]).
    pub fn deallocate(env: Env, data: Bytes, assets: i128) -> i128 {
        let _ = data;
        assert!(assets > 0, "amount");
        assert!(assets <= Self::liquid_assets(env.clone()), "locked");
        let vault = Self::vault(env.clone());
        vault.require_auth();
        let asset = Self::asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).transfer(&self_addr, &vault, &assets);
        bump_instance(&env);
        assets
    }

    /// Full held underlying (includes locked). Vault valuation via `real_assets`.
    pub fn real_assets(env: Env) -> i128 {
        Self::balance(env)
    }

    /// Unlocked underlying that `deallocate` may take (`real_assets` − locked).
    pub fn liquid_assets(env: Env) -> i128 {
        let bal = Self::balance(env.clone());
        let locked = Self::locked(env);
        (bal - locked).max(0)
    }

    /// Currently locked (working) amount — in valuation, not deallocatable.
    /// Demo / test only (see [`Self::lock`]).
    pub fn locked(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(0)
    }

    /// **Demo / test only — not for production adapters.**
    /// Set locked amount absolutely (`0`..=`real_assets`).
    /// `lock(0)` unlocks everything; `lock(real_assets)` freezes all for deallocate.
    pub fn lock(env: Env, assets: i128) {
        assert!(assets >= 0, "amount");
        let _owner = Self::require_owner(&env);
        let bal = Self::balance(env.clone());
        assert!(assets <= bal, "exceeds balance");
        env.storage().instance().set(&DataKey::Locked, &assets);
        bump_instance(&env);
    }

    /// **Demo / test only — not for production adapters.**
    /// Pull `assets` from `from` into this adapter (simulates yield / gain).
    /// Does not mint vault shares. `from` must auth and hold the underlying.
    pub fn inject(env: Env, from: Address, assets: i128) {
        assert!(assets > 0, "amount");
        from.require_auth();
        let asset = Self::asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).transfer(&from, &self_addr, &assets);
        bump_instance(&env);
    }

    /// **Demo / test only — not for production adapters.**
    /// Send `assets` from this adapter to `to` (simulates loss).
    /// Owner-only. Uses liquid balance only (cannot skim locked funds).
    pub fn skim(env: Env, to: Address, assets: i128) {
        assert!(assets > 0, "amount");
        let _owner = Self::require_owner(&env);
        assert!(assets <= Self::liquid_assets(env.clone()), "locked");
        let asset = Self::asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).transfer(&self_addr, &to, &assets);
        bump_instance(&env);
    }

    fn balance(env: Env) -> i128 {
        let asset = Self::asset(env.clone());
        let self_addr = env.current_contract_address();
        token::Client::new(&env, &asset).balance(&self_addr)
    }

    fn require_owner(env: &Env) -> Address {
        let owner: Address = env.storage().instance().get(&DataKey::Owner).unwrap();
        owner.require_auth();
        owner
    }
}

fn bump_instance(env: &Env) {
    let max = env.storage().max_ttl();
    env.storage()
        .instance()
        .extend_ttl(max.saturating_sub(WEEK_OF_LEDGERS), max);
}
