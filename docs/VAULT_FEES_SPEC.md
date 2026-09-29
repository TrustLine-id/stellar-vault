# Vault fees: technical specification

Three fees for an SEP-56 (ERC-4626-style) vault:

- a **management fee**: a yearly rate of the assets, not compounded, paid in newly minted shares (section 3);
- a **performance fee**: a share of each holder's own gain, paid in assets when the holder sells. The vault keeps the cost of every holder's shares, and transferred shares keep their cost (sections 4 and 5);
- an **early-exit penalty**: charged on large redemptions and kept in the vault for the remaining holders (section 5).

This document specifies the fee accounting and its integration into this vault, including its
adapters, liquidity management and FIFO withdrawal queue.

The reference implementation in this repo is the inlined module
[`contracts/trustline-vault/src/fees.rs`](../contracts/trustline-vault/src/fees.rs)
(integration notes: [VAULT_FEES_INTEGRATION.md](VAULT_FEES_INTEGRATION.md)).
The vault implements its share token, SEP-56 operations and fee hooks directly. It imports only
OpenZeppelin's `mul_div_with_rounding` utility at `a5bd8cb`; it does not use OpenZeppelin's vault
or token implementation. The contracts have not been externally audited.

**Where each part comes from:**

- **Math:** OpenZeppelin `stellar-contract-utils` at `a5bd8cb`, used only for 256-bit
  multiplication and division.
- **Std:** standard vault management-fee math (yearly rate, fee shares, previews after accrual).
- **Ours:** designed for this vault.

| Part | Source |
|---|---|
| Conversions and rounding: assets + 1, supply + 1,000,000 virtual shares | Implemented in `FeeLib::to_shares` and `FeeLib::to_assets` |
| 256-bit `mul_div` | OpenZeppelin `stellar-contract-utils::math::i128_fixed_point` |
| Share mint, burn and transfer; deposits, exits and events | Implemented in this vault's `lib.rs` and `queue.rs` |
| Management fee in assets: rate × elapsed × total assets, not compounded | Std (bps per year) |
| Fee shares: fee × (supply + virtual shares) / (assets − fee + 1) | Std |
| Previews and conversions on the supply after the fee due | Std |
| Caps: 5%/yr management, 50% performance | Std |
| Delayed changes via curator timelock; accrue before the change | Std (vault `submit` / `revoke` / `set_fees`) |
| Management clock kept while the fee rounds to zero shares | Ours |
| Cost of each holder's shares, moved with transferred shares; performance fee on each sale's gain, in assets | Ours |
| Tiered early-exit penalty; the withdraw search | Ours |

**Why a per-holder performance fee.** A high-water mark on the vault's share price, the other common design, treats all holders alike: whoever buys below the mark gets the recovery without a fee, and whoever buys at the mark pays on gains above it. A cost per holder charges each holder exactly on its own gain. The price of that choice is described in section 6: the fee depends on the owner, so standard previews cannot include it, and a holding's value is below its nominal value.

## 1. Conventions

| Symbol | Meaning |
|---|---|
| `bps` | Basis points; `BPS = 10_000`. |
| `YEAR` | `365 × 86_400` seconds. Time is the ledger timestamp. |
| `mul_div(x, y, d, r)` | `x × y / d` computed in 256 bits, rounded `r` (Floor or Ceil). It panics on overflow of the result or when `d = 0`. |
| `A` | `total_assets() + 1`: effective assets. |
| `S` | `total_supply() + 1_000_000`: effective supply. |

The virtual asset and `VIRTUAL_SHARES = 1_000_000` defend against donation attacks. The fixed
virtual-share offset corresponds to six extra decimal places of conversion precision; deployments
normally expose the share token with the asset's decimals plus six. Every conversion below uses
these effective values.

**Rounding.** Fees round down (Floor), in favour of the holders; conversions round as ERC-4626 requires, in favour of the vault.

## 2. Parameters and state

```
Fees {
  receiver:        Address        // receives the management fee shares and the performance fees; never the vault
  management_bps:  u32            // per year, of total assets, not compounded   (≤ 500   = 5%/yr)
  performance_bps: u32            // of the gain realized at each sale            (≤ 5_000 = 50%)
  penalties:       Vec<(i128 shares_above, u32 bps)>                              (≤ 8 tiers)
}
```

