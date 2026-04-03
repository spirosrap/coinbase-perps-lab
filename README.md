# coinbase-perps-lab

Scratch workspace for Coinbase perps and related trading experiments on this Mac.

## What is here

- `.env` with the trading-related variables copied from `dell-pc` plus local `OPENAI_API_KEY` and `GEMINI_API_KEY`
- `.venv` as the local Python environment for this workspace
- `discover_perp_positions.py` as a read-only CCXT probe for Coinbase INTX perp positions

## Environment setup

Create the virtual environment in this folder:

```bash
cd coinbase-perps-lab
python3 -m venv .venv
```

Activate it:

```bash
source .venv/bin/activate
```

Install the local dependencies:

```bash
python -m pip install -r requirements.txt
```

## Running the position discovery

Run the read-only position check:

```bash
.venv/bin/python discover_perp_positions.py
```

JSON output:

```bash
.venv/bin/python discover_perp_positions.py --json
```

Optional explicit portfolio UUID:

```bash
.venv/bin/python discover_perp_positions.py --portfolio YOUR_INTX_PORTFOLIO_UUID
```

## Notes

- Secrets live in `.env` and are intentionally ignored by git.
- The seed env came from `dell-pc`'s active `crypto-finance/.env`.
- The script prefers `API_KEY_PERPS` and `API_SECRET_PERPS`, then falls back to the other Coinbase key names if needed.
- The script only calls read-only portfolio and position endpoints.
