# 05 — Tokenized gold: warehouse receipt on a Maverick market

Issue an allocated gold receipt, seed liquidity with price-following bins, and let a
permissionless keeper migrate out-of-range liquidity when the price moves.

Asset: `LBMA Allocated Gold — Vault Zurich` (`XAU-ZRH`), 6 decimals, 1.000000 token
= 1 fine troy ounce, quoted in a 6-decimal Token-2022 test stablecoin (`TUSD`).

Maverick is the right venue for a commodity receipt for one reason: bins have a
**kind**. A `Both ↔` bin follows the price in either direction, so market makers do
not have to babysit a range the way they do on CLMM. A keeper can then move stale
bins next to the active one, and LP withdrawals follow the move chain automatically.

---

## Asset design decisions

| Decision | This playbook | Why |
|----------|---------------|-----|
| Unit | 1 token = 1 fine troy ounce | Matches the bar's stated fineness and the LBMA quote |
| Decimals | 6 | Fractional ounces are normal (0.001 oz bars, kilo bars = 32.1507 oz) |
| Allocation | Allocated, serialised bars listed in the URI | Unallocated gold is a credit claim, not a receipt |
| Redemption | Transfer to the vault operator's wallet | Physical delivery happens off chain; the UI has no burn |
| Transferability | Whitelist + KYC only | Bearer metal must not move to unverified wallets |
| Fee | 5 bps capped at 0.01 oz | Vault administration fee |

---

## 0. Prerequisites

| Requirement | Value |
|-------------|-------|
| App / cluster | `bun run dev`, `localnet`, Sample Data **off** |
| Wallets | `VAULT` (issuer/operator), `DEALER`, `INVESTOR`, `KEEPER` |
| Quote mint | A 6-decimal **Token-2022** quote mint (`TUSD`, created in §3b). Pool legs share one token program, so the seeded legacy-SPL `mUSDC` cannot be pooled with the gold mint |
| Price reference | LBMA AM/PM fix, e.g. $2,650.00 per ounce |
| Oracle | Optional; the pool's deviation breaker reads the rails configured at birth |

---

## 1. Write the vault document

The metadata URI is the only place the bar list can live, and it cannot be updated
from the UI. Serve a document that is regenerated whenever the bar list changes:

```json
{
  "vault": "Zurich Freeport, Zone C",
  "operator": "Helvetia Bullion AG",
  "form": "allocated",
  "bars": [
    { "serial": "ZR-100241", "fineness": "999.9", "weightOz": "400.0000" },
    { "serial": "ZR-100242", "fineness": "999.9", "weightOz": "400.0000" }
  ],
  "totalOunces": "800.000000",
  "insurance": { "insurer": "Lloyd's syndicate 1234", "coverage": "all-risk" },
  "auditor": "Bureau Veritas",
  "lastAudit": "2026-09-30",
  "redemption": "physical, Zurich, minimum 1 kg"
}
```

One token per ounce means the mint supply must always be reconcilable to the bar
list. If you add bars, mint; if you deliver metal out, receive the tokens back into
the vault wallet and update the document.

---

## 2. Whitelist and KYC the participants

1. `Enterprise` → **Counterparty address book**: add `VAULT`, `DEALER`,
   `INVESTOR`, `KEEPER`.
2. **Whitelist import (one wallet per line)** → paste the four → **Apply whitelist**.
3. `KYC` → **KYC gate** → **Issue KYC** for `DEALER`, `INVESTOR`, `KEEPER`.
4. `KYC` → **Transfer-hook whitelist** → **Initialize hook** →
   **Build proofs** → **Update root**.
5. Register an entry per wallet (`Use for sender entry` /
   `Use for recipient entry` → **Register entry**).

The keeper only needs an entry if it ever holds the token; it usually just calls
`move_bins`, which is permissionless and needs no whitelist entry.

---

## 3. Create the gold mint

`RWA` → **Create RWA mint**:

| Field | Value |
|-------|-------|
| Decimals | `6` |
| Name | `LBMA Allocated Gold Vault Zurich` |
| Symbol | `XAU-ZRH` |
| URI (ISIN / NAV / prospectus) | `https://vault.example/xau-zrh/bars.json` |