Rules for the penalty tiers:

- `shares_above` is strictly increasing and ≥ 0;
- `bps` never decreases from one tier to the next and each is ≤ 2_000 (20%).

The ascending order matters: section 5.4 relies on it.

Stored state (fee module):

| Key | Content |
|---|---|
| `Fees` | The current parameters. |
| `LastAccrual` | Time of the last management accrual. |
| `Cost(holder)` | Per holder, persistent: what it paid for the shares it holds, in assets. |

There is **no** dedicated `fee_delay` / `PendingFees` in the module. Parameter changes go through the vault’s curator timelock (`PendingOp::SetFees`) — see section 7.

The constructor takes the initial `Fees` (validated). Those parameters apply at once, since investors see them before depositing. It sets `LastAccrual = now`.

## 3. Management fee

`due()` computes the fee due now without writing anything. `accrue()` then:

1. adds the fee's value to the receiver's cost (section 4), so that the fee shares pay no performance fee on themselves;
2. mints the fee shares to `receiver` and emits `FeesAccrued`, if there are any;
3. stores `LastAccrual`.

```
due():
  A = total_assets + 1
  S = total_supply + 1_000_000
  if total_supply == 0:
      return (0 shares, last = now)                                        // no holder, no fee

  fee    = mul_div(A, management_bps × (now − LastAccrual), BPS × YEAR, Floor)   // in assets
  shares = mul_div(fee, S, A − fee, Floor)                                        // in shares
  last   = now if shares > 0 or management_bps == 0, else LastAccrual            // keep accruing
  return (shares, fee, last, S + shares, A)
```

**Why `fee × S / (A − fee)` shares.** After the mint, the receiver's shares are worth exactly `fee`: `x / (S + x) × A = fee` gives `x = fee × S / (A − fee)`. To first order this is `supply × rate × dt / YEAR` shares.

**Why these rules:**

- **Simple rate, not compounded.** `management_bps` is a nominal annual rate: each accrual takes `rate × elapsed / YEAR` of the assets at that moment. Accruals therefore compound, and the effective annual fee depends slightly on how often the vault accrues. It is exactly the nominal rate `r` with one accrual a year, and tends to `1 − e^(−r)` with frequent accruals (daily or more): 1.980% for 2%, 4.877% for 5%. The difference always favours the holders. Displays should call the rate "annual, not compounded". A vault reproducing these fees should use the same convention, so that both charge the same.
- **Not rounded away.** `LastAccrual` advances only when shares were minted, or when the rate is
  0. Otherwise someone calling `accrue` every few seconds could round every fee to zero shares.

Bound: `A − fee > 0` holds as long as `management_bps × dt < BPS × YEAR`, i.e. for 20 years without any accrual at 5%/yr.

**When it accrues:**

- on every deposit, mint, withdraw and redeem, right after the Trustline check and before any price is read;
- on allocate, deallocate and withdrawal-queue processing, before total assets or supply change;
- on `accrue()`, public and unauthenticated (a keeper can call it daily; the fee accrues continuously anyway and is settled at the next interaction);
- on `FeeLib::set_fees`, before a new rate takes effect (called from vault `set_fees` after the timelock).

Share transfers do not accrue: they do not read the price.

## 4. Cost of each holder's shares

Each holder has a **cost**: what was paid, in assets, for the shares it holds. The performance fee of a sale is charged on what the sale pays above the cost of the shares sold (section 5).

### 4.1 Changes to the cost

| Event | Change |
|---|---|
| `deposit(assets)`, shares to `receiver` | `cost(receiver) += assets` |
| `mint(shares)`, shares to `receiver` | `cost(receiver) += preview_mint(shares)`, the assets the library charges |
| Management fee shares | `cost(fee receiver) += fee` (their value) |
| Sale of `s` shares out of a balance `N` | `cost(owner) −= mul_div(cost(owner), s, N, Ceil)` |
| Transfer of `s` shares from `F` (balance `N`) to `T` | `moved = mul_div(cost(F), s, N, Floor)`; `cost(F) −= moved`; `cost(T) += moved` |

