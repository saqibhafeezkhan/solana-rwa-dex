# Proposed On-Chain Features — Solana RWA DEX

An engineering assessment of features worth adding to the ten programs, with
batching examined first. Each proposal states the mechanism, where it plugs into
the existing code, the constraints that decide whether it is feasible, and the
security questions it raises.

This is a proposal document, not a specification. The clearinghouse items **P4**
(deadline + permissionless refund), **P6** (per-mint agent keys), **P7**
(rent reclamation), **P16** (fixed escrow authority) and the low-s normalization are now implemented; everything
else is still a proposal. The current behaviour of every program is documented
in [`features.md`](features.md), the per-program READMEs and
[`security-review.md`](security-review.md).

---

## How to read this

**Ground rules that shape every proposal**

1. **Solana compute budget.** A transaction is capped at **1.4M compute units**.
   The expensive primitives already in the protocol are `secp256k1_recover`
   (~25k CU per call), token transfers (~5–15k CU each), and account
   deserialization. That ceiling, not intent, decides batch sizes.
2. **Account limits.** A transaction can reference **64 accounts** without
   address lookup tables (128 with). Each `SettlementVault` release touches
   roughly four accounts (vault, config, vault token account, taker token
   account), so ~16 vaults is the realistic per-instruction ceiling even before
   compute is considered.
3. **Additive by default.** Proposals below avoid changing existing account
   layouts unless explicitly noted, so they do not invalidate deployed state or
   client decoders.
4. **Keepers are the customers.** Batching exists to let a cranker clear many
   trades/orders per slot. If the batch is all-or-nothing, one bad item blocks
   the rest — so batch semantics matter as much as the batch itself.

---

## Priority summary

| # | Proposal | Program(s) | Value | Effort | Risk |
| :-- | :--- | :--- | :--- | :--- | :--- |
| P1 | `settle_batch` — release many T+N escrows per transaction | `settlement` | High | Med | Med |
| P2 | `cancel_settlement_batch` — compliance exits in bulk | `settlement` | High | Low | Low |
| P3 | `issue_kyc_batch` / `revoke_kyc_batch` — onboarding & sanctions sweeps | `kyc_gate` | High | Low | Low |
| P4 | Settlement deadline + permissionless `refund_expired` (implemented) | `settlement` | High | Low | Low |
| P5 | Anchor events on all programs | all | High | Low | Very low |
| P6 | Per-mint / per-asset-class Transfer Agent keys (implemented) | `settlement` | Med | Med | Med |
| P7 | `close_settlement` — reclaim vault rent (implemented) | `settlement` | Med | Low | Low |
| P8 | Limit-order / partial-fill order book | new program | High | High | High |
| P9 | Transferable LP positions (Token-2022 position tokens) | all AMMs | Med | High | Med |
| P10 | `swap_exact_out` on all engines | AMMs | Med | Med | Med |
| P11 | Protocol fee switch (treasury cut) | AMMs | Med | Low | Low |
| P12 | Maverick keeper bounty for `move_bins` | `maverick` | Med | Low | Low |
| P13 | `close_position` / rent reclamation | AMMs | Low | Low | Low |
| P14 | On-chain swap router (`swap_split`) | new program | Med | High | High |
| P15 | Two-step authority handshake + timelock | all | Med | Med | Low |
| P16 | Fixed escrow authority (one whitelist entry for every escrow) (implemented) | `settlement`, `transfer_hook` | High | Med | Med |

---

## 1. Batching

Batching is the highest-value category because every keeper-driven flow in the
protocol is currently one trade per transaction: T+N settlement, TWAMM cranks
(already batched over orders within one pool), and KYC administration.

### P1 — `settlement::settle_batch`

**Mechanism.** One instruction that releases many `Pending` vaults, iterating the
vaults passed as remaining accounts. Each vault keeps its own gates: slot passed
**and** a valid Transfer Agent signature over its own
`keccak256(trade_id ‖ taker ‖ mint ‖ amount)` preimage.

**Signature scheme.** Two options:

- **(a) Per-vault signatures.** Each vault carries its own `(signature,
  recovery_id)`. No new trust assumptions, no new preimage, and the Transfer
  Agent's existing signing backend is unchanged. Cost: one `secp256k1_recover`
  per vault, the dominant CU term.
