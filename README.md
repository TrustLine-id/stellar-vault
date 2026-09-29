# Trustline Stellar Vault (SEP-56)

> Part of the Trustline Stellar / Soroban stack, developed with support from the [Stellar Community Fund](https://communityfund.stellar.org) (**SCF #44**).

Tokenized vault for Stellar Soroban, protected by [Trustline](https://www.trustline.id): SEP-56 share token, yield adapters, Owner / Curator / Allocator / Sentinel roles, curator timelock, per-adapter caps, fees, and a FIFO withdrawal queue.

## Yield adapters

The vault does not hold strategy logic itself: invested capital sits in **yield adapters** (separate Soroban contracts). A vault can register **several** adapters at once; idle underlying stays on the vault, and **Allocators** decide how much to `allocate` / `deallocate` into each one (subject to per-adapter caps). One adapter may optionally be the **liquidity adapter** for deposit auto-invest and withdrawal shortfalls. Design, interface, and vault-side management API: [docs/ADAPTERS.md](docs/ADAPTERS.md).

## Security

These contracts have **not been externally audited**. Do not use them with real value without your own review. Report vulnerabilities privately to security@trustline.id.

## Features

- **SEP-56 vault** — deposit / mint / withdraw / redeem with Trustline-gated investor entrypoints
- **Yield adapters** — multi-adapter portfolio; Allocator-controlled `allocate` / `deallocate` ([docs/ADAPTERS.md](docs/ADAPTERS.md))
- **Roles** — Owner, Curator, Allocator, Sentinel; optional **Queue manager** (queue layer only)
- **Curator timelock** — per-op delays; `submit` → wait → execute; Curator|Sentinel `revoke`
- **Per-adapter caps** — absolute and relative (WAD)
- **Fees** — management, per-holder performance, early-exit penalty ([docs](docs/VAULT_FEES_INTEGRATION.md))
- **Withdrawal queue** — FIFO escrow; settle immediately only when alone + liquid; otherwise enqueue and `process_withdrawals(max)`; deposit does not auto-allocate to the liquidity adapter while the queue is non-empty

## Roles (no inheritance)

| Role | Immediate | Via `submit` → execute target |
|------|-----------|-------------------------------|
| **Owner** | `set_owner`, `set_curator`, `set_is_sentinel` | — |
| **Curator** | decrease caps; `revoke`; `set_queue_manager` | adapters, allocators, increase caps, **SetFees**, timelock admin |
| **Allocator** | `allocate`, `deallocate`, `set_liquidity_adapter` | — |
| **Sentinel** | `revoke`, `deallocate`, decrease caps | — |
| **Queue manager** | `cancel_withdrawal` on any queued id (queue layer only) | — |
| **Anyone** | `accrue`; execute due pending ops; `process_withdrawals(max)` (`0` = no limit) | — |

## Constructor

The vault starts with **only the Owner role assigned**. Its constructor is:

| Parameter | Meaning |
|-----------|---------|
| `validation_engine` | Dedicated Trustline Validation Engine instance used by all gated calls |
| `asset` | Underlying SEP-41 asset contract |
| `owner` | Initial and only role holder at deployment |
| `decimals` | Share-token decimals, at most 18; conventionally asset decimals + 6 |
| `name`, `symbol` | Share-token metadata |
| `fees` | Initial receiver, management/performance rates and penalty tiers; validated immediately |
| `default_timelock` | Initial delay, in seconds, for all nine operation kinds; maximum 365 days |

The constructor initializes empty allocator, sentinel, adapter and withdrawal-queue state. Initial
fees apply immediately; no Curator, Allocator, Sentinel, queue manager or liquidity adapter is
assigned automatically.

Typical bootstrap:

1. Owner → `set_curator` (optional `set_is_sentinel`)
2. Curator → `submit` + execute `set_is_allocator` / `add_adapter` / caps (optional `set_queue_manager`)
3. Allocator → `set_liquidity_adapter` / `allocate` / `deallocate`

## Fees

Accounting lives in [`fees.rs`](contracts/trustline-vault/src/fees.rs). Parameter changes use the curator timelock (no dedicated fee delay):

1. Curator `submit(SetFees(…))` (+ Trustline)
2. Wait `timelock(SetFees)`
3. Anyone `set_fees(…)` (accrues management under the old rate first)
4. Curator or Sentinel may `revoke` meanwhile

| Doc | Content |
|-----|---------|
| [VAULT_FEES_INTEGRATION.md](docs/VAULT_FEES_INTEGRATION.md) | How this vault wires the fee module |
| [VAULT_FEES_SPEC.md](docs/VAULT_FEES_SPEC.md) | Full technical specification |
| [VAULT_FEES_FORMULAS.md](docs/VAULT_FEES_FORMULAS.md) | Equations and rounding |

## Withdrawal queue

All `withdraw` / `redeem` go through a FIFO queue ([`queue.rs`](contracts/trustline-vault/src/queue.rs)):

1. Escrow shares on the vault + `detach_cost`
2. **Fast path** — if the queue was empty, settle this request in the same tx when liquid (`idle + liq adapter`)
3. **Slow path** — if others were already waiting, enqueue only (no backlog flush); anyone calls `process_withdrawals(max)` later (`max == 0` → no limit; use a small `max` if the tx hits resource limits)
4. Request **owner** or optional **queue manager** may `cancel_withdrawal(caller, id)` (shares + cost restored to the request owner)
5. While the queue is non-empty, deposit **does not** auto-allocate to the liquidity adapter
6. Curator may `set_queue_manager` / clear it (role lives only in the queue layer; gone if the queue is disabled)
7. Storage: one persistent node per request + instance head/tail (not one giant instance vector); min `MIN_WITHDRAWAL_SHARES` (1000), except full-balance / dust exits

## Timelock / caps

- Per-op delays (`default_timelock` at deploy; adjustable via `IncreaseTimelock` / `DecreaseTimelock` / `Abdicate`)
- Caps keyed by **adapter address** (absolute + relative WAD); increases timelocked, decreases immediate for Curator|Sentinel

Details: [docs/ADAPTERS.md](docs/ADAPTERS.md).

## Layout

```text
contracts/
  trustline-vault/   # SEP-56 vault (lib, fees, timelock, caps, adapter client, queue)
  dummy-adapter/     # Reference adapter for demos / tests (not for production)
  mock-validation-engine/  # Pass-through VE for integration tests (Trustline off)
docs/
  ADAPTERS.md        # Yield adapter surface and vault management API
  VAULT_FEES_*.md    # Fee spec, formulas, integration notes
rust-toolchain.toml  # pinned Rust (1.98.0) + wasm32v1-none
versions.env         # pinned Rust + Stellar CLI (shared with CI)
wasm-hashes.txt      # expected sha256 of release WASM artifacts
```

## Prerequisites

- [Rust](https://www.rust-lang.org/) **1.98.0** with target `wasm32v1-none` ([`rust-toolchain.toml`](rust-toolchain.toml) installs it via rustup)
- [Stellar CLI](https://developers.stellar.org/docs/tools/cli) **27.1.0** (same pin as [`versions.env`](versions.env) and CI)
- A [TrustlineOracleVE](https://github.com/TrustLine-id/stellar-validation-engine) WASM hash from published [`deployments.json`](https://github.com/TrustLine-id/stellar-validation-engine/blob/master/deployments.json) (`VE_WASM_HASH`)
- [`trustline-sdk`](https://crates.io/crates/trustline-sdk) `0.2.0` (pulled from crates.io by default; locked in `Cargo.lock`)

Optional local SDK override for development: copy
[`.cargo/config.toml.example`](.cargo/config.toml.example) to `.cargo/config.toml` (gitignored).
Without that file, builds (including the deploy script) use crates.io.

## Build

```bash
stellar contract build
./scripts/check-wasm-hashes.sh
```

WASM artifacts land under `target/wasm32v1-none/release/`. Their sha256 digests must match the
committed baselines in [`wasm-hashes.txt`](wasm-hashes.txt). After an intentional contract change,
rebuild without a local `.cargo/config.toml` and refresh the file with
`./scripts/record-wasm-hashes.sh`.

CI builds from a clean clone with the pinned Rust and Stellar CLI, then fails if the digests
drift.

## Deploy (testnet)

Pick the VE WASM hash for your network from Trustline’s published
[`deployments.json`](https://github.com/TrustLine-id/stellar-validation-engine/blob/master/deployments.json)
(core is deployed once per chain by Trustline — you only instantiate a client VE). Then:

```bash
export STELLAR_ACCOUNT=alice
export VE_WASM_HASH=…          # testnet → ve_uploads[].wasm_hash in deployments.json
./scripts/deploy-testnet.sh
```

Deploys `TrustlineOracleVE` + vault + dummy adapter, **disables Trustline** to
bootstrap curator / allocators / adapter registration / caps (and adapter
`set_is_allocator`), then **re-enables Trustline**. Contract ids go to
`scripts/last-deploy.env` (gitignored).

## Test

Integration tests use an in-repo `mock-validation-engine` that always passes (Trustline effectively off) so investor, role, adapter, fee, and queue paths can be exercised end-to-end with the `dummy-adapter`.

```bash
cargo test --workspace
```

CI (GitHub Actions) runs `stellar contract build`, `./scripts/check-wasm-hashes.sh`, then
`cargo test --workspace` on a clean clone (no local `.cargo/config.toml` override).

End-to-end Testnet UI: [stellar-vault-app](https://github.com/TrustLine-id/stellar-vault-app).

## Related repositories

| Repo | Role |
|------|------|
| [stellar-sdk](https://github.com/TrustLine-id/stellar-sdk) | `trustline-sdk` crate |
| [stellar-validation-engine](https://github.com/TrustLine-id/stellar-validation-engine) | On-chain VE, registry, sanctions |
| [stellar-demo-app](https://github.com/TrustLine-id/stellar-demo-app) | General Trustline React demo |
| [stellar-vault-app](https://github.com/TrustLine-id/stellar-vault-app) | Reference UI to deploy and operate Trustline vaults on Testnet |

## License

Copyright (c) 2026 [Trustline Digital Asset Ltd.](https://www.trustline.id). GNU GPL v2 (or later) — see [LICENSE](LICENSE).

## Links

- **Homepage:** https://www.trustline.id
- **Repository:** https://github.com/TrustLine-id/stellar-vault
- **Issues:** https://github.com/TrustLine-id/stellar-vault/issues
