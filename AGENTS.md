# AGENTS.md

## Project purpose

This repository is a small Rust-based lab for Coinbase INTX perpetuals.

Current scope:
- load credentials from `.env`
- inspect Coinbase INTX portfolios and open perpetual positions
- support a Rust CLI and a local Rust dashboard for the same read-only analytics path

Default assumption:
- keep work read-only unless the user explicitly asks for trading, order placement, or account mutations

## Environment

For Rust work, use the repo-local Cargo project:

```bash
cargo run --bin discover_perp_positions_rust
cargo run --bin perps_dashboard
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

- Keep Rust code compatible with the checked-in Cargo manifest and stable toolchain
- Keep dependencies minimal and project-local
- Prefer small, focused scripts over large frameworks
- Preserve the repo's read-only posture by default
- If you add a mutating script later, make the destructive or trading behavior explicit in the filename and README

## Coinbase notes

- Use the direct Rust client for Coinbase work in this repo
- Fetch portfolios first, then select the `INTX` portfolio before querying positions
- Prefer returning concise summaries plus optional JSON output for automation
- When behavior depends on Coinbase specifics, verify against current upstream docs or observed API behavior
- For execution sizing, treat leverage as a whole-number control in this workflow; do not target fractional leverage such as `1.5x` or `2.5x`
- When raw sizing math lands between integer leverage steps, prefer the higher whole-number leverage if it stays within the dashboard cap, then adjust margin allocation to preserve target notional

## Documentation rules

- Keep README examples public-safe and GitHub-friendly
- Use relative paths in documentation, not absolute home-directory paths
- Do not mention machine-specific usernames, private hosts, or local workstation details in public docs
- Update `README.md` when setup, commands, or expected environment variables change

## Git rules

- This repo is intended to be public-safe
- Stage only safe files
- Confirm `.env` remains ignored before committing
- Confirm `target/` remains ignored before committing
- Keep commits scoped and descriptive

## Validation

For changes that affect the current workflow, run:

For Rust changes, run:

```bash
cargo run --bin discover_perp_positions_rust
cargo run --bin discover_perp_positions_rust -- --json
cargo run --bin perps_dashboard
```