### 4.2 Transfers

A transfer carries the sender's average cost with the shares. The sender pays nothing, and its
average cost is unchanged. The receiver's average becomes the weighted average of what it held
and what it received. The vault calls `FeeLib::record_transfer` before its own `move_shares`
implementation from both SEP-41 `transfer` and `transfer_from`.

This keeps shares transferable, so they remain usable as collateral or in pools. It is up to the receiver to value the shares it receives: `value_of(holder, shares)` (section 6) gives what they are worth to it, net of the fees it would pay on selling them.

Consequences, by design:

- Total cost is conserved: gains and losses move with the shares.
- Merging nets gains against losses. One address holding shares bought at 1 and shares bought at 2, sold at 1.5, pays nothing. This also applies to a contract holding the shares of several users (a pool, a lending market, another vault): they share one average cost.
- **Integrators should segregate accounts.** A protocol that holds shares for several depositors (a lending market, a pool) and wants each to keep its own cost should hold each depositor's shares at a separate address (a sub-account per depositor). The vault keeps one cost per address, so nothing needs to change on its side.

**Netting between two holders.** A holds 100 shares bought at 100; B holds 100 bought at 200; the price is 150. A transfers its shares to B, who now holds 200 at an average cost of 150. Fees at the final sale price `P`, with rate `Y`:

| Final price `P` | Without the transfer (A + B) | With the transfer (B) | Fee not collected |
|---|---|---|---|
| 150 | `Y × 5_000` + 0 | 0 | `Y × 5_000` |
| 175 | `Y × 7_500` + 0 | `Y × 5_000` | `Y × 2_500` |
| 200 or more | `Y × (100 (P − 100) + 100 (P − 200))` | `Y × 200 (P − 150)` | 0 |

- **Above the receiver's cost, nothing leaks.** The receiver takes on the sender's latent fee: its average is 150, not the 175 it would have had by buying 100 shares from the vault at 150.
- **Below it, the receiver's loss shields the sender's gain.** A loss that would never be recovered is worth nothing to its holder, since the fee is `max(0, …)`. By a transfer it becomes a shield for someone else's gain, like selling a tax loss. The fee not collected is at most `Y × min(sender's gain, receiver's unrecovered loss)`. The two holders can share that saving off-chain.
- **One investor averaging down** (buying more at 100 after buying at 200) nets the same way, by definition of an average cost. The transfer only lets two investors do together what one may do alone.
- **Closing it entirely** would make a transfer a sale for the performance fee: the sender pays on its gain, and the receiver starts at the current price. Every transfer with a gain would then be taxed, including a deposit as collateral into a lending market. The reference implementation keeps transfers free and accepts this bounded leak; an implementation that prefers the other choice needs only to call the sale's fee calculation (section 5.1) in `transfer`.

## 5. Sales: penalty and performance fee

### 5.1 The amounts of a sale

Selling `s` shares of `owner` (balance `N`, cost `C`), with `A` and `S` after the management fee due:

```
gross       = mul_div(s, A, S, Floor)                                  // convert_to_assets
after       = mul_div(gross, BPS − penalty_bps(s), BPS, Floor)
penalty     = gross − after                                            // stays in the vault
cost_out    = mul_div(C, min(s, N), N, Ceil)                           // the sold shares' cost
performance = mul_div(max(after − cost_out, 0), performance_bps, BPS, Floor)   // to the receiver, in assets
paid        = after − performance                                      // to the seller's receiver
```

The performance fee is charged on the gain net of the seller's own penalty, and after the management fee, which lowered `gross`: no performance fee on money already taken as a fee.

### 5.2 Penalty rate

`penalty_bps(s)` is the `bps` of the last tier whose `shares_above` is strictly less than `s`, or 0 if there is none. It depends only on the number of shares sold, not on the owner, the supply or the liquidity.

### 5.3 Redeem (shares given)

`redeem(s)` refuses more than the owner's balance, escrows the shares and attempts immediate
settlement. It returns `paid` when settled immediately, or `0` when queued. A queued request is
valued again when it settles.

### 5.4 Withdraw (assets wanted)

