# Security review — `programs/`

Scope: all ten Anchor programs under `programs/` (~15 200 lines of Rust).

| Program | Lines | Role |
| --- | --- | --- |
| `rwa_mint` | 448 | Token-2022 RWA mint factory (hook, metadata, fee, permanent delegate) |
| `transfer_hook` | 1 091 | Merkle whitelist enforced on every transfer of an RWA token |
| `kyc_gate` | 615 | Soulbound KYC attestations + `verify_kyc` CPI target |
| `oracle` | 670 | TWAP observations + circuit breaker |
| `settlement` | 724 | T+N escrow with a secp256k1 Transfer Agent |
| `hooks` | 253 | Routes AMM swap output into settlement escrow |
| `dlmm` | 2 505 | Bin-based AMM |
| `maverick` | 3 354 | Bin-based AMM with bin moves |
| `clmm` | 2 416 | Concentrated liquidity AMM |
| `twamm` | 3 094 | Long-term (TWAP) order execution |

> `programs/order_book` (~1 360 lines) was added **after** this review and was
> not covered by it. It now is: see [Round 2](#round-2--independent-re-review-of-the-whole-tree)
> below, which reviews it in full and re-checks every remaining claim in this
> document against the current source.

Method: manual read of every handler and every `#[derive(Accounts)]` context,
plus targeted pattern searches (`unwrap`, `as u64`, signer/owner/`has_one`
constraints, `invoke`/`invoke_signed`, unchecked/optional accounts, `init_if_needed`).
`libs/math_core` was treated as a trusted dependency and not re-reviewed.

**Overall:** the trust model is unusually explicit for a repo this size — every
privileged instruction is documented, authority rotation refuses the default
pubkey where it matters, one-time initializers are gated on the program's own
upgrade authority (read straight out of the loader's `ProgramData`), and the
"rail" accounts (KYC / oracle / settlement) are pinned per pool at creation
with program-ID sentinels for the disabled case. The findings below are mostly
deviations between the documented invariants and the code, plus a few
unguarded destinations.

---

## Summary

| ID | Severity | Program | Finding | Status |
| --- | --- | --- | --- | --- |
| D-1 | Release-blocking | build | `target/deploy/*-keypair.json` disagree with the declared ids | **Fixed** — ids adopted via `anchor keys sync` |
| H-1 | High | `settlement` | `cancel_settlement` refund destination is unconstrained | **Fixed** |
| M-1 | Medium | `hooks`, `twamm` | Custody source is any hook-authority-owned account, not the pinned ATA | **Fixed** |
| M-2 | Medium | all AMMs | `custody_token_account` documented as ATA-pinned but only owner-checked downstream | **Fixed** |
| M-3 | Medium | `settlement` | `initiate_settlement` sweeps the *entire* source balance | **Fixed** |
| M-4 | Medium | `maverick` | `move_bins` is permissionless over other LPs' bins | **Accepted** — by design; gating is unsafe, rationale below |
| M-5 | Medium | `dlmm`, `maverick`, `twamm` | `set_authority` accepts `Pubkey::default()` | **Fixed** |
| M-6 | Medium | all AMMs | Deposit/withdrawal token accounts not bound to `user` | **Fixed** |
| M-7 | Medium | `rwa_mint` | Permissionless mint factory with arbitrary extensions | **Accepted** — by design; issuer recorded on-chain, documented below |
| M-8 | Medium | `maverick` | `add_liquidity` wrote `pool.has_static_baseline` on a read-only account, so the write was dropped | **Fixed** |
| M-9 | Medium | `maverick` | `move_bins` exhausted the SBF heap (two live `Vec<Bin>`), so bin moves reverted on-chain | **Fixed** |
| L-1 | Low | `oracle` | Accumulator overflow silently swallowed | **Fixed** |
| L-2 | Low | `oracle` | Circuit breaker skipped until 2 observations exist | Open — inherent, alarm-logged |
| L-3 | Low | `clmm` | No admin/pause at all; only the oracle rail can halt swaps | **Fixed** |
| L-4 | Low | `dlmm`, `maverick` | First-depositor share inflation (ERC-4626 pattern) | **Fixed** |
| L-5 | Low | `transfer_hook` | `owner` account is unused by `execute` | **Documented** |
| L-6 | Info | `dlmm`, `maverick` | `value_in_a` share pricing is single-sided | **Documented** |
| L-7 | Info | `transfer_hook`, `kyc_gate`, `settlement` | `require_upgrade_authority` duplicated 3× | **Fixed** |
| L-8 | Info | `maverick` | `Vec<Bin>` has variable-length elements, so the account is not fixed-stride | **Fixed** — `move_state` is a fixed-size `MoveState`, stride 70 |

**Remediation rounds (this branch):** D-1, H-1, M-1, M-2, M-3, M-5, M-6, M-8,
M-9 and L-8 are fixed and verified, along with L-1, L-3, L-4 and L-7. The tree is
`anchor build` clean, 196 unit tests pass across 24 targets, and every
`tests/e2e` suite is green (`clmm` 11, `dlmm` 7, `oracle` 14, `kyc_gate` 9,
`maverick` 9). Three findings are recorded as accepted-by-design rather than
patched: M-4 (gating `move_bins` to a source-bin position holder is unsafe — see
below), M-7 (a permissionless mint factory is a registry problem, not a contract
bug; the mint authority already records the issuer on-chain), and L-2 (the
breaker is inert until a TWAP exists — inherent). L-5 and L-6 remain documented.
Each fixed section below carries the change that landed; the original finding
text is kept for context.

> **Note on the M-6 round.** The AMM deposit/withdrawal/swap token accounts were
> bound to the signing owner with `token::authority` (and, for the bin AMMs and
> the TWAMM, `token::mint`). A new `clmm` e2e case
> (`a_swap_refuses_token_accounts_the_trader_does_not_own`) drives a swap whose
> output/input account is owned by a third party and asserts the refusal
> (`ConstraintTokenOwner`), so the binding is pinned by a test rather than by
> inspection alone.

---

## Release-blocking (deployment integrity)

### D-1 — three `target/deploy/*-keypair.json` files do not match their declared ids

Not a program bug, but it blocks a correct deployment of the current tree. The
program address is determined by the keypair at first deploy, while the crates
and `Anchor.toml` pin it in source. For three programs the two disagree:

| Program | `declare_id!` + `Anchor.toml` | `target/deploy/<name>-keypair.json` |
| --- | --- | --- |
| `settlement` | `49ppDVbrZSqwLDj8sitSapEsMMRfC2Z7wCeMAwhPVQoY` | `58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6` |
| `hooks` | `3DN93k4QRu7Ds2aL2Bba5VnDu18EoRzTNV18jJizAdoY` | `B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA` |
| `clmm` | `HLMAwimHtgMV8ME9X4RaoHCAdfU35hrovEg4AU5ifw7u` | `GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU` |

The other seven (plus a stray `math_core-keypair.json` for what is now a library)
agree. Deploying with the stale keypairs would publish `hooks` and `clmm` at
addresses no crate or client can reach, and `settlement` likewise — every CPI
into them and every PDA derived from them would be wrong. The e2e suites are
unaffected because they are addressed by program id and load the `.so` images.

Both deploy scripts verify each keypair against the declared id and refuse to
continue on a mismatch, so this cannot go out silently. Resolving it is a
project decision, because the two directions are not equivalent:

> **Status: FIXED.** The keypair-derived ids were adopted (the second option
> below): `anchor keys sync` rewrote `declare_id!` in `hooks`, `settlement` and
> `clmm` plus `[programs.localnet]`, and `[programs.devnet]`, `scripts/programs.sh`,
> `programs/settlement` `POOL_PROGRAMS[0]`, `tests/e2e/tests/common/mod.rs`,
> `tests/e2e/tests/clmm.rs` and the three READMEs were updated by hand. The table
> below intentionally keeps the **old** ids to record what changed.
> **New ids:** `hooks` = `B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA`,
> `settlement` = `58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6`,
> `clmm` = `GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU`.

* **Keep the declared ids** (no source or client churn) — requires the original
  keypairs, which are not in the repo (`target/` is gitignored). If they are lost,
  the ids cannot be reproduced.
* **Adopt the keypair-derived ids** — run `anchor keys sync` and then update
  `Anchor.toml` **and** the hard-coded ids in `tests/e2e/tests/common/mod.rs`
  (`HOOKS_PROGRAM_ID`, `SETTLEMENT_PROGRAM_ID`, and `CLMM` in the suites).
  This changes published addresses and any existing client PDA derivations.

Note the blast radius: the keypairs and `.so` files under `target/deploy` are
**not** tracked (`target/` is gitignored), so these three are stale *local* build
artifacts, not a committed one. They only bite the machine that has them, and
`anchor build` is what would normally refresh them — which is why both deploy
scripts check rather than assume. Separately, 2 701 files under `target/`
(`debug/` fingerprints, `.rustc_info.json`, `CACHEDIR.TAG`, `flycheck0/`) *are*
tracked despite the ignore rule, which is why the working tree shows build churn
as modified files; `git rm -r --cached target` would clean that up.

## High

### H-1 — `cancel_settlement` refund destination is unconstrained

> **Status: FIXED.** `SettlementVault` gained `refund_source: Pubkey`
> (`LEN` 98 → 162), written at initiation from the funding account, and
> `CancelSettlement.refund_token_account` is now constrained to
> `refund_token_account.key() == settlement_vault.refund_source`
> (`ErrorCode::InvalidRefundAccount`), alongside the existing mint check. A
> refund can only ever go back to the account that funded the escrow.

`programs/settlement/src/lib.rs:381` (constraint at `:674`)

```rust
#[account(
    mut,
    constraint = refund_token_account.mint == settlement_vault.token_mint
        @ ErrorCode::MintMismatch
)]
pub refund_token_account: InterfaceAccount<'info, TokenAccount>,
```

The handler transfers the full escrow to `refund_token_account`, which is only
checked to be *a token account of the right mint*. There is no check on its
`owner`, and no recorded refund destination on the vault.

Two consequences:

1. **Config path** — the settlement authority can move a stuck trade's escrow to
   any account of that mint. The authority is privileged (it names the Transfer
   Agent), so this is a key-compromise blast-radius issue rather than a new
   capability.
2. **Pool path** — the initiating pool PDA can cancel its own pending trade and
   redirect the escrowed swap output *anywhere*. The escrow's economic owner is
   the `taker` recorded on the vault (`settlement_vault.taker`), and the pool is
   not obliged to pay it back. A wired AMM program that exposes a settlement
   cancellation CPI — or any upgrade to one — can therefore divert a trader's
   escrowed output to a wallet of its choosing.

**Recommendation:** record the intended refund destination on the vault at
initiation (the `taker`'s token account, or the original source account) and
constrain `refund_token_account` to it, exactly as `settle` already does for
`taker_token_account` (`constraint = taker_token_account.owner ==
settlement_vault.taker`).

Note: none of the four AMMs in this repo currently CPI `cancel_settlement`, so
H-1 is not reachable by the shipped programs alone — it is reachable by any
future wiring and by a compromised config authority today.

---

## Medium

### M-1 / M-2 — the "custody ATA" is not pinned in code

> **Status: FIXED** (both variants). `hooks::after_swap`'s `amm_vault` is now
> constrained to
> `get_associated_token_address_with_program_id(hook_authority, token_mint, token_program)`
> (`ErrorCode::InvalidCustodyAccount`); the now-unused `VaultNotOwnedByHook`
> variant was removed. The same `require_keys_eq!` pinning was added in each
> AMM's settlement rail — `dlmm`, `maverick` and `clmm` on `swap`, and `twamm`
> in the shared `settle_proceeds` helper — so a caller can no longer substitute
> a second hook-authority-owned account for the ATA the AMMs pay into.

`programs/hooks/src/lib.rs:182`, and the AMMs that call it
(`dlmm:800/892`, `clmm:1226/1272`, `maverick:983/1075`, `twamm:823/838, 1098/1113`)

The `Swap` account doc comments and every AMM README claim `custody_token_account`
is "**address-pinned to the hook authority's ATA in the handler**"
(e.g. `programs/dlmm/src/lib.rs:1856`). No such check exists in any of the four
AMM handlers — the settlement rail step only verifies that `hooks_authority` is
the `[b"hook_authority"]` PDA and that `trade_id != 0`. The account is validated
downstream, and there only by ownership:

```rust
// hooks/src/lib.rs:182
constraint = amm_vault.owner == hook_authority.key() @ ErrorCode::VaultNotOwnedByHook,
constraint = amm_vault.mint == token_mint.key()      @ ErrorCode::MintMismatch
```

The token program lets *anyone* create a token account owned by an arbitrary
pubkey (no owner signature is required to set `owner`), and a PDA can own many
accounts, not just its ATA. So a caller can substitute a second account owned by
the hook authority for the same mint. Combined with M-3 (whole-balance sweep)
this is not a direct theft — the funds still land in an escrow the hook authority
owns — but it breaks the "the custody balance is exactly this trade's output"
invariant the design depends on, and it is the difference between "the AMM
decides where the output goes" and "the caller does".

**Recommendation:** in each AMM handler (and in `hooks::after_swap`), require

```rust
require_keys_eq!(
    custody.key(),
    get_associated_token_address_with_program_id(&hooks_authority.key(), &out_mint.key(), &token_program.key()),
    ErrorCode::InvalidVaultTokenAccount
);
```

which is already the pattern used in `hooks` for the *escrow* account. Either
that, or update the docs to say ownership is the check.

### M-3 — `initiate_settlement` sweeps the entire source balance

> **Status: FIXED.** `initiate_settlement` now takes an explicit `amount: u64`
> (rejected when zero or larger than the source balance —
> `ErrorCode::InvalidSettlementAmount`), measures the escrow's before/after
> balance delta around the transfer, requires the delta to be non-zero
> (`EmptySettlement`), and stores the *delta* on the vault rather than the
> source's raw balance. Pre-existing dust in the source can no longer be
> attributed to a trade. `hooks::after_swap` gained the matching `amount`
> parameter (`ErrorCode::InvalidAmount`) and forwards it.

`programs/settlement/src/lib.rs:185`

```rust
let amount = ctx.accounts.source_token_account.amount;
```

The escrowed `amount` is whatever the source account happens to hold, not a
caller-specified amount. The design relies on the source being a *per-trade*
account holding exactly that trade's output, which holds only because the AMM
transfer and the hook CPI are atomic in one transaction. Any balance that is
already in the account at hook time (a donation, a leftover from a direct
`hooks::after_swap` call, a future wiring that does not sweep immediately) is
attributed to this trade and bound into the Transfer Agent's signed message.

**Recommendation:** pass an explicit `amount` and verify it against the
before/after delta of the source account, or take the delta inside the hook and
forward it, so the escrowed amount is provably the flow of this trade.

### M-4 — `move_bins` is permissionless over other LPs' bins

> **Status: ACCEPTED, by design.** The recommendation to gate the move on the
> caller holding a position in the source bin was considered and rejected: a bin
> can hold liquidity that was *absorbed* from another bin (its `supply > 0` with
> no `Position` PDA pointing at it), so such a bin would be unmovable by anyone
> and its liquidity stranded there. `check_move`/`merge_bins` already make the
> move value-neutral — reserves are re-priced at the destination, the
> destination's value-per-share is held constant, and the source's LPs can always
> withdraw the same value by following the `move_state` chain — so the residual is
> a griefing/exposure lever, not a loss of funds. `programs/maverick/README.md`
> documents that moves are permissionless and incentivized off-chain. The
> original text is kept below.

`programs/maverick/src/lib.rs:571`

Any caller may move any out-of-range bin, including a bin they hold no position
in. `check_move` enforces direction and range, and `merge_bins` re-prices the
moved reserves at the destination price while keeping the destination's
value-per-share constant — so no tokens are created or destroyed and the
destination's LPs are not diluted. What is *not* neutral is that an LP's
liquidity can be relocated to a distant bin, changing its exposure and its
withdrawal path, at a stranger's discretion. The `moved_to` chain and
`share_ratio` conservatism make this a griefing / UX lever rather than a loss of
funds.

**Recommendation:** accept as intended and document it clearly (the code comment
already says "incentivized off-chain"), or gate it: require the mover to hold a
position in the source bin, or let positions opt out of being moved.

### M-5 — `set_authority` accepts the default pubkey

> **Status: FIXED.** All three AMMs now open `set_authority` with
> `require_keys_neq!(new_authority, Pubkey::default(), ErrorCode::InvalidAuthority)`,
> matching the `transfer_hook` / `kyc_gate` precedent. `dlmm`, `maverick` and
> `twamm` each gained the `InvalidAuthority` variant (appended, so no existing
> code shifts; the numbered tables in each README are updated). An e2e assertion
> in `tests/e2e/tests/dlmm.rs` covers the rejection and confirms the stored
> authority is unchanged.

`dlmm:984`, `maverick:1127`, `twamm:1205` — all three do `pool.authority =
new_authority;` with no validation. `transfer_hook` (`:153`) and `kyc_gate`
(`:335`) both reject `Pubkey::default()`; the AMMs do not. Rotating to the
default key bricks `set_paused` / `set_authority` forever: `has_one = authority`
can never be satisfied, so a paused pool could never be resumed. The same class
of typo the other programs defend against.

**Recommendation:** add the same guard:

```rust
require_keys_neq!(new_authority, Pubkey::default(), ErrorCode::InvalidAuthority);
```

(`ErrorCode::InvalidAuthority` already exists in each AMM's error enum.)

### M-6 — deposit / withdrawal token accounts are not bound to the signer

> **Status: FIXED.** Every AMM token account that the signer supplies is now
> bound to that signer and to the pool's mint: `dlmm`, `maverick` and `twamm`
> add `token::mint = mint_a|mint_b, token::authority = user|owner`, and `clmm`
> adds `token::authority = user` alongside its existing mint constraint, in
> `add_liquidity`, `remove_liquidity`, `collect_fees`, `swap` and (twamm)
> `create_long_term_order` / `withdraw_proceeds` / `cancel_order`. The input leg
> was already safe because the token program authorizes it, but the output leg
> can no longer be routed to an account the trader does not control. A negative
> `clmm` e2e case pins the rejection.
>
> `clmm` still uses the legacy SPL-Token `token::transfer` rather than
> `transfer_checked`; that is a supported-mint question (clmm takes legacy SPL
> Token accounts, not Token-2022) and is left to a separate change.

`dlmm:1662/1721`, `maverick:2188/2248`, `clmm:1615/1652/1701`, `twamm:277/359`

`user_token_a` / `user_token_b` (and `owner_token_a` / `owner_token_b`) carry no
`token::authority` / `owner` constraint and no mint constraint:

```rust
#[account(mut)]
pub user_token_a: Box<InterfaceAccount<'info, TokenAccount>>,
```

For the input leg the token program is the backstop: the transfer is authorized
by `user` signing as owner/delegate, and the SPL token program rejects a
source/destination mint mismatch. So funds cannot be moved out of a third
party's account. The residual issues are:

* the swap **output** can be routed to an account the caller does not own
  (self-inflicted, but trivially mistyped and a footgun for integrators);
* `clmm` uses `token::transfer` (not `transfer_checked`) for both legs
  (`:760`, `:820`, `:1050`, `:1085`), so decimals are not re-validated by the
  program itself;
* integration-layer bugs cannot be caught by the program.

**Recommendation:** add `token::mint = mint_a` and
`token::authority = user` (or an explicit `owner == user` constraint) on all
four token accounts in each context, and switch `clmm` to `transfer_checked`.

### M-7 — `rwa_mint` is a permissionless mint factory

> **Status: ACCEPTED, documented.** A permissionless factory is the intended
> shape of the program (each mint is independent; the whitelist is enforced by
> the global `transfer_hook` config, not per mint), so this is a registry/
> attribution problem rather than a contract vulnerability. Gating
> `create_rwa_mint` on the upgrade authority would remove the permissionless
> factory use case, so the call is documented here instead: consumers must
> treat mint metadata as unattested and filter by issuer off-chain.
>
> Note that the recommendation to "record the issuing institution on-chain" is
> already satisfied in the sense that matters for filtering: `create_rwa_mint`
> makes the `authority` signer the mint authority and the TokenMetadata update
> authority, so a mint's issuer is readable from its own state. What the program
> deliberately does *not* do is attest that the issuer is a trusted one — that is
> an allowlist, i.e. a registry a consumer applies off-chain, not a per-mint
> contract check. The original text is kept below.

`programs/rwa_mint/src/lib.rs:53`

`create_rwa_mint` has no access control, and every security-relevant parameter is
a caller argument: `transfer_hook_program`, `delegate` (full `PermanentDelegate`
seizure power), `fee_authority`, and the metadata strings. Anyone can therefore
mint a token that points its `TransferHook` at the *genuine* `transfer_hook`
program, carries a `TransferFeeConfig` and a `PermanentDelegate`, and presents
attacker-chosen name/symbol/URI.

This is not a break of any single mint's integrity (each mint is independent, and
the hook/config PDAs are per-program, not per-mint), but there is no registry to
distinguish an operator-issued RWA from a convincing imitation, and the
`PermanentDelegate` the docs describe as "regulatory seizure (multisig-gated)" is
in fact whatever key the creator passed.

**Recommendation:** gate `create_rwa_mint` on the program's upgrade authority (or
a configured issuer registry), or have it record the issuing institution on-chain
so consumers can filter by issuer. At minimum, document that mint creation is
open and that metadata is not attested.

---

### M-8 — `add_liquidity` mutates a read-only account, so the static-baseline flag never sticks

`programs/maverick/src/lib.rs:2226` (context), `:391` (write)

`add_liquidity` ends by setting the pool's one-way latch:

```rust
if kind == BinKind::Static {
    pool.has_static_baseline = true;
}
```

but `AddLiquidity` declares `pool` without `mut`:

```rust
#[account(
    seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
    bump = pool.bump
)]
pub pool: Box<Account<'info, Pool>>,
```

Anchor keys its write-back off that attribute, so the assignment is discarded: the
account is loaded, the flag is set in memory, and the exit path never serialises
it back. Compare `MoveBins` and `Swap`, which both carry `mut` on the same
account precisely because they write it.

The consequence is not a lost flag but a **dead feature**: `NoStaticBaseline`
(`:312`) gates every non-`Static` kind on that flag, so with it permanently
`false` no `Right`, `Left` or `Both` bin can ever be funded — and bin moves,
the program's headline mechanism, need exactly those kinds. Every directed move
and every move-chain withdrawal is unreachable, independently of M-9.

This is also the sort of bug a unit test cannot catch (the handler's own logic is
right; only the account wiring is wrong), which is why the e2e suite caught it and
the unit suite never did.

> **Status: FIXED.** `AddLiquidity.pool` now carries `mut`, with a comment
> naming the flag it writes, and `tests/e2e/tests/maverick.rs` passes the account
> as writable. Three maverick e2e tests that failed on the missing baseline now
> pass.
>
> The rest of the workspace was swept for the same shape — a handler writing an
> account field whose context omits `mut`. `AddLiquidity.pool` is the only
> occurrence. Every other write is covered: the field carries `mut`, or it is
> `init`/`init_if_needed` (which make the account writable implicitly, e.g.
> `twamm`'s `lp_position`), or it goes through the zero-copy `load_mut`
> (`oracle`'s `oracle_state`).

### M-9 — `move_bins` exhausts the SBF heap and reverts

`programs/maverick/src/lib.rs:604`

When the destination bin lives in another array, `move_bins` deserialises that
array into its own `Account<BinArray>` while the source array is still live:

```rust
let account = hop_array_info(program_id, &pool_key, to_index, ctx.remaining_accounts)?;
let mut to_array = Account::<BinArray>::try_from(account)?;
```

Each `BinArray` materialises a `Vec<Bin>` of 50 elements. `Bin` is 80 bytes in
memory, so one array is ~4 KB of heap and this instruction needs two at once.
Measured on the built image (LiteSVM, `target/deploy/maverick.so`), the second
`try_from` is where it dies — the instruction reaches it with a valid array
(`data_len` 3552, index −1, `bins` length prefix 50) and the runtime reports:

```
Program log: Instruction: MoveBins
Program log: Error: memory allocation failed, out of memory
Program ... failed: SBF program panicked
```

It is not a corrupt-account problem, and it is specific to `move_bins`: every
other handler that touches a second array either holds only one at a time
(`swap` deserialises inside a loop and drops each array before the next) or reads
a single bin through a helper. `move_bins` is the only place that keeps two live.

Effect: a bin move across array boundaries — the cross-array hop the design is
built around — cannot execute on-chain at all. Same-array moves (where one
account serves both) are unaffected.

**Why the size is not the whole story.** The bound is not 32 KiB of *live*
memory, it is 32 KiB of *cumulative* allocation. `solana_program_entrypoint`'s
allocator is a bump allocator whose `dealloc` is a documented no-op:

```rust
unsafe fn dealloc(&self, _: *mut u8, _: Layout) {
    // I'm a bump allocator, I don't free
}
```

and the arena is `HEAP_LENGTH = 32 * 1024`. `Vec<T>` deserialisation grows by
doubling, so a single 50-element `Vec<Bin>` costs roughly 2.5× its final size
(~11 KiB on BPF, where `u128` aligns to 8) rather than 4.4 KiB, and each
transient copy is charged for the rest of the instruction. Three of them — the
context's own array, the copy `hop_array_info` built only to read three fields,
and the caller's — is what crossed the line. This is a general constraint for
this program, not a one-off: **any** handler that materialises two bin arrays
would fail the same way, and the cross-array withdrawal path had the same latent
shape.

> **Status: FIXED.** The destination bin is now read and written **in place**,
> with no `Vec<Bin>` at all:
>
> * `bin_byte_offset` locates the bin's byte offset and `read_bin_at` /
>   `write_bin_at` read and write one bin there. (Originally it *walked* the
>   variable-length elements; the L-8 fix made the stride constant, so it is now
>   a plain `BINS_OFFSET + index * Bin::LEN` multiplication.) The write is
>   fixed-size: every serialized bin is `Bin::LEN` bytes.
> * `hop_array_info` validates the array's pool, index and bin count straight out
>   of the account's bytes instead of deserialising it to read three fields and
>   discarding the result.
> * The cross-array branch of `remove_liquidity` and the move-chain walk
>   (`load_bin`) use the same helpers, so no path materialises a second array.
> * Every other handler is unaffected, because each materialises exactly one
>   array: `initialize_pool`, `initialize_bin_array`, `add_liquidity` and `swap`.
>
> Verified: `move_bins` and a two-hop cross-array withdrawal now execute on the
> built image. Five new unit tests pin the byte-level helpers — that reading in
> place agrees with deserialising the whole array, that a moved bin shifts every
offset after it by `MOVE_REF_LEN` and leaves the bins before it alone, that a
> fixed-size write round-trips without moving its neighbours, and that a bin
> gaining a `MoveRef` is refused by the in-place writer rather than silently
> corrupting the vector. The whole `maverick` e2e suite passes.

### L-8 — `Vec<Bin>` is not fixed-stride, which breaks consumers that index it

`programs/maverick/src/lib.rs:2067`

`Bin.moved_to` is an `Option<MoveRef>`, and Anchor's `Option` serialises to a
single tag byte when it is `None` but tag + `i32` + `u128` when it is `Some`. A
`Vec<Bin>` therefore has **variable-length elements**: 50 bytes for a bin that
has not moved, 70 for one that has. `Bin::LEN` (70) is the worst case, and it is
the right size for `space` — an array whose bins have all moved fills it exactly,
and anything less leaves the tail zeroed.

On-chain this is harmless: Anchor reads the vector sequentially. Off-chain it is
a trap — any consumer that computes `52 + index * 70` to find a bin reads
misaligned bytes as soon as an earlier bin has moved, and there is no fixed
stride to switch to. This is what the `tests/e2e` harness did, and it is why
nine of the ten maverick e2e cases were red against a correct program (see the
note below).

**Recommendation:** document the layout in the README's data-structure section
(the offsets are already listed, but not the fact that the stride depends on
`moved_to`), and have off-chain readers walk the vector rather than index it —
or make `moved_to` a fixed-size `{ moved: bool, to_bin_id: i32, share_ratio: u128 }`
so the stride is constant and indexable. The fixed-size encoding was chosen; it
keeps `Bin::LEN` at 70 because the old `Some` arm was already 21 bytes.

> **Status: FIXED.** `Bin.moved_to: Option<MoveRef>` is replaced by
> `Bin.move_state: MoveState { moved: bool, to_bin_id: i32, share_ratio: u128 }`,
> a fixed-size struct (`MoveState::LEN` = 21). Every serialized bin is now
> `Bin::LEN` = 70 bytes whether or not it has moved, so the vector is a constant
> stride and `ARRAY_BINS + index * 70` is correct for every consumer. `Bin::LEN`
> was already 70 (the old `Option<MoveRef>::Some` was also 1 + 4 + 16 = 21
> bytes), so `BinArray::LEN` and account allocation are unchanged: the fix
> removes the shorter 50-byte *unmoved* encoding rather than growing the account,
> and no state migration is needed. As a side effect the in-place helpers simplify:
> `bin_byte_offset` is plain multiplication, `bin_span` is gone, and
> `write_bin_at` no longer refuses a bin that is recording a move. The maverick
> unit tests for the old variable stride (`a_moved_bin_shifts_every_offset_after_it`,
> `a_growing_bin_is_refused_by_the_in_place_writer`) are rewritten for the fixed
> one, and the `tests/e2e` reader indexes instead of walking. `maverick`'s README
> data-structure section now documents the fixed 70-byte stride.

---

## Low / Informational

### L-1 — `oracle` swallows accumulator overflow

> **Status: FIXED.** `write_observation` now returns `Result` and propagates
> `MathOverflow` on both the elapsed-time conversion and the accumulator add
> (the previous `.unwrap_or(...)` fallbacks are gone); `record_observation`
> propagates it with `?`. The original text is kept below.

`programs/oracle/src/lib.rs:227` (`write_observation`)

```rust
last_observation.cumulative_price_x64.checked_add(
    last_observation.sqrt_price_x64
        .checked_mul(u128::try_from(now - last_observation.timestamp).unwrap_or(0))
        .unwrap_or(last_observation.cumulative_price_x64),
).unwrap_or(last_observation.cumulative_price_x64)
```

Every overflow path silently falls back to the previous cumulative value,
dropping the elapsed time from the TWAP with no error and no event. A TWAP that
is quietly wrong is worse for a circuit breaker than one that stops.

**Recommendation:** propagate `MathOverflow` (the `get_twap_impl` path already
does), or emit an event when the fallback is taken.

### L-2 — the circuit breaker is inert until two observations exist

`programs/oracle/src/lib.rs:143` (`check_deviation`) and `:178` (`trip_breaker`)
both return `Ok(())` when the TWAP is unavailable. That is deliberate and
documented, but it means every freshly created pool has an unguarded window
until its second observation lands. There is a log line but no event a keeper
can alarm on.

### L-3 — `clmm` has no admin path

> **Status: FIXED.** `Pool` gained `authority` / `paused`, plus `set_paused` and
> `set_authority` (admin `has_one = authority`). `swap` and `add_liquidity`
> refuse a paused pool; `remove_liquidity` and `collect_fees` stay open so
> capital is never trapped. The original text is kept below.

`clmm`'s `Pool` has no `paused` field and no instruction sets an authority
(`clmm/src/lib.rs:1455`). Swaps can only be halted by the oracle rail latching
`is_paused` via `trip_breaker`. A CLMM pool initialized with `oracle_state`
left off (the sentinel path) can never be paused by anyone, for any reason. Given
the RWA context this is likely unintended; consider an admin pause, or making the
oracle rail mandatory.

### L-4 — `dlmm` first-depositor share inflation

> **Status: FIXED (mitigated).** `dlmm` and `maverick` now add
> `MIN_INITIAL_SHARES` to a bin's first mint, so an inflation attempt costs the
> attacker value instead of the next depositor. The original text is kept below.

`programs/dlmm/src/lib.rs:312`

```rust
let shares_to_mint = if bin.supply == 0 {
    deposit_value
} else {
    deposit_value * bin.supply / reserve_value
};
```

The standard ERC-4626 rounding exposure. `bin.reserve_*` is program accounting
(not a vault balance read), so a plain token donation cannot inflate it — but a
first depositor can still grow `reserve_value` relative to `supply` by paying
swap fees into a bin they alone own, which rounds the next depositor's shares
down. `require!(shares_to_mint > 0)` means the victim loses value rather than the
whole deposit, and the attacker must fund the "attack" through real fees, so the
economics are marginal.

**Recommendation:** lock a minimum first-deposit share amount, or seed the bin
with a small burned share balance at pool creation.

### L-5 — `transfer_hook::execute` takes an unused `owner` account

> **Status: DOCUMENTED.** The `owner` field of `Execute` carries an explicit
> "passed by the token program, deliberately unused" note, and the handler reads
> the source owner from `source_token.owner` (which Token-2022 authenticates).
> The original text is kept below.

`programs/transfer_hook/src/lib.rs:519`

`owner` is `UncheckedAccount` and constrained nowhere; the handler correctly
reads the source owner from `source_token.owner` instead. It is required only
because Token-2022 passes it in the fixed account order. Harmless, but a
reviewer's eye is drawn to an unvalidated account called `owner` — worth a
"passed by the token program, deliberately unused" note on the struct (there is
one, but it sits on the `owner` field of `Execute` while the handler relies on
`source_token.owner`; make the link explicit).

### L-6 — single-sided share pricing

`dlmm` and `maverick` price LP shares exclusively in token A
(`value_in_a(a, b, price) = a + b·price`). For a bin whose composition is almost
entirely token B this concentrates all rounding error on one side. The
`round_up`/`round_down` split at deposit/withdrawal is correct and always favours
the pool; no action needed beyond awareness.

### L-7 — duplicated `require_upgrade_authority`

> **Status: FIXED.** The loader-layout parser now lives once in
> `libs/program_governance`; `transfer_hook`, `kyc_gate` and `settlement` each
> call it and map the error onto their own `UnauthorizedInitializer`. The
> original text is kept below.

`transfer_hook:55`, `kyc_gate:110`, `settlement:85` carry three byte-identical
copies of the loader-layout parser (4-byte tag, 8-byte slot, `Option<Pubkey>`).
A future `UpgradeableLoaderState` change has to be found and fixed in three
places. `hooks` and the four AMMs are correct to not have one (they have no
one-time privileged initializer), but the three copies should move into a shared
`libs/` helper.

---

## Appendix — the `maverick` e2e suite was red against a correct program

Worth recording, because it cost real signal: at the start of this review nine of
the ten `maverick` e2e cases failed. Two of those failures were the reviewer's
own instrumentation lying, and untangling them is what surfaced M-8, M-9 and L-8.

1. **A fixed-stride reader on a variable-stride account (L-8).** `bin()` in
   `tests/e2e/tests/maverick.rs` located a bin at `ARRAY_BINS + index * 70`. A bin
   that has not moved serialises to 50 bytes, so from the second bin on the test
   read zeros and garbage — for instance bin 1 read as `price = 0` and bin 2 as a
   ~2^128 value, on an account whose real bin 1 price was `18465199121032091051`,
   exactly what `priced_bins` computes. The program was right; the reader was
   wrong. It was rewritten as a walk — the only correct way to index a
   variable-stride vector. (The program has since moved to a fixed `MoveState`,
   L-8, so both the on-chain helpers and the e2e reader index with a constant
   stride again; the walk is preserved here as the record of what was wrong.)
2. **A dropped write (M-8).** With the reader fixed, five cases still failed on
   `NoStaticBaseline` / a false `has_static_baseline` — the flag genuinely never
   persisted, because of the missing `mut`.

After both: 5 of 10 pass, up from 1. The five that remain are not all the same
kind of problem, and they are listed here rather than fixed silently:

| Case | Cause |
| --- | --- |
| `move_bins_carries_liquidity_to_the_price...` | M-9 (SBF OOM in `move_bins`) |
| `a_static_bin_never_moves_and_a_move_must_go_toward_the_price` | expects `StaticCannotMove` (6019) first; the program reached the destination-array lookup and returned `MissingBinArray` (6025) |
| `directional_liquidity_needs_a_static_baseline...` | `AlreadyProcessed`: the harness sent a byte-identical instruction twice with the same blockhash |
| `a_moved_bin_cannot_move_again_or_take_deposits`, `only_the_authority_can_pause...` | `AccountNotInitialized` (3012) — bin −1 lives in array −1, which the tests never created |

All five are now resolved, and it is worth being clear about which side each one
was wrong on, because only one of them was the program's fault:

* **M-9** — the program (see above).
* **The error-ordering pair** — two cases independently expected validation of
  the bin being moved to happen *before* the destination array is located. That
  is the better contract (a static bin has no destination it could move to, and a
  caller should not have to supply a valid one to be told so), so the program was
  changed to match: `check_move_source` now runs against the source alone, before
  `hop_array_info`. Within it, `moved_to` is now asked **before** the reserves,
  because a moved bin keeps its supply but not its reserves — the old order
  reported it as merely empty, and `BinAlreadyMoved` is the answer that describes
  what happened.
* **`AlreadyProcessed`** — the harness. `common::send_with` did not chain the
  blockhash the way `send_error_logs` (and the `clmm`, `oracle` and `kyc_gate`
  helpers) already did, so re-sending an identical instruction collided on
  signature instead of executing. Fixed at the one place all of them go through.
  It moves no clock, so slot-sensitive suites are unaffected — verified by
  re-running them.
* **The two `AccountNotInitialized` cases** — the tests deposited into bin −1
  without creating array −1; one of them also set up an invalid move (a `Both`
  bin into a funded `Static` bin, which a merge must refuse) and measured its
  withdrawal against the wrong baseline.

With those done the suite is **10/10 and green**, alongside `clmm` 10, `dlmm` 7,
`oracle` 14 and `kyc_gate` 9 — 50 e2e cases, all passing.

---

## Verified as sound (positive findings)

Worth recording, because these are the checks most likely to be missing:

* **One-time initializers** (`transfer_hook::initialize`, `kyc_gate::initialize_kyc_mint`,
  `settlement::initialize`) are gated on the program's own upgrade authority,
  derived and owner-pinned before the `ProgramData` bytes are parsed, and refuse
  the `None` (immutable-program) case rather than failing open.
* **Mint/pool PDAs cannot be front-run**: `oracle::initialize` requires the pool
  PDA to sign, and each AMM creates its oracle inside `initialize_pool`, so the
  threshold and unpause owner are fixed atomically with the pool.
* **Oracle prices are authenticated**: `check_deviation`, `record_observation`
  and `trip_breaker` all require the pool PDA's signature, and the price is read
  from pool state (`bin_price_raw` / `sqrt_price_x64` / `marginal_price`), never
  from a caller argument. A permissionless caller cannot forge or pause.
* **Pause semantics are exit-friendly**: `remove_liquidity` in `dlmm`,
  `maverick` and `twamm`, and `withdraw_proceeds` / `cancel_order` in `twamm`,
  deliberately bypass the pause so capital is never trapped.
* **KYC attestations are soulbound and revocable**: `NonTransferable` prevents
  any transfer by anyone, `PermanentDelegate` (the config PDA) enables
  `revoke_kyc` without the institution's cooperation, and `verify_kyc` binds the
  attestation to both the configured mint and the signing user.
* **Transfer hook proofs are domain-separated** (RFC 6962-style leaf/node
  prefixes), sorted-pair so proofs are position-free, and *both* source and
  destination must prove membership.
* **Settlement signature binding** covers `(trade_id, taker, mint, amount)`, and
  `settle` re-checks the vault token account, the taker account owner and both
  mints against the vault's immutable record.
* **Rail-account policy** is consistently enforced: a disabled rail's accounts
  must be the AMM program's own ID sentinel, so an attacker cannot smuggle an
  unused rail's accounts past the pool's recorded policy.
* **No unchecked arithmetic in value paths**: every amount/fee/share computation
  uses `checked_*` with typed error codes; `overflow-checks = true` is set for
  release builds in the workspace profile.

---

## Follow-up round — settlement hardening (this branch)

Changes since the table above was written, and the gaps that remain:

* **Low-s is enforced** (`HighSignatureS`): a signature and its `(n − s, v')`
  twin are no longer both accepted, so an audit log keeps one wire form per
  authorization. The predicate is unit-tested at the boundary, the high twin and
  the maximum.
* **The release window is closed.** The vault records
  `expiry_slot = settlement_slot + EXPIRY_WINDOW_SLOTS` (216 000 slots, a program
  constant). `settle` refuses at or after it; the permissionless
  `refund_expired` pays the pinned `refund_source` and marks the trade `Expired`.
  A keeper can crank the refund but cannot redirect it.
* **Rent is recoverable.** `close_settlement` (config authority only) closes a
  terminal trade's empty escrow and vault PDA and returns both rents. It refuses
  a `Pending` vault and a non-empty escrow.
* **Transfer Agent keys can be per-mint.** `[b"agent", mint]` overrides the
  config's global key when `settle` is passed that account; `clear_mint_agent`
  removes the override. The optional account keeps the old path byte-identical.
* **`InvalidSettlementAmount` now matches its check**: the amount is rejected
  when zero **or larger than the source balance** (the message previously
  described only the zero case).
* **The vault layout changed** (`expiry_slot` inserted after `settlement_slot`),
  so every decoder/filter was updated: `SETTLEMENT_VAULT_LEN` 162 → 170, the web
  decoder, the seeder and the `clmm` e2e offset constants.

**Still open:**

* **Token-2022 is uniform across the four engines now.** `clmm` was converted
  to `token_interface` (`InterfaceAccount`, `Interface<TokenInterface>`,
  `transfer_checked`) and forwards hook accounts from the accounts that follow
  its tick walk; a pool whose mints are Token-2022 accounts deposits and swaps
  like any other (covered by `a_token_2022_pool_deposits_and_swaps`).
* **`clmm` requires exact receipts, and grosses up for a Token-2022 fee.** A
  position is minted with the liquidity its deposit amounts were computed for and
  a swap's output is priced on the input the vault received, so both inbound
  transfers call `transfer_tokens_exact`: it adds the mint's fee to the amount
  (`calculate_pre_fee_amount`, at the clock's epoch) and sends the gross, so
  the vault is credited exactly `amount` and the sender pays the token's fee. A
  fee-bearing mint therefore backs a working pool — `initialize_pool` sizes each
  vault from the extensions the mint requires, and a fee-bearing mint requires
  `TransferFeeAmount`. Payouts are not grossed up: the vault is debited exactly
  `amount` whichever extension the mint carries, since paying the fee on top
  would draw on the other LPs' reserves. One trader-side consequence, covered by
  the same test: `min_amount_out` is enforced against what leaves the vault, so
  an output mint with a fee delivers `amount_out` minus that fee.
* **The short-transfer refusal is now an assertion, not a scenario.**
  `transfer_tokens_exact` still measures the vault's balance delta and raises
  `TransferAmountMismatch` if it is short, but a conforming token program cannot
  produce that: the gross-up is the token program's own inverse of the fee it
  charges, including when the mint's `maximum_fee` cap binds. The failing side is
  kept as defence in depth (a token program that ever charges a fee the
  prediction misses is refused rather than credited short) and is **not reachable
  end-to-end**, so it has no scenario test; its passing side runs in every
  deposit and swap test. Both fee paths are covered as positive cases:
  `a_fee_bearing_pool_deposits_and_swaps` (a real 1% mint, vault sized for the
  extension, deposit and swap each netting the exact priced amount) and
  `a_fee_raised_after_funding_keeps_the_pool_trading` (the issuer raises the fee
  with `SetTransferFee`, effective two epochs later, and the pool keeps trading
  with the fee paid on top).
* **`twamm` forwards hook accounts too.** The walk stops at the first
  account the program does not own, so hook extras can follow the order list and
  are threaded into every transfer and into `hooks::after_swap`.
* **A pool over a hook-gated mint needs the pool PDA whitelisted.** The hook
  checks both sides of every transfer, and the pool PDA owns the vaults, so the
  client forwards an entry for it (`hookAccountsForTransfers`); the entry itself
  has to exist before the pool can trade, which is a deployment step, not
  something the program can do for itself.
* **`refund_expired`, `close_settlement` and the mint-agent instructions have no
  end-to-end test.** Reaching expiry needs 216 000 slots; the TS suite does not
  warp that far. They are unit-tested (sizes, low-s, expiry arithmetic) and
  exercised by the client builders, but not integration-verified.

---

# Round 2 — independent re-review of the whole tree

Scope: all **eleven** programs under `programs/` (~20 800 lines of Rust) plus
both shared crates, `libs/math_core` and `libs/program_governance`. This round
was run independently of round 1 — its findings were not taken as given — and it
covers `programs/order_book`, which round 1 left out.

Method: a full read of `order_book`, `hooks`, `settlement`, `oracle`,
`kyc_gate`, `transfer_hook`, `rwa_mint`, `program_governance` and
`math_core/fixed_point`; a handler-plus-`#[derive(Accounts)]` read of every
value path in `dlmm`, `maverick`, `clmm` and `twamm` (deposit, withdrawal,
swap, order lifecycle, rails, admin), leaving the per-AMM fill loops and curve
internals to their unit suites; and pattern sweeps for the bug classes round 1
turned up — narrowing casts, `unwrap`/`panic`, an account a handler writes whose
context omits `mut`, token accounts not bound to the signer, PDAs pinned by
ownership rather than by derivation, `init_if_needed`, `set_authority`
defaults, and missing signer/owner constraints.

`cargo test --workspace`: **209 unit tests across 13 targets, all green**
(`exit 0`) — the count the README claims.

## Summary of new findings

| ID | Severity | Program | Finding | Status |
| --- | --- | --- | --- | --- |
| S-1 | Medium | `hooks`, `settlement` | A routed trade's refund goes to the hook authority's custody ATA, not the trader. The balance is stranded there, and any caller can then escrow it naming itself as the beneficiary | **Fixed** — see [Remediation](#remediation-this-branch) |
| S-2 | Low | `hooks` | `after_swap` requires no signer and takes the settlement beneficiary (`taker`) and unlock horizon (`settlement_slots`) as caller arguments | **Accepted** — it is S-1's mechanism, and the fix removes the balances it acted on. Rationale below |
| S-3 | Low | `settlement` | A donated balance blocks `close_settlement` permanently, because refunds move only `vault.amount` | **Fixed** |
| S-4 | Info | `settlement` | Routed vault and escrow rent is paid by the hook authority but refunded to the config authority | **Accepted** — documented in the settlement README; a fix means a vault layout change for a bookkeeping concern, which is not worth the migration |
| S-5 | Info | `oracle` | `unwrap()` inside seed constraints turns a malformed `oracle_state` into a panic rather than a constraint error | **Fixed** — the checks now derive from `pool`/`authority`, or run in the handler as `InvalidOracleState` |

---

## S-1 — routed refunds land in the custody ATA, where they are stranded and re-claimable

### Where the funder and the beneficiary diverge

`initiate_settlement` records the escrow's **funder** as the refund destination:

```rust
// programs/settlement/src/lib.rs:419
vault.refund_source = ctx.accounts.source_token_account.key();
```

and both refund paths pin the destination to it (round 1's H-1 fix):

```rust
// programs/settlement/src/lib.rs:1119 (CancelSettlement) and :1173 (RefundExpired)
refund_token_account.key() == settlement_vault.refund_source
    @ ErrorCode::InvalidRefundAccount,
```

On the **routed** flow that funder is the hooks program's custody ATA, not the
trader. `hooks::after_swap` hands `amm_vault` over as `source_token_account`,
and `AfterSwap` pins `amm_vault` to exactly the hook authority's ATA for the
mint (`hooks/src/lib.rs:200-215`), while the vault's beneficiary is whatever
`taker` the caller passed:

```rust
// programs/settlement/src/lib.rs:404
vault.taker = ctx.accounts.taker.key();
```

So on a routed trade the two keys are different accounts *by construction*:
`taker` is the trader — the AMMs pass a signer (their `user`, or the order owner
in `twamm`'s case) and `order_book` passes the maker — while `refund_source` is
the hook authority's custody ATA. The release
path pays the trader — `taker_token_account.owner == vault.taker` — and both
refund paths pay the custody ATA.

### Nothing can move value back out of custody

`hooks` exposes exactly two instructions: `after_swap` and
`fund_hook_authority` (lamports only). So a routed trade that lapses past
`EXPIRY_WINDOW_SLOTS` — 216 000 slots, roughly a day
(`settlement/src/lib.rs:118`) — or that the settlement authority cancels,
leaves the trader's output sitting in a PDA-owned token account with no
instruction that pays it out to anyone.

### Any caller can take the beneficiary role for that balance

`after_swap` requires no signer at all, takes `taker` as a plain `AccountInfo`
(`hooks/src/lib.rs:200`) and `settlement_slots` as a caller argument, and
validates only the amount:

```rust
// programs/hooks/src/lib.rs:70
require!(
    amount > 0 && amount <= ctx.accounts.amm_vault.amount,
    ErrorCode::InvalidAmount
);
```

A third party can therefore escrow the stranded balance under a fresh
`trade_id`, naming itself as `taker` and choosing `settlement_slots` large
enough to push `expiry_slot` out. Two consequences:

1. **Indefinite lock.** The permissionless `refund_expired` cannot fire before
   the attacker-chosen `expiry_slot`, so a refund the desk was entitled to can
   be delayed arbitrarily (up to the `checked_add` ceiling on
   `settlement_slot + EXPIRY_WINDOW_SLOTS`).
2. **Beneficiary misdirection.** The fabricated vault is indistinguishable from
   a real one on chain: `Pending`, funded with the right mint, with a recorded
   `taker` that is the attacker. The Transfer Agent's release signature binds
   `(trade_id, taker, mint, amount)`, so a TA that signs this vault releases the
   balance to the attacker, and nothing on chain can tell it that the vault was
   never a trade.

**Reachability.** Every routed swap (`dlmm`, `maverick`, `clmm`, `twamm`) and
every routed `order_book::fill_order` reaches it — but only through a trade that
actually reaches a refund, i.e. the TA did not release inside the window, or the
settlement authority cancelled. The signed happy path pays the trader and is
unaffected. It is *not* a route to stealing a live escrow.

**Why H-1 is still the right fix.** Pinning the refund to the funder is correct
for the standalone (authority-custody) flow, where funder and beneficiary are
the same desk. The routed flow is the case where they differ, and the code uses
`refund_source` for both. Fixing S-1 does not require reopening H-1: give the
vault a *separate* refund destination, or make the routed flow's funding account
one that is allowed to receive the refund.

**Options:**

* record the trader as the refund destination on the routed flow — e.g. have
  `hooks::after_swap` (and `order_book::settle_proceeds`) pass the beneficiary's
  ATA as the account that is charged, or store a second field on the vault;
* add a `hooks::sweep_custody` that pays the balance to a *recorded* trader and
  is callable only by the AMM programs / the hook authority;
* at minimum, gate `after_swap`'s `taker` to the transaction's signer, and bound
  `settlement_slots`.

**Coverage.** At the time of the finding, `refund_expired`, `refund_source`,
`cancel_settlement` and `close_settlement` were mentioned **nowhere** under
`tests/` — the least-exercised code in the repository was the code this finding
lived in. The remediation below adds an end-to-end case for the refund path
(`refund_expired_of_a_routed_trade_pays_the_maker_not_the_custody_account`);
`cancel_settlement` and `close_settlement` still have none.

---

## S-2 — `after_swap` is unauthenticated with a caller-chosen beneficiary

The narrow statement of S-1's mechanism, worth recording on its own because it
is a property of the instruction rather than of the refund path: `after_swap`
has no signer and no check binding `taker` to the transaction. The AMMs are
good citizens: `dlmm:928`, `maverick:1125` and `clmm:2058` pass their `user`
signer, `twamm:1806` passes the order owner (`taker: &Signer`), and `order_book`
passes the maker — but the instruction's guarantees do not come from the caller. Anything sitting in the
custody ATA can be escrowed for an arbitrary beneficiary. On-chain, the only
reason this is not direct theft is that the release still needs the Transfer
Agent's secp256k1 signature (S-1's option list closes it properly).

---

## S-3 — a donated balance blocks `close_settlement` forever

`settle`, `cancel_settlement` and `refund_expired` each move exactly
`settlement_vault.amount` out of the escrow. That amount is the measured arrival
delta at initiation, so anything sent to the escrow afterwards — a donation of
one raw unit to a publicly derivable PDA address — stays there. `close_settlement`
requires an empty escrow:

```rust
// programs/settlement/src/lib.rs:739
require!(
    ctx.accounts.vault_token_account.amount == 0,
    ErrorCode::EscrowNotEmpty
);
```

and `refund_expired` refuses a non-`Pending` vault, so once a trade is terminal
the dust can never be moved. The escrow and its rent are then unrecoverable for
the life of the program. No funds are trapped (the trade's own `amount` has
already been paid out) and only the config authority can even attempt the close,
so this is griefing rather than loss — but it is cheap and irreversible, and it
is the same shape as `order_book`'s donation case, which the order book handles
by sweeping the whole balance on cancel.

**Option:** have the three payout paths move the escrow's whole balance (or let
`close_settlement` sweep the remainder to the config authority before closing),
the way `cancel_order` does in `order_book`.

---

## S-4 — routed rent is paid by one key and refunded to another

`InitiateSettlement` names the caller's `authority` as both the rent payer for
the vault and for the escrow (`payer = authority`, `settlement/src/lib.rs:894`
for the vault and `:932` for the escrow). On the routed flow that payer is the
hook authority PDA (which is why
`AfterSwap.hook_authority` must be `mut`). `close_settlement` pays both rents to
the **config authority** (`close = authority` plus an explicit close into the
same account). The hook authority therefore subsidises every routed trade's rent
and never gets it back. Informational — it is a bookkeeping/operational-budget
issue for a PDA the protocol has to keep funded (`hooks::fund_hook_authority`),
not a security boundary.

---

## S-5 — `unwrap()` in the oracle's seed constraints

The oracle's four instruction contexts derive their PDA from the state they are
given:

```rust
// programs/oracle/src/lib.rs:521, :532, :538, :550, :562
#[account(mut, seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
```

`load()` returns a `Result` — a non-`OracleState` account, or a zero-copy account
whose discriminator does not match, errors there. `.unwrap()` converts that into
a panic, so a malformed account aborts the instruction instead of failing a
constraint. The blast radius is nil (the transaction fails either way and the
account is program-owned), and it is the only `.unwrap()` in a constraint in the
tree; it is recorded only because the reviewer's next question is "what happens
when that account is garbage", and the answer should be an error code, not a
panic.

---

## Remediation (this branch)

Four of the five findings are addressed in code; the fifth is accepted with a
written rationale. Every change below compiles and the workspace unit suite is
green (209 tests, 13 targets, `cargo test --workspace` exit 0), as is the web
type-check and production build (`cd app/web && bun run build`, exit 0).

### S-1 — routed refunds now pay the recorded taker

`programs/settlement/src/lib.rs` gains `require_refund_destination`, one helper
both refund paths call, and the account-level pin the two contexts used to carry
is replaced by it (the address rule is now conditional, so it cannot live in a
`#[account(constraint = …)]`):

```rust
if vault.refund_source == custody {
    // Routed trade: the funder is the hook authority's custody ATA, which
    // nothing can pay out of. Return the output to the trader.
    require_keys_eq!(refund.owner, vault.taker, ErrorCode::InvalidRefundAccount);
} else {
    // Standalone trade: the funder is the desk's own account, as in H-1.
    require_keys_eq!(refund.key(), vault.refund_source, ErrorCode::InvalidRefundAccount);
}
```

`custody` is `get_associated_token_address_with_program_id(hook_authority, vault.token_mint, token_program)`,
where `hook_authority` is the `[b"hook_authority"]` PDA of the hooks program
(new `HOOKS_PROGRAM_ID` constant, pinned by address for the same reason
`POOL_PROGRAMS` is — `hooks` depends on this crate, so this crate cannot depend
back on it).

Why this is the right shape:

* **It closes the hole without reopening H-1.** Both branches are functions of
  what the trade recorded at initiation (`refund_source`, `taker`, `token_mint`),
  so a canceller still cannot name an arbitrary destination. The change is that
  the funder is no longer assumed to be a key that can *receive*.
* **No layout, account-list or CPI change.** The vault keeps its fields, the
  refund instructions keep their accounts, and neither the AMMs' nor
  `order_book`'s CPIs are touched — so no decoder, offset or client instruction
  moved. (A `refund_destination` field on the vault would have been the other
  design; it costs a `SETTLEMENT_VAULT_LEN` bump plus the web/seeder/e2e offset
  updates that came with the last one, and it would have had to be threaded
  through every CPI that creates a vault.)
* **The standalone flow is byte-for-byte unchanged** — `refund_source` is not an
  ATA of the hooks authority there, so the `else` branch runs exactly as before,
  which is what the existing `tests/ts` cancellation still exercises.

Client follow-through: the web console reads the rule (`SettlementPage`
defaults the refund field to the taker's account when the vault's
`refund_source` is the hooks custody ATA, and accepts either destination), and
the `settlementCancel` parameter is documented as such.

The rule is pinned by an executed test — see [the regression test that landed
with S-1](#the-regression-test-that-landed-with-s-1).

### S-3 — terminal escrows can always be closed

Two halves:

* Both refund paths now move the escrow's **whole balance**
  (`vault_token_account.amount`) instead of the recorded `vault.amount`. Nothing
  is lost by paying the donation along: unlike `settle`, a refund has no signed
  amount to honour. After a refund the escrow is genuinely empty, so
  `close_settlement` works.
* `close_settlement` no longer *refuses* a non-empty escrow; it sweeps the
  residue to the config authority's ATA for the mint. That account is a new
  field (`CloseSettlement.residual_account`) pinned by derivation
  (`InvalidRefundAccount`) and by `token::mint`/`token::authority`, so the sweep
  cannot be redirected. The account has to exist, so the web console creates it
  in the same transaction. The `EscrowNotEmpty` check is kept as an assertion
  *after* the sweep (with a `reload()`, since the transfer leaves the in-memory
  balance stale), so the error variant still means what it says.

### S-5 — the oracle's PDA checks no longer panic

`oracle_state.load().unwrap()` was evaluating inside five seed constraints, so a
malformed account aborted the program instead of failing a check. All five are
replaced by equivalent, non-panicking checks:

* `record_observation` derives the PDA from `authority.key()` (the signing pool,
  which the handler already requires to equal `oracle_state.pool`);
  `check_deviation` and `trip_breaker` from `pool.key()`. `seeds = [b"oracle",
  pool.key().as_ref()]` is the same shape `initialize` already used in this file.
* `get_twap` and `unpause_pool` carry no pool account to derive from, so their
  constraint is gone and a new `require_oracle_pda` helper checks the address in
  the handler against the pool the state records — a new appended error,
  `InvalidOracleState` (6011). `unpause_pool` takes a read borrow to resolve the
  pool before the write borrow, since the address check needs the state.

No account list changed, so no client moved; the binding strength is the same
(the handler check is the check the seed was performing).

### S-2 — accepted, and narrowed

`after_swap`'s missing signer and caller-chosen `taker` are real, and they are
precisely the mechanism S-1 exploited: a stranger could take the beneficiary
role for a balance sitting in custody. The fix for S-1 removes the way balances
end up there — refunds no longer strand funds in the custody ATA, and every
AMM/routed path pays the custody account and calls `after_swap` inside one
instruction, so there is no idle window — and what remains is that a **donation**
to the custody ATA is claimable by whoever names themselves next. Claiming it
still requires a Transfer Agent signature over a trade the desk never made.

Gating the instruction properly (a signer, or a caller allow-list) is not free:
the four AMMs call it with their own user signer while `order_book` calls it
with the *maker*, who does not sign the fill — so a signer requirement cannot be
imposed without breaking the order book's routing, and a program allow-list
would have to live in the hooks program with no state to hold it. Recorded as
accepted, with the S-1 fix as the mitigation that matters. If this is revisited,
the natural home is a `hooks` program-side allow-list gated on the config's
`hooks_authority` and the AMM program ids.

### S-4 — accepted

A rent-payer field on `SettlementVault` would fix a bookkeeping asymmetry at the
cost of a layout change (and the offset updates it forces on the web decoder,
the seeder and the e2e constants). The exposure is a funded PDA being drained of
its rent float by its own trades, recoverable with `fund_hook_authority`, so it
is documented instead: `programs/settlement/README.md` now says who funds routed
rent and who reclaims it.

### The regression test that landed with S-1

`tests/e2e/tests/order_book.rs` gains
`refund_expired_of_a_routed_trade_pays_the_maker_not_the_custody_account`. It
drives a real routed fill (`order_book` → `hooks::after_swap` →
`initiate_settlement`), then writes `Clock.slot = vault.expiry_slot` directly —
the harness can set the whole `Clock` sysvar, so the 216 000-slot deadline is
reachable without executing those slots — and asserts:

1. refunding to the hook authority's custody ATA, the account that funded the
   escrow, is refused with `InvalidRefundAccount` (6016), and nothing moves;
2. refunding to the vault's recorded taker — the maker — succeeds for an
   unprivileged cranker;
3. the maker's balance rises by the escrowed 200 quote, the escrow ends empty,
   **the custody ATA ends empty**, and the vault terminates as `Expired`.

That pins S-1's fix (and its reviewer's reasoning) to an executed transaction
rather than to inspection, and it is the first test anywhere in the repository to
cover a settlement refund. Suite counts after the change: `clmm` 14, `dlmm` 7,
`kyc_gate` 9, `maverick` 9, `oracle` 14, `order_book` 11 — **64 cases, all green**
(`cd tests/e2e && cargo test`, exit 0) against freshly built SBF images.

### What the remediation does not prove

* **`cancel_settlement` and `close_settlement` are still untested.** The fixed
  refund rule is shared by `cancel_settlement`, so it is covered by extraction
  rather than by its own case, and the S-3 residual sweep in `close_settlement`
  is covered by neither a unit nor an e2e test. Both are exercised only in the
  sense that the program compiles and the suites around them run.
* **The refused-destination assertion is on the settlement program's own error
  code**, which is what makes it precise; if the check ever moves (as S-5's did
  in the oracle), the test has to move with it.
* **No program was exercised on a live cluster**, and the `tests/ts` suite (which
  reaches `cancel_settlement` for the standalone flow, and therefore does cover
  the branch the new rule keeps unchanged) needs a validator plus the `tests/ts`
  harness; it was not run in this round.

---

## Round-1 findings re-verified against the current tree

Every item round 1 marked **fixed** is present in the code as described. Spot-checks
that were re-run in this round:

| Round-1 item | Re-verified where |
| --- | --- |
| H-1 (refund destination pinned) | `settlement/src/lib.rs:419`, `:1119`, `:1173` — present (and the source of S-1 on the routed flow) |
| M-1 / M-2 (custody ATA pinned by derivation) | `hooks/src/lib.rs:200-215`, `dlmm:918`, `maverick:1115`, `clmm:2049`, `twamm:1801` |
| M-3 (explicit settlement amount + delta accounting) | `hooks/src/lib.rs:64-74`, `settlement/src/lib.rs:354-358`, `:491-507` |
| M-5 (`set_authority` rejects the default pubkey) | `dlmm:1039`, `maverick:1196`, `clmm:1289`, `twamm:1247` |
| M-6 (trader token accounts bound to the signer) | `token::mint`/`token::authority = user` on every deposit, withdrawal, collect, swap and order context in all four AMMs |
| M-8 (`AddLiquidity.pool` writable) | `maverick:2394-2403` carries `mut` with the flag it writes named in the comment |
| L-3 (`clmm` admin pause) | `clmm:1273`, `:1285` |
| L-4 (first-depositor inflation) | `MIN_INITIAL_SHARES` at `dlmm:71`/`:313` and `maverick:107`/`:367` |
| L-7 (shared upgrade-authority guard) | every caller routes through `libs/program_governance`; no duplicate parser remains |

No narrowing cast in a value path is unguarded: the `as` conversions flagged by
the sweep are `u64`/`u16`/`u8` → `u128` widenings or `usize` → `u64` sizing
casts, and every narrowing conversion goes through `u64::try_from`/`u128::try_from`
with a typed error. `overflow-checks = true` is still set for the release profile
in the workspace `Cargo.toml`.

---

## `order_book` — first dedicated review

Covered here for the first time. The program's own
[README](../programs/order_book/README.md) states its invariants; each one was
checked against the code, and the account wiring was checked against the handler
that writes it.

**Verified sound:**

* **Book creation is upgrade-authority gated** (`order_book/src/lib.rs:102-109`),
  so a market cannot be squatted with an attacker-owned pause key. Both rails are
  fixed at creation, each pinned to a canonical PDA when on
  (`kyc-config` from `kyc_gate`, `hook_authority` from `hooks`) and required to be
  *all sentinel* when off.
* **One escrow per order, pinned by derivation.** The escrow is the order PDA's
  ATA, pinned with `get_associated_token_address_with_program_id` before any
  transfer, and its mint is checked against the order's side (`WrongEscrowMint`).
  The side is what selects the mint, so a fill cannot be pointed at the order's
  account for the other leg.
* **Deposits are measured, not asserted.** `create_order` credits the
  post-transfer balance delta, so a Token-2022 transfer fee (or a pre-funded
  escrow ATA) cannot leave the order claiming more than it holds.
* **State before external calls.** `fill_order` writes `remaining_amount`,
  `filled_base` and `filled_quote` and calls `order.exit(&crate::ID)` before the
  KYC CPI and either transfer, so a transfer hook running mid-instruction cannot
  re-enter against a stale `remaining_amount`. Only a mint whose hook is under
  the protocol's own control can re-enter at all here, since the book's mints are
  chosen by the upgrade authority — and the escrow leg is released last, after
  the payment leg, so payment still precedes delivery.
* **Rounding always favours the maker.** An ask's quote leg rounds up
  (`quote_for_base`), a bid's base leg rounds down (`base_for_quote`), and a leg
  that rounds to zero is refused (`ZeroFillLeg`) rather than given away. Both are
  unit-tested at the boundary, and `u64::try_from` rejects an out-of-range result
  instead of wrapping.
* **Exits cannot be trapped.** `cancel_order` ignores pause, KYC and expiry,
  sweeps the escrow's **whole** balance and closes both accounts, so a donation to
  the publicly derivable escrow ATA cannot brick the refund — the property S-3 is
  missing in `settlement`.
* **Pause is non-trapping.** `set_paused` refuses `create_order` and `fill_order`
  only; `cancel_order` is unaffected. `set_authority` refuses the default pubkey.
* **Proceeds are pinned, and the rail is enforced both ways.** With the rail off,
  `proceeds_token` must be the maker's ATA for the payment mint; with it on, it
  must be the hook authority's custody ATA by derivation *and* by owner and mint,
  and the accounting figures are pinned to `[b"settlement", trade_id]`,
  `[b"escrow", trade_id]` and the canonical settlement config.
  A rail that is off rejects **every** one of its accounts (including the
  `proceeds_token` derivation and the sentinel checks on the unused programs), so
  an unused rail cannot be smuggled past the book's recorded policy.
* **The vault beneficiary is the maker.** `fill_order` passes `maker` as
  `hooks::after_swap`'s `taker`, so a routed fill's T+N release is signed for the
  party that placed the order rather than whoever filled it.
* **The book is not written by a fill.** `FillOrder.book` is read-only and the
  handler does not write it — no repeat of the M-8 shape.
* **The kyc rail verifies the right party**: the maker at creation, the taker at
  fill, and the config must equal the book's recorded one.

**Residual notes (no action requested):**

* `associated_token_program` is a required account in `FillOrder` and is pinned
  to the sentinel only on the rail-**off** path; on the routed path it is
  unvalidated. It is unused on both paths (the hooks CPI does not take it), so
  this is dead weight in the layout rather than an exploitable gap.
* A book cannot be closed or migrated (`Book` has no `close`), and price is
  maker-set with no oracle rail — both documented in the README as design
  choices.
* `cancel_order` refunds the whole escrow balance, donations included, so the
  refund is not an exact accounting signal; the README says so, and
  `remaining_amount` is the accurate field.

`order_book`'s unit suite (5 tests) covers the leg math, dust rejection and
overflow; its LiteSVM suite (10 cases) covers partial fills, escrow isolation,
side guards, pause/expiry semantics and a full routed fill to a Transfer Agent
signature. Nothing in the file changed this round.

---

## What this round did not establish

The honest limits of the pass above:

* **S-1 was a code-path derivation, not an executed proof.** There is no test to
  run: reaching `refund_expired` needs 216 000 slots, and no harness in the repo
  warps there. The finding rested on the four constraints quoted above
  (`refund_source` assignment, `refund_token_account` pinning, `amm_vault`
  pinning, and `after_swap`'s lack of a signer), each of which is exact. The
  remediation replaced that reasoning with an executed test — writing the
  `Clock` sysvar reaches the deadline without the warp — so the refund path is
  now covered the way the rest of this tree is; the two paths the test does not
  reach (`cancel_settlement`, `close_settlement`) are listed under "What the
  remediation does not prove".
* **The four AMMs' internals were not re-derived.** The bin fill loops
  (`dlmm`/`maverick`), the tick walk and fee accumulators (`clmm`) and the TWAMM
  segment/netting math were read at the boundary (entry checks, share pricing,
  rail ordering, payout transfers) and left to the unit suites, which carry 53
  `math_core` tests plus 23 `dlmm`, 41 `maverick`, 26 `clmm` and 26 `twamm`. A
  dedicated math review of those is a separate exercise.
* **No program was executed on a cluster.** Verification was static plus
  `cargo test --workspace`. `tests/e2e` (LiteSVM, built SBF images) was not run
  this round; it requires `anchor build` first, and its own README says so.
* **`Twamm`'s order-list walk** — `walk_order_list` over caller-supplied order
  accounts — is the most intricate surface in the repository. The link checks
  (`expected` must be the key the previous order points at, the head must be
  empty or live after the walk, an insertion point may not skip an order
  expiring earlier, and hook accounts may not interleave with order accounts)
  were read and are mutually reinforcing, but this was a read, not a fuzz. It is
  where a fuzzer (Trident) would be worth its runtime.