- **(b) One signature over a batch Merkle root.** The agent signs a root over the
  batch; the instruction verifies a Merkle path per vault. Cheaper per vault
  (~1 Keccak fold vs. a recover) and mirrors the whitelist design already in the
  codebase, but it is a genuinely new signing convention the agent must adopt.

**Recommendation:** ship (a) first. It is a pure loop over the existing `settle`
body and introduces no new cryptographic surface. Add (b) only if keeper CU
costs prove to be the bottleneck.

**Semantics.** Skip vaults whose status is not `Pending` (rather than reverting),
and require **at least one** release so a no-op batch fails loudly. This keeps an
already-settled vault from blocking the rest — the alternative, all-or-nothing,
turns one stale vault in the keeper's list into a failed transaction.

**Constraints.** ~4 accounts and ~25k CU per vault → **~12–16 vaults per
instruction** is the realistic ceiling. Expose a `max_batch` constant and let
keepers chunk; do not accept an unbounded number.

**Security.** The batch must not weaken any per-vault check: the slot gate, the
recovery comparison, and the taker-ownership check on the payout account all
still apply per item. The main new risk is **account duplication** — the same
vault supplied twice would attempt a double release; the loop must reject
duplicates explicitly (a settled vault's status flips to `Settled`, so the second
visit would skip, but relying on that is fragile against reentrancy ordering).

### P2 — `settlement::cancel_settlement_batch`

Same shape, authority-only, for compliance exits (a sanctions batch, a
jurisdiction change). Low risk because the refund destination is already pinned
to each vault's recorded `refund_source` (the H-1 fix), so a batch cannot divert
funds. Skip non-`Pending` vaults.

### P3 — `kyc_gate::issue_kyc_batch` / `revoke_kyc_batch`

Onboarding a cohort and offboarding a sanctions hit are naturally batch
operations. Neither is CU-heavy: a mint/burn plus a token-account read per
institution. The ceiling here is accounts (~3 per institution: config, mint,
attestation account), so ~20–40 per instruction.

**Design note:** `issue_kyc` requires an empty account and `revoke_kyc` requires
exactly one token — the batch must preserve both invariants per item and skip
(or fail) on duplicates. For sanctions sweeps, a **skip-and-report** semantic
(burn those that exist, ignore those that do not) is more useful than
all-or-nothing, so the sweep is idempotent and re-runnable.

### `kyc_gate::set_authority` needs no batching

It is a single write. But see P15 (two-step handshake) — this is the one
authority rotation in the protocol where a typo is unrecoverable.

### `transfer_hook`: publish the root and the pause atomically

Transfers cannot be batched at the hook level (Token-2022 invokes the hook once
per transfer). The useful addition is atomicity on the **admin** side: a single
`publish_root(root, paused)` that sets the root and the pause flag together, so
the documented safe sequence (clear off-chain → pause → publish → resume) cannot
be split across transactions with a half-applied state in between. The current
two-instruction `update_root` + `set_paused` is fine but leaves a window.

### `rwa_mint::mint_to_batch`

A distribution to many holders (coupons, redemptions) in one transaction. Each
item is ~2 accounts + a mint CPI. Modest value; only worth it if distributions
are a real workflow.

### What batching does *not* fix

Batching reduces transaction count, not total work — every item still performs
its full validation. It also does not reduce the **account** count, which is the
binding constraint long before compute. A batch that needs 20 vaults (80
accounts) will not fit without address lookup tables.

---

## 2. Clearinghouse upgrades

### P4 — Settlement deadline and permissionless `refund_expired`

**Status: implemented.** `initiate_settlement` now records
`expiry_slot = settlement_slot + EXPIRY_WINDOW_SLOTS` (216 000 slots, a program
constant), `settle` refuses at or after that slot, and the permissionless
`refund_expired` returns the full escrow to the pinned `refund_source` and marks
the trade `Expired`. The deadline is one constant for every trade rather than a
per-trade argument, so the AMM/hooks CPIs did not change.

**Security (as delivered):** expiry is refund-only (never a release), reuses the
pinned `refund_source`, and is blocked once the trade is no longer `Pending`. A
keeper can crank it but cannot redirect a unit. Watch the interaction with P1: an
expiry batch is another remaining-accounts loop.

### P16 — A fixed escrow authority, so one whitelist entry covers every trade

