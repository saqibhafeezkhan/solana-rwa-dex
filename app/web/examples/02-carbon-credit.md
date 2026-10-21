# 02 — Create a carbon credit and retire it through the UI

Issue a verified carbon removal credit on chain, open a market around its spot
price, sell a forward delivery to a corporate buyer, and retire the tonne against
a registry account.

Asset: `Katingan Peatland Restoration — VCU Vintage 2024` (`VCU24`), 6 decimals,
1.000000 token = 1 tonne CO2e, whitelisted registry + buyers only.

This playbook follows the same compliance chain as
[01](01-tokenized-fund-nav.md), so the whitelist mechanics are abbreviated. What is
specific to carbon is: **serial numbers instead of ISINs**, **retirement instead of
redemption**, and a **settlement window measured in days** because a registry
issuance or retirement is not instant.

---

## What "a carbon credit" has to carry

| Attribute | On chain? | Where |
|-----------|-----------|-------|
| Project / methodology | Off chain | Metadata URI document |
| Vintage year | In the symbol (`VCU24`) | Token metadata |
| Serial block | Off chain, one document per block | Metadata URI |
| Tonnes | On chain | Token amount, 1 token = 1 tCO2e |
| Registry | On chain, as a whitelisted wallet | Transfer-hook entry |
| Retirement record | Off chain receipt + on-chain transfer to the retirement wallet | Registry workflow |
| Double-counting controls | On chain | Pause flag, whitelist root, permanent delegate |

There is **no burn instruction** in the UI. Retirement is therefore modelled as a
transfer to a registry-controlled retirement wallet that never sends the tokens
back out. That is the same custody pattern registries use off chain (retire to a
registry account), and the transfer hook lets you enforce it: only whitelisted
wallet pairs can move the token at all.