This is the SEP-56 `withdraw` call, where the investor names a target asset amount instead of the
shares to sell. The vault computes and escrows the required shares when the request is created.
If queued, its fee amounts and payout are recomputed when those shares settle.

The shares burned are the fewest whose sale pays at least `assets`. Within a penalty tier the payout grows with the shares, but it drops where a tier starts: the vault searches each tier's range of shares, lowest first, by bisection. The first range whose top pays enough holds the answer:

```
ranges = [0, t1], [t1 + 1, t2], …, [tn + 1, N]        // t = shares_above of each tier, capped at N
for each range [lo, hi], in order:
    if paid(hi) ≥ assets:
        return the smallest s in [lo, hi] with paid(s) ≥ assets      // bisection
reject when the owner's balance cannot cover any range
```

On immediate settlement, the seller receives exactly `assets`. The rounding surplus,
`paid(s) − assets`, stays in the vault and is accounted as penalty. If the request is queued, the
shares and target asset amount are fixed but the sale is valued again at settlement: the payout
is capped at `assets` and may be lower if the share value has fallen.

Example, with one tier "more than 100 shares: 5%", a price of 1 and no gain:
- **100 assets:** 100 shares, not more than 100: no penalty.
- **101 assets:** 101 shares would be more than 100, so 5% applies. The smallest amount of shares
  in the 5% range that pays 101 is ⌈101 / 0.95⌉ = 107. On immediate settlement, the investor
  burns 107 shares, receives exactly 101, and 6 stay in the vault.

### 5.5 Execution and withdrawal queue

1. The operator authorizes the entrypoint. If it differs from the owner, the vault spends its
   share allowance.
2. The vault detaches the requested shares' proportional cost and escrows the shares on itself.
3. If the request is alone and liquid, it settles immediately. Otherwise it stays in the FIFO
   queue until `process_withdrawals` can settle it.
4. At settlement, the vault recomputes the sale at the current share price, pulls available
   liquidity from the liquidity adapter when needed, burns the escrowed shares and transfers the
   payout. A `withdraw` request caps the payout at its requested asset amount; if the value has
   fallen, it can settle for less. A queued `redeem` returns `0` from the initial call and pays its
   then-current net value at settlement.
5. The vault transfers any performance fee to the fee receiver and emits `PerformanceFeePaid`.
   A penalty remains in the vault and emits `PenaltyPaid`. Settlement also emits the vault's
   `Withdraw` event and the queue's `WithdrawSettled` event.

## 6. Previews, conversions and valuation

`convert_to_shares`, `convert_to_assets`, `preview_deposit` and `preview_mint` use the vault's
local conversion functions on the effective values after the management fee due now, without
writing:

- `convert_to_shares` and `preview_deposit`: `mul_div(assets, S, A, Floor)`;
- `convert_to_assets`: `mul_div(shares, A, S, Floor)`, the **nominal value**;
- `preview_mint`: `mul_div(shares, A, S, Ceil)`.

The performance fee depends on the owner, whom the standard previews do not know:

- `preview_redeem(s)` and `preview_withdraw(assets)` include the penalty but **not** the performance fee. They are exact for a holder without gain, and optimistic otherwise.
- `max_withdraw(owner)` knows the owner: it is the current `paid` value of its whole balance,
  before any later queue-settlement price change.
- **`value_of(owner, shares) → (gross, penalty, performance, net)`** gives the exact amounts of section 5.1 for that owner. With the owner's whole balance, `net` is the **real value of its holding**: the nominal value minus the penalty and the performance fee it would pay on selling. Wallets and dashboards should display it. A lender taking shares as collateral should value them with it, for the holder whose cost they carry.
- `cost_basis(holder)` returns the cost of section 4.

`max_deposit`, `max_mint` and `max_redeem` return unlimited, unlimited, and the owner's balance.

A preview is exact for a transaction in the same ledger. A later transaction may get slightly less, because the management fee keeps accruing in between.

Negative amounts: the four operations refuse them; previews treat them as 0.

## 7. Changing fees

Fee **accounting** stays in the fees module (`FeeLib`). Fee **governance** uses the vault’s general curator timelock — there is no separate fee delay.

