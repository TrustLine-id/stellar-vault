# Yield Adapters

This document defines the **yield adapter** surface used by `trustline-vault`,
adapted to Soroban and
[SEP-56](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0056.md).

## Role in the vault

```text
 Investors ──deposit/mint/withdraw/redeem──►  Trustline Vault  ◄──allocate/deallocate──  Allocator
                                                    │
                          idle underlying (SEP-41)  │
                                                    ▼
                                          registered adapters[]
                                                    │
                              optional liquidity_adapter (+ data)
```

- **Idle assets** sit on the vault contract (token balance of `asset`).
- **Adapters** hold (or report) invested capital. `total_assets` ≈ idle + Σ `real_assets()`.
- Adapters are **registered / unregistered** dynamically by the vault **Curator** (Trustline-gated).
- One registered adapter may be designated the **liquidity adapter** by an **Allocator**. When set:
  - **Deposits / mints** transfer underlying into the vault, then immediately
    `allocate` that amount into the liquidity adapter.
  - **Withdrawals / redemptions** consume idle first; any shortfall is
    `deallocate`d from the liquidity adapter.

Fees use the inlined [`fees`](../contracts/trustline-vault/src/fees.rs) module. Curator **timelock** ([`timelock`](../contracts/trustline-vault/src/timelock.rs)): adapters, allocators, timelock admin, **SetFees**, **per-adapter cap increases**. Caps ([`caps`](../contracts/trustline-vault/src/caps.rs)): keyed by adapter address — abs / rel (WAD). Sentinel: `revoke`, `deallocate`, **cap decreases**. Withdrawals ([`queue`](../contracts/trustline-vault/src/queue.rs)): FIFO escrow; owner `cancel_withdrawal`; deposit auto-allocate to liq adapter only when queue empty.

## Vault roles

Roles **do not inherit**. Owner is not an Allocator by default (bootstrap with owner only).

| Role | Cardinality | Trustline setters / actions |
|------|-------------|----------------------------|
| **Owner** | 1 | `set_owner`, `set_curator` |
| **Curator** | 0…1 (`None` until set) | `set_is_allocator`, `add_adapter`, `remove_adapter` |
| **Allocator** | N | `allocate`, `deallocate`, `set_liquidity_adapter` |
| Investor | — | `deposit` / `mint` / `withdraw` / `redeem` (+ share token) |

Constructor takes **`owner` only**. Sequence after deploy:

1. Owner → `set_curator`
2. Curator → `set_is_allocator(who, true)` (+ `add_adapter`)
3. Allocator → `set_liquidity_adapter` / `allocate` / `deallocate`

Allocator-gated entrypoints take an explicit `allocator: Address` (Soroban auth; must be in the allocators set). Intent prototypes include that address.

## Interface (`YieldAdapter`)

Implemented as a Soroban `#[contractclient]` trait in
[`contracts/trustline-vault/src/adapter.rs`](../contracts/trustline-vault/src/adapter.rs).
Any adapter WASM must export these functions with matching types:

| Method | Args | Returns | Notes |
|--------|------|---------|--------|
| `allocate` | `data: Bytes`, `assets: i128` | `i128` | Pull underlying **from the vault**. Caller = bound vault. |
| `deallocate` | `data: Bytes`, `assets: i128` | `i128` | Push underlying **back to the vault**. Caller = bound vault. Must not exceed `liquid_assets`. |
| `real_assets` | — | `i128` | Mark-to-market (or balance) in underlying units. |
| `liquid_assets` | — | `i128` | Immediately deallocatable amount (≤ `real_assets` when capital is locked / illiquid). Used by vault `available_liquidity` and the withdrawal queue. |
| `asset` | — | `Address` | Underlying SEP-41 asset. |
| `vault` | — | `Address` | Vault allowed to allocate/deallocate. |
| `owner` | — | `Address` | Adapter-local owner (≠ vault owner). |
| `allocators` | — | `Vec<Address>` | Adapter allocator list (future use). |
| `is_allocator` | `who: Address` | `bool` | Membership check. |

