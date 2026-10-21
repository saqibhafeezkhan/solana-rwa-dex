# 03 — Tokenized treasury bill: TWAMM distribution and maturity redemption

Issue a 91-day treasury bill at a discount, place it on a time-weighted schedule
instead of in one clip, trade it on a concentrated range, and pay holders back at
par through T+N escrow.

Asset: `US T-Bill 2027-01-15` (`TBILL-0127`), 6 decimals, priced per $100 of face
value, quoted in a 6-decimal stablecoin.

Why TWAMM for bills: a dealer rarely wants to dump $1M of paper in one print, and a
treasury desk rarely wants to lift it in one clip. A long-term order sells a fixed
amount per second, so the flow is spread over days and the market sees a steady
schedule instead of one spike. The `Execution analytics` panel then shows the
average price actually achieved against spot.

---

## 0. Prerequisites

| Requirement | Value |
|-------------|-------|
| App / cluster | `bun run dev`, `localnet`, Sample Data **off** |
| Wallets | `ISSUER` (bill issuer / dealer), `BUYER` (treasury desk), `AGENT` (transfer agent), optional `LP` |
| Quote mint | A 6-decimal **Token-2022** quote mint (`TUSD`, created in §3b). TWAMM and CLMM carry one token program for both legs, so the seeded legacy-SPL `mUSDC` cannot be pooled with the bill mint |
| Settlement config | Must exist on the cluster (`Settlement` → **Initialize config** by the program deployer) |
| Hook authority | Funded — `KYC` → **Hooks escrow router** → **Fund hook authority** |

Bill terms used throughout:

| Term | Value |
|------|-------|
| Face value | $100.00 per token |
| Issue price | $98.7500 |
| Maturity | 2027-01-15 |
| Discount | 1.25 points over 91 days |

---

## 1. Create the bill mint

`RWA` → **Create RWA mint**:

| Field | Value | Note |
|-------|-------|------|
| Decimals | `6` | One token = $100 face; 6 decimals keeps human price = pool price |
| Name | `US Treasury Bill 2027-01-15` | |
| Symbol | `TBILL-0127` | Maturity encoded in the ticker |
| URI (ISIN / NAV / prospectus) | `https://desk.example/tbill-0127/terms.json` | CUSIP/ISIN, auction result, maturity |

