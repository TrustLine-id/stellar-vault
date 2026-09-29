# Vault fees: formulas

This page gives the fee calculations as equations. The logic lives in
[`contracts/trustline-vault/src/fees.rs`](../contracts/trustline-vault/src/fees.rs) and
[`lib.rs`](../contracts/trustline-vault/src/lib.rs). Full spec: [VAULT_FEES_SPEC.md](VAULT_FEES_SPEC.md).

Notation for rounding: $\lfloor x \rfloor$ rounds down and $\lceil x \rceil$ rounds up. Every
product is computed in 256 bits before dividing through OpenZeppelin's
`stellar-contract-utils::mul_div_with_rounding`. This math utility is the vault's only
OpenZeppelin dependency; the share token and vault operations are implemented locally.

## 1. Inputs

**Parameters** (the `Fees` struct in [fees.rs](../contracts/trustline-vault/src/fees.rs)),
set at deploy and changed only after the curator timelock:

| Symbol | Code | Meaning | Bound |
|---|---|---|---|
| $r_m$ | `management_bps` | management fee, bps per year, not compounded | $r_m \le 500$ (5%/yr) |
| $r_p$ | `performance_bps` | performance fee, bps of the gain realized at a sale | $r_p \le 5000$ (50%) |
| $(\theta_k, \pi_k)_{k=1..n}$ | `penalties` | redeeming more than $\theta_k$ shares costs $\pi_k$ bps | $\theta_k$ strictly increasing, $\pi_k$ non-decreasing, $\pi_k \le 2000$, $n \le 8$ |
| $R$ | `receiver` | receives the fees | $R \ne$ the vault |

**State:**

| Symbol | Meaning |
|---|---|
| $t_m$ | time of the last management accrual (`LastAccrual`) |
| $C_h$ | per holder $h$: what it paid for the shares it holds, in assets (`Cost(h)`) |

**Vault values, read now:**

| Symbol | Code | Meaning |
|---|---|---|
| $t$ | `e.ledger().timestamp()` | now, in seconds |
| $A_0$ | `total_assets` | idle + Σ adapter `real_assets` |
| $N$ | share `total_supply` | shares in circulation |
| $n_h$ | share `balance(h)` | shares held by $h$ |
| $V$ | `virtual_shares` | donation-attack offset; **1,000,000** in this vault (`VIRTUAL_SHARES`) |

**Constants:**

$$
B = 10^4 \text{ (bps)},\qquad Y = 365 \times 86\,400 \text{ s}
$$

At deployment, $t_m = t_{\text{deploy}}$.

## 2. Effective values

Conversions use one virtual asset and $V$ virtual shares:

$$
A = A_0 + 1, \qquad S_0 = N + V
$$

## 3. Management fee: `due` and `accrue`

$r_m$ is a nominal annual rate, **not compounded**: each accrual takes the pro rata share of the assets present at that moment.

**Empty vault.** If $N = 0$: no fee, and $t_m \leftarrow t$.

**Fee in assets:**

$$
\Delta t = t - t_m, \qquad F_m = \left\lfloor \frac{A \cdot r_m \cdot \Delta t}{B \cdot Y} \right\rfloor
$$

**Shares minted.** They are worth exactly $F_m$ after the mint, since
$\frac{m}{S_0 + m} A = F_m$:

$$
m = \left\lfloor \frac{F_m \, S_0}{A - F_m} \right\rfloor
$$

**Clock.** It does not move while the fee rounds to zero shares:

$$
t_m \leftarrow \begin{cases} t & \text{if } m > 0 \text{ or } r_m = 0 \\ t_m & \text{otherwise} \end{cases}
$$

**Mint.** The receiver's cost rises by the value of its new shares, so they pay no performance
fee on themselves:

$$
\text{cost}(R) \mathrel{+}= F_m, \qquad n_R \mathrel{+}= m, \qquad S = S_0 + m
$$

Without rounding, with $f = \dfrac{r_m}{B}\cdot\dfrac{\Delta t}{Y}$:

