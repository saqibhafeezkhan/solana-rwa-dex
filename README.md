# RWA DEX on Solana

A Solana-based decentralized exchange for real-world assets (RWA), built around
institutional-grade execution, compliance-aware settlement, and a Vite-based
trading console.

The protocol pairs **Token-2022 RWA mints** (transfer hook + metadata + transfer
fee + permanent delegate) with **KYC-gated pools**, **TWAP circuit breakers**,
and a **T+N settlement clearinghouse**, then ships four AMM engines that all
route through the same rails.

## Status

| Layer | State |
| --- | --- |
| Anchor programs | 11 programs + 2 shared libs, `anchor build` clean |
| Frontend | Vite + React + TypeScript trading console (`app/web`) |
| Unit tests | `cargo test --workspace` — 209 tests, green |
| End-to-end tests | `tests/e2e` (LiteSVM, built SBF images) — 63 cases, green |
| Security review | [`docs/security-review.md`](docs/security-review.md) — findings fixed or explicitly accepted |

## Repository layout

```text
solana-rwa-dex/
├── Anchor.toml                 # program ids, local validator, test config
├── Cargo.toml                  # workspace: programs/* + libs/*
├── rust-toolchain.toml
├── programs/                   # eleven deployable Anchor programs
│   ├── rwa_mint/
│   ├── transfer_hook/
│   ├── kyc_gate/
│   ├── order_book/
│   ├── oracle/
│   ├── settlement/
│   ├── hooks/
│   ├── dlmm/
│   ├── maverick/
│   ├── clmm/
│   └── twamm/
├── libs/                       # shared crates compiled into the programs
│   ├── math_core/              # Q64.64 fixed-point math, u256, tick/sqrt-price
│   └── program_governance/     # shared `require_upgrade_authority`
├── app/
│   ├── web/                    # Vite + React trading console (see web/README.md)
│   └── web-README.md           # legacy pointer → web/README.md
├── tests/                      # see tests/README.md for the three layers
│   ├── e2e/                    # LiteSVM end-to-end suite (its own workspace)
│   ├── ts/                     # TypeScript integration suite (live validator)
│   └── test_initialize.rs      # stale `anchor init` scaffold, not compiled
├── docs/
│   ├── README.md               # documentation hub and index
│   ├── features.md             # full-stack feature catalog
│   ├── proposed-features.md    # candidate additions and feasibility analysis
│   ├── security-review.md      # findings, fixes and accepted risks
│   ├── architecture.md
│   ├── risk.md
│   └── trading-strategies.md
├── scripts/                    # deploy + validator helpers (see scripts/README.md)
├── packages/shared/            # planned framework-free client package
├── services/                   # planned indexer / oracle / analytics specs
└── infra/                      # planned environments, topology, ops
```

## On-chain programs

| Program | Role |
| --- | --- |
| `rwa_mint` | Creates and manages Token-2022 RWA mints (transfer hook, metadata pointer, transfer fee, permanent delegate). Permissionless factory; the mint authority is the on-chain issuer. |
| `transfer_hook` | RFC-6962 Merkle whitelist enforced on every transfer, checking **both** source and destination proofs. The proofs are held in per-institution `WhitelistEntry` PDAs, since Token-2022 builds the hook's instruction data itself and forwards nothing from the transfer. |
| `kyc_gate` | Issues soulbound (non-transferable) KYC attestations and exposes `verify_kyc`, which pools CPI into before a swap. |
| `oracle` | Time-weighted average price from pool-recorded observations, plus a circuit breaker that blocks deviating trades. |
| `settlement` | T+N escrow for swap output: release requires the settlement slot, an active expiry window, and a valid secp256k1 Transfer Agent signature over `keccak256(trade_id ‖ taker ‖ mint ‖ amount)`. Includes hard deadline (`refund_expired`) and optional per-mint agent keys. |
| `order_book` | Resting limit orders with partial fills. Makers escrow base (ask) or quote (bid) into an order-owned ATA; takers fill any amount at the maker's price. Proceeds route through the hooks → settlement T+N rail when wired, and every order's funds are isolated in its own escrow. |
| `hooks` | Post-swap hook (`after_swap`) that routes AMM output into the settlement escrow instead of paying the taker directly. |
| `dlmm` | Bin-based dynamic liquidity market maker. |
| `maverick` | Dynamic distribution AMM with permissionless bin moves that follow the price. |
| `clmm` | Uniswap-v3-style concentrated liquidity, continuous Q64.64 curve, exact fee accounting, Token-2022 transfer fee gross-up, admin pause. |
| `twamm` | Time-weighted average market maker for institutional block orders; opposing orders net before the embedded AMM is touched. |