> Decimals note: this playbook uses **6 decimals** so that 1.000000 = 1 tonne and
> the DLMM bin price equals the human dollar price (both mints 6 decimals). If your
> registry model needs **0 decimals** (whole tonnes only), see
> [the 0-decimal variant](#variant-0-decimal-whole-tonne-credits) — the pool bin
> arithmetic changes and the standard NAV shortcuts will mislead you.

---

## 0. Prerequisites

- App running, cluster `localnet`, wallet connected with SOL, **Sample Data off**,
  role `Admin`, execution mode `Direct`.
- Four wallets: `DEVELOPER` (issuer), `REGISTRY` (retirement/custody account),
  `BUYER` (corporate offtaker), `MARKET_MAKER` (optional liquidity provider).
- A 6-decimal **Token-2022** quote mint (`TUSD`, created in §5b). Pools carry one
  token program for both legs, so the seeded legacy-SPL `mUSDC` cannot be paired
  with a Token-2022 credit mint.
- The registry has issued a serial block off chain and given you the document that
  the metadata URI will serve.

---

## 1. Prepare the registry document

The metadata URI is written once at mint creation and cannot be changed from the
UI, so serve a document that covers the **whole serial block** rather than one
file per tonne:

```json
{
  "registry": "Verra",
  "projectId": "VCS-1234",
  "projectName": "Katingan Peatland Restoration",
  "methodology": "VM0007",
  "vintage": 2024,
  "serialBlock": "1234-2024-001-000123456 to 000223455",
  "tonnes": 100000,
  "removalType": "avoided-emissions + peatland restoration",
  "sdgClaims": ["13", "15"],
  "documents": {
    "issuance": "https://registry.example/vcs-1234/issuance.pdf",
    "monitoringReport": "https://registry.example/vcs-1234/mr-2024.pdf"
  }
}
```

If you later issue another serial block, create a **new mint** for it — one mint per
vintage/serial block is the cleanest mapping, and it keeps retirement accounting
per block.

---

## 2. Whitelist the registry, the buyer and the market maker

1. `Enterprise` → **Counterparty address book**: add `REGISTRY`, `BUYER`,
   `MARKET_MAKER` with labels you will recognise during settlement.
2. Same panel → **Whitelist import (one wallet per line)**: paste `DEVELOPER`,
   `REGISTRY`, `BUYER`, `MARKET_MAKER`.
3. **Apply whitelist**. The table should show **in whitelist** for each.

The registry account is the important one: it is the only address credits are ever
allowed to leave circulation to, so it must be in the tree *before* the first
transfer.

---

## 3. Issue KYC attestations

`KYC` → **KYC gate**:

1. **Initialize KYC mint** if the badge says `not deployed` (program deployer only).
2. **Institution wallet** = `BUYER` → **Issue KYC**.
3. Repeat for `REGISTRY` and `MARKET_MAKER`.
4. **Verify my attestation on-chain** for the developer wallet.

Carbon pools in this app are usually created with the KYC rail on, so a buyer
without an attestation cannot even quote.

---

## 4. Publish the whitelist root and register entries

`KYC` → **Transfer-hook whitelist**:

1. **Initialize hook** once if needed.
2. Paste the four wallets into **Whitelist (comma/space-separated pubkeys)**.
3. **Merkle proof builder** → **Build proofs** → **Copy root**.
4. Paste into **Whitelist root (64 hex)** → **Update root**.
5. Build proofs for `DEVELOPER` → `REGISTRY` (press **Build proofs**), press
   **Use for sender entry** and **Use for recipient entry**, then **Register entry**.
6. Change the destination wallet to `BUYER`, press **Build proofs** again, load both
   proofs and register that entry. Repeat for `MARKET_MAKER`.

Four wallets means a 2-level tree, so every proof has 2 nodes. If you see an empty
proof field, the tree has a single member — add the other wallets first.

> This root covers wallets only. Once the mints exist you will publish a second
> root that adds the pool and order PDAs (§5b), and every entry has to be
> registered again.

---

## 5. Create the credit mint

`RWA` → **Create RWA mint**:

| Field | Value |
|-------|-------|
| Decimals | `6` (1.000000 = 1 tonne CO2e) |
| Name | `Katingan Peatland Restoration VCU 2024` |
| Symbol | `VCU24` |
| URI (ISIN / NAV / prospectus) | `https://registry.example/vcs-1234/issuance.json` |

Press **Create mint (generates a fresh keypair)**. The new address appears as the
placeholder of **Mint (defaults to the freshly created one)**; recover it with
[Reading the new mint's address](README.md#reading-the-new-mints-address) before
leaving the page, since the market step needs it.

Then, in **Mint, transfer & fees**, press **Register hook metas for this mint**.
This is the per-mint setup that lets Token-2022 find the hook accounts; without it
every transfer fails even with a perfect whitelist.

---

## 5b. Create the quote mint and whitelist the custody accounts

1. `RWA` → **Create RWA mint** again for the quote: **Decimals** `6`, **Name**
   `Test USD`, **Symbol** `TUSD`, any stable URI. Recover the address and press
   **Register hook metas for this mint** for it too.
2. Mint quote supply to the developer wallet (**Amount** = `100000000000000`).
3. `DLMM` → **Pool selector**: **Mint A** = `VCU24`, **Mint B** = `TUSD`, copy the
   **Derived pool PDA** (do not initialize the pool yet).
4. Collect the other custody addresses:

   ```bash
   bun examples/pdas.ts --pair <VCU24> <TUSD> \
     --maker <DEVELOPER> --order-id 1700000000000 --escrow-mint <VCU24>
   ```

   Use the printed **order PDA** for the forward ask; it is the owner of the
   order's credit escrow. Also note the **hook authority** and **settlement escrow
   authority** PDAs.
5. `Enterprise` → **Whitelist import**: append the pool PDA, order PDA, hook
   authority and escrow authority to the four wallets → **Apply whitelist**.
6. `KYC` → **Transfer-hook whitelist**: paste the extended list → **Build proofs**
   → **Copy root** → **Update root**, then register an entry for **every** member
   again (wallets and PDAs).

Why the PDAs: the hook checks the owner of both token accounts, and a pool vault is
owned by the pool PDA while an order escrow is owned by the order PDA. Without
their entries the deposit into the pool or the escrow fails before the hook is even
reached.

---

## 6. Issue the credits

In **Mint, transfer & fees**:

1. **Amount** = `100000000000` → 100,000.000000 tonnes (raw, 6 decimals).
2. Leave **Holder** empty (the developer wallet).
3. Press **Mint to my ATA**.

Now deliver the tranche the buyer has contracted:

1. **Amount** = `25000000000` → 25,000 tonnes.
2. **Destination (transfer)** = `BUYER`.
3. Confirm the green note `Proofs reproduce the on-chain root and both entries are
   registered`.
4. Press **Transfer (hook-approved)**.

The recipient's ATA is created in the same transaction. Set **Holder** = `BUYER` in
**Mint inspector** to confirm the balance.

### Optional: set a marketplace fee

| Field | Value | Meaning |
|-------|-------|---------|
| New transfer fee (bps) | `10` | 0.10% per transfer |
| New maximum fee | `500000` | Cap at 0.5 tonnes |

Press **Update transfer fee**. On carbon, a fee is usually a marketplace or
registry administration fee. Keep it low — retirement transfers are small and the
fee is deducted from the tonne being retired, which confuses reconciliation.

---

## 7. Open a spot market

On `DLMM`, first set **Pool selector** → **Mint A** = `VCU24`, **Mint B** = `TUSD`. The create panel takes the pair from the selector, not from its own
fields, and **Initialize pool** stays disabled until both mints are filled.

Then, in **Create DLMM pool**:

| Field | Value | Why |
|-------|-------|-----|
| Active bin id | `2527` | ≈ $12.50 per tonne at a 10 bp step (see below) |
| Bin step (bps) | `10` | 0.1% per bin — normal volatility for carbon |
| Base fee (bps) | `25` | 0.25% per swap |
| Require KYC | `On` | Only attested institutions |
| Oracle deviation (bps, 0 = off) | `300` | 3% breaker; carbon moves in jumps |
| Settlement routing | `On` | Registry delivery is not instant |
| T+N slots | `300` | ≈ 2 minutes on localnet; see the note on days below |

Press **Initialize pool**.

### Choosing the bin

Bins are geometric: `price ≈ (1 + bin_step/10000)^bin_id`, with bin 0 = 1.0.

- At a **10 bp** step, `bin_id = round(ln(12.50) / ln(1.001)) = 2527`.
- The **Target NAV / price** field on `DLMM` accepts any price: type `12.5` and
  press **Select bin at NAV** to compute the bin for you.
- The `NAV $1.00` and `NAV $1.01` shortcut buttons are fund-share shortcuts; do not
  use them for carbon.

> The field is named "NAV" but it is a generic price → bin helper: it interprets
> the number you type as a **raw** price ratio. That equals the human dollar price
> only when both mints share decimals — with 6/6 you can type `12.5` directly.

### Seed liquidity

With the pool loaded, `DLMM` → **Liquidity**:

| Field | Value |
|-------|-------|
| Bin id | `2527` (set by the shortcut) |
| Amount A | `10000000000` → 10,000 tonnes |
| Amount B | `125000000000` → 125,000 quote units |

Press **Add liquidity**. Deposit into neighbouring bins (`2520`–`2535`) so a normal
day's trading does not exhaust one bin. If a bin's array is missing, open
**Admin & maintenance**, enter **New bin array index** and press
**Initialize bin array**; a bin cannot take liquidity before its array exists.

`My position & balances` shows shares at the selected bin, and the **Bin liquidity
heatmap** shades the depth you supplied.

---

## 8. Sell a forward delivery on the order book

A corporate buyer often wants a fixed price for a vintage that has not been
delivered yet. Use a limit ask instead of a swap.

`Orders` → **Market**:

1. **Base mint** = `VCU24`, **Quote mint** = `TUSD`.
2. If there is no book: **KYC rail** = `Require attestation`,
   **Settlement rail** = `T+N escrow`, **T+ slots** = `300` → **Initialize book**.

`Orders` → **Place a limit order**:

| Field | Value | Note |
|-------|-------|------|
| Side | `Ask — sell base` | Deliver credits for quote |
| Price | `13.2500` | $13.25 per tonne, a premium to the $12.50 spot |
| Amount | `5000000000` | 5,000 tonnes in **base** raw units |
| Expiry (minutes) | `43200` | 30 days |
| Order id | `1700000000000` | The PDA you whitelisted in §5b — overwrite the clock-seeded value |

Press **Place order**. The credits are escrowed in the order's own account, so they
cannot be double-sold.

The buyer then opens **Resting orders**, presses **Fill**, leaves **Amount** at the
remaining 5,000 tonnes, and sets **Max pay** to the quote they will pay (the page
pre-fills it at the resting price). Press **Fill order**.

Because the book is settlement-routed, the fill writes a fresh `trade_id` and the
quote proceeds land in **Settlement escrows** rather than paying out directly.

---

## 9. Release payment against retirement

Carbon settlement is deliberately slower than the trade, because the registry has
to confirm the serial block before money moves.

1. `Settlement` → **Settlement escrows** → tick **Mine only** → press **Load** on
   the trade. The countdown shows the settlement slot and flips to `ready` when it
   passes.
2. The registry signs `keccak256(trade_id ‖ taker ‖ mint ‖ amount)` with the
   transfer agent key it registered (`Settlement` → **Set transfer agent**, or
   **Set mint agent** for this mint only) and publishes `r ‖ s` + recovery id.
3. Paste both, press **Settle (release escrow)**.

If the registry rejects the serial block, **Cancel settlement** refunds the buyer
and leaves the credits where they were. If nobody settles, **Refund expired
escrow** is the cleanup path, and **Close & reclaim rent** recovers the vault rent
afterwards.

> **Slots vs. days.** On localnet a slot is ~400 ms, so `T+300 slots` is minutes.
> Real registry settlement is T+1 to T+5 days; on mainnet that is roughly
> 216,000–1,080,000 slots. Pick the slot count you actually need — the escrow is a
> slot deadline, not a calendar date.

---

## 10. Retire the tonnes

Retirement is a transfer to the registry's retirement wallet, which is whitelisted
and never sends tokens back:

1. `RWA` → **Mint, transfer & fees**.
2. **Amount** = `1000000000` → 1,000 tonnes (the retirement batch).
3. **Destination (transfer)** = `REGISTRY`.
4. Confirm the green compliance note and press **Transfer (hook-approved)**.

The registry then issues an off-chain retirement certificate referencing the
transaction signature from the toast. The tokens sit in the registry account, which
is the on-chain proof that those serials are out of circulation.

To make retirement tamper-evident, copy the transfer signature into the registry's
retirement record. The `Audit` page lists it with slot, block hash and status, and
**Export CSV** gives the compliance file.

### Freeze circulation during reconciliation

Between the vintage close and the registry's issuance confirmation:

- `KYC` → **Transfer-hook whitelist** → **Pause transfers**. Any transfer attempt
  is rejected by the hook while the flag is set, including the developer's own.
- Resume with **Resume transfers** once the serial block is confirmed.

Pausing does not change balances, and escrows already created stay where they are.
A release that moves *this* mint will fail while the flag is set, so resume before
settling a retirement-funded trade.

---

## 11. Read the credit back

`Audit` → **NAV attestation viewer** works for any RWA mint, carbon included:

| Read-out | Expected for this asset |
|----------|------------------------|
| Legal name | `Katingan Peatland Restoration VCU 2024` |
| Ticker | `VCU24` |
| Metadata URI | Registry issuance document |
| Decimals | `6` |
| Total supply | Credits outstanding, including retired ones still held by the registry |
| Detected ISIN | `—` (carbon uses registry serials, not securities identifiers) |
| Detected CUSIP | `—` |

The viewer detects ISIN/CUSIP by pattern, so `—` is expected here and is not an
error. The authoritative record is the registry document behind the URI.

`Portfolio` → **Liquidity positions** shows the DLMM position; **Settlement escrows**
shows anything still pending.

---

## Variant: 0-decimal whole-tonne credits

If the registry only issues whole tonnes, create the mint with **Decimals = 0**.
Consequences:

- `Amount` fields become whole tonnes: `1000` = 1,000 tonnes; `1` = 1 tonne.
- Bin prices are **raw token ratios**. With 0-decimal credits and a 6-decimal
  stablecoin, human price = `bin_price × 10^(0−6)`. Bin 0 is $0.000001, not $1.
- To peg $12.50, either:
  - type `12500000` into **Target NAV / price** and press **Select bin at NAV**
    (the helper wants the raw ratio), or
  - keep the credit mint at 6 decimals (recommended) and avoid the conversion.

The rest of the flow is unchanged: whitelist, entries, hook metas, mint, transfer,
book, settlement, retirement.

---

## Verification checklist

- [ ] `KYC` → **Transfer-hook whitelist**: computed root = on-chain root; the
      developer, registry, buyer and market maker each have an entry, plus the
      pool PDA, order PDA, hook authority and escrow authority.
- [ ] `RWA` → **Mint inspector**: supply equals issued + retired; decimals `6`;
      hook metas registered.
- [ ] The buyer's balance matches the delivered tranche, and the registry's balance
      matches everything retired.
- [ ] `DLMM` → **Pool state**: KYC required, oracle deviation `300`, settlement
      wired, active bin near `2527`.
- [ ] `Orders` → **Resting orders**: the ask is listed, escrowed, or filled.
- [ ] `Settlement` → escrow status moves `pending` → `settled` (or `cancelled`).
- [ ] `Audit` → the retirement and delivery transactions appear with `success`.
- [ ] **Pause transfers** rejects a test transfer, and **Resume transfers** restores
      it.

## Failures specific to carbon credits

| Symptom | Cause | Fix |
|---------|-------|-----|
| Retirement transfer rejected | The registry wallet is not in the tree or has no entry | Add it and register its entry |
| `Detected ISIN: —` | Expected — carbon has no ISIN | Read the URI/serial block instead |
| Pool quotes at an absurd price | Decimals of the credit and quote differ | Use matching decimals or offset the bin as shown above |
| Fill fails after a root update | Entries were invalidated | Rebuild proofs and re-register both wallets |
| Registry cannot settle | Its secp256k1 key was not set, or the trade id was reused | **Set transfer agent** / **Set mint agent**, then retry with a fresh fill |
| `Place order` fails with a missing account | The order PDA for that id is not whitelisted | Use the id registered in §5b, or whitelist the new order PDA |
| First pool deposit fails | The pool PDA has no whitelist entry | Add it, `Update root`, re-register every entry |
| A buyer with an attestation still cannot trade | The book was initialized with the KYC rail matching a *different* KYC config | Re-check **KYC rail** on the book card |
| Transfers stop for everyone | **Pause transfers** was left on after reconciliation | **Resume transfers** |
| Credits "double counted" in a report | Registry account balance was omitted from the circulating supply | Circulating = supply − registry balance |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Decimals | 6 (or 0) | 6 keeps human price = bin price with a 6-dec quote |
| Transfer fee | 10 bps, cap 0.5 t | Optional marketplace/registry fee |
| DLMM bin step | 10 bps | Finer for high-value removals, coarser for cheap offsets |
| DLMM base fee | 25 bps | Carbon spreads are wider than FX |
| Oracle deviation | 300 bps | Carbon gaps; a tight breaker halts constantly |
| Settlement | On, T+300 slots | Scale to the registry's real confirmation time |
| Order price | $13.25 | Forward premium over spot |
| Order expiry | 43,200 min (30 days) | Vintage delivery cycles |
| Retirement | Transfer to `REGISTRY` | No burn instruction exists in the UI |
