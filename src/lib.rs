use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use dotenvy::dotenv;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::SecretKey;
use rand::RngCore;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const API_HOST: &str = "api.coinbase.com";
const API_BASE: &str = "https://api.coinbase.com";
const ORDER_BOOK_LEVEL_LIMIT: usize = 100;
const SLIPPAGE_NOTIONAL_TARGETS: [f64; 4] = [5_000.0, 10_000.0, 20_000.0, 40_000.0];
pub const ANALYSIS_BASIS: &str =
    "Heuristic snapshot derived from Coinbase position, product, portfolio summary, and product book endpoints. Any dashboard model output is a separate experimental overlay, not a trade guarantee.";

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

#[derive(Debug, Deserialize)]
struct ProductsResponse {
    products: Vec<ListedProduct>,
}

#[derive(Debug, Deserialize)]
struct ListedProduct {
    product_id: String,
    #[serde(default)]
    is_disabled: bool,
    #[serde(default)]
    trading_disabled: bool,
    #[serde(default)]
    cancel_only: bool,
    #[serde(default)]
    future_product_details: Option<FutureProductDetails>,
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
struct ProductBookResponse {
    pricebook: PriceBook,
    #[serde(default)]
    mid_market: Option<String>,
    #[serde(default)]
    spread_bps: Option<String>,
    #[serde(default)]
    spread_absolute: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PriceBook {
    #[allow(dead_code)]
    product_id: String,
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
    #[serde(default)]
    time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BookLevel {
    price: String,
    size: String,
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

#[derive(Debug, Deserialize)]
struct OrdersResponse {
    #[serde(default)]
    orders: Vec<RawOrder>,
    #[serde(default)]
    has_next: bool,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawOrder {
    order_id: String,
    product_id: String,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    created_time: Option<String>,
    #[serde(default)]
    last_update_time: Option<String>,
    #[serde(default)]
    completion_percentage: Option<String>,
    #[serde(default)]
    average_filled_price: Option<String>,
    #[serde(default)]
    filled_size: Option<String>,
    #[serde(default)]
    total_fees: Option<String>,
    #[serde(default)]
    trigger_status: Option<String>,
    #[serde(default)]
    order_type: Option<String>,
    #[serde(default)]
    order_placement_source: Option<String>,
    #[serde(default)]
    client_order_id: Option<String>,
    #[serde(default)]
    leverage: Option<String>,
    #[serde(default)]
    margin_type: Option<String>,
    #[serde(default)]
    order_configuration: Value,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
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
    pub open_orders: Vec<OpenOrderSummary>,
    pub watch_markets: Vec<WatchMarketSummary>,
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
    pub order_book: Option<OrderBookSummary>,
    pub distance_to_liquidation_pct: Option<f64>,
    pub market_bias: String,
    pub position_outlook: String,
    pub outlook_confidence: String,
    pub signals: Vec<String>,
    pub projections: ProjectionSummary,
}

#[derive(Debug, Serialize, Clone)]
pub struct OpenOrderSummary {
    pub order_id: String,
    pub product_id: String,
    pub side: Option<String>,
    pub status: String,
    pub created_time: Option<String>,
    pub last_update_time: Option<String>,
    pub order_type: Option<String>,
    pub trigger_status: Option<String>,
    pub order_placement_source: Option<String>,
    pub client_order_id: Option<String>,
    pub configuration_label: Option<String>,
    pub base_size: Option<String>,
    pub quote_size: Option<String>,
    pub filled_size: Option<String>,
    pub completion_percentage: Option<String>,
    pub average_filled_price: Option<String>,
    pub total_fees: Option<String>,
    pub limit_price: Option<String>,
    pub stop_price: Option<String>,
    pub stop_trigger_price: Option<String>,
    pub end_time: Option<String>,
    pub post_only: Option<bool>,
    pub reduce_only: Option<bool>,
    pub leverage: Option<String>,
    pub margin_mode: Option<String>,
    pub cleanup_candidate: bool,
    pub cleanup_reason: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct WatchMarketSummary {
    pub symbol: String,
    pub display_name: Option<String>,
    pub underlying_type: Option<String>,
    pub mark_price: Option<String>,
    pub index_price: Option<String>,
    pub price_change_24h_pct: Option<f64>,
    pub basis_pct: Option<f64>,
    pub funding_rate_pct: Option<f64>,
    pub funding_direction: Option<String>,
    pub funding_intensity: Option<String>,
    pub open_interest: Option<String>,
    pub open_interest_notional: Option<f64>,
    pub max_leverage: Option<String>,
    pub order_book: Option<OrderBookSummary>,
    pub market_bias: String,
    pub signals: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectionSummary {
    pub up_1pct_pnl: Option<f64>,
    pub up_3pct_pnl: Option<f64>,
    pub down_1pct_pnl: Option<f64>,
    pub down_3pct_pnl: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
pub struct OrderBookSummary {
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub mid_market: Option<f64>,
    pub spread_absolute: Option<f64>,
    pub spread_bps: Option<f64>,
    pub book_time: Option<String>,
    pub bid_levels: usize,
    pub ask_levels: usize,
    pub visible_bid_notional: Option<f64>,
    pub visible_ask_notional: Option<f64>,
    pub top_5_bid_notional: Option<f64>,
    pub top_5_ask_notional: Option<f64>,
    pub top_5_imbalance_pct: Option<f64>,
    pub buy_slippage: Vec<SlippageEstimate>,
    pub sell_slippage: Vec<SlippageEstimate>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SlippageEstimate {
    pub quote_notional: f64,
    pub average_price: Option<f64>,
    pub worst_price: Option<f64>,
    pub slippage_bps: Option<f64>,
    pub filled_quote: Option<f64>,
    pub filled_base: Option<f64>,
    pub fill_pct: Option<f64>,
    pub complete: bool,
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
    order_book: Option<OrderBookSummary>,
    distance_to_liquidation_pct: Option<f64>,
    market_bias: String,
    position_outlook: String,
    outlook_confidence: String,
    signals: Vec<String>,
    projections: ProjectionSummary,
}

#[derive(Debug, Clone, Copy)]
struct ParsedBookLevel {
    price: f64,
    size: f64,
}

#[derive(Debug, Clone, Copy)]
enum ExecutionSide {
    Buy,
    Sell,
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

fn get_public_json<T>(client: &Client, path: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let response = client
        .get(format!("{API_BASE}{path}"))
        .header(CACHE_CONTROL, "no-cache")
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

fn fetch_product_book(
    client: &Client,
    _credentials: &Credentials,
    symbol: &str,
) -> Result<ProductBookResponse> {
    let path = format!(
        "/api/v3/brokerage/market/product_book?product_id={symbol}&limit={ORDER_BOOK_LEVEL_LIMIT}"
    );
    get_public_json(client, &path)
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

fn fetch_perpetual_future_products(
    client: &Client,
    credentials: &Credentials,
) -> Result<Vec<ListedProduct>> {
    let path = "/api/v3/brokerage/products";
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
        .query(&[
            ("product_type", "FUTURE"),
            ("contract_expiry_type", "PERPETUAL"),
            ("limit", "500"),
        ])
        .send()
        .with_context(|| format!("request failed for GET {path}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("Coinbase returned {status} for GET {path}: {body}");
    }

    let response: ProductsResponse = response
        .json::<ProductsResponse>()
        .with_context(|| format!("failed to decode Coinbase JSON for GET {path}"))?;
    Ok(response.products)
}

fn fetch_open_orders(client: &Client, credentials: &Credentials) -> Result<Vec<RawOrder>> {
    let path = "/api/v3/brokerage/orders/historical/batch";
    let active_statuses = ["OPEN", "PENDING", "QUEUED", "CANCEL_QUEUED", "EDIT_QUEUED"];
    let mut orders = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let mut pages_fetched = 0usize;

    loop {
        if pages_fetched >= 4 {
            break;
        }
        let token = build_jwt(&credentials.api_key, &credentials.api_secret, "GET", path)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .context("failed to build authorization header")?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let mut params = vec![
            ("product_type", "FUTURE".to_string()),
            ("contract_expiry_type", "PERPETUAL".to_string()),
            ("limit", "100".to_string()),
        ];
        if let Some(value) = cursor.as_deref() {
            params.push(("cursor", value.to_string()));
        }

        let response = client
            .get(format!("{API_BASE}{path}"))
            .headers(headers)
            .query(&params)
            .send()
            .with_context(|| format!("request failed for GET {path}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Coinbase returned {status} for GET {path}: {body}");
        }

        let response: OrdersResponse = response
            .json::<OrdersResponse>()
            .with_context(|| format!("failed to decode Coinbase JSON for GET {path}"))?;
        orders.extend(response.orders);
        pages_fetched += 1;
        if !response.has_next {
            break;
        }
        let Some(next_cursor) = response.cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            break;
        }
        cursor = Some(next_cursor);
    }

    orders.retain(|order| {
        order.status
            .as_deref()
            .map(|status| active_statuses.contains(&status))
            .unwrap_or(false)
    });
    orders.sort_by(|left, right| right.created_time.cmp(&left.created_time));
    orders.dedup_by(|left, right| left.order_id == right.order_id);
    Ok(orders)
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

fn value_as_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(item) => Some(item.clone()),
        Value::Number(item) => Some(item.to_string()),
        Value::Bool(item) => Some(item.to_string()),
        _ => None,
    }
}

fn value_as_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(item) => Some(*item),
        Value::String(item) => match item.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug, Default)]
struct ParsedOrderConfig {
    label: Option<String>,
    base_size: Option<String>,
    quote_size: Option<String>,
    limit_price: Option<String>,
    stop_price: Option<String>,
    stop_trigger_price: Option<String>,
    end_time: Option<String>,
    post_only: Option<bool>,
    reduce_only: Option<bool>,
}

fn parse_order_config(config: &Value, extra: &HashMap<String, Value>) -> ParsedOrderConfig {
    let mut parsed = ParsedOrderConfig::default();
    let Some(map) = config.as_object() else {
        parsed.reduce_only = value_as_bool(extra.get("reduce_only").or(extra.get("close_only")));
        return parsed;
    };

    let selected = map.iter().find_map(|(label, value)| {
        value.as_object().and_then(|details| {
            (!details.is_empty()).then_some((label.as_str(), details))
        })
    });

    if let Some((label, details)) = selected {
        parsed.label = Some(label.to_string());
        parsed.base_size = value_as_string(details.get("base_size"));
        parsed.quote_size = value_as_string(details.get("quote_size"));
        parsed.limit_price = value_as_string(details.get("limit_price"));
        parsed.stop_price = value_as_string(details.get("stop_price"));
        parsed.stop_trigger_price = value_as_string(
            details
                .get("stop_trigger_price")
                .or(details.get("stop_direction")),
        );
        parsed.end_time = value_as_string(details.get("end_time"));
        parsed.post_only = value_as_bool(details.get("post_only"));
        parsed.reduce_only = value_as_bool(
            details
                .get("reduce_only")
                .or(details.get("close_only"))
                .or(extra.get("reduce_only"))
                .or(extra.get("close_only")),
        );
    } else {
        parsed.reduce_only = value_as_bool(extra.get("reduce_only").or(extra.get("close_only")));
    }

    parsed
}

fn normalize_order_side(side: Option<&str>) -> Option<String> {
    match side {
        Some("BUY") => Some("buy".to_string()),
        Some("SELL") => Some("sell".to_string()),
        Some(other) if !other.is_empty() => Some(other.to_ascii_lowercase()),
        _ => None,
    }
}

fn summarize_open_order(
    order: RawOrder,
    active_position_symbols: &HashMap<String, String>,
) -> OpenOrderSummary {
    let config = parse_order_config(&order.order_configuration, &order.extra);
    let cleanup_reason = if config.reduce_only == Some(true)
        && !active_position_symbols.contains_key(&order.product_id)
    {
        Some(format!(
            "No live position is open on {}, but this order is still marked reduce-only.",
            order.product_id
        ))
    } else {
        None
    };

    OpenOrderSummary {
        order_id: order.order_id,
        product_id: order.product_id,
        side: normalize_order_side(order.side.as_deref()),
        status: order.status.unwrap_or_else(|| "unknown".to_string()),
        created_time: order.created_time,
        last_update_time: order.last_update_time,
        order_type: order.order_type,
        trigger_status: order.trigger_status,
        order_placement_source: order.order_placement_source,
        client_order_id: order.client_order_id,
        configuration_label: config.label,
        base_size: config.base_size,
        quote_size: config.quote_size,
        filled_size: order.filled_size,
        completion_percentage: order.completion_percentage,
        average_filled_price: order.average_filled_price,
        total_fees: order.total_fees,
        limit_price: config.limit_price,
        stop_price: config.stop_price,
        stop_trigger_price: config.stop_trigger_price,
        end_time: config.end_time,
        post_only: config.post_only,
        reduce_only: config.reduce_only,
        leverage: order.leverage,
        margin_mode: normalize_margin_mode(order.margin_type.as_deref()),
        cleanup_candidate: cleanup_reason.is_some(),
        cleanup_reason,
    }
}

pub fn format_opt(value: Option<f64>, decimals: usize) -> Option<String> {
    value.map(|item| format!("{item:.decimals$}"))
}

pub fn format_pct(value: Option<f64>) -> Option<String> {
    format_opt(value, 2).map(|item| format!("{item}%"))
}

fn parse_book_levels(levels: &[BookLevel]) -> Vec<ParsedBookLevel> {
    levels
        .iter()
        .filter_map(|level| {
            let price = parse_f64(Some(level.price.as_str()))?;
            let size = parse_f64(Some(level.size.as_str()))?;
            (price > 0.0 && size > 0.0).then_some(ParsedBookLevel { price, size })
        })
        .collect()
}

fn sum_quote_depth(levels: &[ParsedBookLevel], limit: Option<usize>) -> Option<f64> {
    let iter = match limit {
        Some(level_limit) => levels.iter().take(level_limit).collect::<Vec<_>>(),
        None => levels.iter().collect::<Vec<_>>(),
    };

    if iter.is_empty() {
        return None;
    }

    Some(iter.into_iter().map(|level| level.price * level.size).sum())
}

fn estimate_quote_execution(
    levels: &[ParsedBookLevel],
    target_quote: f64,
    reference_price: Option<f64>,
    side: ExecutionSide,
) -> SlippageEstimate {
    let mut remaining_quote = target_quote.max(0.0);
    let mut filled_quote = 0.0;
    let mut filled_base = 0.0;
    let mut worst_price = None;

    for level in levels {
        if remaining_quote <= 1e-9 {
            break;
        }

        let available_quote = level.price * level.size;
        if available_quote <= 0.0 {
            continue;
        }

        let take_quote = remaining_quote.min(available_quote);
        let take_base = take_quote / level.price;

        filled_quote += take_quote;
        filled_base += take_base;
        remaining_quote -= take_quote;
        worst_price = Some(level.price);
    }

    let complete = remaining_quote <= target_quote.max(1.0) * 1e-6;
    let average_price = (filled_base > 0.0).then_some(filled_quote / filled_base);
    let slippage_bps = average_price.zip(reference_price).and_then(|(avg, reference)| {
        if reference <= 0.0 {
            None
        } else {
            let bps = match side {
                ExecutionSide::Buy => ((avg - reference) / reference) * 10_000.0,
                ExecutionSide::Sell => ((reference - avg) / reference) * 10_000.0,
            };
            Some(bps.max(0.0))
        }
    });

    SlippageEstimate {
        quote_notional: target_quote,
        average_price,
        worst_price,
        slippage_bps,
        filled_quote: (filled_quote > 0.0).then_some(filled_quote),
        filled_base: (filled_base > 0.0).then_some(filled_base),
        fill_pct: (target_quote > 0.0).then_some((filled_quote / target_quote).min(1.0) * 100.0),
        complete,
    }
}

fn build_order_book_summary(book: &ProductBookResponse) -> Option<OrderBookSummary> {
    let bids = parse_book_levels(&book.pricebook.bids);
    let asks = parse_book_levels(&book.pricebook.asks);
    if bids.is_empty() && asks.is_empty() {
        return None;
    }

    let best_bid = bids.first().map(|level| level.price);
    let best_ask = asks.first().map(|level| level.price);
    let mid_market = parse_f64(book.mid_market.as_deref())
        .or_else(|| best_bid.zip(best_ask).map(|(bid, ask)| (bid + ask) / 2.0));
    let spread_absolute = parse_f64(book.spread_absolute.as_deref())
        .or_else(|| best_ask.zip(best_bid).map(|(ask, bid)| ask - bid));
    let spread_bps = parse_f64(book.spread_bps.as_deref()).or_else(|| {
        mid_market.zip(spread_absolute).and_then(|(mid, spread)| {
            (mid > 0.0).then_some((spread / mid) * 10_000.0)
        })
    });
    let visible_bid_notional = sum_quote_depth(&bids, None);
    let visible_ask_notional = sum_quote_depth(&asks, None);
    let top_5_bid_notional = sum_quote_depth(&bids, Some(5));
    let top_5_ask_notional = sum_quote_depth(&asks, Some(5));
    let top_5_imbalance_pct = top_5_bid_notional
        .zip(top_5_ask_notional)
        .and_then(|(bid, ask)| {
            let total = bid + ask;
            (total > 0.0).then_some(((bid - ask) / total) * 100.0)
        });

    let buy_slippage = SLIPPAGE_NOTIONAL_TARGETS
        .iter()
        .map(|target| estimate_quote_execution(&asks, *target, best_ask, ExecutionSide::Buy))
        .collect();
    let sell_slippage = SLIPPAGE_NOTIONAL_TARGETS
        .iter()
        .map(|target| estimate_quote_execution(&bids, *target, best_bid, ExecutionSide::Sell))
        .collect();

    Some(OrderBookSummary {
        best_bid,
        best_ask,
        mid_market,
        spread_absolute,
        spread_bps,
        book_time: book.pricebook.time.clone(),
        bid_levels: bids.len(),
        ask_levels: asks.len(),
        visible_bid_notional,
        visible_ask_notional,
        top_5_bid_notional,
        top_5_ask_notional,
        top_5_imbalance_pct,
        buy_slippage,
        sell_slippage,
    })
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

fn analyze_position(
    position: &RawPosition,
    product: Option<&ProductResponse>,
    product_book: Option<&ProductBookResponse>,
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
    let order_book = product_book.and_then(build_order_book_summary);

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
        signals.push(format!("Your position is {share:.2}% of current open interest."));
    }
    if let Some(book) = order_book.as_ref() {
        if let Some(spread_bps) = book.spread_bps {
            let spread_absolute = book
                .spread_absolute
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "unknown".to_string());
            signals.push(format!(
                "Top-of-book spread is {spread_bps:.2} bps ({spread_absolute} absolute)."
            ));
        }
        if let Some(imbalance) = book.top_5_imbalance_pct {
            let lean = if imbalance > 5.0 {
                "bid-heavy"
            } else if imbalance < -5.0 {
                "ask-heavy"
            } else {
                "balanced"
            };
            signals.push(format!(
                "Near-touch depth imbalance across the top 5 levels is {imbalance:.2}% ({lean})."
            ));
        }

        let buy_10k = book
            .buy_slippage
            .iter()
            .find(|estimate| (estimate.quote_notional - 10_000.0).abs() < 0.5);
        let sell_10k = book
            .sell_slippage
            .iter()
            .find(|estimate| (estimate.quote_notional - 10_000.0).abs() < 0.5);

        if let (Some(buy), Some(sell)) = (buy_10k, sell_10k) {
            if buy.complete && sell.complete {
                signals.push(format!(
                    "Estimated market-order slippage for $10k quote notional is {} bps to buy and {} bps to sell.",
                    format_opt(buy.slippage_bps, 2)
                        .as_deref()
                        .unwrap_or("unknown"),
                    format_opt(sell.slippage_bps, 2)
                        .as_deref()
                        .unwrap_or("unknown"),
                ));
            }
        }

        let buy_max_complete = book
            .buy_slippage
            .last()
            .map(|estimate| estimate.complete)
            .unwrap_or(false);
        let sell_max_complete = book
            .sell_slippage
            .last()
            .map(|estimate| estimate.complete)
            .unwrap_or(false);
        if !buy_max_complete || !sell_max_complete {
            signals.push(
                "The fetched order-book ladder does not fully cover the largest preset execution size on at least one side."
                    .to_string(),
            );
        }
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
        order_book,
        distance_to_liquidation_pct,
        market_bias,
        position_outlook,
        outlook_confidence,
        signals,
        projections,
    }
}

fn summarize_watch_market(
    symbol: &str,
    product: Option<&ProductResponse>,
    product_book: Option<&ProductBookResponse>,
) -> WatchMarketSummary {
    let price_change_24h_pct =
        product.and_then(|item| parse_f64(item.price_percentage_change_24h.as_deref()));
    let mark_price = product.and_then(|item| parse_f64(item.price.as_deref()));
    let index_price = product.and_then(product_index_price);
    let basis_pct = mark_price
        .zip(index_price)
        .and_then(|(mark, index)| (index != 0.0).then_some(((mark - index) / index) * 100.0));
    let funding_rate_pct = product.and_then(product_funding_rate).map(|value| value * 100.0);
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
    let open_interest = product.and_then(product_open_interest);
    let open_interest_notional = open_interest.zip(mark_price).map(|(oi, mark)| oi * mark);
    let order_book = product_book.and_then(build_order_book_summary);
    let (market_bias, _, _, mut signals) = compute_market_bias(
        price_change_24h_pct,
        basis_pct,
        funding_rate_pct,
        funding_intensity.as_deref(),
    );

    if let Some(book) = order_book.as_ref() {
        if let Some(spread_bps) = book.spread_bps {
            signals.push(format!(
                "Top-of-book spread is {spread_bps:.2} bps in the current snapshot."
            ));
        }
        if let Some(imbalance) = book.top_5_imbalance_pct {
            let lean = if imbalance > 5.0 {
                "bid-heavy"
            } else if imbalance < -5.0 {
                "ask-heavy"
            } else {
                "balanced"
            };
            signals.push(format!(
                "Near-touch depth imbalance is {imbalance:.2}% ({lean})."
            ));
        }
        let buy_5k = book
            .buy_slippage
            .iter()
            .find(|estimate| (estimate.quote_notional - 5_000.0).abs() < 0.5);
        if let Some(buy) = buy_5k {
            signals.push(format!(
                "Estimated $5k aggressive buy slippage is {} bps.",
                format_opt(buy.slippage_bps, 2)
                    .as_deref()
                    .unwrap_or("unknown")
            ));
        }
    }

    WatchMarketSummary {
        symbol: symbol.to_string(),
        display_name: product.and_then(product_display_name),
        underlying_type: product.and_then(product_underlying_type),
        mark_price: format_opt(mark_price, 2),
        index_price: format_opt(index_price, 2),
        price_change_24h_pct,
        basis_pct,
        funding_rate_pct,
        funding_direction,
        funding_intensity,
        open_interest: format_opt(open_interest, 2),
        open_interest_notional,
        max_leverage: product.and_then(product_max_leverage),
        order_book,
        market_bias,
        signals,
    }
}

fn summarize_position(
    position: RawPosition,
    product: Option<&ProductResponse>,
    product_book: Option<&ProductBookResponse>,
    portfolio_state: Option<&IntxPortfolioState>,
) -> PositionSummary {
    let analytics = analyze_position(&position, product, product_book, portfolio_state);

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
        order_book: analytics.order_book,
        distance_to_liquidation_pct: analytics.distance_to_liquidation_pct,
        market_bias: analytics.market_bias,
        position_outlook: analytics.position_outlook,
        outlook_confidence: analytics.outlook_confidence,
        signals: analytics.signals,
        projections: analytics.projections,
    }
}

pub fn load_output(portfolio_id: Option<&str>) -> Result<Output> {
    load_output_with_watch(portfolio_id, &[])
}

pub fn load_available_stock_watch_symbols() -> Result<Vec<String>> {
    dotenv().ok();

    let credentials = get_credentials()?;
    let client = build_client()?;
    let mut symbols = fetch_perpetual_future_products(&client, &credentials)?
        .into_iter()
        .filter(|product| !product.is_disabled && !product.trading_disabled && !product.cancel_only)
        .filter_map(|product| {
            let underlying_type = product
                .future_product_details
                .as_ref()
                .and_then(|details| details.perpetual_details.as_ref())
                .and_then(|details| details.underlying_type.as_deref());
            matches!(underlying_type, Some("EQUITY") | Some("EQUITY_ETF"))
                .then_some(product.product_id)
        })
        .collect::<Vec<_>>();
    symbols.sort();
    symbols.dedup();
    Ok(symbols)
}

pub fn load_output_with_watch(
    portfolio_id: Option<&str>,
    watch_symbols: &[String],
) -> Result<Output> {
    let _ = dotenv();
    let credentials = get_credentials()?;
    let client = build_client()?;
    let portfolios = fetch_portfolios(&client, &credentials)?;
    let portfolio = select_portfolio(&portfolios, portfolio_id)?;
    let positions = fetch_positions(&client, &credentials, &portfolio.uuid)?;
    let portfolio_states = fetch_portfolio_summary(&client, &credentials, &portfolio.uuid)?;
    let raw_open_orders = fetch_open_orders(&client, &credentials)?;

    let mut product_cache = HashMap::new();
    let mut product_book_cache = HashMap::new();
    let mut all_symbols = positions
        .iter()
        .map(|position| position.symbol.clone())
        .collect::<Vec<_>>();
    all_symbols.extend(watch_symbols.iter().cloned());
    all_symbols.extend(raw_open_orders.iter().map(|order| order.product_id.clone()));
    all_symbols.sort();
    all_symbols.dedup();
    let fetch_results = thread::scope(|scope| {
        let mut handles = Vec::new();
        for symbol in all_symbols.clone() {
            let client = client.clone();
            let credentials = credentials.clone();
            handles.push(scope.spawn(move || {
                let product = fetch_product(&client, &credentials, &symbol);
                let product_book = fetch_product_book(&client, &credentials, &symbol);
                (symbol, product, product_book)
            }));
        }

        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow!("product fetch worker panicked"))
            })
            .collect::<Result<Vec<_>>>()
    })?;

    for (symbol, product, product_book) in fetch_results {
        product_cache.insert(symbol.clone(), product);
        product_book_cache.insert(symbol, product_book);
    }

    let portfolio_state_lookup: HashMap<&str, &IntxPortfolioState> = portfolio_states
        .iter()
        .map(|state| (state.portfolio_uuid.as_str(), state))
        .collect();

    let active_position_symbols = positions
        .iter()
        .filter_map(|position| {
            normalize_side(position.position_side.as_deref())
                .map(|side| (position.symbol.clone(), side))
        })
        .collect::<HashMap<_, _>>();

    let positions = positions
        .into_iter()
        .map(|position| {
            let product = product_cache
                .get(&position.symbol)
                .and_then(|result| result.as_ref().ok());
            let product_book = product_book_cache
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
            summarize_position(position, product, product_book, portfolio_state)
        })
        .collect::<Vec<_>>();

    let mut open_orders = raw_open_orders
        .into_iter()
        .map(|order| summarize_open_order(order, &active_position_symbols))
        .collect::<Vec<_>>();
    open_orders.sort_by(|left, right| right.created_time.cmp(&left.created_time));

    let position_symbol_set = positions
        .iter()
        .map(|position| position.symbol.clone())
        .collect::<HashSet<_>>();
    let mut watch_symbols = watch_symbols.to_vec();
    watch_symbols.extend(open_orders.iter().map(|order| order.product_id.clone()));
    watch_symbols.sort();
    watch_symbols.dedup();
    let watch_markets = watch_symbols
        .into_iter()
        .filter(|symbol| !position_symbol_set.contains(symbol))
        .map(|symbol| {
            let product = product_cache.get(&symbol).and_then(|result| result.as_ref().ok());
            let product_book = product_book_cache
                .get(&symbol)
                .and_then(|result| result.as_ref().ok());
            summarize_watch_market(&symbol, product, product_book)
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
        open_orders,
        watch_markets,
    })
}

fn format_quote_notional(target_quote: f64) -> String {
    if (target_quote % 1_000.0).abs() < f64::EPSILON {
        format!("${:.0}k", target_quote / 1_000.0)
    } else {
        format!("${target_quote:.0}")
    }
}

fn render_slippage_estimate_cli(estimate: &SlippageEstimate) -> String {
    let mut summary = format!(
        "{} {}bps @{}",
        format_quote_notional(estimate.quote_notional),
        format_opt(estimate.slippage_bps, 2)
            .as_deref()
            .unwrap_or("unknown"),
        format_opt(estimate.average_price, 2)
            .as_deref()
            .unwrap_or("unknown"),
    );

    if !estimate.complete {
        summary.push_str(&format!(
            " (partial {}%)",
            format_opt(estimate.fill_pct, 1)
                .as_deref()
                .unwrap_or("unknown")
        ));
    }

    summary
}

fn render_slippage_side_cli(estimates: &[SlippageEstimate]) -> String {
    if estimates.is_empty() {
        return "unknown".to_string();
    }

    estimates
        .iter()
        .map(render_slippage_estimate_cli)
        .collect::<Vec<_>>()
        .join(" | ")
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
        "   Market: 24h={} | basis={} | funding={} ({}, {}) | openInterest={} (~{} notional, your share {}%) | maxLev={}x",
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
        format_opt(position.position_share_of_open_interest_pct, 2)
            .as_deref()
            .unwrap_or("unknown"),
        position.max_leverage.as_deref().unwrap_or("unknown"),
    ));
    if let Some(book) = position.order_book.as_ref() {
        lines.push(format!(
            "   Execution: bestBid={} | bestAsk={} | spread={} ({} bps) | bookLevels={}/{} | top5Depth={}/{} | imbalance={}",
            format_opt(book.best_bid, 2).as_deref().unwrap_or("unknown"),
            format_opt(book.best_ask, 2).as_deref().unwrap_or("unknown"),
            format_opt(book.spread_absolute, 4)
                .as_deref()
                .unwrap_or("unknown"),
            format_opt(book.spread_bps, 2).as_deref().unwrap_or("unknown"),
            book.bid_levels,
            book.ask_levels,
            format_opt(book.top_5_bid_notional, 0)
                .as_deref()
                .unwrap_or("unknown"),
            format_opt(book.top_5_ask_notional, 0)
                .as_deref()
                .unwrap_or("unknown"),
            format_pct(book.top_5_imbalance_pct)
                .as_deref()
                .unwrap_or("unknown"),
        ));
        lines.push(format!(
            "   Buy slip: {}",
            render_slippage_side_cli(&book.buy_slippage)
        ));
        lines.push(format!(
            "   Sell slip: {}",
            render_slippage_side_cli(&book.sell_slippage)
        ));
    }
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

fn render_open_order_lines(index: usize, order: &OpenOrderSummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Order {}. {} {} {}",
        index + 1,
        order.product_id,
        order.side.as_deref().unwrap_or("unknown"),
        order.status
    ));
    lines.push(format!(
        "   Config: {} | type={} | reduceOnly={} | base={} | limit={} | stop={} | trigger={} | created={}",
        order
            .configuration_label
            .as_deref()
            .unwrap_or("unknown"),
        order.order_type.as_deref().unwrap_or("unknown"),
        order
            .reduce_only
            .map(|value| value.to_string())
            .as_deref()
            .unwrap_or("unknown"),
        order.base_size.as_deref().unwrap_or("unknown"),
        order.limit_price.as_deref().unwrap_or("n/a"),
        order.stop_price.as_deref().unwrap_or("n/a"),
        order.stop_trigger_price.as_deref().unwrap_or("n/a"),
        order.created_time.as_deref().unwrap_or("unknown"),
    ));
    if let Some(reason) = order.cleanup_reason.as_deref() {
        lines.push(format!("   Cleanup: {reason}"));
    }
    lines.join("\n")
}

fn render_watch_market_lines(index: usize, watch: &WatchMarketSummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Watch {}. {}{}",
        index + 1,
        watch.symbol,
        watch
            .display_name
            .as_deref()
            .map(|item| format!(" ({item})"))
            .unwrap_or_default()
    ));
    lines.push(format!(
        "   Market: mark={} | index={} | 24h={} | basis={} | funding={} ({}, {}) | bias={}",
        watch.mark_price.as_deref().unwrap_or("unknown"),
        watch.index_price.as_deref().unwrap_or("unknown"),
        format_pct(watch.price_change_24h_pct)
            .as_deref()
            .unwrap_or("unknown"),
        format_pct(watch.basis_pct).as_deref().unwrap_or("unknown"),
        format_opt(watch.funding_rate_pct, 4)
            .map(|item| format!("{item}%"))
            .as_deref()
            .unwrap_or("unknown"),
        watch
            .funding_direction
            .as_deref()
            .unwrap_or("unknown funding"),
        watch
            .funding_intensity
            .as_deref()
            .unwrap_or("unclassified"),
        watch.market_bias,
    ));
    if let Some(book) = watch.order_book.as_ref() {
        lines.push(format!(
            "   Execution: spread={} bps | top5Imbalance={} | buy5k={} bps | buy10k={} bps",
            format_opt(book.spread_bps, 2).as_deref().unwrap_or("unknown"),
            format_pct(book.top_5_imbalance_pct)
                .as_deref()
                .unwrap_or("unknown"),
            book.buy_slippage
                .iter()
                .find(|estimate| (estimate.quote_notional - 5_000.0).abs() < 0.5)
                .and_then(|estimate| format_opt(estimate.slippage_bps, 2))
                .as_deref()
                .unwrap_or("unknown"),
            book.buy_slippage
                .iter()
                .find(|estimate| (estimate.quote_notional - 10_000.0).abs() < 0.5)
                .and_then(|estimate| format_opt(estimate.slippage_bps, 2))
                .as_deref()
                .unwrap_or("unknown"),
        ));
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
        format!("Open orders: {}", output.open_orders.len()),
    ];

    for (index, position) in output.positions.iter().enumerate() {
        lines.push(render_position_lines(index, position));
    }

    if output.positions.is_empty() {
        lines.push("No open perp positions found.".to_string());
    }

    if !output.open_orders.is_empty() {
        lines.push("Open orders:".to_string());
        for (index, order) in output.open_orders.iter().enumerate() {
            lines.push(render_open_order_lines(index, order));
        }
    }

    if !output.watch_markets.is_empty() {
        lines.push("Watch markets:".to_string());
        for (index, watch) in output.watch_markets.iter().enumerate() {
            lines.push(render_watch_market_lines(index, watch));
        }
    }

    lines.join("\n")
}
