# 04 — Private credit: sell a receivable on the limit order book

Tokenize a short-dated trade-finance receivable and sell it at a discount to a
credit fund through the order book, with compliance gating on both sides and T+N
escrow for the cash leg.

Asset: `Northwind Invoice INV-8842` (`INV8842`), 2 decimals, $1,000,000 face,
60-day tenor, offered at 97.20 (a 2.80% discount).

This is the playbook for **private, negotiated paper**: there is no public pool, no
continuous quote, and no pretence of price discovery. One originator posts an ask,
one or two funds bid, the trade clears at a price both sides already agreed, and the
cash waits in escrow until the assignment documents are confirmed.

---

## Why the order book and not an AMM

| Requirement | Order book | AMM |
|-------------|-----------|-----|
| Negotiated price for a single CUSIP | Yes — a resting ask at one price | No — pool price is shared by everyone |
| Partial fill of a $1M line | Yes — fill any amount up to the remainder | Yes, but with slippage |
| Compliance hold before cash moves | Yes — T+N escrow | Only with the settlement rail |
| Cancel before funding | Yes — full refund of escrow + rent | Remove liquidity instead |
| No public price discovery | Yes — the book is pair-scoped | No — anyone can swap |

---

## 0. Prerequisites

| Requirement | Value |
|-------------|-------|
| App / cluster | `bun run dev`, `localnet`, Sample Data **off** |
| Wallets | `ORIGINATOR` (seller), `FUND` (buyer), `SERVICER`, `AGENT` |
| Quote mint | A 6-decimal legacy-SPL stablecoin (`mUSDC` from the seed script). The order book resolves a token program per mint, so a mixed pair is fine here — unlike the AMM pages, which need both legs under one program |
| Settlement config | Initialised, with a transfer agent key set |
| Roles | `Compliance Officer` and `Treasury Manager` for the four-eyes flow |

Receivable terms used throughout:

| Term | Value |
|------|-------|
| Face value | $1,000,000.00 |
| Tenor | 60 days from issue |
| Offer price | 97.20 per 100 of face |
| Discount | $28,000 |
| Obligor | Northwind Manufacturing Ltd |
| Recourse | Full recourse to the originator until the obligor pays |

Decimals note: this mint uses **2 decimals** because the asset is money-like and
face value is quoted to the cent. `1,000,000.00` face is `100000000` raw. The Orders
page's **Price** field is a human number and handles the decimals for you; the
`Amount` field is always raw.

---

## 1. Set up the desk roles

The enterprise surfaces are where a private-credit desk separates duties.

1. `Enterprise` → **Role-based access**:
   - `Compliance Officer` — reviews the audit trail and approves settlements,
     manages the address book.
   - `Treasury Manager` — adds liquidity, approves and executes proposals, manages
     the address book.
   - `Trader` — initiates swaps and TWAMM orders, queues proposals.
   - `Admin` — everything, including pause and authority rotation.
2. Set the top-bar role to `Compliance Officer` while you prepare the line, then
   switch to `Treasury Manager` to execute. The read-outs **Can approve proposals**
   and **Can manage address book** update immediately.
3. `Enterprise` → **Execution mode**: choose `Multisig proposal` if a second pair of
   eyes must approve every action before it is broadcast. `Direct` signs from the
   connected wallet.

An action performed while `Multisig` is selected appears in **Multisig proposal
queue** instead of being sent. An approver with `Treasury Manager` or `Admin`
presses **Approve** to rebuild and broadcast it, or **Reject** to discard it.
Proposals are stored locally (base64 instruction payloads) and survive a reload.

> Creating an RWA mint needs an extra keypair signature, so it cannot be staged as
> a proposal — it is direct-only. Everything downstream (transfer, order placement,
> cancellation, settlement) can be queued.

---

## 2. Add the counterparties

`Enterprise` → **Counterparty address book**:

| Label | Wallet |
|-------|--------|
| `Originator — Northwind` | `ORIGINATOR` |
| `Fund — Meridian Credit` | `FUND` |
| `Servicer — Apex` | `SERVICER` |
| `Transfer agent` | `AGENT` |

Press **Add / update** for each. The **Whitelist** column checks each saved address
against the imported list and reports **in whitelist** / **not in whitelist** /
**not checked** — a quick pre-trade sanity check.

Then, under **Whitelist import (one wallet per line)**, paste all four addresses and
press **Apply whitelist**.

---

## 3. KYC and whitelist the line

1. `KYC` → **KYC gate** → **Issue KYC** for `FUND`, `SERVICER` and `AGENT`
   (the originator is already attested from setup).
