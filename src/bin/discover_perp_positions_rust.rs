use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use clap::Parser;
use dotenvy::dotenv;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::SecretKey;
use rand::RngCore;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

const API_HOST: &str = "api.coinbase.com";
const API_BASE: &str = "https://api.coinbase.com";

#[derive(Parser, Debug)]
#[command(about = "Discover open Coinbase INTX perpetual positions without CCXT.")]
struct Args {
    #[arg(long, help = "Optional explicit INTX portfolio UUID")]
    portfolio: Option<String>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Clone)]
struct Credentials {
    api_key: String,
    api_secret: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct PortfoliosResponse {
    portfolios: Vec<Portfolio>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Portfolio {
    name: Option<String>,
    uuid: String,
    #[serde(rename = "type")]
    portfolio_type: String,
    deleted: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct Money {
    value: String,
}

#[derive(Debug, Deserialize)]
struct PositionsResponse {
    positions: Vec<RawPosition>,
}

#[derive(Debug, Deserialize)]
struct RawPosition {
    symbol: String,
    position_side: Option<String>,
    margin_type: Option<String>,
    net_size: Option<String>,
    leverage: Option<String>,
    mark_price: Option<Money>,
    unrealized_pnl: Option<Money>,
    liquidation_price: Option<Money>,
    position_notional: Option<Money>,
    entry_vwap: Option<Money>,
}

#[derive(Debug, Serialize)]
struct PositionSummary {
    symbol: String,
    side: Option<String>,
    contracts: Option<String>,
    notional: Option<String>,
    entry_price: Option<String>,
    mark_price: Option<String>,
    unrealized_pnl: Option<String>,
    liquidation_price: Option<String>,
    leverage: Option<String>,
    margin_mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct Output {
    credential_source: String,
    portfolio: PortfolioSummary,
    portfolio_count: usize,
    positions: Vec<PositionSummary>,
}

#[derive(Debug, Serialize)]
struct PortfolioSummary {
    id: String,
    portfolio_type: String,
}

fn now_unix() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before unix epoch")?
        .as_secs())
}

fn random_hex_16() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn encode_segment(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn normalize_private_key(raw: &str) -> String {
    raw.replace("\\n", "\n").trim().to_string()
}

fn get_credentials() -> Result<Credentials> {
    let pairs = [
        ("API_KEY_PERPS", "API_SECRET_PERPS"),
        ("COINBASE_API_KEY", "COINBASE_API_SECRET"),
        ("API_KEY", "API_SECRET"),
    ];
    for (key_name, secret_name) in pairs {
        if let (Ok(api_key), Ok(api_secret)) = (env::var(key_name), env::var(secret_name)) {
            if !api_key.is_empty() && !api_secret.is_empty() {
                return Ok(Credentials {
                    api_key,
                    api_secret,
                    source: key_name.to_string(),
                });
            }
        }
    }
    bail!(
        "No Coinbase credential pair found. Expected API_KEY_PERPS/API_SECRET_PERPS, \
COINBASE_API_KEY/COINBASE_API_SECRET, or API_KEY/API_SECRET."
    );
}

fn build_jwt(api_key: &str, api_secret: &str, method: &str, path: &str) -> Result<String> {
    let issued_at = now_unix()?;
    let uri = format!("{method} {API_HOST}{path}");
    let nonce = random_hex_16();

    let header = json!({
        "typ": "JWT",
        "alg": "ES256",
        "kid": api_key,
        "nonce": nonce,
    });
    let claims = json!({
        "sub": api_key,
        "iss": "cdp",
        "nbf": issued_at,
        "exp": issued_at + 120,
        "uri": uri,
    });

    let encoded_header = encode_segment(&header)?;
    let encoded_claims = encode_segment(&claims)?;
    let signing_input = format!("{encoded_header}.{encoded_claims}");

    let pem = normalize_private_key(api_secret);
    let secret_key = SecretKey::from_sec1_pem(&pem).context("failed to parse ES256 private key")?;
    let signing_key = SigningKey::from(secret_key);
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    let encoded_signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("{signing_input}.{encoded_signature}"))
}

fn build_client() -> Result<Client> {
    Client::builder()
        .build()
        .context("failed to build HTTP client")
}

fn get_json<T>(client: &Client, credentials: &Credentials, path: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let token = build_jwt(&credentials.api_key, &credentials.api_secret, "GET", path)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .context("failed to build authorization header")?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let response = client
        .get(format!("{API_BASE}{path}"))
        .headers(headers)
        .send()
        .with_context(|| format!("request failed for GET {path}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("Coinbase returned {status} for GET {path}: {body}");
    }

    response
        .json::<T>()
        .with_context(|| format!("failed to decode Coinbase JSON for GET {path}"))
}

fn fetch_portfolios(client: &Client, credentials: &Credentials) -> Result<Vec<Portfolio>> {
    let response: PortfoliosResponse = get_json(client, credentials, "/api/v3/brokerage/portfolios")?;
    Ok(response.portfolios)
}

fn select_portfolio(portfolios: &[Portfolio], requested: Option<&str>) -> Result<Portfolio> {
    if let Some(requested_id) = requested {
        return portfolios
            .iter()
            .find(|portfolio| portfolio.uuid == requested_id)
            .cloned()
            .ok_or_else(|| anyhow!("portfolio \"{requested_id}\" was not found"));
    }

    portfolios
        .iter()
        .find(|portfolio| portfolio.portfolio_type.eq_ignore_ascii_case("INTX"))
        .cloned()
        .ok_or_else(|| anyhow!("no INTX portfolio was found for these credentials"))
}

fn normalize_side(side: Option<&str>) -> Option<String> {
    match side {
        Some("POSITION_SIDE_LONG") => Some("long".to_string()),
        Some("POSITION_SIDE_SHORT") => Some("short".to_string()),
        Some("POSITION_SIDE_UNKNOWN") => Some("unknown".to_string()),
        Some(other) if !other.is_empty() => Some(other.to_string()),
        _ => None,
    }
}

fn normalize_margin_mode(mode: Option<&str>) -> Option<String> {
    match mode {
        Some("MARGIN_TYPE_ISOLATED") => Some("isolated".to_string()),
        Some("MARGIN_TYPE_CROSS") => Some("cross".to_string()),
        Some("MARGIN_TYPE_UNSPECIFIED") => Some("unspecified".to_string()),
        Some(other) if !other.is_empty() => Some(other.to_string()),
        _ => None,
    }
}

fn money_value(money: Option<&Money>) -> Option<String> {
    money.map(|item| item.value.clone())
}

fn summarize_position(position: RawPosition) -> PositionSummary {
    PositionSummary {
        symbol: position.symbol,
        side: normalize_side(position.position_side.as_deref()),
        contracts: position.net_size,
        notional: money_value(position.position_notional.as_ref()),
        entry_price: money_value(position.entry_vwap.as_ref()),
        mark_price: money_value(position.mark_price.as_ref()),
        unrealized_pnl: money_value(position.unrealized_pnl.as_ref()),
        liquidation_price: money_value(position.liquidation_price.as_ref()),
        leverage: position.leverage,
        margin_mode: normalize_margin_mode(position.margin_type.as_deref()),
    }
}

fn fetch_positions(
    client: &Client,
    credentials: &Credentials,
    portfolio_id: &str,
) -> Result<Vec<PositionSummary>> {
    let path = format!("/api/v3/brokerage/intx/positions/{portfolio_id}");
    let response: PositionsResponse = get_json(client, credentials, &path)?;
    Ok(response
        .positions
        .into_iter()
        .map(summarize_position)
        .collect())
}

fn main() -> Result<()> {
    let _ = dotenv();
    let args = Args::parse();
    let credentials = get_credentials()?;
    let client = build_client()?;
    let portfolios = fetch_portfolios(&client, &credentials)?;
    let portfolio = select_portfolio(&portfolios, args.portfolio.as_deref())?;
    let positions = fetch_positions(&client, &credentials, &portfolio.uuid)?;

    let output = Output {
        credential_source: credentials.source,
        portfolio: PortfolioSummary {
            id: portfolio.uuid.clone(),
            portfolio_type: portfolio.portfolio_type.clone(),
        },
        portfolio_count: portfolios.len(),
        positions,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("Credential source: {}", output.credential_source);
    println!(
        "Portfolio: {} ({})",
        output.portfolio.id, output.portfolio.portfolio_type
    );
    println!("Portfolio count: {}", output.portfolio_count);
    println!("Open positions: {}", output.positions.len());

    if output.positions.is_empty() {
        println!("No open perp positions found.");
        return Ok(());
    }

    for (index, position) in output.positions.iter().enumerate() {
        println!(
            "{}. {} | {} | contracts={} | notional={} | entry={} | liq={} | leverage={} | marginMode={}",
            index + 1,
            position.symbol,
            position.side.as_deref().unwrap_or("unknown"),
            position.contracts.as_deref().unwrap_or("unknown"),
            position.notional.as_deref().unwrap_or("unknown"),
            position.entry_price.as_deref().unwrap_or("unknown"),
            position
                .liquidation_price
                .as_deref()
                .unwrap_or("unknown"),
            position.leverage.as_deref().unwrap_or("unknown"),
            position.margin_mode.as_deref().unwrap_or("unknown"),
        );
    }

    Ok(())
}
