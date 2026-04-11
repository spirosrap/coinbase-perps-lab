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

- `ALPHA_VANTAGE_API_KEY`
- `OPENAI_API_KEY`
- `GEMINI_API_KEY`

`ALPHA_VANTAGE_API_KEY` is optional for core position discovery, but it is used by the dashboard's equity-earnings proxy layer. The other optional keys are only kept for future experiments in the same workspace.

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
- an experimental multi-horizon directional model in the dashboard for `1h`, `4h`, and `next close`
- the model now persists a local Coinbase candle archive inside the dashboard history file and evaluates on that archive instead of only the short live fetch window

The dashboard shows the same snapshot in a browser-friendly layout and polls the local backend for refreshes.

The default history file now persists:

- recent raw dashboard samples
- 5-minute rollups
- a per-symbol local candle archive used by the experimental model

If you leave the dashboard running, it also builds a rolling in-memory microstructure history per symbol so you can compare:

- spread stability over time
- top-5 bid/ask depth imbalance over time
- buy and sell slippage for `$10k` and `$40k` quote notionals over time
- whether sweep costs are recovering after a thinner patch of liquidity

By default, that history is also persisted locally to `.local/perps_dashboard_history.json`, so it survives dashboard restarts.

The dashboard now keeps two time horizons:

- recent raw history: up to `240` high-resolution samples at the normal refresh interval
- long-horizon rollups: `5` minute buckets, retained for up to `14` days

The dashboard also adds a conservative setup layer:

- official Fed monetary-policy headlines from the Federal Reserve RSS feed
- scheduled macro events from the official FOMC calendar and the White House / OIRA principal economic indicators schedule
- current coverage includes FOMC, CPI, jobs, PCE, GDP, retail sales, and PPI
- Google News RSS headline coverage for a fixed geopolitics query, folded into a heuristic headline-risk overlay
- Alpha Vantage earnings-calendar coverage for a fixed large-cap proxy watchlist (`AAPL`, `MSFT`, `NVDA`, `AMZN`, `META`, `GOOGL`, `TSLA`) when the watched market is a US equity ETF/perp
- a combined risk level derived from scheduled events plus the headline-risk overlay
- a heuristic setup status and suggested max leverage per position
- live open-order visibility for current futures/perpetual orders
- stale reduce-only cleanup review when no matching position is open
- live stock-perp watch cards for all currently available Coinbase INTX equity and equity-ETF perpetuals
- a strict pass/fail long-entry gate for flat-mode watch cards
- a percentage-based entry sizing plan for flat-mode watch cards, including margin use, reserve, and actual leverage guidance
- an experimental multi-horizon directional baseline model that trains locally on `5` minute data and shows `1h`, `4h`, and `next close` forecasts, with walk-forward validation stats, model variant, class balance, and edge versus baseline
- explicit model-data readiness guidance showing collected local history, activation threshold, and longer-horizon trust thresholds

This setup layer is intentionally conservative and non-binding. It is context, not financial advice.

## Interpreting the Rust output

