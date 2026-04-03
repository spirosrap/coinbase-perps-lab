# AGENTS.md

## Project purpose

This repository is a small CCXT-based lab for Coinbase INTX perpetuals.

Current scope:
- create and use a local Python environment
- load credentials from `.env`
- inspect Coinbase INTX portfolios and open perpetual positions
- support both the Python and Rust read-only discovery paths

Default assumption:
- keep work read-only unless the user explicitly asks for trading, order placement, or account mutations

## Environment

Use the local virtual environment in this repo:

```bash
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -r requirements.txt
```

Prefer running commands through the repo-local interpreter:

```bash
.venv/bin/python discover_perp_positions.py
```

For Rust work, use the repo-local Cargo project:

```bash
cargo run --bin discover_perp_positions_rust
```

## Secrets

- Never commit `.env`
- Never print or paste secret values into commits, docs, logs, or issue text
- Use `.env.example` for templates and placeholders only
- If docs need environment-variable examples, use fake values only

Supported Coinbase credential pairs:
- `API_KEY_PERPS` and `API_SECRET_PERPS`
- `COINBASE_API_KEY` and `COINBASE_API_SECRET`
- `API_KEY` and `API_SECRET`

The current discovery script prefers the perps pair first.

## Code guidelines

- Target Python 3.9 compatibility unless the user explicitly upgrades the project runtime
- Keep Rust code compatible with the checked-in Cargo manifest and stable toolchain
- Keep dependencies minimal and project-local
- Prefer small, focused scripts over large frameworks
- Preserve the repo's read-only posture by default
- If you add a mutating script later, make the destructive or trading behavior explicit in the filename and README

## Coinbase and CCXT notes

- Use `ccxt.coinbase` for the current INTX workflow in this repo
- Use the direct Rust client for Rust work; do not assume CCXT has official Rust support
- Fetch portfolios first, then select the `INTX` portfolio before querying positions
- Prefer returning concise summaries plus optional JSON output for automation
- When behavior depends on Coinbase or CCXT specifics, verify against current upstream docs or installed library behavior

## Documentation rules

- Keep README examples public-safe and GitHub-friendly
- Use relative paths in documentation, not absolute home-directory paths
- Do not mention machine-specific usernames, private hosts, or local workstation details in public docs
- Update `README.md` when setup, commands, or expected environment variables change

## Git rules

- This repo is intended to be public-safe
- Stage only safe files
- Confirm `.env` and `.venv` remain ignored before committing
- Confirm `target/` remains ignored before committing
- Keep commits scoped and descriptive

## Validation

For changes that affect the current workflow, run:

```bash
.venv/bin/python discover_perp_positions.py
```

If output format changes, also test:

```bash
.venv/bin/python discover_perp_positions.py --json
```

For Rust changes, also run:

```bash
cargo run --bin discover_perp_positions_rust
cargo run --bin discover_perp_positions_rust -- --json
```
