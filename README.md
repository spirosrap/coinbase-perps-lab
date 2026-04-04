# coinbase-perps-lab

Small Rust-based lab for inspecting Coinbase INTX perpetual positions and viewing them in a local dashboard.

## What this repo does

- Loads Coinbase credentials from `.env`
- Uses direct Coinbase REST calls from Rust
- Lists open INTX perpetual positions from a CLI
- Serves the same analytics in a local web dashboard
- Keeps secrets out of git with `.gitignore`

## Requirements

- Rust toolchain
- A Coinbase Advanced Trade / INTX account with perpetuals access
- Coinbase API credentials that can read portfolio and position data

## Quick start

Clone the repo and enter it:

```bash
git clone <your-repo-url>
cd coinbase-perps-lab
```

Install the standard Rust toolchain:

```bash
rustup default stable
```

If `cargo` is not on your `PATH` after installing with `rustup`, load it for the current shell:

```bash
. "$HOME/.cargo/env"
```

Create your local env file from the template:

```bash
cp .env.example .env
```

Then edit `.env` and add your real credentials.

## Environment variables

The script supports these Coinbase credential pairs:

- `API_KEY_PERPS` and `API_SECRET_PERPS`
- `COINBASE_API_KEY` and `COINBASE_API_SECRET`
- `API_KEY` and `API_SECRET`

It prefers `API_KEY_PERPS` and `API_SECRET_PERPS` first.

Optional extras:

- `OPENAI_API_KEY`
- `GEMINI_API_KEY`

Those optional keys are not required for position discovery, but the template leaves room for future experiments in the same workspace.

## Usage

Run the Rust version:

```bash
cargo run --bin discover_perp_positions_rust
```

Run the local web dashboard:

```bash
cargo run --bin perps_dashboard
```

Then open `http://127.0.0.1:3000` in your browser.

Rust JSON output:

```bash
cargo run --bin discover_perp_positions_rust -- --json
```

Rust with an explicit portfolio UUID:

```bash
cargo run --bin discover_perp_positions_rust -- --portfolio YOUR_INTX_PORTFOLIO_UUID
```

Dashboard with a custom bind address or explicit portfolio UUID:

```bash
cargo run --bin perps_dashboard -- --bind 127.0.0.1:3000 --portfolio YOUR_INTX_PORTFOLIO_UUID
```

Dashboard with an explicit persistent history file path:

```bash
cargo run --bin perps_dashboard -- --history-file .local/perps_dashboard_history.json
```

The Rust output now includes additional derived context per position:

- effective leverage from portfolio collateral, alongside the raw API leverage field
- mark vs index basis
- 24h price change
- funding rate and funding direction
- best bid, best ask, and top-of-book spread
- live order-book slippage estimates for preset execution sizes
- liquidation distance and liquidation buffer
- open interest and max leverage
- heuristic market bias and position outlook labels
- simple scenario projections for `+1%`, `+3%`, `-1%`, and `-3%` moves from the current mark

The dashboard shows the same snapshot in a browser-friendly layout and polls the local backend for refreshes.

If you leave the dashboard running, it also builds a rolling in-memory microstructure history per symbol so you can compare:

- spread stability over time
- top-5 bid/ask depth imbalance over time
- buy and sell slippage for `$10k` and `$40k` quote notionals over time
- whether sweep costs are recovering after a thinner patch of liquidity

By default, that history is also persisted locally to `.local/perps_dashboard_history.json`, so it survives dashboard restarts.

## Interpreting the Rust output

- `apiLev` is the raw leverage field returned by Coinbase's position endpoint
- `effectiveLev` is computed from `position_notional / collateral`, which is often the more useful risk number
- `basis` is the percentage difference between perp mark and index price
- `funding` is shown per funding interval, with a direction label to indicate which side is paying
- `funding intensity` classifies the size of the funding rate: `near zero`, `tiny`, `noticeable`, `elevated`, `large`, or `very large`
- `open interest` is the total number of open contracts in the market
- `open interest notional` converts that contract count to quote notional at the current mark
- `position share of open interest` shows how large your position is relative to the whole market
- `best bid` and `best ask` are the current top-of-book prices from Coinbase's product book
- `spread` is the current top-of-book gap between best bid and best ask, shown in absolute terms and basis points
- `top 5 bid depth` and `top 5 ask depth` convert the first five levels on each side into quote notional
- `top 5 imbalance` compares those top-five quote notionals as `(bid depth - ask depth) / total depth`
- `buy slip` and `sell slip` estimate market-order impact for preset quote notionals (`$5k`, `$10k`, `$20k`, `$40k`)
- slippage is measured against the current best ask for buys and the current best bid for sells, so it reflects incremental execution cost beyond the top level
- `liqDistance` is the percentage move from the current mark to the estimated liquidation price
- `market bias` and `position outlook` are heuristic labels derived from 24h price change, basis, funding, entry distance, and liquidation distance
- `Projections` are simple mark-to-market PnL scenarios, not forecasts

