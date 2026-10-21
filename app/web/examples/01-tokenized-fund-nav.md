# 01 — Create a NAV-bearing fund share in the UI

This is the reference walkthrough. You will issue a tokenized money-market fund
share whose NAV target is **$1.00**, publish the NAV attestation, put the share on
a DLMM market pegged at NAV, take a limit order against it, and release the
proceeds through T+N escrow.

Asset: `Meridian USD Liquidity Fund — Class A` (`MMF-A`), 6 decimals, NAV target
$1.00, daily NAV attestation, whitelisted institutional holders only.

> "Creating NAV" in this app means two things, and both are covered below:
> 1. the **token** that represents one share of the fund (its metadata URI is the
>    NAV attestation), and
> 2. the **venue** that trades it at NAV — DLMM bins pegged to $1.00, and a limit
>    book whose price is quoted per share.

Nothing in the UI stores a NAV number on chain. The chain stores the mint and its
metadata pointer; the URI serves the document with the current NAV, and the market
price is whatever the pool and the book say.

---

## 0. Prerequisites

| Requirement | How |
|-------------|-----|
| App running | `bun run dev` |
| Cluster | `localnet` (top bar select) |
| Wallet | **Connect wallet**, funded with SOL for rent and fees |
| Sample Data | **Off** |
| Role | `Admin` for the first pass (local UI gate only) |
| Execution mode | `Direct` |
| Investor wallet | Any second pubkey; it will need a whitelist entry and an attestation |
| Quote mint | A 6-decimal **Token-2022** quote mint you create in §4b (`TUSD`). A pool carries one token program for both legs, so the seeded legacy-SPL `mUSDC` cannot be paired with an RWA mint |

Three wallets to have ready:

- `ISSUER` — the connected wallet. Becomes mint authority, fee authority, metadata
  authority, permanent delegate and hook authority for the new mint.
- `INVESTOR` — the counterparty that receives shares.
- `AGENT` — the transfer agent. Its secp256k1 key signs settlement releases; it is
  not a Solana signer, so you only need the 128-hex `x ‖ y` public key later.

---

## 1. Import the institution whitelist

The RWA transfer form derives proofs from a locally stored wallet list, and that
list has to be exactly the set whose root is published on chain.

1. Open `Enterprise`.
2. In **Counterparty address book**, fill **Label** = `Investor — Northwind`,
   **Wallet address (base58)** = `INVESTOR`, press **Add / update**. Repeat for
   `Transfer Agent` (`AGENT`).
3. Under **Whitelist import (one wallet per line)**, paste one wallet per line —
   issuer, investor, agent — and press **Apply whitelist**.
4. Check the table: every saved address should read **in whitelist**.

> The list is persisted in `localStorage` under `rwadex.whitelist`. It is not the
> on-chain root; it is the input to the proof builder. Publishing comes next.

---

## 2. Issue KYC attestations

KYC and the whitelist are independent gates. Pools with the KYC rail check the
attestation; RWA transfers check the whitelist. Do both so the fund can trade on
either kind of venue.

1. Open `KYC` → **KYC gate**. If the badge says `not deployed`, press
   **Initialize KYC mint** once from the program deployer key.
2. **Institution wallet** = `INVESTOR`, press **Issue KYC**.
3. Repeat for `AGENT`.
4. Press **Verify my attestation on-chain** from the issuer wallet to confirm the
   gate accepts your own attestation.
5. The top bar should now show `Accredited: Verified` for the connected wallet.

Expect the toast `KYC issue` per wallet. A wallet with `KYC: none` will be refused
by any pool created with `Require KYC = On`.

---

## 3. Publish the whitelist root

1. Still on `KYC` → **Transfer-hook whitelist**.
2. If the config does not exist yet, press **Initialize hook** once.
3. Paste the same three wallets into **Whitelist (comma/space-separated pubkeys)**.
   The tree visualiser appears with one leaf per wallet, ordered by leaf hash.
