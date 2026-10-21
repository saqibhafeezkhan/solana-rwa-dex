# Oracle service

> **Status: planned.** No code in this directory yet. What follows is the
> operator-side contract around the on-chain `oracle` program, not a second
> price oracle.

## What already exists on-chain

`programs/oracle` is a per-pool TWAP ring buffer plus a latching circuit breaker.
Its trust model decides what an off-chain service can and cannot do:

| Instruction | Signer | Meaning for a keeper |
| :--- | :--- | :--- |
| `initialize` | the pool PDA | Only the AMM that owns the pool can create its oracle, so threshold and owner are fixed atomically with the pool |
| `record_observation` | the pool PDA | Observations are written by swaps, never by a keeper. There is no "push price" instruction to abuse |
| `check_deviation` | the pool PDA | Pure check; reverts the swap it guards. Writes nothing |
| `trip_breaker` | the pool PDA | **Latches** the pause in a transaction that succeeds. Reachable only through the owning pool's instruction |
| `unpause_pool` | `oracle_state.owner` | The only way back; deliberately not a keeper key |

So the service is not a price feed. It is monitoring, alerting and reporting
around a breaker whose inputs are already on-chain.

## Responsibilities

1. **Watch for unarmed breakers.** A pool with fewer than two observations (or
   no recorded price yet) has `check_deviation` skipping its check with
   `ALARM: circuit breaker skipped (TWAP not available yet; < 2 observations)`.
   That is a *silent* loss of protection, visible only in logs and state. Track
   `observation_count` per pool and alert when it stays below 2.
2. **Watch for tripped breakers.** `is_paused` is set by `trip_breaker`. Alert
   immediately, with the pool, the deviation that tripped it and the 30-minute
   TWAP it was compared against.
3. **Watch for stale prices.** The TWAP accumulator extrapolates the newest
   observation forward, so a pool that has not traded for hours still returns a
   number. Report **age of newest observation** alongside every TWAP; a fresh
   average of a stale price is a misleading dashboard.
4. **Report deviation headroom.** For each pool: current spot, 30-minute TWAP,
   `deviation_bps` and `max_deviation_bps`, so operators see how close a pool is
   to tripping *before* it does.
5. **Publish reference valuations.** Off-chain NAV/reference marks for RWA
   collateral, used by dashboards and by the human decision to unpause. These are
   labelled as reference data; they never become on-chain prices.
6. **Page the right human.** Trips and unarmed breakers go to the owner
   multisig, not to an automated unpause. The breaker cannot be cleared by the
   service by design.

## Cadence

| Job | Interval | Notes |
| :--- | :--- | :--- |
| Account poll (`oracle_state` per pool) | 30–60 s | Cheap: one `getMultipleAccounts` per batch |
| Log scan for `ALARM:` / `Circuit breaker tripped` | per slot, via `logsSubscribe` | Catches the low-history case the state cannot distinguish from "fine" |
| Deviation computation | with each poll | Same formula as the program: `|spot − twap| · 10 000 / twap` |
| Reference marks | daily (or per valuation policy) | Signed by the valuation desk; stored with source and timestamp |
| Heartbeat | 1 min | Emits indexer-lag-style health: last slot seen, pools covered, RPC errors |

Store every poll as a time series; the interesting incidents are the ones where
the deviation crept up over an hour, and a current-state-only poll loses that.

## Alerts and runbook

| Severity | Condition | Action |
| :--- | :--- | :--- |
| P1 | `is_paused = 1` | Verify with an independent reference price; confirm the tripping transaction; owner multisig reviews and calls `unpause_pool` |
| P1 | `observation_count < 2` for > 1 hour | The pool's breaker is not armed; investigate why the pool has no swaps, and consider whether the threshold is meaningful at launch |
| P2 | deviation > 80 % of `max_deviation_bps` | Warn the desk; check for a real price move vs a thin book |
| P2 | RPC errors / stale heartbeat | The monitor itself is blind — treat as an outage |
| P3 | reference mark older than its policy window | Valuation data is stale; flag the affected dashboards |

Deliberately **not** automated: unpausing, changing `max_deviation_bps`
(immutable after `initialize`), and creating oracles (pool-program-only).

## Failure modes

* **No history** — the breaker allows every trade. This is the documented L-2
  window; the `ALARM` log plus `observation_count` is the only signal.
* **Thin market, one price** — a single trade sets both spot and the newest
  observation, so deviation looks healthy while the mark is unrepresentative.
  Cross-check against reference marks, not just the TWAP.
* **Ring-buffer wrap** — 100 observations deep. A pool trading faster than the
  ring fills still has history, but a *quiet* pool's oldest observation may be
  well beyond the 30-minute window; `get_twap` returns `None` until enough
  history exists, which the breaker treats as "allow".
* **Clock skew** — the window is `unix_timestamp`, not slots. A cluster with
  drifting timestamps distorts the average; alert if local clock and cluster
  clock diverge.
* **Owner key loss** — nothing can unpause. The owner must be a multisig with a
  tested recovery path, decided at pool creation.

## Scope boundary

Not in this service: writing observations (pool-signed), changing thresholds
(immutable), holding trading keys, or making valuation an on-chain input. If a
future design needs an external push oracle, that is a **new program** with its
own trust model and audit — not an addition to this keeper.

## Verification

Once implemented, the service should be tested against a localnet with a
deliberately mispriced pool: the breaker must trip, the alert must fire, and the
`is_paused` transition must be visible within one poll interval. The on-chain
half of that flow is already covered by `tests/e2e/tests/oracle.rs` (14 cases).