2. **Verify my attestation on-chain** to confirm the gate accepts the issuer.
3. `KYC` → **Transfer-hook whitelist** → **Initialize hook** if needed.
4. Paste the four wallets → **Build proofs** → copy the root → **Update root**.
5. Register an entry for every wallet: pick the pair in the proof builder, press
   **Build proofs**, load it with **Use for sender entry** / **Use for recipient
   entry**, then **Register entry**. Repeat per wallet — press **Build proofs** again
   each time the pair changes.

A receivable transfer fails closed: if the fund's entry is missing, the sale cannot
settle even if the cash is escrowed. Register entries *before* the book goes live.

---

## 4. Tokenize the receivable

`RWA` → **Create RWA mint**:

| Field | Value | Note |
|-------|-------|------|
| Decimals | `2` | Money-like; $1.00 = `100` raw |
| Name | `Northwind Invoice INV-8842 60d` | |
| Symbol | `INV8842` | |
| URI (ISIN / NAV / prospectus) | `https://servicer.example/inv-8842/pack.json` | Assignment agreement, obligor, due date, recourse |

Press **Create mint (generates a fresh keypair)** and recover the address from the
**Mint (defaults to the freshly created one)** placeholder with
[Reading the new mint's address](README.md#reading-the-new-mints-address) — the
book needs it and the placeholder is not selectable.

Serve a stable document for the whole receivable:

```json
{
  "invoiceId": "INV-8842",
  "obligor": "Northwind Manufacturing Ltd",
  "obligorRating": "BB-",
  "faceValue": "1000000.00",
  "currency": "USD",
  "issueDate": "2026-10-10",
  "dueDate": "2026-12-09",
  "recourse": "full",
  "servicer": "Apex Credit Services",
  "documents": {
    "assignment": "https://servicer.example/inv-8842/assignment.pdf",
    "obligorConfirmation": "https://servicer.example/inv-8842/confirmation.pdf",
    "kycPack": "https://servicer.example/inv-8842/kyc.pdf"
  }
}
```

Then press **Register hook metas for this mint** — one call, before any transfer.

### Issue the face amount

`RWA` → **Mint, transfer & fees**:

| Field | Value | Result |
|-------|-------|--------|
| Amount | `100000000` | $1,000,000.00 face (raw, 2 decimals) |
| Holder | empty | To the originator |

Press **Mint to my ATA**. **Mint inspector** should read `Supply = 100000000`.

### Set the servicing fee, if any

| Field | Value | Meaning |
|-------|-------|---------|
| New transfer fee (bps) | `25` | 0.25% servicing/administration fee per transfer |
| New maximum fee | `250000` | Cap at $2,500 per transfer |

Press **Update transfer fee**. A servicing fee is charged on the recipient side, so
the fund receives face minus the fee on delivery. For clean assignment accounting,
many desks keep this at `0` and pay servicing off chain.

---

## 4b. Whitelist the order PDA and the settlement custody

An ask escrows the receivable in the order's own token account, which is owned by
the **order PDA**. The hook checks that owner, so the order cannot even be placed
until its PDA has an entry. Pick the order id now, in advance:

1. Derive the addresses for order id `1700000000000`:

   ```bash
   bun examples/pdas.ts --pair <INV8842> <mUSDC> \
     --maker <ORIGINATOR> --order-id 1700000000000 --escrow-mint <INV8842>
   ```

   It prints the book PDA, the **order PDA**, and the escrow ATA for reference.
2. `Enterprise` → **Whitelist import**: append the order PDA, the hook authority
   PDA (`KYC` → **Hooks escrow router**) and the settlement escrow authority PDA
   (`bun examples/pdas.ts`) to the four wallets → **Apply whitelist**.
3. `KYC` → **Transfer-hook whitelist**: paste the extended list → **Build proofs**
   → **Copy root** → **Update root**, then register an entry for every wallet **and**
   every PDA again.

Why each address:

| Address | Why it needs an entry |
|---------|-----------------------|
| Order PDA | Owns the receivable escrow on an ask — required for this trade |
| Hook authority PDA | Holds settlement-routed RWA output in custody; exercised when the receivable is the settling leg (for example a bid that pays in receivable) |
| Settlement escrow authority PDA | Owns every trade's escrow vault when the escrowed mint is hooked |

The **book PDA** does not own a token account, so it needs no entry.

The quote leg here is legacy SPL and never runs the hook, so the cash side adds no
requirements of its own. Registering the two settlement PDAs now costs one root
rotation and saves another later.

---

## 5. Open the book

`Orders` → **Market**:

1. **Base mint** = `INV8842`, **Quote mint** = the stablecoin.
2. If the book does not exist, the initialize form appears:
   - **KYC rail** = `Require attestation`
   - **Settlement rail** = `T+N escrow`
   - **T+ slots** = `300`
3. Press **Initialize book**.

Only the program's upgrade authority can create a book for a new pair. If you are
not that key, the form still appears but **Initialize book** fails; if the book
already exists, the form is replaced by the read-out.

The **Market** read-out then shows `KYC rail: required` and
`Settlement rail: T+300 slots`, plus the `paused` / `open` page badge and the
number of listed orders.

> **Rails are fixed at birth.** There is no "add the settlement rail later" control.
> If you initialize a book with `Direct payout`, the only way to route proceeds into
> escrow is a new pair (or a new book address), so choose deliberately.

---

## 6. Post the ask

The originator sells $400,000 of face to the fund.

`Orders` → **Place a limit order**:

| Field | Value | Reasoning |
|-------|-------|-----------|
| Side | `Ask — sell base` | Sell face for cash |
| Price | `97.2000` | Human quote per base unit; the page scales by `10^(6−2) × 1e9` |
| Amount | `40000000` | $400,000.00 face in raw base units |
| Expiry (minutes) | `1440` | A 1-day quoting window on 60-day paper; cancel and repost instead of leaving a stale quote |
| Order id | `1700000000000` | The PDA whitelisted in §4b — overwrite the clock-seeded value |

Press **Place order**.

What happened on chain:

- A new order PDA at `(book, wallet, order_id)`.
- The $400,000 face is **escrowed** into that order's own token account. It cannot
  be sold twice.
- The order is visible to any wallet that loads the pair, but only attested traders
  can fill it because the book's KYC rail is on.

> **One order id per price level.** `order_id` is part of the PDA, so reusing an id
> collides with the existing order. The field starts from the clock and advances
> after each placement, but here you overwrite it with the id registered in §4b —
> that is what makes the escrow owner known before the order exists. Cancel before
> reusing an id.

---

## 7. Bid for the paper

The fund's desk wants a margin, so it posts a competing bid underneath:

| Field | Value |
|-------|-------|
| Side | `Bid — buy base` |
| Price | `97.0000` |
| Amount | `388000000000` — the **quote** leg in raw units, not face |

Careful: for a bid, the **Amount** is the **quote** leg in raw units, not face.
$388,000 of cash at 97.00 buys exactly $400,000 of face. Enter `388000000000`
(6-decimal quote), not `40000000` (2-decimal face).

Press **Place order**. The bid's cash is escrowed.

If the originator accepts, it can fill the bid directly: `Resting orders` →
**Fill** → **Amount** = the quote leg → **Max pay** = the face it will deliver.
Otherwise the fund lifts the ask instead (next section). Either side filling
produces the same order state and the same settlement escrow.

---

## 8. Fill and settle

`Orders` → **Resting orders** → press **Fill** on the ask.

The **Fill order <id>** panel opens:

| Field | Value | Note |
|-------|-------|------|
| Amount | `40000000` | The escrow leg: face for an ask |
| Max pay | pre-filled at 97.20 → `388800000000` | Your slippage bound in quote raw units |

Press **Fill order**. The page:

1. Creates the taker's missing base/quote ATAs and the proceeds ATA in the same
   transaction.
2. Appends the transfer-hook accounts for both legs (both wallets are whitelisted).
3. Names a fresh `trade_id` because the book is settlement-routed.
4. Moves the face to the fund and locks the cash in escrow.

Now `Settlement` → **Settlement escrows**:

1. Tick **Mine only** to filter to the connected wallet, then press **Load** on the
   trade.
2. The row shows `Trade id`, `Asset`, `Notional`, `Settlement slot`, `Expiry slot`,
   a live `Countdown`, `Taker`, `Refund source` and `Status`.
3. When the countdown reads `ready`, the transfer agent signs
   `keccak256(trade_id ‖ taker ‖ mint ‖ amount)` and publishes `r ‖ s` (128 hex) and
   the recovery id.
4. Paste both → **Settle (release escrow)**.

The signature *is* the authorisation — the agent needs no SOL and no wallet.

### The compliance exits

| Situation | Action | Effect |
|-----------|--------|--------|
| Obligor confirmation never arrives | **Cancel settlement** | Cash refunds to the refund source; the face stays with the fund |
| The fund fails to complete its side | **Cancel settlement** from compliance | Same refund path |
| Nobody settles and the expiry slot passes | **Refund expired escrow** | Refund after expiry |
| Escrow is finished and empty | **Close & reclaim rent** | Rent returns to the payer |
| Price moved before fill | **Cancel** on your own resting order | Full escrow balance plus rent returned |

`Cancel settlement` is the sanctioned "walk away" and does not require the agent's
signature — which is exactly why the `Compliance Officer` role exists.

---

## 9. Partial fills and the remaining line

Receivables often clear in slices. A $400,000 ask can be filled $150,000 / $250,000
by two funds:

1. First fill: **Amount** = `15000000`; the order's `Remaining` drops to
   `25000000`.
2. `Resting orders` → **Refresh** (or wait for the refresh after a fill).
3. Second fill: **Amount** = `25000000`, **Max pay** recalculated from the
   pre-filled value.

Each fill creates its own `trade_id` and its own escrow, so the two funds settle
independently. `Filled (base / quote)` on the order row accumulates both legs.

If the remaining amount should no longer be offered, press **Cancel** — it refunds
the **whole** escrow balance, including any tokens donated to the escrow account,
plus the order and escrow rent.

---

## 10. Record the trade

`Audit`:

- **On-chain audit trail** lists every transaction the connected wallet signed.
  **Export CSV** names the file `rwadex-audit-<wallet6>`; **Print / PDF** produces a
  statement for the credit committee.
- Block hashes are only fetched for the 10 most recent transactions (older ones may
  be pruned by the RPC).
- **NAV attestation viewer** on `INV8842` shows legal name, ticker, URI, decimals,
  total supply and any detected ISIN/CUSIP. An invoice has no ISIN, so those rows
  read `—`; the URI pack is the authoritative document.

`Enterprise` → **Multisig proposal queue** is the audit trail for anything that was
staged rather than sent, including who approved it and when.

`Portfolio` → **Rescan** shows the wallet's liquidity positions, TWAMM orders and any
pending **Settlement escrows**. Limit orders — including the remaining ask — are read
on the `Orders` page.

---

## Verification checklist

- [ ] `Orders` → the **Market** read-out shows `KYC rail: required`,
      `Settlement rail: T+300 slots`, the page badge reads `open`, and the pair
      matches the mint addresses.
- [ ] The order PDA, hook authority and settlement escrow authority each have a
      whitelist entry (check the computed root and the `KYC` entries).
- [ ] `Resting orders` shows the ask with `Remaining` equal to the unfilled face and
      `Filled (base / quote)` accumulating as expected.
- [ ] `RWA` → **Mint inspector** supply is unchanged after trading (tokens moved,
      not minted).
- [ ] The fund's balance equals the face it bought; the originator's escrow is empty
      after a full fill.
- [ ] `Settlement` shows one escrow per fill, each with a distinct `Trade id`.
- [ ] `Settle` succeeds only after the slot and only with the agent's signature.
- [ ] `Cancel settlement` refunds the payer and leaves the face with the buyer.
- [ ] `Enterprise` → **Multisig proposal queue** shows the staged actions and their
      approver when multisig mode was used.

## Failures specific to private credit

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Fill order` disabled | No order selected, or the order was filled/cancelled | Refresh **Resting orders**, select again |
| Fill reverts with a hook error | One side's whitelist entry is missing or was invalidated | Re-register both entries for that pair |
| `Place order` fails with a missing hook account | The order PDA for that id is not whitelisted | Use the pre-registered id (`1700000000000`) or whitelist the new order PDA |
| `Fill order` fails on a settlement book | `trade_id` namespace collision | Retry — the page generates a fresh random id per fill |
| Bid fills for the wrong size | Bid **Amount** is quote, not face | Convert: quote = face × price / 100 |
| Price rejected by the book | Decimals mismatch between the mint and the quote | Recompute the human price (`raw = human × 10^(quote−base) × 1e9`) |
| Settlement escrow missing | The book was created with `Direct payout` | Rails are fixed; use a settlement-routed book |
| Agent cannot settle | No agent key, or a per-mint agent overrides it | **Set transfer agent** or **Set mint agent** |
| Proposal will not execute | Approval role is `Trader` or `Compliance Officer` | Switch to `Treasury Manager` or `Admin` and press **Approve** |
| Mint creation cannot be staged | It needs an extra keypair signer | Create it in `Direct` mode |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Decimals | 2 | Money-like asset; `100` raw = $1.00 |
| Face | $1,000,000.00 | `100000000` raw |
| Offer price | 97.20 | 2.80 points below face; ≈ 17.5% annualised over 60 days |
| Ask size | $400,000 face | Partial fills supported |
| Expiry | 1,440 min | Cancel and repost rather than leave stale quotes |
| Book rails | KYC on, settlement on, T+300 | Fixed at book creation |
| Servicing fee | 0–25 bps | Recipient-side, capped |
| Settlement exits | Cancel / refund expired / close | Cancel needs no agent signature |
