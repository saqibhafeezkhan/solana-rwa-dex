# 08 — Oracle circuit breaker: a risk-desk runbook

Arm the deviation breaker on a tokenized-fund market, then run the response when
it fires: who may trip it, what stops, what keeps working, and how to bring the
pool back without leaving it exposed.

Asset: `TBILL` — a tokenized T-bill fund share, 6 decimals, quoted in a 6-decimal
Token-2022 stablecoin (`TUSD`). Venue: a DLMM pool, though the same panel and the
same rules apply on CLMM and Maverick.

The breaker is **not** a price feed and not an admin pause. It is a comparison
between the pool's own spot price and the TWAP the oracle last wrote; when the
gap exceeds the threshold the pool recorded at birth, the oracle latches a pause
on that one pool. It can only ever pause — releasing it is a separate, owner-only
call.

---

## What the breaker measures

| Piece | Value | Where it comes from |
|-------|-------|---------------------|
| Spot price | The pool's current price | Pool state — never a caller argument |
| TWAP | Time-weighted average over the oracle's window (30 minutes) | `[b"oracle", pool]` observations |
| Deviation | `abs(spot − twap) / twap`, in basis points | Computed on chain |
| Threshold | **Oracle deviation (bps, 0 = off)** at pool creation | Immutable after birth |
| Warm-up | At least 2 observations inside the window | Observations are written by swaps |

Consequences worth internalising before the first trade:

- **The threshold cannot be changed.** It is part of the pool's wiring, like the
  KYC rail and the settlement route. Choosing it is a deployment decision; a
  mistake means creating a new pool.
- **No swaps → no observations → no protection.** Until the pool has two
  observations within the window, a trip attempt is a no-op, not an error. The
  program logs `ALARM: trip_breaker no-op (TWAP not available yet; < 2
  observations)` so a keeper can alarm on it.
- **The breaker reads stored values, not UI prices.** The panel's deviation and
  the on-chain check use the same units; do not compute your own percentage from
  a chart and expect the two to agree to the basis point.

---

## 0. Prerequisites

| Requirement | Value |
|-------------|-------|
| App / cluster | `bun run dev`, `localnet`, Sample Data **off** |
| Wallets | `ISSUER` (pool authority), `RISK` (oracle owner — set at pool creation), `LP`, `TRADER` |
| Pool | DLMM `TBILL/TUSD`, liquidity seeded in the bins around the active bin |
| Rails | KYC **On** (if holders are gated), **Oracle deviation (bps, 0 = off)** non-zero, settlement optional |
| Monitoring | A keeper that reads the oracle account (or the panel) and trips on a breach |

The oracle owner is fixed atomically with the pool, and only that key can unpause
an oracle trip. Decide who holds it before creating the pool — typically the risk
desk, never the market maker's hot wallet.

---

## 1. Choose the threshold

There is one field for it: `DLMM` → **Create DLMM pool** → **Oracle deviation
(bps, 0 = off)**.

| Asset class | Suggested threshold | Why |
|-------------|---------------------|-----|
| T-bill fund, deep book | 150 bps | NAV moves by cents; a 1.5% gap is a broken quote, not a market move |
| Investment-grade credit fund | 400 bps | Wider legitimate spread and thinner books |
| Tokenized gold | 200 bps | Macro prints gap the fix; see [05](05-tokenized-gold-maverick.md) |
| Carbon / private assets | 600+ bps | Valuation is appraisal-driven and quotes are thin |

**0 disables the rail permanently.** The pool then has no oracle account at all
and the panel shows a `no oracle` badge — not "loading", not "unavailable". If
you want the protection, set the number at creation.

The other rails are decided in the same form and are equally immutable: **Require
KYC**, **Settlement routing**, **T+N slots**, **Bin step (bps)**, **Base fee
(bps)**.

---

## 2. Verify the panel before anyone trades

`DLMM` → **Oracle & risk** panel:

| Field | Should read |
|-------|-------------|
| Badge (top right) | `live` |
| **TWAP price** | The price the oracle last wrote — `—` until the first observation |
| **Spot (pool)** | The pool's current price |
| **Deviation** | `N bps`, with `threshold M bps` under it |
| **Observations** | A count and the timestamp of the newest write |
| **Oracle account** | The `[b"oracle", pool]` PDA |
| **Max deviation** | The bps you chose in §1 |

The bar under the numbers turns `warn` at 70% of the threshold and `danger` at
100%. It is a read-out, not a control: nothing in the panel changes the
threshold.

---

## 3. Warm the TWAP, then arm

1. Have `LP` add a small amount of liquidity and `TRADER` do two small swaps.
   Each swap writes an observation; two are enough for a TWAP.
2. Re-read **Observations** — it must be at least `2`, and the newest timestamp
   must be inside the 30-minute window.
3. Until then, treat the pool as unprotected: cap order size, or keep the admin
   pause on and take trades off chain.

A keeper that wants a machine-readable signal can watch for the `ALARM:` log
line, which is emitted exactly when the breaker could not act because history is
missing.

---

## 4. When it trips

Three ways a pause can land, and they are independent:

| Switch | Who | Cleared by | Scope |
|--------|-----|------------|-------|
| **Trip circuit breaker** | Anyone (the price comes from the pool's own state) | **Unpause pool (oracle owner)** in **Oracle & risk** | That pool's oracle latch |
| **Pause pool** | Pool authority | **Unpause pool** in the pool's Admin panel | That pool's admin flag |
| **Pause transfers** | Hook authority (`KYC` page) | **Resume transfers** | Every RWA transfer using the hook |

The buttons differ by venue: `DLMM` and `Maverick` label it **Trip circuit
breaker**; `CLMM` labels it **Trip circuit breaker (arm)**. The trip only
succeeds when the deviation is genuinely past the threshold; inside it, the
instruction fails with `PriceWithinThreshold` and the pool stays live. That is
the property to rely on: a caller cannot pause a healthy pool.

While a pool is paused, expect the banner:

> ⚠ Pool paused: swaps, deposits and bin moves are refused; withdrawals stay
> open. Unpause from the Admin panel.

What still works is deliberate — capital is never trapped:

| Action | Paused pool |
|--------|-------------|
| Swap | Refused |
| Add liquidity / deposit | Refused |
| `Move bins` (Maverick keeper) | Refused |
| Remove liquidity / withdraw | Allowed |
| Collect fees | Allowed |
| `Cancel settlement` / `refund_expired` | Allowed (different program) |
| Order book `Cancel order` | Allowed (different program) |

---

## 5. Bring it back

1. Confirm spot is back inside the threshold — the **Deviation** number in
   **Oracle & risk** below **Max deviation**, and the bar out of `danger`.
2. The oracle owner presses **Unpause pool (oracle owner)**.
3. If the pool was *also* admin-paused, the authority presses **Unpause pool** in
   the Admin panel as well. The two flags are separate, and the oracle unpause
   does not clear the admin one (nor the other way round).

The oracle owner's unpause is unconditional: the program does not re-check the
deviation. That is why step 1 matters — unpausing while spot is still outside the
threshold simply lets the next swap trip it again.

---

## 6. Incident runbook

1. **Detect** — keeper alarm on `ALARM: trip_breaker no-op`, a `danger` bar, or a
   failed swap with a max-deviation error.
2. **Contain** — press **Trip circuit breaker** if it has not fired; optionally
   **Pause transfers** on `KYC` if the asset's NAV itself is in question.
3. **Triage** — is the spot price wrong (a thin book, a fat-finger swap, an
   oracle-free asset repricing) or is the TWAP stale (no trades for 30 minutes,
   which empties the window)?
4. **Wait for the window** — the TWAP only moves when observations are written.
   A pool whose price is legitimately repricing will look "outside threshold"
   until enough post-reprice trades land. Do not unpause on every tick.
5. **Restore** — unpause (oracle, then admin if needed), then watch the next few
   swaps. If it re-trips immediately, the market has genuinely repriced; the
   correct response is a new pool with a threshold sized for the new regime, not
   repeated unpauses.
6. **Record** — `Audit` → **On-chain audit trail** for the swaps around the
   incident. The trip itself is a pool/oracle state change, so note the slot and
   the signature in the incident log.

---

## Verification checklist

- [ ] **Oracle & risk** shows `live`, the expected **Max deviation**, and a
      non-zero **Observations** count.
- [ ] Two swaps produce two observations; the **TWAP price** is no longer `—`.
- [ ] A trip attempt while spot is inside the threshold fails
      (`PriceWithinThreshold`) and the pool still swaps.
- [ ] A trip while outside pauses the pool: swap and deposit refused,
      withdrawal succeeds.
- [ ] **Unpause pool (oracle owner)** from the oracle owner clears the latch.
- [ ] **Unpause pool** from the admin panel clears only the admin flag — with
      both set, the pool needs both unpauses.
- [ ] A pool created with **Oracle deviation** `0` shows `no oracle` and cannot
      be tripped at all.

## Failures specific to the breaker

| Symptom | Cause | Fix |
|---------|-------|-----|
| `no oracle` badge on a new pool | Threshold was `0` at creation | Create a new pool with a non-zero **Oracle deviation** |
| Deviation stays `—` | Fewer than two observations | Trade twice, or accept the pool is unprotected |
| Trip fails, pool stays live | Spot is inside the threshold (`PriceWithinThreshold`) | Nothing to trip; watch the meter |
| Trip does nothing but logs `ALARM:` | TWAP unavailable (no history in the window) | Warm the pool, or pause manually |
| `Unpause pool (oracle owner)` is missing | Wallet is not the oracle owner | Ask the owner; the pool authority cannot do it |
| Pool still refuses swaps after the oracle unpause | Admin pause is also set | **Unpause pool** in the Admin panel |
| Pool re-trips a minute after unpausing | Spot is still outside the threshold | Let the TWAP catch up; consider a new pool |
| Deviation in a chart does not match the panel | Chart is showing a UI price, not the stored value the breaker compares | Trust the panel / on-chain state |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Venue | DLMM `TBILL/TUSD` | Same panel on CLMM and Maverick |
| Threshold | 150 bps | Immutable after creation |
| Window | 30 minutes | `DEVIATION_WINDOW_SECONDS` |
| Warm-up | 2 observations | Inert until then |
| Observations kept | 100 | Ring buffer on the oracle account |
| Trip caller | Anyone | Price supplied by the pool's own CPI |
| Unpause | Oracle owner | Unconditional; check the deviation first |
| Admin pause | Pool authority | Separate flag, separate unpause |
| Exit behaviour | Withdrawals, fee collection, order cancel, escrow cancel | Never blocked by a pause |