$$
m = S_0 \, \frac{f}{1 - f} \;\approx\; N \cdot \frac{r_m}{B} \cdot \frac{\Delta t}{Y}
$$

Because successive accruals compound, the effective annual fee depends on how often the vault accrues. With $\bar r = r_m / B$ and $k$ equal accruals in a year:

$$
\text{effective annual fee} = 1 - \left(1 - \frac{\bar r}{k}\right)^{k}, \qquad \bar r \;(k = 1) \;\ge\; \text{effective} \;\ge\; 1 - e^{-\bar r} \;(k \to \infty)
$$

For example, $\bar r = 2\%$ gives between 2.000% and 1.980%; $\bar r = 5\%$ gives between 5.000% and 4.877%.

**When it accrues:**
- on every deposit, mint, withdraw and redeem, after the Trustline check ([lib.rs](../contracts/trustline-vault/src/lib.rs) investor entrypoints);
- on allocation, deallocation and withdrawal-queue processing;
- on `accrue()`, by anyone;
- on `FeeLib::set_fees`, before a new rate (vault `set_fees` after timelock).

## 4. Conversions

With $S$ and $A$ after the management fee due:

$$
\text{to\_shares}(a) = \frac{a \, S}{A}, \qquad \text{to\_assets}(s) = \frac{s \, A}{S}
$$

| Function | Formula | Rounding |
|---|---|---|
| `convert_to_shares`, `preview_deposit` | $\lfloor a S / A \rfloor$ | down |
| `convert_to_assets` (nominal value) | $\lfloor s A / S \rfloor$ | down |
| `preview_mint` | $\lceil s A / S \rceil$ | up |

## 5. Cost of each holder's shares

$C_h$ is what holder $h$ paid for the shares it holds, in assets. `FeeLib::cost_basis` reads it;
the internal `set_cost` and `add_cost` helpers update it.

**Changes:**

| Event | Change | Code path |
|---|---|---|
| deposit of $a$ assets | $C_{\text{receiver}} \mathrel{+}= a$ | `record_purchase` |
| mint of $s$ shares | $C_{\text{receiver}} \mathrel{+}= \lceil s A / S \rceil$ | `record_purchase` |
| management fee | $C_R \mathrel{+}= F_m$ | `accrue` |
| sale of $s$ shares | $C_h \mathrel{-}= \text{cost}_{\text{out}}$ (§6.1) | `settle_sale` |
| transfer of $s$ shares from $F$ to $T$ | see below | `record_transfer` |

**Transfer.** The shares carry the sender's average cost:

$$
\text{moved} = \left\lfloor \frac{C_F \cdot s}{n_F} \right\rfloor, \qquad C_F \mathrel{-}= \text{moved}, \qquad C_T \mathrel{+}= \text{moved}
$$

The sender's average $C_F / n_F$ is unchanged. The receiver's average becomes the weighted average:

$$
\frac{C_T + \text{moved}}{n_T + s}
$$

## 6. Sales: penalty and performance fee

### 6.1 The amounts of selling $s$ shares of owner $h$

**Value:**

$$
g = \left\lfloor \frac{s \, A}{S} \right\rfloor
$$

**After the penalty**, with $\pi(s)$ of §6.2:

$$
q = \left\lfloor \frac{g \,\bigl(B - \pi(s)\bigr)}{B} \right\rfloor, \qquad \text{penalty} = g - q
$$

**Cost of the shares sold:**

$$
\text{cost}_{\text{out}} = \left\lceil \frac{C_h \cdot \min(s, n_h)}{n_h} \right\rceil
$$

**Performance fee**, on the gain net of the penalty and of the management fee, which already
lowered $g$:

$$
\text{performance} = \left\lfloor \frac{\max\bigl(q - \text{cost}_{\text{out}},\, 0\bigr) \cdot r_p}{B} \right\rfloor
$$

**Paid to the seller's receiver:**

$$
\text{paid} = q - \text{performance}
$$

With an average cost $\bar c = C_h / n_h$ per share and a price $p = A / S$, and no rounding or penalty, this is:

$$
\text{performance} = r_p \cdot s \cdot \max(p - \bar c,\ 0)
$$