Press **Create mint (generates a fresh keypair)** and recover the address from the
**Mint (defaults to the freshly created one)** placeholder with
[Reading the new mint's address](README.md#reading-the-new-mints-address) before
leaving the page — the pool step needs it.

Then **Register hook metas for this mint**.

---

## 3b. Create the quote mint and whitelist the custody accounts

1. `RWA` → **Create RWA mint**: **Decimals** `6`, **Name** `Test USD`, **Symbol**
   `TUSD`, stable URI. Recover the address, press
   **Register hook metas for this mint**, then mint `100000000000000` to the vault
   operator.
2. `Maverick` → **Pool selector**: **Mint A** = gold mint, **Mint B** = `TUSD`. Copy
   the **Derived pool PDA** (the pool does not exist yet).
3. Collect the custody addresses:

   ```bash
   bun examples/pdas.ts --pair <XAU-ZRH> <TUSD> \
     --maker <VAULT> --order-id 1700000000000 --escrow-mint <XAU-ZRH>
   ```

4. `Enterprise` → **Whitelist import**: add the pool PDA, the order PDA (for the
   OTC ask in §8), the hook authority PDA and the settlement escrow authority PDA
   to the four wallets → **Apply whitelist**.
5. `KYC` → **Transfer-hook whitelist**: paste the extended list → **Update root**,
   then register an entry for every wallet **and** every PDA again.

The pool vaults are owned by the pool PDA and the OTC escrow is owned by the order
PDA, so both are transfer counterparties for the hook. Skipping them makes the
first deposit or order placement fail with a missing-account error.

### Mint the receipted ounces and set the fee

| Field | Value | Result |
|-------|-------|--------|
| Amount | `800000000` | 800.000000 oz (raw, 6 decimals) |
| Holder | empty | To the vault operator |

Press **Mint to my ATA**.

| Field | Value |
|-------|-------|
| New transfer fee (bps) | `5` |
| New maximum fee | `10000` (0.01 oz) |

Press **Update transfer fee**.

Deliver the dealer's tranche with **Destination (transfer)** = `DEALER` and the
amount in raw ounces, after confirming the green compliance note.

---

## 4. Create the Maverick pool

On `Maverick`, set **Pool selector** → **Mint A** = gold mint, **Mint B** = `TUSD`
first. The create panel has no mint fields; it reads the selector, and
**Initialize pool** stays disabled until both are filled.

Then, in **Create Maverick pool**:

| Field | Value | Why |
|-------|-------|-----|
| Active bin id | `7886` | ≈ $2,650 per ounce at a 10 bp step |
| Bin step (bps) | `10` | 0.1% per bin — gold's daily range fits a few bins |
| Base fee (bps) | `5` | 0.05% per swap |
| Require KYC | `On` | Only attested participants |
| Oracle deviation (bps, 0 = off) | `200` | 2% breaker; gold gaps on macro prints |
| Settlement routing | `On` | Large OTC tickets settle T+N |
| T+N slots | `300` | ≈ 2 minutes on localnet |

Press **Initialize pool**.

Bin arithmetic: `price ≈ (1 + bin_step/10000)^bin_id`, bin 0 = 1.0.

```
bin = ln(2650) / ln(1.001) ≈ 7886
```

Both mints have 6 decimals, so the bin ratio is the human dollar price. If your gold
mint had 0 decimals, bin 0 would be $0.000001 per ounce and the same bin id would be
wrong — see the decimals note in [02](02-carbon-credit.md#variant-0-decimal-whole-tonne-credits).

### Load the pool

After **Initialize pool**, **Pool state** shows reserves, active bin and wiring
badges (`oracle`, `T+N`). On a reload, re-enter the two mints in **Pool selector**;
the **Derived pool PDA** recomputes and the state loads.

---

## 5. Seed liquidity with bin kinds

`Maverick` → **Add liquidity**. The subtitle is the rule to remember: *the first
deposit into a bin fixes its kind; moving kinds need a static baseline first.*

| Bin | Kind | Amount A (oz, raw) | Amount B (quote, raw) | Purpose |
|-----|------|--------------------|-----------------------|---------|
| `7886` (active) | `Both` | `50000000` (50 oz) | `132500000000` (132,500) | Core market-making bin, follows price either way |
| `7896` (+0.1%) | `Right (follow price up)` | `25000000` (25 oz) | `66250000000` | Sell-side ladder above spot |
| `7876` (−0.1%) | `Left (follow price down)` | `25000000` (25 oz) | `66250000000` | Buy-side ladder below spot |
| `7800` (deep) | `Static` | `100000000` (100 oz) | `0` | Static baseline that never migrates |

Press **Add liquidity** per bin.

Notes:

- A bin only accepts liquidity once its array exists. Open **Admin & maintenance**,
  set **New bin array index** and press **Initialize bin array** first. The array
  index is derived from the bin id (50 bins per Maverick array).
- `Static` is the baseline: if you want a bin to change kind later, it needs a
  static deposit first.
- `My position & balances` shows the position at the selected bin; shares follow the
  move chain on withdrawal.

### Project the kinds before committing size

`Maverick` → **Mode simulator**:

1. Click the mode tab (`Static`, `Right →`, `← Left`, `Both ↔`) — the selection
   drives the kind used by **Add liquidity**.
2. Drag **Price / NAV move** from −30% to +30%.
3. Read **Bins migrated**, **In-range depth**, **Projected daily fees** and the
   fee-vs-impermanent-loss split.

This is a projection, not on-chain state; the `Before` histogram is read from the
pool, the `Projected` one is modelled. Use it to choose between `Both ↔` (more fee
capture, more migration) and `Static` (predictable inventory, no migration).

---

## 6. Swap against the ladder

`Maverick` → **Swap**:

| Field | Value |
|-------|-------|
| Amount in | raw input, e.g. `5000000000` (5,000 quote) |
| Min out | your slippage floor in the output token's raw units |
| Direction | `Sell B (bins down)` to buy gold with quote, `Sell A (bins up)` to sell gold |
| Arrays to walk | number of bin arrays the trade may cross |
| Trade id | non-zero only on settlement-wired pools |

The page derives the bin-array walk from the active bin, so you only choose how many
arrays to allow. Too few and a large order stops early; too many and the transaction
burns compute on empty arrays.

---

## 7. Run the keeper

When the price moves a few bins, the active bin is no longer where the liquidity is.
`Move bins (keeper)` is permissionless — anyone can re-centre stale liquidity, and
that is the mechanism that keeps a commodity market from going hollow.

1. `Maverick` → **On-chain bins (kind & move chain)** → set **Bin id to inspect**
   to the stale bin and press **Inspect active bin**. The table shows which bins
   hold liquidity, the kind fixed by the first deposit, the raw price, and where a
   previous `move_bins` sent each bin's shares.
2. Find the out-of-range bin — e.g. `7886` after the price ran to `7906` — and the
   destination, usually just below the new active bin (e.g. `7905`).
3. `Maverick` → **Move bins (keeper)**:
   - **From bin** = `7886`
   - **To bin** = `7905`
4. Press **Move bins**.

What changes:

- The from-bin keeps its **shares** but its **reserves** go to zero.
- Withdrawals follow the move chain, and the walk is derived from the chain — the
  **Remove liquidity** panel shows a read-only **Hop arrays (derived)** field. There
  is nothing to type, even after several moves.
- The simulation in **Mode simulator** is not affected; the on-chain table is the
  truth.

### Remove liquidity after a move

`Maverick` → **Remove liquidity**:

| Field | Value |
|-------|-------|
| Burn shares | how many shares to redeem |
| Min out A / Min out B | slippage floors |
| Hop arrays (derived) | read-only |

Press **Remove liquidity**. The hops are computed from the move chain, so a position
that was moved twice still withdraws in one transaction.

---

## 8. Settle a large OTC ticket

A 200 oz ticket is too large for the ladder. Take it through the book and settle
T+N:

`Orders` → **Market**: **Base mint** = gold, **Quote mint** = `TUSD`. If the
book does not exist, initialize it with **KYC rail** = `Require attestation`,
**Settlement rail** = `T+N escrow`, **T+ slots** = `300` → **Initialize book**.

`Place a limit order`:

| Field | Value |
|-------|-------|
| Side | `Ask — sell base` |
| Price | `2658.50` (a $8.50 premium over the fix) |
| Amount | `200000000` (200 oz, raw base) |
| Expiry (minutes) | `240` |
| Order id | `1700000000000` — the PDA you whitelisted in §3b |

The buyer presses **Fill**, reviews the pre-filled **Max pay**, and confirms. The
cash lands in `Settlement` → **Settlement escrows**; press **Load**, wait for
`ready`, then **Settle (release escrow)** with the agent's secp256k1 signature.

---

## 9. Pause transfers during a bar audit

Gold receipts must freeze while the vault is being audited or re-barred:

1. `KYC` → **Transfer-hook whitelist** → **Pause transfers**.
2. Every RWA transfer of `XAU-ZRH` is rejected by the hook, including the operator's
   own deliveries.
3. After the audit, **Resume transfers**.

Note the difference between the two pause controls:

| Control | Scope | What it stops |
|---------|-------|---------------|
| **Pause transfers** (hook) | All RWA transfers of every mint using this hook | Token movement |
| **Pause pool** (pool admin) | One pool | Swaps and deposits; withdrawals and fee collection stay open. Cleared with **Unpause pool** |
| **Trip circuit breaker** (anyone, oracle-wired pools) | One pool | Latches the *oracle's* pause on that pool. The oracle owner clears it with **Unpause pool (oracle owner)** in the risk panel — the admin pause toggle does not |

A physical audit should usually **Pause transfers** *and* **Pause pool**, so neither
the token nor its market can move on stale bar data. If the pool is oracle-wired,
**Trip circuit breaker** adds the oracle latch on top; it is cleared separately by
the oracle owner.

---

## 10. Inspect and reconcile

- `Audit` → **NAV attestation viewer** on the gold mint: name, ticker, URI,
  decimals, total supply (ounces outstanding), and any detected ISIN. A warehouse
  receipt normally has no ISIN, so that row reads `—`.
- `Audit` → **On-chain audit trail**: export the trade blotter with **Export CSV**,
  or print a statement.
- `Portfolio` → **Rescan**: Maverick positions (with the bin and shares) and pending
  escrows. Resting limit orders are read on the `Orders` page.

Reconciliation rule: `total supply × 1 oz = total ounces in the bar document`. If
they diverge, the bars and the chain disagree — pause transfers before doing
anything else.

---

## Verification checklist

- [ ] `RWA` → supply equals the ounces in the URI document.
- [ ] `Maverick` → **Pool state** active bin is `7886`; KYC and settlement badges
      are present.
- [ ] The pool PDA, order PDA, hook authority and settlement escrow authority each
      have a whitelist entry.
- [ ] Each seeded bin shows the intended **Kind** in **On-chain bins**, with
      non-zero reserves.
- [ ] `Mode simulator` reports `In-range depth` consistent with the kind you chose.
- [ ] After **Move bins**, the from-bin has `0` reserves but keeps its shares, and
      **Hop arrays (derived)** is populated on the remove panel.
- [ ] **Remove liquidity** succeeds in one transaction after a move.
- [ ] **Pause transfers** blocks a test transfer; **Resume transfers** restores it.
- [ ] OTC fill creates an escrow that only releases after the slot with a valid
      agent signature.

## Failures specific to gold

| Symptom | Cause | Fix |
|---------|-------|-----|
| Pool quotes at 1/1,000,000 of the expected price | Mint decimals differ from the quote's | Match decimals or offset the active bin |
| `Add liquidity` rejected | The bin's array does not exist, the bin is too far from active, or the pool PDA has no whitelist entry | **Initialize bin array**, stay in range, and whitelist the pool PDA |
| First deposit fails with a missing account | The pool PDA is not whitelisted (it owns the vaults) | Add it, `Update root`, re-register every entry |
| Kind switch has no effect | The first deposit already fixed the kind | Deposit static first, then add the moving kind |
| Withdrawal fails after a keeper move | Stale page state | Refresh, then **Remove liquidity** (hops are re-derived) |
| Swap stops early | `Arrays to walk` too small | Raise it, or split the order |
| Transfers rejected during audit | **Pause transfers** is on | **Resume transfers** after the audit |
| `Min out` reverts on a volatile print | Breaker tripped or price gapped past the walk | Retry with a wider min/max or fewer arrays |
| Supply ≠ bar document | Unreconciled mint or delivery | Pause transfers, fix the bar document or the supply, then resume |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Decimals | 6 | Fractional ounces; quote also 6 |
| Spot | $2,650/oz | LBMA fix |
| Active bin | 7,886 | `ln(price)/ln(1.001)` |
| Bin step | 10 bps | Gold's daily range fits a handful of bins |
| Base fee | 5 bps | 0.05% |
| Bin kinds | `Both ↔` core, `Right →` / `← Left` ladder, `Static` base | First deposit fixes it |
| Keeper | `Move bins`, permissionless | Remove follows the derived hop chain |
| Oracle deviation | 200 bps | Macro-print gaps |
| Settlement | T+300 slots | Large OTC tickets |
| Transfer fee | 5 bps, cap 0.01 oz | Vault administration |