| Step | Who | Effect |
|---|---|---|
| `submit(SetFees(receiver, management_bps, performance_bps, penalties))` | Curator (+ Trustline) | Stores a pending op with `eta = now + timelock(SetFees)`, replacing any pending op for that kind. |
| `revoke(kind)` | Curator or Sentinel | Drops the pending `SetFees` (same as other timelocked ops). |
| `set_fees(…)` | anyone | Requires `begin_execute(SetFees)` (due). Calls `FeeLib::set_fees`: accrues management under the old rate, then stores the new fees. Emits `FeesChanged`. |

Details:

- **Flat arguments** on `submit` / `set_fees` (not a struct), so Trustline intents stay describable.
- **Delay** is `timelock(SetFees)`, adjustable like other ops (`IncreaseTimelock` / `DecreaseTimelock`), not a constructor constant.
- **Anyone can execute** once due; cancel = `revoke`.
- **The management rate** never applies to time before it: the old rate is settled first.
- **A new performance rate** applies to sales that follow, on the whole gain of the shares sold. Holders who disagree sell during the delay.

**Validation** fails with `InvalidFees` (900) when:

- `receiver` is the vault itself;
- `management_bps > 500` or `performance_bps > 5_000`;
- there are more than 8 tiers;
- a tier's `shares_above` does not increase or a tier's `bps` decreases;
- any tier's `bps > 2_000`.

Validation runs in the constructor and when `set_fees` executes. `submit` may queue invalid
parameters, but execution will reject them without applying the change.

## 8. Events and errors

| Event (topics) | Data |
|---|---|
| `["fees", "accrued", receiver]` | `[management_shares, management_assets]`: the fee shares and their value, added to the receiver's cost |
| `["cost", holder]` | `cost`: the holder's new total cost, at every change (purchase, sale, transfer, management fee, withdrawal request) |
| `["cost", "moved", from, to]` | `[shares, cost]`: shares transferred and the part of the sender's cost that went with them |
| `["performance", owner]` | `[shares, assets]`: shares sold, performance fee paid to the receiver |
| `["penalty", owner]` | `[shares, assets]`: shares sold, assets kept in the vault |
| `["fees", "changed"]` | `[fees]` |

Pending and revoked fee proposals use the vault timelock events. Share accounting is implemented
locally and currently emits no separate mint, burn or transfer event. Deposits emit `Deposit`;
settled exits emit `Withdraw` plus `WithdrawSettled`; queue creation and cancellation emit
`WithdrawEnqueued` and `WithdrawCancelled`.

| Error | Code |
|---|---|
| `InvalidFees` | 900 |

Balance, allowance, amount, minimum-request and maximum-exit failures are assertion errors rather
than numbered fee errors. Timelock readiness and unknown pending operations use the vault's
timelock errors, not a fee-module `FeesNotReady`.

## 9. Trustline

**What Trustline checks.** `deposit`, `mint`, `withdraw`, `redeem`, and Curator `submit` (including `SetFees`) require a Trustline proof for the exact call. Investor intents use the operator as sender; fee submissions use the Curator. Intent data is encoded as the backend does: `utf8(function) || XDR(args)`. The vault has no `*_intent_data` view.

**What it does not check.** Share transfers, `accrue`, `set_fees`, and `revoke` need no proof: transfers only move shares and their cost; `accrue` / `set_fees` can only mint what is due or apply what was announced after the timelock; `revoke` only drops a pending op (Curator or Sentinel).

## 10. Adapters, liquidity and the withdrawal queue

**Total assets.**

- `due` and the sales must use the same `total_assets` as the conversions. With adapters, that is idle assets plus the adapters' valuation.
- The vault accrues before allocation, deallocation and queue settlement. Registering or removing
  an empty adapter does not accrue.
- If an adapter's valuation can jump or be manipulated, use the manipulation-resistant valuation the vault already relies on for conversions. One option is a cap on the rate of increase of total assets per second. A sale at a manipulated high price would pay a higher performance fee, and more importantly more assets.

**Interest-based vaults.** A vault that already accrues interest can keep the management fee as specified. Do not stack a vault-wide performance fee on each accrual's interest with the per-holder fee of this spec: choose one.

**Withdrawal queues** (requests when liquidity is short):

