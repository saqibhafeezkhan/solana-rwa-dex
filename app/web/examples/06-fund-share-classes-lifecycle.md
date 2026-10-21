# 06 — Two share classes and the fund lifecycle runbook

Issue a fund with an income class and an accumulating class, run a subscription
window, rotate authorities, offboard an investor, and wind the fund down — all from
the UI.

Assets:

| Class | Symbol | Decimals | NAV | Fee | Policy |
|-------|--------|----------|-----|-----|--------|
| A (income) | `MFA` | 6 | $1.00 | 2 bps | Distributing |
| B (accumulating) | `MFB` | 6 | $1.25 | 0 bps | Reinvests |

One fund, two mints, two pools, one whitelist and one KYC gate. The classes differ
only in their metadata document, transfer fee and price level — everything else is
shared infrastructure, which is why authority rotation and offboarding are fund-wide
operations rather than per-class ones.

> Read [01](01-tokenized-fund-nav.md) for the mechanics of creating the first mint
> and pool. This file is the **operations** companion: it covers the things that
> happen after launch.

---

## Part 1 — Issue the two classes

### 1.1 Whitelist and KYC once for the whole fund

1. `Enterprise` → **Whitelist import (one wallet per line)**: issuer, custodian,
   transfer agent and every investor. Press **Apply whitelist**.
2. `Enterprise` → **Counterparty address book**: save each wallet with a label; the
   **Whitelist** column confirms membership.
3. `KYC` → **KYC gate** → **Issue KYC** per investor and per service provider.
4. `KYC` → **Transfer-hook whitelist** → paste the same list → **Build proofs** →
   **Update root** → register an entry per wallet (press **Build proofs** again for
   each sender/recipient pair; the displayed proofs do not refresh on their own).

Both mints use the **same hook program**, so one root and one set of entries cover
`MFA`, `MFB` and every future class. That is the main operational advantage of a
single-fund architecture.

> This first root covers wallets. It gets re-published with the pool and custody
> PDAs in §1.2b, and every entry has to be registered again.

### 1.2 Create both mints

`RWA` → **Create RWA mint**, twice:

| Field | Class A | Class B |
|-------|---------|---------|
| Decimals | `6` | `6` |
| Name | `Meridian Income Fund Class A` | `Meridian Accumulating Fund Class B` |
| Symbol | `MFA` | `MFB` |
| URI (ISIN / NAV / prospectus) | `https://fund.example/mfa/attestation.json` | `https://fund.example/mfb/attestation.json` |

