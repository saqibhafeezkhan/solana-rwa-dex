# Web frontend

This file is a leftover from before the frontend lived inside `app/web/`. The
frontend is **not** built from this folder, and nothing here is loaded by it.

The real documentation is [`web/README.md`](./web/README.md): stack,
localnet bootstrap, seeding, the manual-trade and orders flows, and the test
commands.

Quick pointer to the code:

| Path | Contents |
| :--- | :--- |
| `app/web/src/App.tsx` | Shell, routing, wallet/cluster state |
| `app/web/src/core` | PDAs, account decoders, instruction builders, Merkle helpers, RPC plumbing |
| `app/web/src/ui/pages` | One file per page (markets, trade, orders, portfolio, risk, admin) |
| `app/web/src/ui/components.tsx` | Shared presentational components |
| `app/web/scripts` | `bootstrap-localnet.sh`, `seed-localnet.ts` |
| `app/web/tests` | `bun test` unit tests for the decoders |
