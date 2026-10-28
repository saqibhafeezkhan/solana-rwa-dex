# 07 — One desk, one Transfer Agent per asset

Clear two different instruments through the same settlement program while each
asset class stays under its own Transfer Agent. The per-mint override is what
makes this possible: the config keeps a global key as a fallback, and
`[b"agent", mint]` replaces it for a single mint.

Assets:

| Tick | Instrument | 6 decimals | Clearing agent |
|------|------------|------------|----------------|
| `TBILL` | Tokenized T-bill fund, Series 2027 | 1 token = 1 fund share | Meridian Trust (Agent A) |
| `TCP` | 90-day trade-finance commercial paper | 1 token = 1 note | Northbridge Clearing (Agent B) |

Quote for both pools: a 6-decimal Token-2022 test stablecoin (`TUSD`).

Why two keys: the desk can move either instrument, but only the agent that
clears *that* asset class may release its escrow. A single global key would let
either agent release either instrument — the override is a key rotation scoped
to one mint, not a second policy, so every other check (low-s signature form,
the signed preimage, the expiry window) is unchanged.

---

## Key topology

| Scope | Account | This playbook |
|-------|---------|---------------|
| Global fallback | `settlement_config.transfer_agent_secp256k1` | Desk omnibus key (`DESK`) — used only for mints with no override |
| Per-mint override | `[b"agent", TBILL]` | Meridian Trust key (`AGENT_A`) |
| Per-mint override | `[b"agent", TCP]` | Northbridge Clearing key (`AGENT_B`) |

Rules that matter operationally:

- An override is **installed forever** until it is cleared; there is no expiry
  and no automatic rotation.
- The override **wins** whenever it exists. A signature from the global key is
  rejected for that mint, even if the global key is also valid for other mints.
- Installing a new key over an existing override **updates it in place** (same
  PDA, same rent). Pending escrows immediately start requiring the new key.
- Clearing an override **closes the PDA** and returns its rent to the config
  authority. From then on the mint falls back to the global key — including
  escrows that were opened while the override existed.

---

## 0. Prerequisites

| Requirement | Value |
|-------------|-------|
| App / cluster | `bun run dev`, `localnet`, Sample Data **off** |
| Wallets | `ISSUER` (creates both mints), `DESK` (settlement config authority), `AGENT_A`, `AGENT_B` (secp256k1 keys, not Solana wallets), `INVESTOR`, `TAKER` |
| Mints | `TBILL`, `TCP`, `TUSD` — all Token-2022 (a pool has one token program for both legs) |
| Settlement config | Initialized once, by the settlement program's upgrade authority |
| Rails | **Pool gate** = `Only wired AMM pools may initiate`, if only pools may open escrows |

The two agent keys are 64-byte uncompressed secp256k1 public keys (`x‖y`, no
`0x04` prefix), shown in the UI as 128 hex characters. If your agent hands you a
33-byte compressed key or an address, convert it before typing — the program
compares the raw recovered key.

---

## 1. Create the assets and their venue

Follow [01](01-tokenized-fund-nav.md) for the mint + whitelist mechanics; the
only differences here:

1. `RWA` → **Create RWA mint** twice, once with `TBILL` and once with `TCP`
   metadata. Both get the same hook, metadata pointer, fee config and permanent
   delegate.
2. `KYC` → **Transfer-hook whitelist** → **Update root**, then register entries
   for `ISSUER`, `INVESTOR`, `TAKER`, both pool PDAs, the hook authority and the
   settlement escrow authority. One tree covers both mints, so a root update
   means re-registering every entry.
3. `DLMM` → **Create DLMM pool** for `TBILL/TUSD` and `TCP/TUSD` with
   **Settlement routing: On** and **T+N slots: 300**. The escrow routing is what
   produces the trades this playbook clears.

---

## 2. Initialize the clearinghouse config (once)

`Settlement` → config panel:

1. Set **Pool gate** to `Only wired AMM pools may initiate`, so only a wired AMM
   pool or the hooks authority can open an escrow. `Standalone authority flow
   allowed` also lets a wallet escrow tokens it owns — useful for a desk test,
   wrong for production. The choice is immutable once the config exists.
2. **Initialize config**.

Expect: the panel reproduces the config (authority, gate, global agent). The
button is disabled for everyone except the program's upgrade authority, so this
runs once at deployment.

---

## 3. Register the three keys

There are two separate controls, and getting them the wrong way round is the
most common mistake in this setup:

| Control | Field(s) | Writes |
|---------|----------|--------|
| `Set transfer agent` | **Transfer agent secp256k1 (128 hex, x‖y)** | the config's global key |
| `Set mint agent` | **Per-mint agent — mint** + **Per-mint agent secp256k1 (128 hex, x‖y)** | `[b"agent", mint]` for that one mint |

Steps:

1. Paste `DESK`'s key into **Transfer agent secp256k1** → **Set transfer agent**.
   Expect: the config panel now shows the omnibus key.
2. Set the mint field to the `TBILL` **mint address** (not the pool), paste
   `AGENT_A`, press **Set mint agent**.
3. Repeat with the `TCP` mint and `AGENT_B`.

Expect, on `Settlement` → **Escrow inspector** after each install: the
**Transfer agent** row reads `per-mint override <agent PDA>` for that mint, or
`config global key` when no override is installed.

A zero key is refused (`InvalidTransferAgentKey`); the button stays disabled
until the field holds exactly 128 hex characters.

---

## 4. Open an escrow for each asset

Anything that routes proceeds sets this up — a DLMM swap with settlement routing
on, an `Orders` fill, or `Settlement` → **Initiate settlement (lock escrow)** for
a desk-only transfer.

1. Trade `TBILL` against `TUSD` on `DLMM`. With routing on, the swap's output
   leg is escrowed instead of paid out.
2. Open `Settlement` → **Settlement escrows**. The new row is the `TBILL` trade;
   loading it fills the **Escrow inspector**, whose **Transfer agent** row names
   the governing key.
3. Press **Load** on the `TBILL` row. The lifecycle form fills with the trade id,
   mint, taker and the slot countdown.
4. Read **Escrow inspector** → **Transfer agent**: it must say
   `per-mint override …`. If it says `config global key`, the *Per-mint agent —
   mint* field is pointing at a different mint than the escrow.

Repeat with `TCP`. The two escrows are independent: same desk, different agent.

---

## 5. Release each escrow with its own agent

The agent signs exactly one preimage, with no domain string of its own — the
mint and taker are the domain separation:

```
keccak256( trade_id (u64, little-endian)
        ‖ taker     (32 bytes)
        ‖ mint      (32 bytes)
        ‖ amount    (u64, little-endian) )
```

The signature is secp256k1 over that digest, in low-s form, submitted as `r‖s`
(128 hex) plus the recovery id. Because the mint is inside the preimage, a
signature for the `TBILL` escrow cannot be replayed on a `TCP` escrow even at the
same trade id.

1. `Settlement` → **Settlement escrows** → **Load** the `TBILL` row.
2. Paste `AGENT_A`'s **Signature r‖s (128 hex)** and **Recovery id**.
3. Leave **Taker token account** blank to use the taker's canonical ATA.
4. **Settle (release escrow)**.

Negative check worth running once, on a disposable localnet trade: paste
`AGENT_B`'s signature for the `TBILL` escrow. Expect
`UnauthorizedTransferAgent` (6005) and an unchanged escrow — the wrong agent
cannot move this asset.

Then release the `TCP` escrow with `AGENT_B`'s signature. The UI attaches the
override account automatically whenever the mint has one; there is nothing else
to select.

---

## 6. Rotate an agent mid-flight

When an asset changes clearing agent:

1. `Settlement` → **Per-mint agent — mint** = the asset's mint, paste the new
   key → **Set mint agent**. The PDA address does not change; the key inside it
   does.