Recover each mint address from the **Mint (defaults to the freshly created one)**
placeholder with
[Reading the new mint's address](README.md#reading-the-new-mints-address) — do it
before leaving the page for the second class — and press **Register hook metas for
this mint** once per mint. The hook metas are per mint even though the hook program
is shared.

### 1.2b Create the quote mint and whitelist the custody accounts

Both classes are Token-2022, and a DLMM pool carries **one token program** for its
two legs, so the quote has to be Token-2022 as well.

1. `RWA` → **Create RWA mint**: **Decimals** `6`, **Name** `Test USD`, **Symbol**
   `TUSD`, stable URI. Recover the address, press
   **Register hook metas for this mint**, then mint `100000000000000` to the issuer.
2. `DLMM` → **Pool selector**: pair `MFA` / `TUSD` → copy the **Derived pool PDA**.
   Repeat for `MFB` / `TUSD`. Do not create either pool yet.
3. `bun examples/pdas.ts` → copy the **hook authority** and **settlement escrow
   authority** PDAs. Settlement-routed output parks in the hook authority's custody
   account and pays out of the escrow authority's vault.
4. `Enterprise` → **Whitelist import**: append both pool PDAs, the hook authority
   and the settlement escrow authority → **Apply whitelist**.
5. `KYC` → **Transfer-hook whitelist**: paste the extended list → **Build proofs**
   → **Copy root** → **Update root**, then register an entry for every wallet and
   every PDA again.

If you plan to quote class shares on the order book, derive those order PDAs too
(`bun examples/pdas.ts --pair <CLASS> <TUSD> --maker <ISSUER> --order-id <id>`) and
whitelist them **before** placing the order — the order PDA owns the share escrow.

### 1.3 Set per-class fees

`RWA` → **Mint, transfer & fees**, once per mint (change the **Mint** field between
calls):

| Class | New transfer fee (bps) | New maximum fee (raw) |
|-------|------------------------|-----------------------|
| A | `2` | `50000` (0.05 shares) |
| B | `0` | `0` |

Press **Update transfer fee** per class. Fees are a mint-level setting; the fund's
"one fee policy" still has to be applied class by class.

### 1.4 Mint the seed inventory

For each class: **Amount** = the seed in raw units (e.g. `1000000000000` = 1M
shares), **Holder** empty, **Mint to my ATA**.

### 1.5 Two pools, two price levels

`DLMM` → **Create DLMM pool** per class. Set the pair in **Pool selector** first (the
create panel reads it from there), then fill the create fields. Bin 0 is price 1.0,
so the class NAV decides the active bin:

| Field | Class A | Class B |
|-------|---------|---------|
| Mint A | `MFA` | `MFB` |
| Mint B | `TUSD` | `TUSD` |
| Active bin id | `0` (≈ $1.00 per unit) | `2232` (≈ $1.25 per unit) |
| Bin step (bps) | `1` | `1` |
| Base fee (bps) | `2` | `2` |
| Require KYC | `On` | `On` |
| Oracle deviation | `50` bps | `50` bps |
| Settlement routing | `On` | `On` |
| T+N slots | `100` | `100` |

`bin_id = round(ln(NAV) / ln(1.0001))` for a 1 bp step:
`ln(1.25)/ln(1.0001) ≈ 2231.6`, which the helper rounds to `2232`.
Use **Target NAV / price** = `1.25` + **Select bin at NAV** rather than typing the
bin by hand, and remember the helper wants the **raw** ratio if the class decimals
differ from the quote's.

Seed liquidity around each active bin, then confirm **Pool state** for both.

---

## Part 2 — Run a subscription window

A window is a compliance process wrapped around ordinary transfers. Nothing on chain
knows what a "window" is; the freeze flag and the published attestation are the
controls.

### 2.1 Before the window opens

1. **Add every new subscriber to the whitelist and publish the new root.** A root
   change invalidates existing entries, so:
   - `Enterprise` → **Whitelist import** → add the new wallets → **Apply whitelist**.
   - `KYC` → **Transfer-hook whitelist** → **Build proofs** → **Update root**.
   - **Re-register an entry for every existing wallet**, not only the new ones.
2. **Issue KYC** for each new subscriber.
3. Confirm both classes' escrows and authorities are healthy:
   - `KYC` → **Hooks escrow router** shows a funded hook authority.
   - `Settlement` → **Clearinghouse config** shows the transfer agent key.
4. Set the top-bar execution mode. `Multisig proposal` is the right choice when a
   treasury committee must approve deliveries.

### 2.2 During the window

For each subscription, in NAV order:

1. Mint the class shares to the issuer (if the shares do not already exist):
   `RWA` → **Mint to my ATA** with the class mint and the raw amount.
2. Deliver them: **Destination (transfer)** = subscriber, confirm the green
   compliance note, press **Transfer (hook-approved)**.
3. Record the cash leg. Either receive stablecoin directly, or lock the subscriber's
   payment in a settlement escrow if the cash arrives after the shares.

Do not change the root mid-window. If a subscriber must be added, the root changes
and every entry needs re-registering — finish the current batch first.

### 2.3 At cut-off: freeze and strike

1. `KYC` → **Transfer-hook whitelist** → **Pause transfers**.
   The cap table is now frozen: no further RWA transfers, including the issuer's.
2. `DLMM` → **Admin & maintenance** → **Pause pool** on both pools. Swaps stop;
   withdrawals and fee collection stay open. **Trip circuit breaker** is the
   oracle-driven variant and latches the same pause flag.
3. Strike the NAV for both classes and publish the two attestation documents.
   `Audit` → **NAV attestation viewer** reads each mint's name, ticker, URI,
   decimals, supply and detected ISIN/CUSIP.
4. Cash settlements continue normally — **Pause transfers** blocks the RWA mints,
   not stablecoin escrows. `Settlement` → **Settlement escrows** → **Load** →
   **Settle (release escrow)** as payments arrive.

### 2.4 After the window

1. `KYC` → **Resume transfers**.
2. `DLMM` → **Admin & maintenance** → **Unpause pool** on both pools.
3. `Portfolio` → **Rescan** and reconcile each class's supply against the
   subscription log.
4. `Audit` → **Export CSV** for the subscription file; **Print / PDF** for the
   committee pack.

---

## Part 3 — Redemption window

1. Holder transfers shares back to the issuer: **Destination (transfer)** = issuer,
   after the entry check.
2. The issuer locks cash at the struck NAV:
   `Settlement` → **Escrow lifecycle** → **Trade id** (fresh) →
   **Settlement slot (absolute)** → **Amount** → **Mint** = `TUSD` →
   **Taker (recipient wallet)** = holder → **Initiate settlement (lock escrow)**.
3. At the slot, the transfer agent signs and releases:
   **Signature r‖s (128 hex)** + **Recovery id** → **Settle (release escrow)**.
4. Cancel the redemption and refund the issuer with **Cancel settlement** if the
   holder fails to deliver shares.

The shares received by the issuer stay outstanding. There is no burn in the UI; the
issuer holds them as treasury shares, and supply is reported net of treasury in the
attestation.

---

## Part 4 — Offboard an investor

Order matters, because an entry is required for **both** sides of any transfer:

1. Confirm the investor still has a **valid entry** (a recent root update may have
   invalidated it — rebuild and re-register if the proof no longer folds).
2. Have them transfer their full balance to the issuer or the next holder.
3. `KYC` → **Transfer-hook whitelist** → **Institution wallet (entry)** =
   investor → **Close entry (delist)**.
4. **Revoke KYC** for the same wallet on **KYC gate**.
5. Re-publish the root **without** the departed wallet, then **re-register every
   remaining entry** — a new root invalidates all existing paths.
6. Check `Enterprise` → **Counterparty address book**: the row now shows
   **not in whitelist**. Remove it from the book if the relationship is over.

If you close the entry while the wallet still holds tokens, those tokens are
stranded: no transfer can name that wallet as sender or recipient. Re-register the
entry to unwind, then repeat the sequence in order.

---

## Part 5 — Rotate authorities

Every authority in the app rotates with a single call from the current authority.
Verify the incoming address off chain first — the hints say so explicitly, because
the incoming key never has to consent.

| Authority | Where | Field | Notes |
|-----------|-------|-------|-------|
| KYC authority | `KYC` → **KYC gate** | **New authority** → **Rotate KYC authority** | Issues and revokes attestations |
| Hook authority | `KYC` → **Transfer-hook whitelist** | **New hook authority** → **Rotate hook authority** | Publishes roots, pauses transfers |
| Mint / metadata / fee / delegate | Created with the mint | — | No rotation control in the UI; plan the mint authority as a durable key or a multisig |
| Pool authority | Each venue's **Admin** panel | **New authority** → **Rotate authority** | CLMM, DLMM, Maverick, TWAMM |
| Transfer agent (secp256k1) | `Settlement` → **Clearinghouse config** | **Transfer agent secp256k1 (128 hex, x‖y)** → **Set transfer agent** | Per-mint override with **Set mint agent** |
| Settlement hooks authority | `Settlement` → **Clearinghouse config** | **Hooks authority (base58)** → **Set hooks authority** | Needed so AMM-routed output can be escrowed |

### Rotation runbook

1. Add the new authority's **wallet** to the whitelist and register its entry (it
   will need to sign RWA moves and own escrows).