`data` is an opaque, adapter-specific blob. The vault stores `liquidity_data: Bytes`
for the liquidity adapter. Manual `allocate` / `deallocate` from a vault Allocator
pass explicit `data`.

### Adapter-local roles

Independent of vault roles:

| Role | Cardinality | Trustline setters |
|------|-------------|-------------------|
| **Owner** | 1 | `set_owner`, `set_is_allocator` |
| **Allocator** | N | *(reserved — stored for future adapter-side execution)* |

Constructor: **`owner` only**. `allocate` / `deallocate` remain **vault-only** today;
the adapter Allocator array does not gate those calls yet.

### Auth expectations

1. **Vault → adapter**: `allocate` / `deallocate` must `require_auth` on the vault
   address. Never accept arbitrary callers.
2. **Adapter owner** setters: Trustline (`require_trustline!`) + owner auth.
3. On `allocate`, the dummy **pulls** via SEP-41 `transfer(vault → adapter)`. The
   vault must call `authorize_as_current_contract` for that nested transfer before
   invoking the adapter.

### Dummy adapter pattern (this repo)

[`dummy-adapter`](../contracts/dummy-adapter) is a **reference / demo adapter only — not for production**.

- Bound to one `vault` + one `asset` + **owner** at construction.
- `allocate` / `deallocate`: vault auth; move SEP-41 balances.
- `real_assets` / `liquid_assets`: token balance; optional locked slice via test helper `lock`.
- `set_owner` / `set_is_allocator`: Trustline-gated.
- **Test / demo helpers** (`inject`, `skim`, `lock`): simulate gain, loss, and illiquid working capital. Do **not** ship these entrypoints on a production adapter.

## Vault-side management API

| Function | Who | Trustline | Timelock | Description |
|----------|-----|-----------|----------|-------------|
| `set_owner` | Owner | yes | — | Transfer ownership |
| `set_curator` | Owner | yes | — | Appoint curator |
| `set_is_sentinel` | Owner | yes | — | Add/remove sentinel |
| `submit` / `revoke` | Curator / (Curator\|Sentinel) | submit yes / revoke no | per-op | Queue / cancel a pending op |
| `set_is_allocator` | Anyone after submit | no (gated at submit) | per-op | Add/remove allocator |
| `add_adapter` / `remove_adapter` | Anyone after submit | no (gated at submit) | per-op | Register / unregister. **Both require `real_assets() == 0`** (remove also requires vault allocation 0) so `total_assets` cannot jump. |
| `increase_timelock` / `decrease_timelock` / `abdicate` | Anyone after submit | no | per-op | Timelock admin |
| `increase_absolute_cap` / `increase_relative_cap` | Anyone after submit | no | per-op | Raise caps for an adapter |
| `decrease_absolute_cap` / `decrease_relative_cap` | Curator\|Sentinel | no | — | Lower caps immediately |
| `set_liquidity_adapter` | Allocator | yes | — | Set/clear liquidity adapter |
| `allocate` | Allocator | yes | — | Idle → adapter |
| `deallocate` | Allocator **or** Sentinel | yes | — | Adapter → idle |

Investor SEP-56 entrypoints remain Trustline-gated and auto-use the liquidity
adapter when configured.

## Implementing a new adapter

1. Export the `YieldAdapter` methods (including `liquid_assets`, `owner` / `allocators` / `is_allocator`).
2. `__constructor(validation_engine, vault, asset, owner)`.
3. Vault-only `allocate` / `deallocate`; Trustline on `set_owner` / `set_is_allocator`.
4. Conservative `real_assets`; `liquid_assets` ≤ `real_assets` when capital can be locked.
5. Vault Curator `add_adapter`, then Allocator `set_liquidity_adapter` if needed.

See `dummy-adapter` for the reference template (omit its test helpers in production adapters).
