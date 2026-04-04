# coinbase-perps-lab

Small CCXT-based lab for inspecting Coinbase INTX perpetual positions from a local `.env`.

## What this repo does

- Creates an isolated local Python environment in `.venv`
- Loads Coinbase credentials from `.env`
- Uses `ccxt` to discover your INTX portfolio and list open perpetual positions
- Includes a direct Rust implementation for the same read-only workflow plus derived market/risk analytics
- Keeps secrets out of git with `.gitignore`

## Requirements

- Python 3.9+
- Rust toolchain if you want to use the Rust binary
- A Coinbase Advanced Trade / INTX account with perpetuals access
- Coinbase API credentials that can read portfolio and position data

## Quick start

Clone the repo and enter it:

```bash
git clone <your-repo-url>
cd coinbase-perps-lab
```

Create a local virtual environment:

```bash
python3 -m venv .venv
```

Activate it:

```bash
source .venv/bin/activate
```

Install dependencies:

```bash
python -m pip install -r requirements.txt
```

If you want the Rust version too, install the standard Rust toolchain:

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

Run the read-only position check:

```bash
.venv/bin/python discover_perp_positions.py
```

JSON output:

```bash
.venv/bin/python discover_perp_positions.py --json
```

Use an explicit portfolio UUID:

```bash
.venv/bin/python discover_perp_positions.py --portfolio YOUR_INTX_PORTFOLIO_UUID
```

Run the Rust version:

```bash
cargo run --bin discover_perp_positions_rust
```

Rust JSON output:

```bash
cargo run --bin discover_perp_positions_rust -- --json
```

Rust with an explicit portfolio UUID:

```bash
cargo run --bin discover_perp_positions_rust -- --portfolio YOUR_INTX_PORTFOLIO_UUID
```

The Rust output now includes additional derived context per position:

- effective leverage from portfolio collateral, alongside the raw API leverage field
- mark vs index basis
- 24h price change
- funding rate and funding direction
- liquidation distance and liquidation buffer
- open interest and max leverage
- heuristic market bias and position outlook labels
- simple scenario projections for `+1%`, `+3%`, `-1%`, and `-3%` moves from the current mark

## Interpreting the Rust output

- `apiLev` is the raw leverage field returned by Coinbase's position endpoint
- `effectiveLev` is computed from `position_notional / collateral`, which is often the more useful risk number
- `basis` is the percentage difference between perp mark and index price
- `funding` is shown per funding interval, with a direction label to indicate which side is paying
- `liqDistance` is the percentage move from the current mark to the estimated liquidation price
- `market bias` and `position outlook` are heuristic labels derived from 24h price change, basis, funding, entry distance, and liquidation distance
- `Projections` are simple mark-to-market PnL scenarios, not forecasts

Example:

```text
Projections: +1%=3.07 | +3%=9.20 | -1%=-3.07 | -3%=-9.20
```

This means: if the current mark moves up `1%`, the position's unrealized PnL would increase by about `3.07` quote units; if it moves down `3%`, unrealized PnL would decrease by about `9.20`. These projections do not include fees, funding, slippage, or execution effects.

## What the script does

1. Loads variables from `.env`
2. Connects with `ccxt.coinbase`
3. Fetches available portfolios
4. Selects the first `INTX` portfolio unless you pass `--portfolio`
5. Fetches open positions for that portfolio

The Rust binary follows the same flow, but it calls Coinbase's REST API directly instead of using CCXT. It also enriches the raw position snapshot with product metadata and portfolio summary data so the output can show additional context without placing trades.

## Python vs Rust

- The Python script uses official CCXT support for Coinbase
- Official CCXT does not currently ship a Rust implementation
- The Rust binary uses direct Coinbase REST calls with ES256 JWT authentication
- Both tools are read-only and target the same INTX portfolio/positions workflow
- The Rust tool adds heuristic analytics for context, but it is not a predictive trading model

## Security

- `.env` is ignored by git and should never be committed
- `.venv` is also ignored by git
- `target/` is ignored by git
- `.env.example` contains placeholders only
- Use the least-privileged Coinbase credentials available for your workflow

## Files

- `discover_perp_positions.py`: read-only Coinbase INTX position discovery
- `src/bin/discover_perp_positions_rust.rs`: direct Rust version of the same workflow
- `Cargo.toml`: Rust dependencies and binary definition
- `requirements.txt`: local Python dependencies
- `.env.example`: starter environment template
