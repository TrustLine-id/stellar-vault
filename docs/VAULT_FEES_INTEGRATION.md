# Vault fees (inlined module)

Fees live in [`contracts/trustline-vault/src/fees.rs`](../contracts/trustline-vault/src/fees.rs) — a module of the vault crate, not a separate package.

Three fees:

- **Management:** yearly rate of assets, not compounded, paid in new shares.
- **Performance:** on each holder’s own gain, paid in assets at sale; cost basis moves with transfers.
- **Early-exit penalty:** on large sales; stays in the vault.

Spec / formulas: [VAULT_FEES_SPEC.md](VAULT_FEES_SPEC.md), [VAULT_FEES_FORMULAS.md](VAULT_FEES_FORMULAS.md).

The vault implements its share token, SEP-56 operations and fee hooks directly. Its only
OpenZeppelin dependency is `stellar-contract-utils::mul_div_with_rounding` (rev `a5bd8cb`) for
overflow-safe fixed-point arithmetic. The contracts have not been externally audited.

## How this vault wires it

1. `FeeHost` on `Host` in `lib.rs` (total_assets, supply, balances, mint/pay).
2. `#[contractimpl(contracttrait)] impl VaultFees for TrustlineVault { type Host = Host; }`
3. Constructor: `FeeLib::init(env, &fees)` (initial fees apply at once).
4. Hooks: `accrue` before price-sensitive ops, allocate/deallocate and queue processing;
   `record_purchase` on deposit/mint; `record_transfer` before share moves.
5. **Fee parameter changes**: Curator `submit(SetFees…)` → wait `timelock(SetFees)` → anyone `set_fees` → `FeeLib::set_fees` (accrues first). Cancel = `revoke`. No dedicated `fee_delay`.
6. **Withdrawal queue**: `withdraw` / `redeem` escrow shares + `detach_cost`. If the queue was
   empty and liquid, settle immediately; otherwise enqueue only and settle later via
   `process_withdrawals(max)` (`sale_with_cost` → `settle_sale`, FIFO; `max == 0` → no limit).
   Settlement requires liquidity for the user payout plus the performance fee. `withdraw` returns
   the escrowed share count; `redeem` returns the immediate payout or `0` while queued. Request
   owner or queue manager may `cancel_withdrawal` → `attach_cost` + release escrow to the request
   owner. Curator sets the manager via `set_queue_manager`. Storage: persistent linked nodes +
   head/tail; `MIN_WITHDRAWAL_SHARES` (full-balance / dust positions exempt).

Exported via `VaultFees`: `fees`, `cost_basis`, `value_of`, `accrue`.  
Vault entrypoints: `set_fees` (after timelock), `withdrawal_queue`,
`withdrawal_queue_len`, `process_withdrawals`, `cancel_withdrawal`, `queue_manager` and
`set_queue_manager`.
