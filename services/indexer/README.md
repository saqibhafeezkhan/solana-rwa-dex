# Indexer service

> **Status: planned.** This directory contains no code yet. The scope below is
> the contract the service should meet, written against what the programs
> actually expose today.

## Why it is needed

The console reads chain state directly over RPC, which is correct for "what is
true now" and wrong for everything else: fill history, realised spread, T+N
latency, a maker's fill rate, or "what did this book look like an hour ago". The
programs also emit **no `#[event]` types** — every instruction logs with `msg!`
and leaves the durable record in account state. An indexer therefore has to be
built on account diffs and instruction decoding, not on a log schema that does
not exist.

## What it ingests

| Source | Program | Notes |
| :--- | :--- | :--- |
| Pools, vaults, tick/bin arrays, positions | `clmm`, `dlmm`, `maverick` | PDA per pool (`[b"clmm-pool", mint_a, mint_b]`, `[POOL_SEED, …]`), vaults and arrays keyed under the pool |
| TWAMM orders and slices | `twamm` | Order state carries the schedule; executions are per-slice |
| Resting orders, fills | `order_book` | `Book` per mint pair, `Order` per maker id; a fill mutates both and moves tokens |
| Escrows and releases | `settlement`, `hooks` | `SettlementVault` (`[b"settlement", trade_id_le]`) with `Pending` / released / refunded status |
| Compliance state | `kyc_gate` | `[b"kyc-config"]` config, attestation mint, per-user attestations |
| Whitelist root | `transfer_hook` | Root rotations and `WhitelistEntry` PDAs (`[b"whitelist", owner]`) |
| Breaker state | `oracle` | `[b"oracle", pool]` `OracleState` (4 078 bytes), pause flag, observations |

Two ingestion paths, both required:

* **Backfill** — `getSignaturesForAddress` per program id, then
  `getTransaction` with `maxSupportedTransactionVersion: 0`, decoding inner
  instructions too (the hook and settlement work happens inside CPIs).
* **Live** — `logsSubscribe` on the program ids for instruction-level events,
  plus `accountSubscribe` for the handful of accounts that matter to the UI
  (books, pools, vaults). Account subscriptions alone would miss history;
  log subscriptions alone would miss state.

`getProgramAccounts` with a discriminator `memcmp` and a `dataSize` filter is the
right way to bootstrap each table (books, pools, open orders, pending vaults).
Prefer it over replaying history at first boot, and use replay only to fill in
events.

## Normalised model

Floor the schema at these entities; everything else is a view over them:

* `market` — venue (`clmm` | `dlmm` | `maverick` | `order_book`), base/quote
  mint, pool/book address, fee tier, tick spacing or bin step, paused flag.
* `pool_snapshot` — per slot: reserves, price, liquidity; keeps history usable
  after the pool account is closed.
* `order` — book, maker, side, price, original/remaining size, expiry, status.
* `fill` — book, taker, price, size, `trade_id`, signature, slot, settlement
  vault address.
* `settlement_vault` — `trade_id`, taker, mint, amount, unlock slot, status,
  release signature.
* `kyc_attestation` / `whitelist_entry` — subject, issuer, expiry, root version.
* `account_delta` — raw before/after bytes per touched account, keyed by
  signature + instruction index. This is the escape hatch: when a view is wrong,
  the raw diff is what lets you rebuild it.

## Correctness rules

1. **Idempotency.** Primary key `(signature, instruction_index)`. Re-delivery of
   a log, a retried backfill or an overlapping live stream must not double-count
   a fill.
2. **Slot ordering, not arrival ordering.** Apply by `(slot, tx_index,
   instruction_index)`; a websocket can deliver a later slot first.
3. **Finality.** Two levels: `confirmed` for UI responsiveness, `finalized` for
   balances, fills and settlement status. Never mark a vault released on a
   confirmed-but-unfinalised signature.
4. **Account existence is evidence.** A closed order account (rent reclaimed on
   cancel) means "cancelled", not "never existed" — the creation signature in the
   delta log is what distinguishes them.
5. **`trade_id` is caller-chosen.** Do not assume monotonicity; order by slot and
   treat the id as an opaque key scoped to its book/pool.
6. **Token amounts need mint decimals.** Store raw `u64` amounts plus the mint's
   decimals at the time of the event; decimals are mutable only by the mint
   authority, so snapshot them.

## Read API (proposed)

The frontend and analytics service consume the same endpoints:

```
GET /markets?venue=order_book
GET /markets/:address
GET /markets/:address/candles?interval=1m&from=…&to=…
GET /orders?book=…&status=open
GET /orders/:address
GET /fills?book=…&maker=…&limit=…
GET /settlements?status=pending&taker=…
GET /accounts/:address            # decoded, venue-aware
GET /health                        # lag, last indexed slot, queue depth
```

`/health` must report **indexer lag** (cluster slot − last applied slot). A stale
indexer that returns 200 is the failure mode that silently breaks a dashboard.

## Failure modes to design for

| Failure | Symptom | Handling |
| :--- | :--- | :--- |
| RPC rate limit / 429 | Backfill stalls | Bounded concurrency, exponential backoff, resumable cursor per program |
| Websocket drop | Live stream silently stops | Heartbeat on slot; fall back to polling `getSlot` + reconcile |
| Reorg (devnet/localnet) | Applied-then-reverted rows | Keep an unfinalised buffer; delete rows above the reorg slot |
| Unknown account layout | Decoder throws | Store the raw delta and skip the view; never crash the pipeline |
| Program upgrade | New discriminator / layout | Decode by (program id, account owner, size, discriminator); keep old layouts readable |

## Open decisions

* Storage: Postgres for entities + slot history, with Parquet exports for
  analytics. ClickHouse only if query volume justifies it.
* Process shape: one binary with per-program workers, so a slow venue cannot
  starve the others.
* Whether to add typed `#[event]` types to the programs first (see
  `docs/proposed-features.md` §2). That would simplify this service considerably,
  but it is a program change with a redeploy cost.
* Hosting: managed RPC vs a dedicated node. The backfill volume decides it.

## Non-goals

No trading logic, no key custody, no price discovery. The indexer is read-only
with respect to the chain and must never hold a signer.