- **Escrow.** Requested shares move to the vault but stay in the supply until settlement. Their
  proportional cost is detached from the owner and stored on the request. They keep bearing
  management fees, gains and losses until settlement.
- **Cost at request.** Move the requested shares' part of the owner's cost into the request (`FeeLib::detach_cost` in the library). The request then settles on its own cost, whatever the owner does meanwhile (`FeeLib::sale_with_cost`). Cancel restores that cost (`FeeLib::attach_cost`) and returns escrowed shares to the owner.
- **Penalty rate.** The request fixes its share count, but does not snapshot the fee schedule.
  Settlement recomputes the applicable tier from the then-current `Fees.penalties`, as well as
  gross value, performance fee and payout, using the detached cost.
- **Splitting.** A requester can avoid a tier by splitting a redemption. To stop this, compute the tier on the owner's aggregate: pending requests plus redemptions over a rolling window, e.g. 7 days. The reference implementation does not aggregate; there, each redemption needs its own Trustline proof, so a Trustline policy can also limit repeated redemptions.
- **Queue fee.** Do not charge the penalty for being queued. It is a function of size. A fee for instant liquidity, if wanted, is a separate parameter.
- **Maxima.** `max_withdraw` returns the owner's current net value and `max_redeem` its share
  balance. They intentionally do not cap on immediate liquidity because an illiquid exit may
  enter the queue. A partial exit below `MIN_WITHDRAWAL_SHARES = 1_000` is rejected unless it is
  the holder's full balance or the entire balance is already below that floor.
- **Paying the performance fee.** It is always paid in assets at settlement, from the same
  liquidity as the seller's payout.

**Receivers.**

- The receiver may be another vault, for profit sharing, or a treasury multisig. It receives management fee shares (with their value as cost) and performance fees in assets. Its own sales pay the penalty like anyone's; exempt it explicitly if that is intended.
- Split receivers are an easy extension: one receiver for management and one for performance.

**Every value increase is a gain.** Penalties left by large exits, donations and investment returns all raise the share price, and all pay the performance fee when a holder sells above its cost.

**Precision.** Products use the imported 256-bit `mul_div_with_rounding` utility.

**Storage.** One persistent entry per holder, kept alive with the balance (same `max_ttl` policy: extended whenever the balance is bumped or the cost is written).

## 11. Invariants and test checklist

1. The receiver's new management fee shares are worth `fee` (within rounding), measured at the post-mint price; holders lose exactly that value.
2. Calling `accrue` N times over a period mints what one call at the end mints, within N shares of rounding. It never mints less because of frequent calls.
3. A sale pays a performance fee only on `after − cost_out > 0`: never on a loss, never on a recovery below cost.
4. Two holders who bought at different prices and sell at the same price pay different fees, each on its own gain.
5. A transfer moves cost pro rata: the sender's average is unchanged, the total is conserved (within rounding), and the sender pays nothing.
6. Management fee shares pay no performance fee on their own value.
7. A penalty stays in `total_assets`; the other holders' value rises by it, pro rata, and their costs do not change: it is part of their gain.
8. An immediately settled redeem pays `paid` of section 5.1 and an immediately settled withdraw
   pays exactly its target assets. Queued requests are revalued at settlement.
9. `value_of(owner, balance)` equals what selling everything pays in the same ledger; `max_withdraw(owner)` equals its `net`.
10. `preview_deposit` and `preview_mint` equal the operation's result in the same ledger; `preview_redeem` does too for a holder without gain.
11. Fee changes: only the Curator proposes; the Curator or a Sentinel may revoke. Nothing applies
    before `eta`, and anyone can execute after it. The management fee under the old rate accrues
    first.
12. Invalid parameters (section 7) are refused at deployment and when an executed fee change
    reaches `set_fees`; `submit` itself does not validate them.

## 12. Tests

The contract tests in
[`contracts/trustline-vault/src/test.rs`](../contracts/trustline-vault/src/test.rs) exercise the
actual `VIRTUAL_SHARES = 1_000_000` configuration. They cover management accrual and rate changes,
holder-specific performance fees, cost movement on transfer, penalty tiers, exact-asset
withdrawals, queued settlement and cancellation, invalid fee parameters, and adapter liquidity.