Shared libraries:

- **`math_core`** — Q64.64 fixed-point arithmetic, 256-bit multiply/divide, tick
  and sqrt-price math, and swap-step computation.
- **`program_governance`** — `require_upgrade_authority`, used by one-time
  initializers to gate on the program's own upgrade authority (read from the
  loader's `ProgramData` account).

## Frontend

`app/web` is a Vite + React + TypeScript console that encodes Anchor
instructions directly (no generated IDL client). It has one page per protocol —
Dashboard, Portfolio, Orders, CLMM, DLMM, Maverick, TWAMM, Settlement, RWA
Assets, KYC & Whitelist — plus Enterprise (roles, multisig proposal queue,
address book) and
Audit (transaction history + NAV metadata) — with wallet/cluster context and
PDAs/decoders mirrored from the on-chain seeds. A sample-data mode (`?sample=1`)
renders the whole console from fixtures without a cluster and blocks sending.

```bash
cd app/web
bun install
bun run dev        # dev server
bun run build      # type-check + production build
bun run lint
```

See [`app/web/README.md`](app/web/README.md) for the client architecture.

## Getting started

Prerequisites: Rust, the Anchor CLI, the Solana CLI (3.1.10), and Bun for the
frontend.

```bash
anchor build                              # compile all programs to target/deploy/*.so
cargo test --workspace                    # unit tests
(cd tests/e2e && cargo test)              # end-to-end tests against the built images
```

The e2e suite is a **separate Cargo workspace** under `tests/e2e`: it executes
the compiled `.so` images inside LiteSVM and depends on the Solana stack that VM
is built from. Run `anchor build` before it so the images are current.

## Program ids

Ids are cluster-independent: they are baked into each crate's `declare_id!` and
into the keypairs under `target/deploy`, so a client's PDAs do not change with
the cluster.

| Program | ID |
| --- | --- |
| `clmm` | `GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU` |
| `dlmm` | `5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c` |
| `hooks` | `B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA` |
| `kyc_gate` | `7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1` |
| `maverick` | `9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4` |
| `oracle` | `4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj` |
| `order_book` | `3ACX2Lc67uZW65PA3QbYRLttmxE766eSDyD9Sd2yggwD` |
| `rwa_mint` | `5GbeJh4sySASsdH2zRdFX85LuXnu5y3rBkqhVi76Lzho` |
| `settlement` | `58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6` |
| `transfer_hook` | `fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS` |
| `twamm` | `FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9` |

Deploy helpers live in `scripts/` (`deploy-localnet.sh`, `deploy-devnet.sh`,
`start-local-validator.sh`).

## Vision and roadmap

The target is an institutional RWA venue, not just a swap app: tokenized
real-estate, private credit, commodities, carbon, treasuries, and structured
products traded with compliance-aware execution and auditable settlement.

Implemented today: Token-2022 asset issuance with a transfer-hook whitelist,
soulbound KYC gating, a TWAP circuit breaker, T+N escrow settlement, a
partial-fill limit order book, and four AMM engines that plug into those rails.

Still ahead:

- Off-chain order management services (order history, fills index, desk APIs)
- Smart routing across pools and venues
- Off-chain indexing and analytics (the `services/` placeholders are unfilled)
- Portfolio strategy automation and rebalancing
- Production monitoring, observability, and multi-asset risk limits

See the [Documentation Hub](docs/README.md), [`docs/architecture.md`](docs/architecture.md),
[`docs/risk.md`](docs/risk.md), [`docs/trading-strategies.md`](docs/trading-strategies.md), and
[`docs/security-review.md`](docs/security-review.md) for full architectural,
risk, trading, and security specifications.
