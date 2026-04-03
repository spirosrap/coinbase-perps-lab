# coinbase-perps-lab

Small CCXT-based lab for inspecting Coinbase INTX perpetual positions from a local `.env`.

## What this repo does

- Creates an isolated local Python environment in `.venv`
- Loads Coinbase credentials from `.env`
- Uses `ccxt` to discover your INTX portfolio and list open perpetual positions
- Keeps secrets out of git with `.gitignore`

## Requirements

- Python 3.9+
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

## What the script does

1. Loads variables from `.env`
2. Connects with `ccxt.coinbase`
3. Fetches available portfolios
4. Selects the first `INTX` portfolio unless you pass `--portfolio`
5. Fetches open positions for that portfolio

## Security

- `.env` is ignored by git and should never be committed
- `.venv` is also ignored by git
- `.env.example` contains placeholders only
- Use the least-privileged Coinbase credentials available for your workflow

## Files

- `discover_perp_positions.py`: read-only Coinbase INTX position discovery
- `requirements.txt`: local Python dependencies
- `.env.example`: starter environment template