**Status: implemented.** The escrow owner is now `[b"escrow_authority"]` under the settlement
program; each trade's token account is still the per-trade `[b"escrow", trade_id_le]` PDA. The
seeder, the web client and `tests/ts/rwa-dex.ts` register and use the single escrow-authority
entry.

**Problem (historical).** `transfer_hook` resolves each side's `WhitelistEntry` from that token account's
`owner` field and checks **both** sides. The escrow token account is owned by the **per-trade
vault PDA**, so every new trade's escrow needs `[b"whitelist", vault]` written before a hooked
(RWA) mint can move into it — and entries are only valid against the root they were written for,
so publishing a tree that contains the new vault invalidates every other member until each is
refreshed. One root rotation per trade is not an operating model.

**Mechanism.** Give the escrow token accounts one fixed owner: a PDA of this program,
`[b"escrow_authority"]`. Keep the accounts themselves per trade (address
`[b"escrow", trade_id_le]`) but set their token-account `owner` to that PDA, and sign the release
and refund with `escrow_authority`'s seeds instead of the vault's. The hook then resolves
`[b"whitelist", escrow_authority]` on every transfer — one entry, registered once, and valid for
as long as the root does not rotate.

**Why it is sound.** The whitelist still gates both sides of every transfer, and the
counterparty (funder, taker, refund source) still has to fold to the live root. Value can only
leave an escrow through `settle` (agent signature, payout pinned to the recorded `taker`) or
`cancel_settlement` (pinned to `refund_source`), and both now sign as `escrow_authority`, which
only this program can produce. Trade identity stays in the per-trade `SettlementVault`.

**Cost (as delivered).** An account-model change: escrow creation (a plain token account via
`create_account` + `initialize_account`, since it is no longer an ATA of its owner), the release
and refund signer seeds, `hooks::after_swap`'s pinning of the escrow account, both client
builders, and the seeder.

### P6 — Per-mint / per-asset-class Transfer Agent keys