2. Rotate on chain: KYC authority, then hook authority, then each pool authority.
3. Update `Enterprise` → **Counterparty address book** labels so the next operator
   reads the right key.
4. If the transfer agent changed, re-sign a test settlement before the next payout.
5. Confirm the old key can no longer act: attempt a harmless call with it and expect
   a failure.

> The KYC page's **New authority** field is shared by the KYC and hook rotation
> buttons. Fill it once, then press each button deliberately — the same value is
> used for both rotations.

---

## Part 6 — Multisig approval of a mint

Mint delivery is the highest-risk action in a fund, so route it through the proposal
queue:

1. Top bar → switch to `Multisig`.
2. Top-bar role → `Trader` (a role that may queue proposals).
3. `RWA` → **Mint to my ATA** with the class mint and amount. Nothing is broadcast;
   the action lands in `Enterprise` → **Multisig proposal queue** as `pending`, with
   its instruction count and timestamp.
4. Switch the role to `Treasury Manager` or `Admin`.
5. Press **Approve**. The proposal is rebuilt and broadcast from the approver's
   wallet; the status becomes `approved`. **Reject** discards it.

Limits to know:

- Proposals are stored **locally** (base64 payloads in `localStorage`) — they are
  not shared between browsers.
- Actions needing an extra keypair signer, such as **Create mint**, cannot be staged
  and stay direct-only.
- The role gate is a UI control. Approving still requires the connected wallet to
  hold the relevant on-chain authority (mint authority, hook authority, and so on),
  or the rebuilt transaction fails.

---

## Part 7 — Incident and wind-down runbooks

### Incident: suspected key compromise

1. **Pause transfers** immediately (`KYC` → **Transfer-hook whitelist**).
2. **Pause pool** on every pool. On oracle-wired pools **Trip circuit breaker (arm)**
   adds the oracle's own latch, which only the oracle owner can clear with
   **Unpause pool (oracle owner)**.
