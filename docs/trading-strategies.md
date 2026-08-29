# Trading Strategy Considerations

## Strategy categories

The DEX should support multiple strategy types:

- Market making for liquid assets
- Passive liquidity provision for less liquid RWA pairs
- TWAP execution for large positions
- VWAP scheduling for order aggregation
- Cross-market arbitrage
- Rebalancing across portfolios
- Risk-adjusted execution based on volatility or market depth

## Execution quality requirements

For RWA assets, execution quality is not only about price. It also includes:

- liquidity availability
- settlement time
- asset eligibility and restrictions
- pre-trade risk thresholds
- counterparty constraints

## Recommended approach

Start with a simpler execution model and add strategy layers gradually:

1. Pool-based swaps
2. Limit orders and partial fills
3. Strategy routing
4. Automated market maker optimization
5. Portfolio-level execution logic
