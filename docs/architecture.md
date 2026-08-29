# Architecture Overview

## System overview

The architecture should be split into four layers:

1. On-chain execution layer
2. Frontend trading layer
3. Off-chain analytics and risk services
4. Compliance and governance layer

## On-chain layer

The Anchor program should handle:

- asset registration
- market creation
- order instruction execution
- vault and custody management
- pricing data validation
- settlement and finalization

## Off-chain layer

Off-chain components should handle:

- market data aggregation
- order matching or strategy routing
- analytics and monitoring
- event indexing for the UI
- risk checks and compliance evaluations

## Frontend layer

The Vite app should focus on user interaction:

- wallet and account connection
- market search
- price and volume charts
- order entry forms
- trade confirmations
- open orders and positions
- admin screens for restricted assets

## Long-term product model

The real product is ultimately a hybrid of traditional exchange infrastructure and blockchain-native settlement. This means the system should be designed for trust, auditability, and institutional operations, not just user-facing swaps.