4. Press **Build proofs** in the **Merkle proof builder**, then **Copy root**.
5. Paste that 64-hex value into **Whitelist root (64 hex)** and press
   **Update root**.
6. Now register each wallet's entry:
   - **Source wallet (sender)** = `ISSUER`, **Destination wallet (recipient)** =
     `INVESTOR`, press **Build proofs**.
   - In the *Source proof (sender)* panel press **Use for sender entry**.
   - In the *Destination proof (recipient)* panel press **Use for recipient entry**.
   - Press **Register entry** in the entry form above.
   - Change **Destination wallet (recipient)** to `AGENT`, press **Build proofs**
     again, load the proofs into the form, and **Register entry** for the agent.

Expect `Hook update root`, then two `Hook set whitelist entry` toasts.

> Register entries **after** the root update, every time. Entries carry a proof
> path; a new root invalidates every path that no longer folds to it.

---

## 4. Create the NAV mint

Open `RWA` → **Create RWA mint**.

| Field | Value | Why |
|-------|-------|-----|
| Decimals | `6` | Matches the stablecoin quote; keeps bin maths 1:1 |
| Name | `Meridian USD Liquidity Fund Class A` | Legal name shown in the NAV attestation viewer |
| Symbol | `MMF-A` | Ticker |
| URI (ISIN / NAV / prospectus) | `https://fund.example/mmf-a/attestation.json` | **Stable** endpoint that always serves the latest NAV attestation |

Press **Create mint (generates a fresh keypair)**. The page generates a mint
keypair, partial-signs with it, and lets the wallet co-sign.

