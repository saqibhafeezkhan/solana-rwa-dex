# RWA DEX Frontend

This folder contains the Vite + React + TypeScript trading interface for the Solana-based RWA DEX.

## Runtime

This project uses Bun instead of npm for package management and local tooling.

## Commands

```bash
bun install
bun run dev
bun run build
bun run preview
```

## Stack

- Vite
- React
- TypeScript
- Bun
- Solana wallet integration (Phantom / Backpack via injection)
- Institutional trading console for every program in the workspace

## Pages

- **Overview** — protocol map, live infrastructure status (settlement config, KYC mint, hook authority), your T+N escrows
- **CLMM** — create pools, add/remove concentrated liquidity over tick ranges, collect fees, swap with an explicit tick walk, trip the circuit breaker
- **DLMM** — create pools, initialize bin arrays, per-bin liquidity, swap across bin arrays, pause/rotate authority
- **Maverick** — bin kinds (static/right/left/both), permissionless `move_bins` keeper, swap, remove along the move chain
- **TWAMM** — LP shares, long-term orders (create / crank / cancel / withdraw), virtual-order execution
- **Settlement** — clearinghouse config, transfer-agent key rotation, escrow initiate / settle (secp256k1) / cancel
- **KYC & Whitelist** — soulbound attestations (issue/revoke/rotate), transfer-hook Merkle root governance, hook-authority funding
- **RWA Assets** — create Token-2022 RWA mints (hook + metadata + transfer fee), mint, transfer with dual Merkle proofs, fee updates

## Client architecture

The app does not use generated IDL clients; it encodes Anchor instructions directly:

- `src/core/borsh.ts` — minimal borsh writer + Anchor discriminators (sha256 of `global:<name>`, cached)
- `src/core/pdas.ts` — every PDA derivation, mirroring the on-chain seeds exactly
- `src/core/instructions-*.ts` — one builder per program; account order matches each `#[derive(Accounts)]` struct; optional rails are left off with the program-ID sentinel
- `src/core/decoders.ts` — field-exact decoders for pool, position, oracle, settlement and token accounts
- `src/ui/` — wallet/cluster context, toast transaction feedback, one page per protocol

Cluster selection (localnet by default, matching `Anchor.toml`) persists in `localStorage`.
