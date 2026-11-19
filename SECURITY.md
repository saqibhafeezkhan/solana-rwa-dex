# Security Policy

The programs in this repository custody tokenized real-world assets and clear
T+N settlements, so reports are taken seriously and triaged before public
disclosure.

## Reporting a vulnerability

Do **not** open a public issue for a suspected vulnerability.

Use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**). Include:

- the program and instruction affected,
- the account or instruction sequence that triggers it,
- the expected versus observed behaviour,
- whether funds, compliance state, or an authority key are at risk.

If private reporting is unavailable, contact the repository maintainers
directly through the address on their GitHub profile and ask for a secure
channel. Never send a private key or seed phrase.

## Scope

In scope:

- the eleven Anchor programs under `programs/` and the shared crates under
  `libs/`,
- the deploy/build scripts under `scripts/`,
- the client instruction builders under `app/web/sdk/src/core/` where they can
  cause a loss of funds or bypass a program guard.

Out of scope: the sample-data console mode, third-party RPC providers, and
griefing that only costs rent.

## What to expect

- Acknowledgement of a complete report within 3 business days.
- A written triage with severity and a remediation plan.
- Credit in the fix notes unless you ask to remain anonymous.

Please give the maintainers a reasonable window to ship and deploy a fix
before publishing details. Findings already listed in
[`docs/security-review.md`](docs/security-review.md) as accepted or documented
are not new reports unless the analysis is wrong.