Example:

```text
Projections: +1%=3.07 | +3%=9.20 | -1%=-3.07 | -3%=-9.20
```

This means: if the current mark moves up `1%`, the position's unrealized PnL would increase by about `3.07` quote units; if it moves down `3%`, unrealized PnL would decrease by about `9.20`. These projections do not include fees, funding, slippage, or execution effects.

Execution estimates are based on the current Coinbase product-book ladder. Example:

```text
Execution: bestBid=652.81 | bestAsk=652.88 | spread=0.0700 (1.07 bps) | bookLevels=39/44
Buy slip: $5k 0.00bps @652.88 | $10k 4.22bps @653.16 | $20k 14.78bps @653.84 | $40k 31.82bps @654.96
Sell slip: $5k 2.72bps @652.63 | $10k 4.83bps @652.49 | $20k 6.84bps @652.36 | $40k 8.78bps @652.24
```

This means:

- the current top-of-book spread is about `1.07 bps`
- buying `$5k` would fully fill at the best ask in this snapshot
- buying larger size walks up the ask ladder, so average execution price gets worse
- selling larger size walks down the bid ladder, so average execution price gets worse
- these are snapshot estimates, not guarantees; they can change before execution

The dashboard history panels build from these same live snapshots. They are intentionally local and rolling:

- history starts when you open the dashboard, or resumes from the local history file if one already exists
- history is kept in memory by the Rust server and also written to a local JSON file
- history is bounded to a recent rolling window, not a permanent time series database
- if you restart the dashboard, history resumes from the last saved local state file
- if Coinbase returns the same book timestamp repeatedly, the server treats that as the same sample and updates it in place
- the default persistence path is `.local/perps_dashboard_history.json`, and you can override it with `--history-file`

Funding intensity thresholds in this tool are heuristic:

- `near zero`: under `0.0005%`
- `tiny`: up to `0.005%`
- `noticeable`: up to `0.02%`
- `elevated`: up to `0.05%`
- `large`: up to `0.10%`
- `very large`: above `0.10%`

Open interest is intentionally kept more factual than interpretive. A single snapshot can tell you:

- how many contracts are open
- roughly how much quote notional that represents at the current mark
- what share of that open interest your position represents

Trend-style interpretations such as "build" or "unwind" require history, not one snapshot.

## What the tool does

1. Loads variables from `.env`
2. Authenticates with Coinbase using ES256 JWTs
3. Fetches available portfolios
4. Selects the first `INTX` portfolio unless you pass `--portfolio`
5. Fetches open positions, product metadata, portfolio summary data, and the live product-book ladder
6. Computes derived analytics and renders them in either CLI or dashboard form
7. In dashboard mode, keeps a bounded rolling history of spread, top-5 imbalance, and selected slippage metrics
8. Persists that dashboard history to a local JSON file so it survives restarts

The Rust binaries call Coinbase's REST API directly. They enrich the raw position snapshot with product metadata, portfolio summary data, and live product-book data so the output can show additional context without placing trades.

The dashboard uses the same Rust analysis path. Coinbase credentials stay in the local Rust process; the browser only receives the computed snapshot JSON from `http://127.0.0.1:3000/api/snapshot`.

## Architecture

- The Rust binary uses direct Coinbase REST calls with ES256 JWT authentication
- Product-book depth is pulled from Coinbase's public `market/product_book` endpoint with `cache-control: no-cache`
- Both Rust binaries are read-only and target the same INTX portfolio/positions workflow
- The analytics layer is shared between the CLI and dashboard
- The heuristic analytics are context, not a predictive trading model
- The dashboard is local-only by default, uses the same read-only Rust snapshot pipeline, and stores rolling history in a local JSON file

## Security

- `.env` is ignored by git and should never be committed
- `target/` is ignored by git
- `.local/` is ignored by git and stores local dashboard state such as persistent microstructure history
- `.env.example` contains placeholders only
- Use the least-privileged Coinbase credentials available for your workflow

## Files

- `src/bin/discover_perp_positions_rust.rs`: Rust CLI for live INTX analytics
- `src/bin/perps_dashboard.rs`: local web dashboard for the same Rust analytics
- `src/lib.rs`: shared Coinbase snapshot and analytics logic used by both Rust binaries
- `Cargo.toml`: Rust dependencies and binary definition
- `.env.example`: starter environment template
