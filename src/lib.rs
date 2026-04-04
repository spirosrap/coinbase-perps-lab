use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use dotenvy::dotenv;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::SecretKey;
use rand::RngCore;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

const API_HOST: &str = "api.coinbase.com";
const API_BASE: &str = "https://api.coinbase.com";
pub const ANALYSIS_BASIS: &str =
    "Heuristic snapshot derived from Coinbase position, product, and portfolio summary endpoints. Not a predictive model.";

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
pub struct Portfolio {
    pub name: Option<String>,
    pub uuid: String,
    #[serde(rename = "type")]
    pub portfolio_type: String,
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Money {
    pub value: String,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PositionsResponse {
    positions: Vec<RawPosition>,
}

#[derive(Debug, Deserialize)]
struct RawPosition {
    portfolio_uuid: Option<String>,
    symbol: String,
    vwap: Option<Money>,
    entry_vwap: Option<Money>,
    mark_price: Option<Money>,
    unrealized_pnl: Option<Money>,
    aggregated_pnl: Option<Money>,
    liquidation_price: Option<Money>,
    position_notional: Option<Money>,
    position_side: Option<String>,
    margin_type: Option<String>,
    net_size: Option<String>,
    leverage: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProductResponse {
    #[serde(default)]
    price: Option<String>,
    #[serde(default)]
    price_percentage_change_24h: Option<String>,
    #[serde(default)]
    future_product_details: Option<FutureProductDetails>,
}

#[derive(Debug, Deserialize)]
struct FutureProductDetails {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    contract_display_name: Option<String>,
    #[serde(default)]
    index_price: Option<String>,
    #[serde(default)]
    funding_rate: Option<String>,
    #[serde(default)]
    open_interest: Option<String>,
    #[serde(default)]
    max_leverage: Option<String>,
    #[serde(default)]
    perpetual_details: Option<PerpetualDetails>,
}

#[derive(Debug, Deserialize)]
struct PerpetualDetails {
    #[serde(default)]
    open_interest: Option<String>,
    #[serde(default)]
    funding_rate: Option<String>,
    #[serde(default)]
    max_leverage: Option<String>,
    #[serde(default)]
    underlying_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IntxPortfolioSummaryResponse {
    portfolios: Vec<IntxPortfolioState>,
}

#[derive(Debug, Deserialize, Clone)]
struct IntxPortfolioState {
    portfolio_uuid: String,
    collateral: String,
    position_notional: String,
    pending_fees: String,
    portfolio_initial_margin: String,
    portfolio_maintenance_margin: String,
    liquidation_buffer: String,
    total_balance: Money,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub credential_source: String,
    pub portfolio: PortfolioSummary,
    pub portfolio_count: usize,
    pub analysis_basis: &'static str,
    pub positions: Vec<PositionSummary>,
}

#[derive(Debug, Serialize)]
pub struct PortfolioSummary {
    pub id: String,
    pub portfolio_type: String,
}

#[derive(Debug, Serialize)]
pub struct PositionSummary {
    pub symbol: String,
    pub display_name: Option<String>,
    pub underlying_type: Option<String>,
    pub side: Option<String>,
    pub contracts: Option<String>,
    pub notional: Option<String>,
    pub entry_price: Option<String>,
    pub mark_price: Option<String>,
    pub index_price: Option<String>,
    pub vwap_price: Option<String>,
    pub unrealized_pnl: Option<String>,
    pub aggregated_pnl: Option<String>,
    pub liquidation_price: Option<String>,
    pub api_leverage: Option<String>,
    pub effective_leverage: Option<f64>,
    pub max_leverage: Option<String>,
    pub margin_mode: Option<String>,
    pub collateral: Option<String>,
    pub total_balance: Option<String>,
    pub pending_fees: Option<String>,
    pub liquidation_buffer: Option<String>,
    pub initial_margin_rate: Option<f64>,
    pub maintenance_margin_rate: Option<f64>,
    pub price_vs_entry_pct: Option<f64>,
    pub price_change_24h_pct: Option<f64>,
    pub basis_pct: Option<f64>,
    pub funding_rate_pct: Option<f64>,
    pub funding_direction: Option<String>,
    pub funding_intensity: Option<String>,
    pub open_interest: Option<String>,
    pub open_interest_notional: Option<f64>,
    pub position_share_of_open_interest_pct: Option<f64>,
    pub open_interest_context: Option<String>,
    pub distance_to_liquidation_pct: Option<f64>,
    pub market_bias: String,
    pub position_outlook: String,
    pub outlook_confidence: String,
    pub signals: Vec<String>,
    pub projections: ProjectionSummary,
}

#[derive(Debug, Serialize)]
pub struct ProjectionSummary {
    pub up_1pct_pnl: Option<f64>,
    pub up_3pct_pnl: Option<f64>,
    pub down_1pct_pnl: Option<f64>,
    pub down_3pct_pnl: Option<f64>,
}

#[derive(Debug)]
struct DerivedAnalytics {
    effective_leverage: Option<f64>,
    initial_margin_rate: Option<f64>,
    maintenance_margin_rate: Option<f64>,
    price_vs_entry_pct: Option<f64>,
    basis_pct: Option<f64>,
    funding_rate_pct: Option<f64>,
    funding_direction: Option<String>,
    funding_intensity: Option<String>,
    open_interest_notional: Option<f64>,
    position_share_of_open_interest_pct: Option<f64>,
    open_interest_context: Option<String>,
    distance_to_liquidation_pct: Option<f64>,
    market_bias: String,
    position_outlook: String,
    outlook_confidence: String,
    signals: Vec<String>,
    projections: ProjectionSummary,
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
    let secret_key =
        SecretKey::from_sec1_pem(&pem).context("failed to parse ES256 private key")?;
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
    let response: PortfoliosResponse =
        get_json(client, credentials, "/api/v3/brokerage/portfolios")?;
    Ok(response.portfolios)
}

fn fetch_positions(
    client: &Client,
    credentials: &Credentials,
    portfolio_id: &str,
) -> Result<Vec<RawPosition>> {
    let path = format!("/api/v3/brokerage/intx/positions/{portfolio_id}");
    let response: PositionsResponse = get_json(client, credentials, &path)?;
    Ok(response.positions)
}

fn fetch_product(
    client: &Client,
    credentials: &Credentials,
    symbol: &str,
) -> Result<ProductResponse> {
    let path = format!("/api/v3/brokerage/products/{symbol}");
    get_json(client, credentials, &path)
}

fn fetch_portfolio_summary(
    client: &Client,
    credentials: &Credentials,
    portfolio_id: &str,
) -> Result<Vec<IntxPortfolioState>> {
    let path = format!("/api/v3/brokerage/intx/portfolio/{portfolio_id}");
    let response: IntxPortfolioSummaryResponse = get_json(client, credentials, &path)?;
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

fn parse_f64(value: Option<&str>) -> Option<f64> {
    value.and_then(|item| item.parse::<f64>().ok())
}

pub fn format_opt(value: Option<f64>, decimals: usize) -> Option<String> {
    value.map(|item| format!("{item:.decimals$}"))
}

pub fn format_pct(value: Option<f64>) -> Option<String> {
    format_opt(value, 2).map(|item| format!("{item}%"))
}

fn product_display_name(product: &ProductResponse) -> Option<String> {
    product
        .future_product_details
        .as_ref()
        .and_then(|details| {
            details
                .display_name
                .clone()
                .or_else(|| details.contract_display_name.clone())
        })
}

fn product_underlying_type(product: &ProductResponse) -> Option<String> {
    product
        .future_product_details
        .as_ref()
        .and_then(|details| details.perpetual_details.as_ref())
        .and_then(|details| details.underlying_type.clone())
}

fn product_index_price(product: &ProductResponse) -> Option<f64> {
    product.future_product_details.as_ref().and_then(|details| {
        parse_f64(details.index_price.as_deref())
            .or_else(|| parse_f64(product.price.as_deref()))
    })
}

fn product_funding_rate(product: &ProductResponse) -> Option<f64> {
    product.future_product_details.as_ref().and_then(|details| {
        details
            .perpetual_details
            .as_ref()
            .and_then(|perps| parse_f64(perps.funding_rate.as_deref()))
            .or_else(|| parse_f64(details.funding_rate.as_deref()))
    })
}

fn product_open_interest(product: &ProductResponse) -> Option<f64> {
    product.future_product_details.as_ref().and_then(|details| {
        details
            .perpetual_details
            .as_ref()
            .and_then(|perps| parse_f64(perps.open_interest.as_deref()))
            .or_else(|| parse_f64(details.open_interest.as_deref()))
    })
}

fn product_max_leverage(product: &ProductResponse) -> Option<String> {
    product.future_product_details.as_ref().and_then(|details| {
        details
            .perpetual_details
            .as_ref()
            .and_then(|perps| perps.max_leverage.clone())
            .or_else(|| details.max_leverage.clone())
    })
}

fn compute_market_bias(
    price_change_24h_pct: Option<f64>,
    basis_pct: Option<f64>,
    funding_rate_pct: Option<f64>,
    funding_intensity: Option<&str>,
) -> (String, usize, i32, Vec<String>) {
    let mut score = 0i32;
    let mut observed = 0usize;
    let mut signals = Vec::new();

    if let Some(change_24h) = price_change_24h_pct {
        observed += 1;
        if change_24h >= 0.75 {
            score += 1;
            signals.push(format!("24h tape is positive at {change_24h:.2}%."));
        } else if change_24h <= -0.75 {
            score -= 1;
            signals.push(format!("24h tape is negative at {change_24h:.2}%."));
        } else {
            signals.push(format!("24h tape is flat-to-neutral at {change_24h:.2}%."));
        }
    }

    if let Some(basis) = basis_pct {
        observed += 1;
        if basis >= 0.15 {
            score += 1;
            signals.push(format!(
                "Perp is trading {basis:.2}% above index, which is a bullish basis."
            ));
        } else if basis <= -0.15 {
            score -= 1;
            signals.push(format!(
                "Perp is trading {basis:.2}% below index, which is a bearish discount."
            ));
        } else {
            signals.push(format!("Perp basis vs index is muted at {basis:.2}%."));
        }
    }

    if let Some(funding) = funding_rate_pct {
        observed += 1;
        if funding >= 0.005 {
            score += 1;
            signals.push(format!(
                "Funding is +{funding:.4}% per interval, which shows long-side demand ({})",
                funding_intensity.unwrap_or("unclassified")
            ));
        } else if funding <= -0.005 {
            score -= 1;
            signals.push(format!(
                "Funding is {funding:.4}% per interval, which shows short-side demand ({})",
                funding_intensity.unwrap_or("unclassified")
            ));
        } else {
            signals.push(format!(
                "Funding is near neutral at {funding:.4}% per interval ({})",
                funding_intensity.unwrap_or("unclassified")
            ));
        }
    }

    let bias = match score {
        2..=i32::MAX => "bullish",
        1 => "mildly bullish",
        0 => "neutral",
        -1 => "mildly bearish",
        i32::MIN..=-2 => "bearish",
    }
    .to_string();

    (bias, observed, score, signals)
}

fn compute_outlook(
    side: Option<&str>,
    price_vs_entry_pct: Option<f64>,
    distance_to_liquidation_pct: Option<f64>,
    bias_score: i32,
    observed_bias_inputs: usize,
) -> (String, String, Vec<String>) {
    let side_multiplier = match side {
        Some("long") => 1,
        Some("short") => -1,
        _ => 0,
    };

    let mut outlook_score = bias_score * side_multiplier;
    let mut signals = Vec::new();

    if let Some(price_vs_entry) = price_vs_entry_pct {
        if price_vs_entry >= 0.25 {
            outlook_score += side_multiplier;
            signals.push(format!(
                "Position is above entry by {price_vs_entry:.2}%, which supports the current side."
            ));
        } else if price_vs_entry <= -0.25 {
            outlook_score -= side_multiplier;
            signals.push(format!(
                "Position is below entry by {price_vs_entry:.2}%, which is pressure on the current side."
            ));
        } else {
            signals.push(format!(
                "Position is close to entry at {price_vs_entry:.2}% vs average entry."
            ));
        }
    }

    if let Some(distance) = distance_to_liquidation_pct {
        if distance < 10.0 {
            outlook_score -= 2;
            signals.push(format!(
                "Liquidation is only {distance:.2}% away, which is high risk."
            ));
        } else if distance < 20.0 {
            outlook_score -= 1;
            signals.push(format!(
                "Liquidation is {distance:.2}% away, which is a moderate risk buffer."
            ));
        } else {
            signals.push(format!(
                "Liquidation is {distance:.2}% away, which is a comfortable buffer."
            ));
        }
    }

    let outlook = match outlook_score {
        2..=i32::MAX => "favorable",
        1 => "constructive",
        0 => "mixed",
        -1 => "cautious",
        i32::MIN..=-2 => "at risk",
    }
    .to_string();

    let confidence = match observed_bias_inputs {
        0 | 1 => "low",
        2 => "medium",
        _ => "medium",
    }
    .to_string();

    (outlook, confidence, signals)
}

fn compute_projections(
    side: Option<&str>,
    mark_price: Option<f64>,
    contracts: Option<f64>,
) -> ProjectionSummary {
    let direction = match side {
        Some("short") => -1.0,
        _ => 1.0,
    };
    let delta = contracts.zip(mark_price).map(|(size, mark)| size * mark * direction);

    ProjectionSummary {
        up_1pct_pnl: delta.map(|item| item * 0.01),
        up_3pct_pnl: delta.map(|item| item * 0.03),
        down_1pct_pnl: delta.map(|item| -item * 0.01),
        down_3pct_pnl: delta.map(|item| -item * 0.03),
    }
}

fn classify_funding_intensity(funding_rate_pct: Option<f64>) -> Option<String> {
    let abs_rate = funding_rate_pct.map(f64::abs)?;
    let label = if abs_rate < 0.0005 {
        "near zero"
    } else if abs_rate <= 0.005 {
        "tiny"
    } else if abs_rate <= 0.02 {
        "noticeable"
    } else if abs_rate <= 0.05 {
        "elevated"
    } else if abs_rate <= 0.10 {
        "large"
    } else {
        "very large"
    };

    Some(label.to_string())
}

fn classify_open_interest_context(position_share_pct: Option<f64>) -> Option<String> {
    let share = position_share_pct?;
    let label = if share < 0.10 {
        "tiny relative to market"
    } else if share < 1.0 {
        "small relative to market"
    } else if share < 5.0 {
        "meaningful relative to market"
    } else {
        "large relative to market"
    };

    Some(label.to_string())
}

fn analyze_position(
    position: &RawPosition,
    product: Option<&ProductResponse>,
    portfolio_state: Option<&IntxPortfolioState>,
) -> DerivedAnalytics {
    let side = normalize_side(position.position_side.as_deref());
    let mark_price = parse_f64(money_value(position.mark_price.as_ref()).as_deref());
    let entry_price = parse_f64(money_value(position.entry_vwap.as_ref()).as_deref());
    let liquidation_price = parse_f64(money_value(position.liquidation_price.as_ref()).as_deref());
    let contracts = parse_f64(position.net_size.as_deref());
    let notional = parse_f64(money_value(position.position_notional.as_ref()).as_deref());

    let price_change_24h_pct =
        product.and_then(|item| parse_f64(item.price_percentage_change_24h.as_deref()));
    let index_price = product.and_then(product_index_price);
    let basis_pct = mark_price
        .zip(index_price)
        .and_then(|(mark, index)| (index != 0.0).then_some(((mark - index) / index) * 100.0));
    let funding_rate_pct = product.and_then(product_funding_rate).map(|value| value * 100.0);
    let open_interest = product.and_then(product_open_interest);

    let funding_direction = funding_rate_pct.map(|value| {
        if value > 0.0 {
            "longs paying shorts".to_string()
        } else if value < 0.0 {
            "shorts paying longs".to_string()
        } else {
            "neutral funding".to_string()
        }
    });
    let funding_intensity = classify_funding_intensity(funding_rate_pct);
    let open_interest_notional = open_interest.zip(mark_price).map(|(oi, mark)| oi * mark);
    let position_share_of_open_interest_pct = contracts
        .zip(open_interest)
        .and_then(|(size, oi)| (oi != 0.0).then_some((size / oi) * 100.0));
    let open_interest_context =
        classify_open_interest_context(position_share_of_open_interest_pct);

    let price_vs_entry_pct = mark_price
        .zip(entry_price)
        .and_then(|(mark, entry)| (entry != 0.0).then_some(((mark - entry) / entry) * 100.0));

    let effective_leverage = portfolio_state.and_then(|state| {
        let collateral = parse_f64(Some(state.collateral.as_str()))?;
        let state_notional = parse_f64(Some(state.position_notional.as_str())).or(notional)?;
        (collateral != 0.0).then_some(state_notional / collateral)
    });

    let initial_margin_rate = portfolio_state
        .and_then(|state| parse_f64(Some(state.portfolio_initial_margin.as_str())).map(|v| v * 100.0));
    let maintenance_margin_rate = portfolio_state
        .and_then(|state| parse_f64(Some(state.portfolio_maintenance_margin.as_str())).map(|v| v * 100.0));

    let distance_to_liquidation_pct = mark_price.zip(liquidation_price).and_then(|(mark, liq)| {
        if mark == 0.0 {
            None
        } else if side.as_deref() == Some("short") {
            Some(((liq - mark) / mark) * 100.0)
        } else {
            Some(((mark - liq) / mark) * 100.0)
        }
    });

    let (market_bias, observed_bias_inputs, bias_score, mut bias_signals) =
        compute_market_bias(
            price_change_24h_pct,
            basis_pct,
            funding_rate_pct,
            funding_intensity.as_deref(),
        );
    let (position_outlook, outlook_confidence, mut outlook_signals) = compute_outlook(
        side.as_deref(),
        price_vs_entry_pct,
        distance_to_liquidation_pct,
        bias_score,
        observed_bias_inputs,
    );

    let projections = compute_projections(side.as_deref(), mark_price, contracts);

    let mut signals = Vec::new();
    signals.append(&mut bias_signals);
    signals.append(&mut outlook_signals);
    if let Some(leverage) = effective_leverage {
        signals.push(format!(
            "Effective leverage from isolated collateral is {leverage:.2}x."
        ));
    }
    if let Some(notional_oi) = open_interest_notional {
        signals.push(format!(
            "Open interest is about {notional_oi:.2} quote notional at the current mark."
        ));
    }
    if let Some(share) = position_share_of_open_interest_pct {
        signals.push(format!(
            "Your position is {share:.2}% of current open interest, which is {}.",
            open_interest_context
                .as_deref()
                .unwrap_or("unclassified")
        ));
    }

    DerivedAnalytics {
        effective_leverage,
        initial_margin_rate,
        maintenance_margin_rate,
        price_vs_entry_pct,
        basis_pct,
        funding_rate_pct,
        funding_direction,
        funding_intensity,
        open_interest_notional,
        position_share_of_open_interest_pct,
        open_interest_context,
        distance_to_liquidation_pct,
        market_bias,
        position_outlook,
        outlook_confidence,
        signals,
        projections,
    }
}

fn summarize_position(
    position: RawPosition,
    product: Option<&ProductResponse>,
    portfolio_state: Option<&IntxPortfolioState>,
) -> PositionSummary {
    let analytics = analyze_position(&position, product, portfolio_state);

    PositionSummary {
        symbol: position.symbol.clone(),
        display_name: product.and_then(product_display_name),
        underlying_type: product.and_then(product_underlying_type),
        side: normalize_side(position.position_side.as_deref()),
        contracts: position.net_size.clone(),
        notional: money_value(position.position_notional.as_ref()),
        entry_price: money_value(position.entry_vwap.as_ref()),
        mark_price: money_value(position.mark_price.as_ref()),
        index_price: format_opt(product.and_then(product_index_price), 2),
        vwap_price: money_value(position.vwap.as_ref()),
        unrealized_pnl: money_value(position.unrealized_pnl.as_ref()),
        aggregated_pnl: money_value(position.aggregated_pnl.as_ref()),
        liquidation_price: money_value(position.liquidation_price.as_ref()),
        api_leverage: position.leverage.clone(),
        effective_leverage: analytics.effective_leverage,
        max_leverage: product.and_then(product_max_leverage),
        margin_mode: normalize_margin_mode(position.margin_type.as_deref()),
        collateral: portfolio_state.map(|state| state.collateral.clone()),
        total_balance: portfolio_state.map(|state| state.total_balance.value.clone()),
        pending_fees: portfolio_state.map(|state| state.pending_fees.clone()),
        liquidation_buffer: portfolio_state.map(|state| state.liquidation_buffer.clone()),
        initial_margin_rate: analytics.initial_margin_rate,
        maintenance_margin_rate: analytics.maintenance_margin_rate,
        price_vs_entry_pct: analytics.price_vs_entry_pct,
        price_change_24h_pct: product.and_then(|item| parse_f64(item.price_percentage_change_24h.as_deref())),
        basis_pct: analytics.basis_pct,
        funding_rate_pct: analytics.funding_rate_pct,
        funding_direction: analytics.funding_direction,
        funding_intensity: analytics.funding_intensity,
        open_interest: format_opt(product.and_then(product_open_interest), 2),
        open_interest_notional: analytics.open_interest_notional,
        position_share_of_open_interest_pct: analytics.position_share_of_open_interest_pct,
        open_interest_context: analytics.open_interest_context,
        distance_to_liquidation_pct: analytics.distance_to_liquidation_pct,
        market_bias: analytics.market_bias,
        position_outlook: analytics.position_outlook,
        outlook_confidence: analytics.outlook_confidence,
        signals: analytics.signals,
        projections: analytics.projections,
    }
}

pub fn load_output(portfolio_id: Option<&str>) -> Result<Output> {
    let _ = dotenv();
    let credentials = get_credentials()?;
    let client = build_client()?;
    let portfolios = fetch_portfolios(&client, &credentials)?;
    let portfolio = select_portfolio(&portfolios, portfolio_id)?;
    let positions = fetch_positions(&client, &credentials, &portfolio.uuid)?;
    let portfolio_states = fetch_portfolio_summary(&client, &credentials, &portfolio.uuid)?;

    let mut product_cache = HashMap::new();
    for position in &positions {
        product_cache
            .entry(position.symbol.clone())
            .or_insert_with(|| fetch_product(&client, &credentials, &position.symbol));
    }

    let portfolio_state_lookup: HashMap<&str, &IntxPortfolioState> = portfolio_states
        .iter()
        .map(|state| (state.portfolio_uuid.as_str(), state))
        .collect();

    let positions = positions
        .into_iter()
        .map(|position| {
            let product = product_cache
                .get(&position.symbol)
                .and_then(|result| result.as_ref().ok());
            let position_portfolio_id = position
                .portfolio_uuid
                .as_deref()
                .unwrap_or(&portfolio.uuid);
            let portfolio_state = portfolio_state_lookup
                .get(position_portfolio_id)
                .copied()
                .or_else(|| portfolio_state_lookup.get(portfolio.uuid.as_str()).copied());
            summarize_position(position, product, portfolio_state)
        })
        .collect::<Vec<_>>();

    Ok(Output {
        credential_source: credentials.source,
        portfolio: PortfolioSummary {
            id: portfolio.uuid.clone(),
            portfolio_type: portfolio.portfolio_type.clone(),
        },
        portfolio_count: portfolios.len(),
        analysis_basis: ANALYSIS_BASIS,
        positions,
    })
}

fn render_position_lines(index: usize, position: &PositionSummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "{}. {}{}",
        index + 1,
        position.symbol,
        position
            .display_name
            .as_deref()
            .map(|item| format!(" ({item})"))
            .unwrap_or_default()
    ));
    lines.push(format!(
        "   Position: {} | contracts={} | entry={} | mark={} | index={} | pnl={} | aggPnl={}",
        position.side.as_deref().unwrap_or("unknown"),
        position.contracts.as_deref().unwrap_or("unknown"),
        position.entry_price.as_deref().unwrap_or("unknown"),
        position.mark_price.as_deref().unwrap_or("unknown"),
        position.index_price.as_deref().unwrap_or("unknown"),
        position.unrealized_pnl.as_deref().unwrap_or("unknown"),
        position.aggregated_pnl.as_deref().unwrap_or("unknown"),
    ));
    lines.push(format!(
        "   Risk: effectiveLev={}x | apiLev={}x | collateral={} | liq={} | liqDistance={} | liqBuffer={}",
        format_opt(position.effective_leverage, 2).as_deref().unwrap_or("unknown"),
        position.api_leverage.as_deref().unwrap_or("unknown"),
        position.collateral.as_deref().unwrap_or("unknown"),
        position.liquidation_price.as_deref().unwrap_or("unknown"),
        format_pct(position.distance_to_liquidation_pct)
            .as_deref()
            .unwrap_or("unknown"),
        position.liquidation_buffer.as_deref().unwrap_or("unknown"),
    ));
    lines.push(format!(
        "   Market: 24h={} | basis={} | funding={} ({}, {}) | openInterest={} (~{} notional, {}) | maxLev={}x",
        format_pct(position.price_change_24h_pct)
            .as_deref()
            .unwrap_or("unknown"),
        format_pct(position.basis_pct).as_deref().unwrap_or("unknown"),
        format_opt(position.funding_rate_pct, 4)
            .map(|item| format!("{item}%"))
            .as_deref()
            .unwrap_or("unknown"),
        position
            .funding_direction
            .as_deref()
            .unwrap_or("unknown funding"),
        position
            .funding_intensity
            .as_deref()
            .unwrap_or("unclassified"),
        position.open_interest.as_deref().unwrap_or("unknown"),
        format_opt(position.open_interest_notional, 2)
            .as_deref()
            .unwrap_or("unknown"),
        position
            .open_interest_context
            .as_deref()
            .unwrap_or("unclassified"),
        position.max_leverage.as_deref().unwrap_or("unknown"),
    ));
    lines.push(format!(
        "   Heuristic outlook: bias={} | position={} | confidence={}",
        position.market_bias, position.position_outlook, position.outlook_confidence
    ));
    lines.push(format!(
        "   Projections: +1%={} | +3%={} | -1%={} | -3%={}",
        format_opt(position.projections.up_1pct_pnl, 2)
            .as_deref()
            .unwrap_or("unknown"),
        format_opt(position.projections.up_3pct_pnl, 2)
            .as_deref()
            .unwrap_or("unknown"),
        format_opt(position.projections.down_1pct_pnl, 2)
            .as_deref()
            .unwrap_or("unknown"),
        format_opt(position.projections.down_3pct_pnl, 2)
            .as_deref()
            .unwrap_or("unknown"),
    ));
    if !position.signals.is_empty() {
        lines.push("   Signals:".to_string());
        for signal in &position.signals {
            lines.push(format!("     - {signal}"));
        }
    }
    lines.join("\n")
}

pub fn render_cli_output(output: &Output) -> String {
    let mut lines = vec![
        format!("Credential source: {}", output.credential_source),
        format!(
            "Portfolio: {} ({})",
            output.portfolio.id, output.portfolio.portfolio_type
        ),
        format!("Portfolio count: {}", output.portfolio_count),
        format!("Analysis basis: {}", output.analysis_basis),
        format!("Open positions: {}", output.positions.len()),
    ];

    if output.positions.is_empty() {
        lines.push("No open perp positions found.".to_string());
        return lines.join("\n");
    }

    for (index, position) in output.positions.iter().enumerate() {
        lines.push(render_position_lines(index, position));
    }

    lines.join("\n")
}
