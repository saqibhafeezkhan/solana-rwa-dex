# Solana RWA DEX — Documentation Hub

Welcome to the comprehensive technical documentation for the **Solana RWA DEX**, an institutional-grade decentralized exchange and clearinghouse engineered for Real World Assets on Solana.

---

## 📚 Core System Guides

| Document | Focus & Audience | Key Topics Covered |
| :--- | :--- | :--- |
| [**Feature Specification**](features.md) | Engineers, Auditors, Product | Complete full-stack feature catalog: every on-chain instruction and guard across the 10 programs + 2 libs, cross-cutting rails and invariants, the web console, off-chain services, tooling, a status matrix, and known gaps. |
| [**Proposed On-Chain Features**](proposed-features.md) | Engineers, Protocol Designers | Assessment of features worth adding: batching (settlement/KYC), settlement deadlines and per-mint agent keys, an order-book program, exact-output swaps, protocol-fee switch, events, and governance hardening — with CU/account ceilings and risks. |
| [**Architecture Overview**](architecture.md) | System Architects, Engineers | Multi-program topology, Token-2022 compliance extensions, the 4 AMM engines, shared libraries, end-to-end trade execution lifecycle, sequence diagrams. |
| [**Risk & Compliance Framework**](risk.md) | Risk Officers, Legal & Compliance | Institutional RWA risk taxonomy, 5-tier defense-in-depth model, dual-proof Merkle whitelisting, soulbound KYC, circuit breakers, T+N clearing, non-trapping capital invariant. |
| [**Trading Strategies & Market Structure**](trading-strategies.md) | Market Makers, Quant Traders, LPs | AMM engine selection matrix, concentrated liquidity on pegged assets, zero-slippage bin trading, dynamic moving bins, institutional TWAMM netting, cross-engine arbitrage under T+N lag. |
| [**Security Review & Audit Report**](security-review.md) | Auditors, Security Engineers | Exhaustive code audit covering all 10 programs (~15,200 lines of Rust), findings D-1 through L-8, verified security invariants, and positive findings. |

---

## 🏛️ On-Chain Program Catalog

The protocol consists of ten modular Anchor programs and two shared Rust crates:

### Core Compliance, Oracle & Settlement Rails

- [`programs/rwa_mint`](../programs/rwa_mint/README.md) — Dynamic Token-2022 mint factory with embedded metadata, metadata pointers, permanent delegate, and transfer fee extensions.
- [`programs/transfer_hook`](../programs/transfer_hook/README.md) — RFC-6962 dual-proof Merkle whitelist enforcing regulatory compliance on both sender and receiver for every token transfer. Each institution's path is stored in its own `WhitelistEntry` PDA, the only route Token-2022 offers the hook.
- [`programs/kyc_gate`](../programs/kyc_gate/README.md) — Soulbound (`NonTransferable`) institutional identity credentials with atomic pre-trade `verify_kyc` CPI verification.
- [`programs/oracle`](../programs/oracle/README.md) — 100-observation TWAP ring buffer and automated circuit breaker protecting against flash loans and price manipulation.
- [`programs/settlement`](../programs/settlement/README.md) — Institutional clearinghouse featuring per-trade T+N escrow vaults and off-chain Transfer Agent secp256k1 cryptographic clearance.
- [`programs/hooks`](../programs/hooks/README.md) — Uniswap v4-style post-swap hook intercepting AMM outputs and routing them into the settlement escrow.

### Automated Market Maker (AMM) Engines

- [`programs/clmm`](../programs/clmm/README.md) — Uniswap v3-style concentrated liquidity market maker with continuous Q64.64 curves, tick bitmaps, and exact fee accounting.
- [`programs/dlmm`](../programs/dlmm/README.md) — Bin-based dynamic liquidity market maker offering zero slippage inside bins and volatility-adjusted dynamic fees.
- [`programs/maverick`](../programs/maverick/README.md) — Dynamic distribution AMM with permissionless directional bin migration (`Right`, `Left`, `Both`), value-neutral merges, and fixed-stride memory layout.
- [`programs/twamm`](../programs/twamm/README.md) — Time-weighted average market maker for institutional block orders, order slicing, and virtual order netting at marginal price.

### Shared Rust Libraries

- [`libs/math_core`](../libs/math_core/README.md) — Q64.64 fixed-point math, 256-bit safe arithmetic, tick-to-sqrt-price and sqrt-price-to-tick transformations, and swap step calculations.
- [`libs/program_governance`](../libs/program_governance) — BPF Upgradeable Loader inspection utility (`require_upgrade_authority`) validating upgrade authorities on one-time initializers.

---

## 🖥️ Client & Off-Chain Infrastructure

- [`app/web`](../app/web/README.md) — Institutional web console built with Vite + React + TypeScript and the Bun runtime, encoding Anchor instructions directly without IDL codegen; includes a transaction-free sample-data mode, role-based access and a multisig proposal queue.

> The four entries below are **placeholders**: each directory has a README describing the intended service, but no implementation ships in this repository yet.

- [`services/indexer`](../services/indexer/README.md) — Intended real-time account-state indexer for market activity, settlement escrows, and audit logs.
- [`services/oracle`](../services/oracle/README.md) — Intended off-chain keeper monitoring reference feeds, TWAP divergence, and automated circuit breaker tripping.
- [`services/analytics`](../services/analytics/README.md) — Intended execution-quality analyzer, TWAMM cost-savings metrics, and portfolio performance tracking.
- [`infra/`](../infra/README.md) — Intended Docker, validator and network deployment configuration.

---

## 🧭 Role-Based Navigation Guide

### For Smart Contract Engineers
1. Read the [Architecture Overview](architecture.md) to understand program interactions and CPI boundaries.
2. Review the [Security Review](security-review.md) to understand account constraints, PDA seeds, and patched vulnerabilities.
3. Review the program-specific READMEs under `programs/<name>/README.md` for exact account structs and error codes.

### For Risk & Compliance Officers
1. Review the [Risk & Compliance Framework](risk.md) for details on investor whitelisting, regulatory seizure controls, and sanctions screening.
2. Examine the [Settlement README](../programs/settlement/README.md) for how the Transfer Agent signs off on trades via secp256k1 signatures.
3. Review the [KYC Gate README](../programs/kyc_gate/README.md) for soulbound attestation issuance and non-consensual revocation.

### For Market Makers & Quant Traders
1. Study the [Trading Strategies & Market Structure Manual](trading-strategies.md) to choose the right AMM engine for your asset class.
2. Review the mathematical models in the CLMM, DLMM, Maverick, and TWAMM documentation.
3. Understand the impact of T+N settlement latency on cross-venue arbitrage formulas.