The new address now shows as the placeholder of
**Mint (defaults to the freshly created one)**. It is not selectable, and the
reference is component state — read it with
[Reading the new mint's address](README.md#reading-the-new-mints-address) before
you leave the page, because the pool step needs it.

The NAV attestation JSON behind the URI is off-chain, for example:

```json
{
  "fund": "Meridian USD Liquidity Fund",
  "shareClass": "A",
  "isin": "XS0000000001",
  "navPerShare": "1.000412",
  "navDate": "2026-10-20",
  "aum": "184230119.42",
  "currency": "USD",
  "administrator": "Northwind Fund Services",
  "attestation": { "type": "administrator-daily", "signature": "0x…" }
}
```

Because the URI cannot be changed from the UI, serve it from a route that reads the
latest NAV rather than uploading a new file per day.

---

## 4b. Create the quote mint

The four AMM venues take **one token program** for both legs. The fund share is
Token-2022, so the quote has to be Token-2022 too — the bootstrap's `mUSDC` is
legacy SPL and would fail when the pool is created. (The order book is the
exception: it resolves a token program per mint.)

`RWA` → **Create RWA mint** again:

| Field | Value |
|-------|-------|
| Decimals | `6` |
| Name | `Test USD` |
| Symbol | `TUSD` |
| URI (ISIN / NAV / prospectus) | `https://fund.example/tusd/terms.json` |

Generate it, recover the address the same way, then press
**Register hook metas for this mint** for `TUSD` as well.

Mint the quote supply to the issuer wallet (**Amount** = `100000000000000` for
100,000,000.000000 `TUSD`). The issuer will seed the pool with it.

> `TUSD` is hook-gated like every RWA mint. That is intentional: one whitelist
> covers both legs, and a hooked quote also means the order-book escrow and
> settlement custody need entries — which is what §4c registers.

---

## 4c. Whitelist the custody accounts

Every owner of a token account is a transfer counterparty for the hook, including
program accounts. Register these **before** the pool and the order, because a root
update invalidates every existing entry.

1. `DLMM` → **Pool selector**: type **Mint A** = NAV mint, **Mint B** = `TUSD`.
   Copy the **Derived pool PDA**. Do **not** press **Initialize pool** yet — the
   address is deterministic, and the pool is easier to create once its entry is in
   place.
2. Read the hook authority from `KYC` → **Hooks escrow router**
   (**Hook authority PDA**).
3. Read the settlement escrow authority from the helper:

   ```bash
   bun examples/pdas.ts --pair <NAV_MINT> <TUSD_MINT> \
     --maker <YOUR_WALLET> --order-id 1700000000000 --escrow-mint <NAV_MINT>
   ```

   That prints the order book PDA and the **order PDA** for the id `1700000000000`
   — the id you will type into the order form in §10 so the escrow owner is known
   in advance.
4. `Enterprise` → **Whitelist import (one wallet per line)**: append the four
   addresses (pool PDA, hook authority, settlement escrow authority, order PDA) to
   the issuer/investor/agent list → **Apply whitelist**.
5. `KYC` → **Transfer-hook whitelist**: paste the full list → **Build proofs** →
   **Copy root** → **Update root**.
6. Now register an entry for **every** member — the three wallets and the four
   PDAs. For each one, set it as the **Destination wallet (recipient)**, press
   **Build proofs**, **Use for recipient entry**, then **Register entry**.

Expect one `Hook set whitelist entry` per address. If a swap or fill later fails
with a missing hook account, this is the step that was skipped.

---

## 5. Register the hook accounts for this mint

Open `RWA` → **Mint, transfer & fees** and press
**Register hook metas for this mint**.

This creates the mint's `extra-account-metas` PDA. Without it Token-2022 cannot
resolve the transfer hook's accounts on this mint and every transfer fails, even
with correct proofs and entries. One call per mint, ever — do it for the NAV mint
and again for `TUSD`.

Expect the toast `Hook metas init`.

---

## 6. Mint the initial shares

Still on `RWA` → **Mint, transfer & fees**:

1. **Amount** = `1000000000000` → 1,000,000.000000 shares (raw units, 6 decimals).
2. **Holder** stays empty, which means the connected wallet.
3. Press **Mint to my ATA**.

The page creates your associated token account if needed, then mints.

Check **Mint inspector**: `Supply` should read `1000000000000` and
`Holder balance` the same. `Hook program`, `Extra account metas` and `Hook config`
are read-only addresses you can compare against the KYC page.

> Minting to a *different* holder works too: put its pubkey in **Holder** before
> pressing **Mint to my ATA**. Only the mint authority (the issuer) can mint.

---

## 7. Deliver shares to the investor

The RWA form checks compliance **before** sending:

| Read-out | Required state |
|----------|----------------|
| Source proof (derived) | Non-empty, built from the imported whitelist |
| Destination proof (derived) | Non-empty |
| Computed root | Equals **On-chain root** |
| Entries | Both wallets registered (no red status line) |

Steps:

1. **Amount** = `250000000000` → 250,000 shares.
2. **Destination (transfer)** = `INVESTOR`.
3. Confirm the page shows the green note
   `Proofs reproduce the on-chain root and both entries are registered: the hook
   will approve this transfer.`
4. Press **Transfer (hook-approved)**.

The page creates the recipient's ATA in the same transaction and then transfers
through the permanent-delegate authority, so the issuer can deliver shares without
the investor signing.

Verify in **Mint inspector** by setting **Holder** = `INVESTOR`: the holder balance
should read `250000000000`.

---

## 8. Set the transfer fee

Still in **Mint, transfer & fees**:

| Field | Value | Meaning |
|-------|-------|---------|
| New transfer fee (bps) | `2` | 0.02% per transfer |
| New maximum fee | `50000` | Cap at 0.05 shares (raw, 6 decimals) |

Press **Update transfer fee**. The fee mints on each transfer and is withheld in
the recipient's account; the fee authority is the issuer wallet. There is no
withdraw-withheld control in the UI, so treat the withheld balance as accrued and
collect it with the fee authority outside this page.

For a fund that should be free to move between custodians, leave the fee at the
`0` set at creation. Raising it later is always possible; the initial value is not
configurable in the create form.

---

## 9. Create the DLMM market pegged to NAV

First fill **Pool selector** on `DLMM`: **Mint A** = the NAV mint, **Mint B** =
`TUSD`. The **Create DLMM pool** panel has **no mint fields** — it reads the pair
from the selector, its button stays disabled until both are set, and the
**Derived pool PDA** updates as you type. Do this before the create step.

Then, in **Create DLMM pool**:

| Field | Value | Why |
|-------|-------|-----|
| Active bin id | `0` | Bin 0 is price 1.0 |
| Bin step (bps) | `1` | 1 bp per bin → $1.01 is ~bin 100 |
| Base fee (bps) | `2` | 0.02% per swap, competitive for a $1 peg |
| Require KYC | `On` | Only attested institutions may trade |
| Oracle deviation (bps, 0 = off) | `50` | Breaker trips on a 0.5% move away from the oracle |
| Settlement routing | `On` | T+N escrow for proceeds |
| T+N slots | `100` | ≈ 40 s on localnet |

Press **Initialize pool**.

> **Decimals matter for the peg.** Bin prices are *raw token* ratios, with bin 0 =
> 1.0. If the NAV mint has 6 decimals and the quote has 6, bin 0 is exactly $1.00.
> If the quote has 9 decimals, bin 0 is 0.001 in human terms. Keep both at the same
> decimals, or convert before choosing `Active bin id`.

### Load the pool

After **Initialize pool**, the selector's pair loads automatically into **Pool
state**. If the page was reloaded, re-enter the two mints in **Pool selector**; the
pool state comes back. Everything below is on the loaded pool.

### Pick the NAV bins

In **Bin liquidity heatmap**:

- **NAV $1.00** selects bin `0`.
- **NAV $1.01** selects the bin whose price is 1.01 (bin `100` at a 1 bp step).
- **Target NAV / price** = `0.9995` + **Select bin at NAV** walks the geometric
  formula `price ≈ (1 + bin_step/10000)^bin_id` and rounds to the nearest bin.

For a $1.00 peg, liquidity belongs in a tight band around bin 0 — bins `-5` to
`+5` cover roughly $0.9995 to $1.0005 at a 1 bp step.

### Provide liquidity

In **Liquidity**:

1. **Bin id** = `0` (set by the shortcut above).
2. **Amount A** = NAV shares in raw units, e.g. `50000000000` (50,000 shares).
3. **Amount B** = quote in raw units, e.g. `50000000000` (50,000 units).
4. Press **Add liquidity**.

Repeat for the neighbouring bins. If a bin's array does not exist yet, open
**Admin & maintenance**, set **New bin array index** and press
**Initialize bin array**; a bin only accepts deposits once its array exists.

**My position & balances** shows the position at the selected bin. Shares are
minted against value in token A.

---

## 10. Take a limit order at NAV

Open `Orders` → **Market**:

1. **Base mint** = NAV mint, **Quote mint** = `TUSD`.
2. If the pair has no book, the initialize form appears. Set
   **KYC rail** = `Require attestation`, **Settlement rail** = `T+N escrow`,
   **T+ slots** = `100`, then **Initialize book**.
3. The **Market** read-out should show `KYC rail: required`,
   `Settlement rail: T+100 slots`, and the page badge should read `open`.

Then **Place a limit order**:

| Field | Value | Note |
|-------|-------|------|
| Side | `Bid — buy base` | Buying shares with quote |
| Price | `1.0000` | Human quote per base; the page scales it to raw |
| Amount | `100000000000` | Raw **quote** units for a bid (100,000 quote) |
| Expiry (minutes) | `1440` | Fills stop after a day; cancel stays open |
| Order id | `1700000000000` | The id whose PDA you whitelisted in §4c — overwrite the clock-seeded value |

Press **Place order**. The quote leg is escrowed into the order's own PDA, whose
entry is already registered, so the deposit passes the hook.

To sell shares instead, choose `Ask — sell base` and enter the amount in **base**
units. The hint on **Amount** states which token is escrowed for the selected side.

> **Price for a $1 NAV.** With 6/6 decimals, `1.000000` becomes the raw price
> `1 × 10^(6−6) × 1e9 = 1e9`. A price of `1.0004` is a 4 bp premium; `0.9996` is a
> 4 bp discount. Repricing around NAV is a normal operation — cancel and replace.

---

## 11. Fill the order

1. In **Resting orders**, press **Fill** on a row.
2. The **Fill order <id>** panel opens with **Amount** pre-filled to the remaining
   escrow and **Max pay** pre-filled to the counter-leg at the resting price.
3. Lower **Max pay** only if you want a tighter slippage bound; raising it lets the
   fill happen if the resting order moved.
4. Press **Fill order**.

The page creates any missing ATAs (taker base, taker quote and proceeds) in the
same transaction, appends the transfer-hook accounts for both legs, and — because
the book is settlement-routed — names a fresh random `trade_id`.

Cancelling your own order is always available, refunds the escrow's whole balance
plus rent, and works even while the book is paused or after expiry.

---

## 12. Release the proceeds through settlement

1. Open `Settlement` → **Settlement escrows**. Tick **Mine only** to filter to
   escrows addressed to you; the new escrow is listed with a live countdown and
   shows `ready` once its settlement slot passes.
2. Press **Load** on the row to populate **Escrow lifecycle** below.
3. Wait until the countdown reads `ready`. Settling earlier fails by design;
   **Cancel settlement** and **Refund expired escrow** are the exits.
4. The transfer agent signs `keccak256(trade_id ‖ taker ‖ mint ‖ amount)` with its
   secp256k1 key and hands you `r ‖ s` (128 hex) plus the recovery id.
5. Fill **Signature r‖s (128 hex)** and **Recovery id**, leave
   **Taker token account** empty to default to the taker's ATA, and press
   **Settle (release escrow)**.

No Solana signer is needed for the release — the secp256k1 signature is the
authorisation. If the trade is rejected by compliance instead, **Cancel
settlement** refunds the locked flow.

---

## 13. Publish and inspect the NAV attestation

1. Open `Audit` → **NAV attestation viewer**.
2. **RWA mint** = the fund mint.
3. Read back:
   - **Legal name** — `Meridian USD Liquidity Fund Class A`
   - **Ticker** — `MMF-A`
   - **Metadata URI** — clickable link to the attestation
   - **Decimals** — `6`
   - **Total supply** — current shares outstanding
   - **Detected ISIN / Detected CUSIP** — parsed out of the metadata text
4. The **On-chain audit trail** panel lists every transaction this wallet signed,
   with slot, block hash, time and status. **Export CSV** downloads what is shown;
   **Print / PDF** produces a statement. Block hashes are fetched for the 10 most
   recent transactions only.

Finally, `Portfolio` → **Portfolio** scans the wallet and lists the DLMM position
(**Liquidity positions**), TWAMM orders (**TWAMM orders**) and pending **Settlement
escrows**. Resting limit orders are read on the `Orders` page, not here. Press
**Rescan** after the trades above.

---

## 14. The daily NAV cycle

The chain does not store NAV, so "keeping NAV correct" is an off-chain loop plus a
few on-chain guards:

1. **Administrator publishes** the daily NAV to the stable attestation URI.
2. **Transfer agent** confirms the bank/custody reconciliation and records the
   attestation hash off chain.
3. **Market makers re-peg**: add or remove liquidity so the active bin tracks
   $1.00. The **Admin & maintenance** panel is for bin arrays and authority, not for
   moving price.
   - With `Oracle deviation` armed, a price that runs away from the oracle trips the
     breaker instead of letting the pool trade at a broken NAV.
4. **Compliance** reviews the `Audit` trail and the escrows due today.
5. **Subscription/redemption window** closes: mint new shares at the published NAV
   (`RWA` → **Mint to my ATA** for the issuer, then **Transfer (hook-approved)** to
   the subscriber), and for redemptions receive shares into the issuer wallet and
   pay out stablecoin.

Because the transfer fee, pause flag, root, and authorities are all on chain, every
step above has an auditable signature.

---

## Verification checklist

- [ ] `KYC` → **KYC gate** shows the issuer and investor as `approved`.
- [ ] `KYC` → **Transfer-hook whitelist** computed root equals the on-chain root,
      and the pool PDA, hook authority, settlement escrow authority and order PDA
      each have an entry alongside the wallets.
- [ ] `RWA` → **Mint inspector** shows the expected `Supply`, `Decimals = 6`, the
      hook program, extra account metas and hook config.
- [ ] Investor balance equals what was delivered, minus nothing (transfers carry the
      fee on the recipient side once a fee is set).
- [ ] The transfer status line was green **before** pressing
      **Transfer (hook-approved)**.
- [ ] `DLMM` → **Pool state** shows the NAV pool, KYC required, settlement wired,
      and the active bin at `0`.
- [ ] `Orders` → **Resting orders** lists the order, and `book` reads `open`.
- [ ] `Settlement` → **Settlement escrows** shows the fill, then `released` after
      **Settle**.
- [ ] `Audit` → **NAV attestation viewer** resolves name, ticker, URI, decimals,
      supply and ISIN.
- [ ] `Portfolio` lists the DLMM position and the escrow after **Rescan**.

## Failures specific to a NAV share

| Symptom | Cause | Fix |
|---------|-------|-----|
| `The imported whitelist does not reproduce the on-chain Merkle root` | The `Enterprise` list and the published root diverged | Re-import the exact published set, or `Update root` to the list you have |
| Green proofs, but **Register both whitelist entries** is disabled | One entry's proof was invalidated by a newer root | Rebuild proofs, re-register both entries |
| Transfer fails right after creating the mint | Hook metas not registered for that mint | **Register hook metas for this mint** |
| Transfer fails for a *new* investor | Their entry was never registered | Register the entry from `KYC` |
| First deposit or swap fails on the new pool | The pool PDA was never whitelisted (or the root changed after it was) | Add the pool PDA, `Update root`, re-register every entry |
| Fill fails with a missing hook account | The order PDA is not whitelisted, because the order id differs from the one you registered | Place the order with the pre-registered id, or register the new order PDA |
| `Amount too small` on mint/transfer | You typed whole shares instead of raw units | Multiply by `10^6` |
| Pool trades at 0.001 × the expected price | Mint decimals differ from the quote's | Use matching decimals or offset `Active bin id` |
| Fill rejected with `more than the order has left` | Another taker filled it first | Refresh **Resting orders** and refill |
| Settle rejected | Slot not reached, or wrong signature | Wait for the countdown, re-sign with the agent key |
| `No TokenMetadata found on this mint` in the viewer | Wrong mint, or not a Token-2022 RWA mint | Paste the exact mint from the RWA page |

## Parameter summary

| Parameter | This playbook | Typical range | Effect |
|-----------|---------------|---------------|--------|
| Decimals | 6 | 6 (fund share) | Must match the quote for 1:1 bin pricing |
| Transfer fee | 2 bps, cap 0.05 | 0–25 bps | Charged on the recipient side per transfer |
| DLMM bin step | 1 bps | 1–10 bps for a $1 peg | Finer step = tighter peg, more bins to seed |
| DLMM base fee | 2 bps | 1–10 bps | LP revenue per swap |
| KYC rail | On | On for regulated funds | Requires an attestation per trader |
| Oracle deviation | 50 bps | 25–200 bps | Circuit-breaker distance from the oracle |
| Settlement | On, T+100 slots | 100–900 slots | Delay between trade and release |
| Order price | 1.0000 | 0.9990–1.0010 | Quoted per share, human units |
| Order expiry | 1440 min | 60–10080 | Fills stop at expiry; cancel stays open |
