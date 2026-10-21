# Infrastructure

> **Status: planned.** No manifests here yet. Local development today is the
> deploy scripts plus `app/web`; this file describes the target topology and the
> operational rules that should hold once services are deployed.

## Environments

| Environment | Chain | Programs | Data | Who uses it |
| :--- | :--- | :--- | :--- | :--- |
| **localnet** | `solana-test-validator` (ports 8899/9900) | all 11, deployed by `scripts/deploy-localnet.sh` | ephemeral ledger in `.anchor/test-ledger` | Development, e2e suites |
| **devnet** | `https://api.devnet.solana.com` | all 11, deployed by `scripts/deploy-devnet.sh` | persistent, publicly readable | Integration, demos |
| **staging** | devnet or a dedicated cluster | same ids | indexer + analytics, non-public | Pre-production checks |
| **production** | mainnet | **not** deployed by these scripts (`deploy-devnet.sh` refuses `*mainnet*`) | indexer + analytics + monitoring | Institutions |

Program ids are identical across clusters: they are baked into `declare_id!` and
into `target/deploy/<name>-keypair.json`, so PDAs and client configs do not
change with the cluster. That is also why the keypairs are the most sensitive
files in the repo — losing one means a new address and a client migration.
Treat `target/deploy/*-keypair.json` as secrets in production.

## Local topology

```
┌──────────────────┐        RPC :8899        ┌──────────────────┐
│  solana-test-    │◄───────────────────────►│  app/web (vite)  │
│  validator       │        WS  :8900         │  :5173           │
│  ledger .anchor/ │                         └──────────────────┘
└────────┬─────────┘
         │ deployed images (target/deploy/*.so)
         ▼
   loader + 11 programs
```

`deploy-localnet.sh` owns: the validator process, its ledger and log, airdrop,
keypair verification and deploy. `app/web` talks to it directly; there is no
backend in the loop for local development.

## Planned service topology

```
                 ┌──────────────┐
   RPC + WS ────►│   indexer    │──► Postgres ──► analytics ──► reports
                 └──────┬───────┘
                        │
                 ┌──────▼───────┐
                 │ oracle keeper│──► alerts (P1/P2/P3)
                 └──────┬───────┘
                        │
                 ┌──────▼───────┐
                 │  app/web     │  (static build; reads RPC for live state,
                 └──────────────┘   indexer for history)
```

Design rules:

* **The indexer is the only service that reads chain history.** The web app keeps
  reading live account state over RPC so a stale indexer cannot make the trading
  UI lie about the current book.
* **No service holds a trading key.** The indexer and analytics are read-only;
  the oracle service alerts and reports, and its only on-chain-adjacent action is
  telling a human to call `unpause_pool`.
* **Postgres is the single store.** Analytics reads it, exports to Parquet for
  large scans; no second write path.

## Configuration

| Variable | Used by | Notes |
| :--- | :--- | :--- |
| `ANCHOR_PROVIDER_URL` | web, tests, scripts' output | Cluster endpoint; `http://127.0.0.1:8899` locally |
| `ANCHOR_WALLET` | deploy scripts | Payer **and** upgrade authority |
| `SOLANA_RPC_URL` | `deploy-devnet.sh` | Default devnet endpoint |
| `SBF_ARCH` | `programs.sh` | Default `v0`; see `scripts/README.md` |
| `DATABASE_URL` | indexer, analytics (planned) | Postgres DSN |
| `RPC_URL`, `WS_URL` | indexer (planned) | Separate endpoints so WS reconnects do not disturb HTTP backfill |
| `COMMITMENT` | indexer (planned) | `confirmed` for UI lag, `finalized` for balances |

Secrets: RPC provider keys, `DATABASE_URL`, and the production upgrade-authority
keypair. None of them belong in the repo, in a committed `.env`, or in the web
bundle. The web build is static and public — anything compiled into it is public.

## Deploy pipeline (proposed)

1. `cargo test --workspace` and `cargo test --manifest-path tests/e2e/Cargo.toml`
   against freshly built images.
2. `bun test`, `bunx tsc -b`, `bunx oxlint .` in `app/web`.
3. `scripts/deploy-devnet.sh --only <changed>` with a staging payer.
4. Smoke: the TypeScript suite against the deployed cluster.
5. Promote the same images to production with the multisig upgrade authority;
   record the program hashes and the deploy slot in the release notes.

Notes:

* **A program upgrade is a state migration decision, not just a code push.**
  Account layouts are read in place; adding fields at the end is safe only if
  every reader tolerates the old size. Check the program README's layout table
  before upgrading an account-owning program.
* **`anchor build --arch v0`** is required for the pinned Agave toolchain; the
  same arch must be used for images that tests run.
* **Never delete `target/deploy/*-keypair.json`** for a deployed program. Without
  it you cannot upgrade at that address.

## Observability

| Signal | Source | Why |
| :--- | :--- | :--- |
| Indexer lag (cluster slot − applied slot) | indexer `/health` | The failure mode that looks healthy |
| RPC error rate and 429s | indexer, keeper | Backfill stalls silently |
| Breaker state per pool | `oracle_state.is_paused` | P1 alert |
| Unarmed breakers (`observation_count < 2`) | `oracle_state` + `ALARM` logs | Silent loss of protection |
| Settlement backlog (pending vaults by age) | indexer | T+N releases are business-critical |
| Web build version + selected cluster | app build metadata (proposed — the console already exposes a cluster selector) | "Which UI is this desk running, against which cluster?" |

Logs are structured JSON with `program`, `signature` and `slot` on every chain
event; alerts carry a runbook link, not just a metric name.

## Backup and recovery

* **Postgres**: nightly base backup + WAL archiving. The indexer can always be
  rebuilt from chain, but a rebuild takes hours — restore first, reconcile after.
* **Indexer cursors**: persisted, so a restart resumes rather than replays.
* **Keypairs**: offline, multisig in production, with a tested recovery
  procedure. Losing the upgrade authority is unrecoverable for the program.
* **Ledger (localnet)**: disposable. `--reset` is the recovery plan.

## Non-goals

No container manifests are committed yet on purpose: a compose file that drifts
from the scripts is worse than none. When services land, compose is for
localnet/staging only, and production runs on a managed platform with the
observability above.