2. From that moment, pending escrows for that mint need the **new** key. A
   signature the old agent produced before the rotation is worthless — the
   preimage does not include the key, only the trade.
3. If the rotation must be deferred, wait until no escrow for that mint is
   `Pending`; a pending escrow cannot be released with the retired key and
   cannot be re-signed after the expiry slot (it can only be refunded).

To retire an override entirely:

1. Set **Per-mint agent — mint**, clear the key field, press **Clear mint agent**.
2. Expect: the override account is gone (rent back to the config authority) and
   the escrow inspector reads `config global key` again.
3. Release or refund any pending escrow for that mint with the global key.

---

## 7. Reconcile monthly

- `Settlement` → **Escrow inspector** for each mint: the **Transfer agent** row
  is the authoritative answer to "who can release this asset today". Compare it
  with the clearing agreement.
- `Audit` → **On-chain audit trail** → **Export CSV**: one row per trade with
  trade id, mint, taker, amount, slot and status. A release that used an
  override looks identical to one that used the global key — the key is not in
  the event — so keep the rotation log beside the export.
- Reclaim rent on terminal escrows with `Settlement` → **Close & reclaim rent**,
  not **Clear mint agent**. Clearing an override says something about the asset;
  the close settles up one trade. On a fee-bearing mint the close also harvests
  the escrow's withheld transfer fee to the mint, so the account can actually be
  closed.

---

## Verification checklist

- [ ] `Settlement` config shows the desk's global key.
- [ ] Both mints report `per-mint override …` in the escrow inspector.
- [ ] A `TBILL` escrow released with `AGENT_B`'s signature is refused
      (`UnauthorizedTransferAgent`) and the balance is untouched.
- [ ] The same escrow released with `AGENT_A`'s signature pays the taker.
- [ ] A `TCP` escrow releases with `AGENT_B`, not `AGENT_A`.
- [ ] Rotating an override leaves the PDA address unchanged.
- [ ] **Clear mint agent** closes the PDA, and the next release for that mint
      accepts the global key.
- [ ] Both mints can still trade on their pools after the key changes (the
      override affects settlement release only, not transfers).

## Failures specific to multi-agent clearing

| Symptom | Cause | Fix |
|---------|-------|-----|
| `UnauthorizedTransferAgent` (6005) on a release | The signature came from a key that is not the one governing that mint | Read the escrow inspector's **Transfer agent** row; use that agent |
| `InvalidMintAgent` (6022) | The client supplied an agent account that is not `[b"agent", mint]` | Refresh the page; the UI derives the account from the mint field |
| `InvalidTransferAgentKey` (6006) | All-zero key | Paste the real 128-hex key |
| Inspector says `config global key` when an override exists | *Per-mint agent — mint* holds a different mint (often a pool address) | Use the mint address |
| Override set, but the release still asks for the old key | The page loaded the escrow before the rotation | **Refresh** the inspector and reload the row |
| A pending escrow cannot be settled after a rotation | The old agent's signature no longer matches the installed key | Have the new agent sign; if past `expiry_slot`, use the refund path |
| Two mints, one signature accepted for both | Impossible by construction (the mint is in the preimage) — check you are not reusing an old trade row | Reload the escrow row for the right mint |

## Parameter summary

| Parameter | This playbook | Notes |
|-----------|---------------|-------|
| Mints | `TBILL`, `TCP` | Token-2022, 6 decimals, one shared hook |
| Global key | Desk omnibus | Fallback only |
| Overrides | `[b"agent", TBILL]`, `[b"agent", TCP]` | One per asset class |
| Override size | 73 bytes | 64-byte key + bump, rent by the authority |
| Release preimage | `keccak256(trade_id ‖ taker ‖ mint ‖ amount)` | Mint inside the message |
| Release window | T+300 slots | Past `expiry_slot` only a refund is possible |
| Gate | Only wired AMM pools may initiate | Wallet initiations refused |
| Signature form | `r‖s`, low-s, + recovery id | The high twin is rejected |