- `apiLev` is the raw leverage field returned by Coinbase's position endpoint
- `effectiveLev` is computed from `position_notional / collateral`, which is often the more useful risk number
- `basis` is the percentage difference between perp mark and index price
- `funding` is shown per funding interval, with a direction label to indicate which side is paying
- `funding intensity` classifies the size of the funding rate: `near zero`, `tiny`, `noticeable`, `elevated`, `large`, or `very large`
- `Entry Gate` is a conservative long re-entry checklist for flat-mode watch cards. It only flips to `ready` when all of its gates are passing.
- `Entry Sizing` converts the current watch state into a conservative allocation plan based on available INTX margin:
- `ready` uses `60%` of available margin, keeps `40%` in reserve, and allows up to `100%` of the current suggested max leverage
- `aligned` but not `ready` uses `40%`, keeps `60%` in reserve, and allows up to `75%` of the leverage cap
- `mixed` uses `25%`, keeps `75%` in reserve, and allows up to `50%` of the leverage cap
- `avoid aggression` uses `0%` and waits
- `Macro Risk` is now a combined context label:
- `scheduled risk` comes from FOMC plus the official White House / OIRA macro calendar, and may also include the earnings proxy schedule for US equity ETF/perp watches
- `headline risk` comes from a keyword-based geopolitical news scan and is heuristic by construction
- the dashboard uses the higher of those two layers as the combined risk label
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
- `setup status` combines event risk, execution costs, book skew, and heuristic market bias into a conservative status such as `aligned`, `mixed`, or `avoid aggression`
- `suggested max leverage` is a conservative cap derived from those same inputs and is meant as a risk-control prompt, not an instruction
- `Experimental Model` is a separate overlay, not part of the execution gate
- it now reports multiple horizons on `5` minute bars:
- `1h`
- `4h`
- `next close` (next U.S. regular cash close)
- when enough local persisted rollup history exists, it augments candle features with local microstructure, funding, basis, open-interest-notional, and market-context-risk features
- otherwise it falls back to a candle-only model and says so explicitly
- if the chronological holdout does not beat a naive baseline, the model neutralizes itself to `50/50` and reports `no_edge`
- `Variant` shows whether you are seeing the `history_augmented` or `candle_only` path
- `Edge vs Base` shows the holdout accuracy delta versus the naive majority-class baseline, in percentage points
- `Evaluation Method` is now an expanding walk-forward evaluation on non-overlapping holdout anchors
- `Holdout Anchors` shows how many independent out-of-sample anchors were actually scored
- `Independent Test Depth` converts those anchors to an approximate time span for fixed-minute horizons
- `Holdout Up` shows the fraction of holdout examples labeled up
- `Majority Side` shows which side the naive baseline would always predict
- `Balanced Acc` is the mean of up-side recall and down-side recall when both classes exist in the holdout set
- `MCC` is the Matthews correlation coefficient, which is more informative than plain accuracy under class imbalance
- `Raw Rollup History` shows how much persisted 5-minute rollup history the dashboard has for that symbol
- raw rollup history is retained archive depth; it is not the same thing as independent holdout depth
- `Augmented Model` shows whether the richer model is active yet or how much local history is still needed
- readiness thresholds are:
- activation at `120` rollup buckets, about `10` hours
- first review at `300` buckets, about `25` hours
- serious trust at `960` buckets, about `80` hours
- treat `P(up 1h)` as experimental context, not a reason by itself to override the risk gate

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
- long-horizon robustness comes from persisted `5` minute rollups, so you can compare current spread, imbalance, and `$40k` sweep costs against a broader baseline

Funding intensity thresholds in this tool are heuristic:

- `near zero`: under `0.0005%`
- `tiny`: up to `0.005%`
- `noticeable`: up to `0.02%`
- `elevated`: up to `0.05%`
- `large`: up to `0.10%`
- `very large`: above `0.10%`

The dashboard no longer depends on a single hardcoded watch symbol. It now pulls the current Coinbase INTX perpetual product list, filters it to stock-linked markets (`EQUITY` and `EQUITY_ETF`), and renders the full available stock-perp universe in the watch section. If you have an open position, the dashboard still renders the remaining stock-perp watch cards underneath the live position cards so the scan stays useful while you are in a trade.

For the full stock-perp scan, the first dashboard snapshot is heavier because it has to refresh multiple markets and warm local caches. After that, the server keeps a short in-memory snapshot cache so normal browser polling does not recompute the entire stock-perp universe on every refresh.

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
9. Maintains longer-horizon `5` minute rollups so the dashboard can compare current microstructure against a broader recent baseline
10. In dashboard mode, loads cached market context from Fed policy feeds, the White House / OIRA release schedule, a Google News geopolitics headline scan, and an Alpha Vantage equity-earnings proxy when relevant, then derives a conservative combined-risk and setup/leverage assessment
11. Pulls recent futures/perpetual orders and filters for active orders so the dashboard can show open orders and stale reduce-only cleanup candidates
12. When no position is open, keeps live watch-market snapshots for recently tracked symbols so the dashboard remains useful in flat mode
13. Builds an experimental local 1-hour directional model from Coinbase public candles and caches the prediction separately from the heuristic execution gate

The Rust binaries call Coinbase's REST API directly. They enrich the raw position snapshot with product metadata, portfolio summary data, and live product-book data so the output can show additional context without placing trades.

The dashboard uses the same Rust analysis path. Coinbase credentials stay in the local Rust process; the browser only receives the computed snapshot JSON from `http://127.0.0.1:3000/api/snapshot`.

## Architecture

- The Rust binary uses direct Coinbase REST calls with ES256 JWT authentication
- Product-book depth is pulled from Coinbase's public `market/product_book` endpoint with `cache-control: no-cache`
- Both Rust binaries are read-only and target the same INTX portfolio/positions workflow
- The analytics layer is shared between the CLI and dashboard
- The orders view uses Coinbase's recent orders endpoint and client-side filtering for active-like futures/perpetual orders
- The heuristic analytics are context, not a predictive trading model
- The dashboard is local-only by default, uses the same read-only Rust snapshot pipeline, stores rolling history in a local JSON file, and derives longer-horizon `5` minute rollups from those persisted samples

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