3. Rotate the hook authority and KYC authority to the incident key.
4. Rotate pool authorities.
5. Publish a new root **without** the compromised wallet, then re-register every
   remaining entry.
6. Review `Audit` → **On-chain audit trail** for the exposure window; export CSV.

The permanent delegate cannot be rotated from the UI, so a compromised mint authority
is a mint-level incident: treat it as a token migration, not a rotation.

### Incident: wrong root published

1. `KYC` → **Update root** with the correct 64-hex value.
2. Re-register **every** entry from proofs rebuilt against the corrected list.
3. `RWA` → **Reload whitelist**, then verify the green
   `Proofs reproduce the on-chain root and both entries are registered` note before
   the next transfer.

### Incident: stuck escrow

| State | Action |
|-------|--------|
| Pending, slot not reached | Wait for the countdown to read `ready` |
| Pending, slot reached, no signature | Ask the transfer agent to sign, or **Cancel settlement** |
| Expired | **Refund expired escrow** |
| Settled | **Close & reclaim rent** |

### Wind-down

1. Stop quoting: `Orders` → **Cancel** every resting order (refund includes donated
   tokens and rent).
2. `DLMM` → **Remove liquidity** from each bin; `CLMM` → **Remove liquidity**;
   **Collect fees** last.
3. **Pause transfers** and **Pause pool** on both venues while the final cap table is
   computed.
4. Settle or cancel every escrow; close the empty ones to recover rent.
5. Receive all shares back to the issuer via **Transfer (hook-approved)**.
6. **Close entry (delist)** every investor, then **Revoke KYC**.
7. Publish the final NAV attestation and keep the URI stable — the `Audit` viewer is
   the permanent record.
8. Rotate all authorities to the wind-down custodian.

---

## Verification checklist

- [ ] Both classes exist, each with hook metas registered and the intended fee.
- [ ] Both pools load by their mint pair and sit at the intended active bin.
- [ ] One whitelist root covers both classes; every wallet has an entry, plus both
      pool PDAs, the hook authority and the settlement escrow authority.
- [ ] A subscription delivered during the window is visible in the investor's
      balance; the cut-off freeze shows `Paused` transfers.
- [ ] The struck NAV documents resolve in **NAV attestation viewer** for both mints.
- [ ] Redemption escrow locks and releases exactly the struck amount.
- [ ] Offboarding left the investor's balance at zero, entry closed, KYC revoked, and
      the remaining entries re-registered against the new root.
- [ ] After rotation, the old authority is rejected and the new one is accepted.
- [ ] The proposal queue shows the approved mint with its approver.
- [ ] After wind-down: no resting orders, no liquidity, no pending escrows, zero
      investor balances.

## Failures specific to multi-class operations

| Symptom | Cause | Fix |
|---------|-------|-----|
| Transfers fail for everyone after adding an investor | Root changed and entries were not re-registered | Rebuild and re-register every entry |
| First deposit fails after the pools are created | The pool PDA has no whitelist entry | Add both pool PDAs, `Update root`, re-register every entry |
| Redemption release fails | The settlement escrow authority was never whitelisted | Whitelist it and re-register every entry |
| One class trades, the other rejects | The pool was created before the mint's hook metas | **Register hook metas for this mint** |
| Investor cannot send after offboarding | Entry was closed while they still held tokens | Re-register the entry, transfer out, then close again |
| Redemption settles to the wrong amount | Struck NAV applied with the wrong decimals | Convert to raw units for the class |
| Proposal will not execute | Connected wallet lacks the on-chain authority | Approve from the authority wallet |
| Pool still paused after the window | The admin pause was not cleared | Press **Unpause pool** in the venue's admin panel |
| NAV viewer shows the wrong class | Mint field still holds the other class | Paste the exact mint address |
| Root update "succeeds" but transfers fail | Invalid entries kept their old proofs | Rebuild and re-register per wallet |

## Parameter summary

| Parameter | Class A | Class B |
|-----------|---------|---------|
| Decimals | 6 | 6 |
| Quote | `TUSD` (Token-2022) | `TUSD` (Token-2022) |
| NAV target | $1.00 | $1.25 |
| DLMM active bin (1 bp) | 0 | 2232 |
| Transfer fee | 2 bps | 0 |
| Distribution | Income | Accumulating |
| Pool rails | KYC on, settlement T+100 | KYC on, settlement T+100 |
| Pause control | Fund-wide hook flag | Fund-wide hook flag |
| Whitelist | Shared | Shared |
