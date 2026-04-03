#!/usr/bin/env python3

import argparse
import json
import os
import warnings
from pathlib import Path
from typing import Optional, Tuple

warnings.filterwarnings(
    "ignore",
    message="urllib3 v2 only supports OpenSSL 1.1.1+",
)

from dotenv import load_dotenv

import ccxt


ROOT = Path(__file__).resolve().parent
ENV_PATH = ROOT / ".env"


def load_local_env() -> None:
    if ENV_PATH.exists():
        load_dotenv(ENV_PATH, override=False)


def get_credentials() -> Tuple[str, str, str]:
    pairs = [
        ("API_KEY_PERPS", "API_SECRET_PERPS"),
        ("COINBASE_API_KEY", "COINBASE_API_SECRET"),
        ("API_KEY", "API_SECRET"),
    ]
    for key_name, secret_name in pairs:
        api_key = os.getenv(key_name)
        secret = os.getenv(secret_name)
        if api_key and secret:
            return api_key, secret, key_name
    raise SystemExit(
        "No Coinbase credential pair found. Expected one of: "
        "API_KEY_PERPS/API_SECRET_PERPS, "
        "COINBASE_API_KEY/COINBASE_API_SECRET, "
        "API_KEY/API_SECRET."
    )


def build_exchange():
    api_key, secret, key_name = get_credentials()
    exchange = ccxt.coinbase(
        {
            "apiKey": api_key,
            "secret": secret,
            "enableRateLimit": True,
            "timeout": 20000,
        }
    )
    return exchange, key_name


def choose_portfolio(exchange, requested_portfolio: Optional[str]):
    portfolios = exchange.fetch_portfolios()
    if requested_portfolio:
        for portfolio in portfolios:
            if portfolio.get("id") == requested_portfolio:
                return portfolio, portfolios
        raise SystemExit(f'Portfolio "{requested_portfolio}" was not found.')
    for portfolio in portfolios:
        if (portfolio.get("type") or "").upper() == "INTX":
            return portfolio, portfolios
    raise SystemExit("No INTX portfolio was found for these credentials.")


def summarize_position(position: dict) -> dict:
    return {
        "symbol": position.get("symbol"),
        "side": position.get("side"),
        "contracts": position.get("contracts"),
        "contractSize": position.get("contractSize"),
        "notional": position.get("notional"),
        "entryPrice": position.get("entryPrice"),
        "markPrice": position.get("markPrice"),
        "unrealizedPnl": position.get("unrealizedPnl"),
        "liquidationPrice": position.get("liquidationPrice"),
        "marginMode": position.get("marginMode"),
        "collateral": position.get("collateral"),
        "leverage": position.get("leverage"),
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Discover open Coinbase INTX perpetual positions with CCXT."
    )
    parser.add_argument(
        "--portfolio",
        help="Optional explicit portfolio UUID. Defaults to the first INTX portfolio.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print machine-readable JSON instead of a text summary.",
    )
    args = parser.parse_args()

    load_local_env()
    exchange, key_name = build_exchange()
    portfolio, portfolios = choose_portfolio(exchange, args.portfolio)
    positions = exchange.fetch_positions(params={"portfolio": portfolio["id"]})
    summaries = [summarize_position(position) for position in positions]

    payload = {
        "credentialSource": key_name,
        "portfolio": {
            "id": portfolio.get("id"),
            "type": portfolio.get("type"),
        },
        "portfolioCount": len(portfolios),
        "positions": summaries,
    }

    if args.json:
        print(json.dumps(payload, indent=2, default=str))
        return 0

    print(f'Credential source: {key_name}')
    print(f'Portfolio: {portfolio.get("id")} ({portfolio.get("type")})')
    print(f"Portfolio count: {len(portfolios)}")
    print(f"Open positions: {len(summaries)}")
    if not summaries:
        print("No open perp positions found.")
        return 0
    for index, position in enumerate(summaries, start=1):
        print(
            f'{index}. {position["symbol"]} | {position["side"]} | '
            f'contracts={position["contracts"]} | notional={position["notional"]} | '
            f'entry={position["entryPrice"]} | liq={position["liquidationPrice"]} | '
            f'leverage={position["leverage"]} | marginMode={position["marginMode"]}'
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