`value_of(owner, shares)` returns $(g, \text{penalty}, \text{performance}, \text{paid})$. With
$s = n_h$, $\text{paid}$ is the real value of the holding. The standard `preview_redeem`
computes the same without an owner, so without the performance fee.

### 6.2 Penalty rate

The rate is that of the highest tier exceeded, or zero:

$$
\pi(s) = \begin{cases} \pi_{k^\star}, & k^\star = \max\{k : s > \theta_k\} \\ 0, & \text{if } s \le \theta_1 \text{ (or no tiers)} \end{cases}
$$

### 6.3 Redeem $s$ shares

Refused if $s > n_h$. Otherwise the vault escrows $s$ shares in the withdrawal queue and burns
them at settlement. The initial call returns $\text{paid}(s)$ if it settles immediately, or `0`
while queued; queued requests are valued again under the current fee schedule when they settle.
A partial request below 1,000 shares is rejected unless it exits the full balance or the holder's
entire balance is already below 1,000 shares.

### 6.4 Withdraw $a$ assets

This is the SEP-56 `withdraw` call: the investor names target assets $a$, not the shares to sell.
The vault computes and escrows the required shares when the request is created. If queued, the
sale amounts are recomputed when those shares settle.

The shares burned are the fewest whose sale pays at least $a$. Within a penalty tier, $\text{paid}(s)$ grows with $s$, but it drops where a tier starts. So the vault searches the ranges

$$
[0, \theta_1],\ [\theta_1 + 1, \theta_2],\ \dots,\ [\theta_n + 1, n_h] \qquad \text{(capped at } n_h\text{)}
$$

in order. The first range $[\ell, u]$ with $\text{paid}(u) \ge a$ holds the answer, found by
bisection:

$$
s^\star = \min \{\, s \in [\ell, u] : \text{paid}(s) \ge a \,\}
$$

On immediate settlement, the owner burns $s^\star$ shares and receives exactly $a$. The rounding
surplus $\text{paid}(s^\star) - a$ stays in the vault as penalty. For a queued request, the shares
and target $a$ are fixed but the sale is valued again at settlement; the payout is capped at $a$
and can be lower if the share value falls. Fee parameters are not snapshotted: settlement uses
the current penalty tiers and performance rate.

For example, take one tier $(\theta_1, \pi_1) = (100, 500)$, a price of 1 and no gain:
- $a = 100$: $\text{paid}(100) = 100$ in the first range, so $s^\star = 100$ with no penalty.
- $a = 101$: the first range tops out at $\text{paid}(100) = 100 < 101$. In the second range,
  $\text{paid}(s) = \lfloor 0.95\, s \rfloor \ge 101$ first at $s^\star = 107$. On immediate
  settlement, the investor burns 107 shares, receives 101, and 6 stay in the vault.

`max_withdraw(owner)` is the current $\text{paid}(n_h)$, before any later queue-settlement price
change.

### 6.5 Execution

- **Owner's cost**, detached when queued: $C_h \mathrel{-}= \text{cost}_{\text{out}}$.
- **Burn and payout**: after FIFO settlement has enough liquidity, the vault's `pay_exit` burns
  the escrowed shares and transfers $\text{paid}$ to the request's receiver.
- **Performance fee**: transferred in assets to $R$.
- **Penalty**: it stays in the vault and raises the value of the remaining shares. For their
  holders it is a gain like any other, and it pays the performance fee when they sell.

## 7. Fee changes

There is no dedicated `fee_delay`. Parameter changes use the vault curator timelock (`submit(SetFees)` → wait `timelock(SetFees)` → `set_fees`). Once due, `FeeLib::set_fees` accrues the management fee at the old rate, then stores the new parameters.

## 8. Executable examples

The tests in
[`contracts/trustline-vault/src/test.rs`](../contracts/trustline-vault/src/test.rs) execute these
formulas with the vault's actual `V = 1_000_000`. They cover management accrual, performance fees
after gains, cost-basis movement on transfer, penalty tiers, exact-asset withdrawals, and queued
settlement.