Press **Create mint (generates a fresh keypair)**. Read the address from the
**Mint (defaults to the freshly created one)** placeholder using
[Reading the new mint's address](README.md#reading-the-new-mints-address) — the
placeholder cannot be selected and the reference is lost on navigation.

The terms document behind the URI should carry at least:

```json
{
  "cusip": "912797XX0",
  "isin": "US912797XX00",
  "faceValue": "100.00",
  "issuePrice": "98.7500",
  "issueDate": "2026-10-16",
  "maturityDate": "2027-01-15",
  "dayCount": "actual/360",
  "discountRate": "4.95%",
  "custodian": "Northwind Custody",
  "settlement": "DVP, T+1"
}
```

Press **Register hook metas for this mint** once, before any transfer.

---

## 2. Whitelist the desk, the agent and the LPs

1. `Enterprise` → **Counterparty address book**: add `BUYER`, `AGENT`, `LP`.
2. **Whitelist import (one wallet per line)**: `ISSUER`, `BUYER`, `AGENT`, `LP` →
   **Apply whitelist**.
3. `KYC` → **KYC gate**: **Issue KYC** for `BUYER`, `AGENT`, `LP`.
4. `KYC` → **Transfer-hook whitelist**: paste the same four wallets →
   **Build proofs** → copy the root → **Update root**.
5. Register an entry per wallet using **Use for sender entry** /
   **Use for recipient entry** → **Register entry**.

See [01 §3](01-tokenized-fund-nav.md#3-publish-the-whitelist-root) for the click
path; every RWA transfer needs both sides registered, and bills will be transferred
at maturity.

---

## 3. Issue the paper

`RWA` → **Mint, transfer & fees**:

| Field | Value | Result |
|-------|-------|--------|
| Amount | `100000000000` | 100,000 bills (raw, 6 decimals) |
| Holder | empty | Minted to the issuer wallet |

Press **Mint to my ATA**. **Mint inspector** should show `Supply =
100000000000`.

Deliver the initial dealer inventory to the desk if the trade is already agreed:

1. **Amount** = `10000000000` → 10,000 bills.
2. **Destination (transfer)** = `BUYER`.
3. Confirm the green note, press **Transfer (hook-approved)**.

Leave the rest with the issuer to seed the pool.

---

## 3b. Create the quote mint and whitelist the pool PDAs

Both venues in this playbook move Token-2022 on both legs, and the hook checks the
owner of both token accounts. The TWAMM genesis deposit happens **inside**
`Initialize pool`, so the pool PDA's entry must exist before you press the button.

1. `RWA` → **Create RWA mint**: **Decimals** `6`, **Name** `Test USD`, **Symbol**
   `TUSD`, stable URI. Recover the address, press
   **Register hook metas for this mint**, then mint `100000000000000` to the issuer.
2. `TWAMM` → **Pool selector**: **Mint A** = bill mint, **Mint B** = `TUSD`. Copy the
   **Derived pool PDA** (do not initialize yet).
3. `CLMM` → **Pool selector**: same pair, copy the CLMM **Derived pool PDA**.
4. `Enterprise` → **Whitelist import**: add both pool PDAs, the hook authority PDA
   (`KYC` → **Hooks escrow router**) and the settlement escrow authority
   (`bun examples/pdas.ts`) to the wallet list → **Apply whitelist**.
5. `KYC` → **Transfer-hook whitelist**: paste the extended list → **Update root**,
   then register an entry for every wallet **and** every PDA again.

If you later use the par-bid alternative in §8, derive that order's PDA too
(`bun examples/pdas.ts --pair <bill> <TUSD> --maker <ISSUER> --order-id 1700000000000`)
and repeat the root update.

---

## 4. Create the TWAMM pool — the genesis deposit sets the price

On `TWAMM`, set **Pool selector** → **Mint A** = the bill mint, **Mint B** =
`TUSD` first. The create panel has no mint fields and reads the pair from the
selector.

Then, in **Create TWAMM pool**, the subtitle is exact about it: *the initial deposit
is the genesis price `P = initial_B / initial_A`; the payer becomes the first LP.*

| Field | Value | Raw amount |
|-------|-------|------------|
| Initial amount A | `100000` bills | `100000000000` |
| Initial amount B | `9,875,000` quote | `9875000000000` |
| Require KYC | `On` | |
| Oracle deviation (bps, 0 = off) | `100` | 1% breaker around the bill price |
| Settlement routing | `On` | Proceeds routed to T+N escrow |
| T+N slots | `150` | ≈ 1 minute on localnet; size it to the DVP window you need |

Press **Initialize pool**.

The implied genesis price is `9,875,000 / 100,000 = 98.75` quote per bill, i.e. the
issue price. Both mints are 6-decimal, so this is also the human dollar price.

> The genesis deposit is not a free parameter you can fix later. If you deposit at
> the wrong ratio, the first swap will arbitrage it back to the market price and
> the initial LP takes the loss. Compute `initial_B` from the price you actually
> want before pressing **Initialize pool**.

### Load the pool

After **Initialize pool**, **Pool state** shows `Reserve A`, `Reserve B`, both sell
rates, `LP supply`, the last execution timestamp and the head order's expiry. On a
reload, re-enter the pair in **Pool selector** to bring it back.

---

## 5. Add and remove LP liquidity

`TWAMM` → **Liquidity**:

- **Amount A** = `5000000000` (5,000 bills), **Amount B** = `493750000000`
  (493,750 quote) → **Add liquidity**. Both legs are required; the deposit defines
  the ratio you are willing to hold.
- **Burn shares** + **Min out A** / **Min out B** → **Remove liquidity**.
  Withdrawals are not blocked while the pool is paused.

LP shares are your claim on reserves and on the fee flow from virtual orders.

---

## 6. Place the paper over 30 days instead of in one clip

`TWAMM` → **Long-term order**. The dealer wants to distribute 100 bills per day for
30 days out of inventory:

| Field | Value | Reasoning |
|-------|-------|-----------|
| Order id | `1` (the default) | Unique per wallet; it is the lookup key for the order panel |
| Amount in | `3000000000` | 3,000 bills in raw base units |
| Duration (s) | `2592000` | 30 days |
| Direction | `Sell A` | The dealer holds bills (A) and receives quote (B) |

The page derives the sell rate from amount / duration:
`3000000000 / 2592000 ≈ 1,157` raw per second = 0.001157 bills per second, about
100 bills per day, about 296,250 quote over the month at 98.75.

Press **Create order**. The input is escrowed, and the pool inserts the order into
its list; the walk box lists the live orders from the head up to the insertion
point, so there is nothing to type.

> **If you want to buy instead**, choose `Sell B` and enter the **quote** amount as
> `Amount in` (e.g. `296250000000` to buy 3,000 bills). One caveat: the analytics
> panel computes `Avg price achieved = proceeds / sold` and compares it with
> `reserve B / reserve A`. For `Sell A` both are quote-per-bill and comparable. For
> `Sell B` the achieved figure is bill-per-quote — the reciprocal — so the panel's
> average and `Slippage vs spot` are inverted for that direction. Read `Sold` and
> `Proceeds` from **My LP position & order** and divide them yourself.

### Crank the virtual orders

Press **Execute virtual orders (crank)** whenever you want the pool to settle the
elapsed time. A keeper or the desk can do this — it is permissionless. Each crank:

1. Advances the pool's sell rates for the elapsed time.
2. Books the counterparty flow into proceeds.
3. Moves the order list forward past expired orders.

Cranking more often gives smoother execution and a tighter `Avg price achieved`
versus spot; cranking rarely batches the same volume into fewer, larger price
impacts.

### Track execution

**Execution analytics** for the loaded order shows:

| Read-out | What it tells you |
|----------|-------------------|
| Progress meter | `sold / amountIn`, from chain |
| Escrowed / Remaining | Total order size and what is left |
| Proceeds | Quote received so far |
| Time left | To expiry, from the order's timestamp |
| Avg price achieved | `proceeds / sold` |
| Spot price | `reserve B / reserve A` right now |
| Slippage vs spot | Negative means you executed below spot; comparable for `Sell A` only |
| Sell rate | Raw input per second |
| Fill curve | **Projected** linear fill; actual execution depends on cranks and flow |

### Withdraw or cancel

- **Withdraw proceeds** — moves accumulated output to your wallet. Proceeds routed
  through settlement appear in `Settlement` instead.
- **Cancel order** — the cancel prefix is derived from the live order list; the
  unsold input is refunded and the order leaves the list.

---

## 7. Offer a secondary market on CLMM

Bills also trade intraday. Create a concentrated range instead of another TWAMM
pool so the desk can quote around the bill's carry.

On `CLMM`, set **Pool selector** → **Mint A (token0)** = bill mint, **Mint B
(token1)** = `TUSD`. As on the other venues, the create panel reads the pair
from the selector.

Then, in **Create CLMM pool**:

| Field | Value | Reasoning |
|-------|-------|-----------|
| Initial price (B per A) | `98.75` | Matches the TWAMM genesis price |
| Tick spacing | `10` | Fine-grained around par |
| Fee pips | `500` | 0.05% per swap |
| Require KYC | `On` | |
| Oracle deviation (bps, 0 = off) | `100` | |
| Settlement routing | `On` | |
| T+N slots | `150` | |

Press **Initialize pool**.

### Pick ticks

A CLMM tick `t` has price `1.0001^t`. For a bill near $98.75:

```
tick = ln(98.75) / ln(1.0001) ≈ 45,926
```

A ±0.25% band is about ±25 ticks (`ln(1.0025)/ln(1.0001) ≈ 25`), so with a `10`
spacing the range is `45,900` to `45,950`. `Add liquidity` requires **Lower tick**
and **Upper tick** to be multiples of the spacing, plus **Liquidity (L)** — a raw
liquidity number, not a token amount. The fee-growth read-out in **Yield & fee analytics** shows what the
position has earned; **Collect fees** sweeps it.

### Swap with an explicit tick walk

`CLMM` → **Swap**:

| Field | Value |
|-------|-------|
| Amount in (raw) | e.g. `5000000000` (5,000 quote) |
| Min amount out | your slippage floor, in raw bill units |
| Direction | `Sell B → Buy A` to buy bills with quote, `Sell A → Buy B` to sell bills |
| Trade id | non-zero only on settlement-wired pools |
| Tick walk (comma-separated) | initialized ticks in the direction of travel |

The walk must list the initialized ticks the trade may cross, in travel order
(descending when selling A→B, ascending when selling B→A). If the walk is short the
swap stops early and `Min amount out` fails; if it is long the program walks ticks
that hold no liquidity and burns compute. On a fresh pool the ticks you just seeded
are the walk.

---

## 8. Pay holders at par through settlement escrow

Bills mature at face value. The UI has no "redeem" instruction, so the issuer pays
par with a settlement escrow per holder — the same rail used for DVP trades.

`Settlement` → **Clearinghouse config** first (once per cluster):

1. **Initialize config** if the badge says not deployed (program deployer only).
2. **Transfer agent secp256k1 (128 hex, x‖y)** → **Set transfer agent**. This is
   the key that signs releases; it does not need SOL.
3. Optional **Per-mint agent** — set a different agent for `TBILL-0127`.
4. **Hooks authority (base58)** — register the hooks PDA if AMM output must be
   escrowed, then **Set hooks authority**.
5. If escrow creation starts failing, top up the hook authority on `KYC`.

Then, per holder:

`Settlement` → **Escrow lifecycle**:

| Field | Value |
|-------|-------|
| Trade id | a fresh unique id (never reuse a live one) |
| Settlement slot (absolute) | the slot at/after maturity |
| Amount | `1000000000000` for 10,000 bills at par (raw, 6 decimals) |
| Mint | `TUSD` |
| Source token account | defaults to the issuer's ATA for that mint |
| Taker (recipient wallet) | `BUYER` (or `LP`) |

Press **Initiate settlement (lock escrow)**. The quote is now locked and addressed
to the holder.

> **Amount is per escrow, in raw base units.** 10,000 bills at par = $1,000,000 =
> `1000000000000` raw at 6 decimals. Enter one escrow per holder, sized to their
> holding; the rail does not pro-rate a list of holders.

At maturity:

1. Ask the transfer agent to sign `keccak256(trade_id ‖ taker ‖ mint ‖ amount)` and
   return `r ‖ s` (128 hex) + recovery id.
2. `Settlement` → **Settlement escrows** → **Load** the row → wait for `ready`.
3. Paste the signature and recovery id → **Settle (release escrow)**.

The holder receives the stablecoin; the issuer still holds the bill tokens, which
it retires off chain against the custodian's record. If the holder fails to
complete their side, **Cancel settlement** refunds the issuer; if nobody acts,
**Refund expired escrow** is the cleanup, and **Close & reclaim rent** recovers the
vault rent.

### Alternative: let the desk sell the bill back

Instead of pre-funding an escrow, the issuer can leave a standing bid at par
(price `100.0000`) on `Orders`. Holders sell into it, and the book's settlement
rail routes the payment through escrow. That is operationally lighter but ties up
issuer cash in the order's escrow from the moment it is placed.

---

## 9. Reconcile

`Audit`:

- **NAV attestation viewer** on the bill mint: legal name, ticker, URI, decimals,
  supply, plus `Detected CUSIP` — the viewer parses CUSIP/ISIN patterns out of the
  metadata, so a CUSIP in the URI or name field is surfaced here.
- **On-chain audit trail**: every transaction the connected wallet signed, with
  slot, block hash, time and status. **Export CSV** for the trade blotter;
  **Print / PDF** for the monthly pack.

`Portfolio` → **Rescan** lists the TWAMM LP shares, CLMM positions
(**Liquidity positions**), the live long-term order (**TWAMM orders**) and any
pending **Settlement escrows** addressed to the wallet.

---

## Verification checklist

- [ ] `TWAMM` → **Pool state**: `Reserve A` / `Reserve B` match the genesis deposit;
      both sell rates are `0` before the first order.
- [ ] Both pool PDAs, the hook authority and the settlement escrow authority have
      whitelist entries (the genesis deposit needs them at creation).
- [ ] **My LP position & order**: LP shares equal the deposit; the order shows
      `Direction Sell B`, the escrowed amount, and `Status live`.
- [ ] After a crank, `Sold` and `Proceeds` increase and `Last execution` moves.
- [ ] `Execution analytics` average price is within a few bps of 98.75 for a
      genesis-priced pool.
- [ ] **Withdraw proceeds** increases the wallet's quote balance.
- [ ] `CLMM` → **Pool state** active tick is near `45,926`; **My position** shows
      the range you seeded.
- [ ] `Settlement` escrow locks the exact par amount and settles only after the
      slot with a valid agent signature.
- [ ] `Audit` shows the maturity release with `success`.

## Failures specific to bills

| Symptom | Cause | Fix |
|---------|-------|-----|
| First swap moves price a long way | Genesis deposit ratio was wrong | There is no re-genesis; create a new pool with the right ratio |
| `Initialize pool` fails with a missing hook account | The TWAMM pool PDA is not whitelisted, and the genesis deposit can't pass the hook | Whitelist the pool PDA, `Update root`, re-register every entry, then initialize |
| Long-term order never fills | No counterparty flow, or no crank | Crank regularly; seed LP liquidity for the pool to trade against |
| `Max pay` / `Min amount out` reverts on CLMM | Tick walk missing an initialized tick | Add the crossed ticks in travel order |
| Order cancel reverts | The order list changed since the walk was derived | Reload the pool and retry (the walk is re-derived on send) |
| `Set transfer agent` disabled or rejected | Not the settlement config authority | Use the deployer key |
| Escrow initiate fails | Hook authority out of lamports, or trade id already used | **Fund hook authority**; pick a fresh trade id |
| Settle rejected although the slot passed | Signature mismatch — wrong mint or amount ordering | Re-sign `trade_id ‖ taker ‖ mint ‖ amount` exactly |
| Par payment and trade id collide | Reused a live id | Each escrow needs its own id, cluster-wide |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Decimals | 6 | Human price = raw ratio with a 6-dec quote |
| Issue price | 98.75 | Genesis deposit decides it |
| TWAMM order | 3,000 bills / 30 days | Rate ≈ 1,157 raw bills per second ≈ 100 bills/day |
| Oracle deviation | 100 bps | Bills are low-volatility; keep the breaker tight |
| CLMM fee | 500 pips | 0.05% |
| CLMM ticks | ~45,926 | `ln(price)/ln(1.0001)`, multiples of spacing |
| Settlement | T+150 slots | Scale to the real DVP cycle |
| Redemption | Par escrow per holder | No redeem instruction exists |