**Status: implemented.** `set_mint_agent` writes a `[b"agent", mint]` PDA with
the same 64-byte uncompressed secp256k1 key format as the config; `settle` uses
it when the caller passes that account and otherwise falls back to the config's
global key. `clear_mint_agent` removes the override and refunds its rent. The
optional account is the settlement program ID (Anchor's `None` sentinel), so the
non-override path is unchanged.

**Design tension (as delivered):** a per-mint PDA adds one optional account to
`settle`. The apply-vs-global choice is the caller's account list, which is
deliberate but means a client must remember to pass the override.

### Signature malleability normalization

**Status: implemented.** `settle` rejects `s > n/2` with `HighSignatureS`, so
each authorization has exactly one wire form for audit logs. The low-s predicate
is unit-tested at the boundary, the high twin and the maximum.

### P7 — `close_settlement`

**Status: implemented.** Once a trade is `Settled`, `Cancelled` or `Expired`,
the config authority can close the empty escrow token account and the vault PDA,
reclaiming both rents. It refuses a `Pending` vault or a non-empty escrow. The
permanent vault record is gone after closing, so an indexer that needs the
history should rely on events (P5) or snapshot before closing.

### P5 — Anchor events

The programs emit `msg!` logs and define **no** `#[event]` types, which pushes
all indexing onto account-state diffs. Adding typed events
(`SettlementInitiated`, `TradeSettled`, `BinMoved`, `VirtualOrdersExecuted`,
`KycIssued`, `KycRevoked`, `RootUpdated`) is low-effort, low-risk, and unblocks
the indexer/analytics services that are currently placeholders.

---

## 3. Order management (the roadmap's biggest gap)

The root README lists "order management services and limit-order / partial-fill
flows" as not-yet-built. A limit order is the feature that turns this from a swap
venue into something desks can use.

### P8 — `order_book` program

**Shape.** A maker escrows the input into a PDA and records a price/expiry; a
taker fills it (fully or partially) against the escrowed balance; the output side
routes through the **existing** `hooks` → `settlement` T+N rail so compliance and
clearing are unchanged. Cancellation returns the unfilled escrow.

**Why a new program and not an AMM.** None of the four engines can express a
resting order with a counterparty who is not the pool: `clmm` range orders are
synthetic and only work against the curve; `dlmm`/`maverick` bins are LP
liquidity, not maker orders; `twamm` orders are time-sliced, not price-triggered.

**Reuse.** The escrow/refund pattern, the `trade_id` uniqueness convention, the
pinned-account discipline (M-1/M-2 fixes), and the settlement CPI are all already
solved in `hooks`/`settlement`. This is largely a composition of existing
patterns.

**Risks.** Partial fills make the escrow accounting the hard part (per-fill
proceeds, fee treatment, and the T+N routing of each fill's output). And the
"maker may front-run the taker's fill" question needs an explicit answer
(cancel delay or a fill-commitment window).

### P9 — Transferable LP positions

Positions are PDAs keyed by owner (`clmm-pos`, `position`, `twamm-order`), so
they cannot be sold, collateralized, or transferred. If LP exposure is meant to
be a tradable claim on an RWA venue, positions should be Token-2022 tokens
(themselves subject to the KYC gate), with the program holding the underlying
liquidity. This is a large change to the position model but a significant
product unlock.

### TWAMM: batch cranks across pools

`execute_virtual_orders` already batches every order in one pool per call. The
next step is a keeper instruction that cranks **several pools** in one
transaction (remaining accounts grouped per pool). Feasible; bounded by the
per-pool oracle/wiring accounts each pool drags in.

---

## 4. Execution-engine additions

### P10 — `swap_exact_out`

All four engines are exact-input only. Exact-output ("buy exactly 100 000
units") is a common institutional need and is the natural inverse of the
`compute_swap_step` machinery already in `math_core`. Requires a new invariant
(the pool must not spend more input than `max_in`, and the fee-on-output
rounding must again favour the pool).

### P11 — Protocol fee switch

None of the engines has a treasury cut. A `protocol_fee_pips` fixed at pool
initialization, accrued in a separate accumulator and claimable by a designated
recipient, is a small change — but note it is a **rent-extraction** decision, not
a technical one, and it must not change the LP fee-growth accounting
(`fee_growth_global_*` must still reflect only LP fees).

### P12 — Maverick keeper bounty

`move_bins` is permissionless but unpaid, so the incentive is the secondary
arbitrage the relocated depth creates. A small bounty paid from the moved bin's
fees (or a `MaverickConfig` treasury) would make keeper behaviour reliable. Must
be paid only on a successful, value-neutral move, and must not come out of other
LPs' fee growth.

### P13 — Rent reclamation

`close_position` for `clmm`/`dlmm`/`maverick` positions and spent tick/bin
accounts would return rent to LPs who fully exit. Small but concrete, and it
removes the "dust position holds rent hostage" annoyance.

### CLMM: price limits and flip staleness

`docs/features.md` notes CLMM has no explicit price limit (the tick walk is the
limit) and no staleness check on tick crossings. An explicit `sqrt_price_limit`
parameter and a `tick_ceil`-style cross-check would make the swap harder to
misuse and the accounting more robust.

---

## 5. Routing

### P14 — On-chain split router

`router::swap_split(legs[])` CPIs into several pools in one transaction, giving
atomic best execution across engines. **Recommendation: do not build this
first.** A client-side router (the web console already derives all PDAs and
builds instructions) gets most of the benefit with none of the account-plumbing
risk, and the cross-engine arbitrage the T+N lag creates is already documented.
An on-chain router becomes worth it when atomicity across venues is required —
at which point the account-count ceiling is the design constraint.

---

## 6. Governance hardening

### P15 — Two-step authority handshake with a timelock

`set_authority` is single-step and takes a plain key (deliberately, so a multisig
vault can be the successor). That is also why a wrong-but-valid address is
unrecoverable. A two-step `propose_authority` / `accept_authority` with an
optional delay removes the footgun without losing the vault-PDA case. The
`docs/features.md` note that "rotation is single-step, with no handshake" is the
gap this closes.

---

## 7. If I were sequencing this

1. **P5 events + P2/P3 batches + P4 expiry** — low effort, immediately unblocks
   indexers and removes the settlement-deadline footgun.
2. **P1 `settle_batch`** — the highest-value keeper primitive, once the batch
   semantics (skip vs. revert) are agreed.
3. **P6 per-mint agent keys** — required before more than one asset class is
   cleared through the same deployment.
4. **P8 order book** — the roadmap's real product gap; build it on the existing
   escrow + settlement rails.
5. Everything else as the asset classes and volumes justify it.

**Explicitly rejected for now:** on-chain routing (P14) and transferable LP
positions (P9) are both good ideas whose cost lands before their value; a
client-side router and a documented position model get further per unit of risk.
