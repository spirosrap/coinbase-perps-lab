use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::America::New_York;
use clap::Parser;
use coinbase_perps_lab::{
    load_output_with_watch, OrderBookSummary, Output, PositionSummary, SlippageEstimate,
    WatchMarketSummary,
};
use pdf_extract::extract_text_from_mem;
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HISTORY_POINTS: usize = 240;
const ROLLUP_BUCKET_MS: u64 = 5 * 60 * 1000;
const MAX_ROLLUP_BUCKETS: usize = 14 * 24 * 12;
const MAX_POINTS_PER_SERIES: usize = 120;
const DEFAULT_HISTORY_FILE: &str = ".local/perps_dashboard_history.json";
const MARKET_CONTEXT_TTL_MS: u64 = 30 * 60 * 1000;
const MODEL_PREDICTION_TTL_MS: u64 = 5 * 60 * 1000;
const MODEL_CANDLE_GRANULARITY: &str = "FIVE_MINUTE";
const MODEL_GRANULARITY_MINUTES: u32 = 5;
const MODEL_HORIZON_MINUTES: u32 = 60;
const MODEL_CANDLE_LIMIT: usize = 350;
const MODEL_CANDLE_WINDOWS: usize = 6;
const MODEL_ACTIVATION_ROLLUP_BUCKETS: usize = 120;
const MODEL_REVIEW_ROLLUP_BUCKETS: usize = 300;
const MODEL_SERIOUS_TRUST_ROLLUP_BUCKETS: usize = 960;
const FED_MONETARY_FEED_URL: &str = "https://www.federalreserve.gov/feeds/press_monetary.xml";
const FED_FOMC_CALENDAR_URL: &str = "https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm";
const OMB_PFEI_PDF_URL_PATTERN: &str =
    "https://www.whitehouse.gov/wp-content/uploads/{upload_year}/09/pfei_schedule_release_dates_cy{year}.pdf";
const GOOGLE_NEWS_GEOPOLITICS_RSS_URL: &str = "https://news.google.com/rss/search?q=%28Iran%20OR%20Israel%20OR%20Ukraine%20OR%20Russia%20OR%20China%20OR%20Taiwan%20OR%20oil%20OR%20tariffs%20OR%20sanctions%20OR%20shipping%29%20when%3A7d&hl=en-US&gl=US&ceid=US:en";
const US_EQUITY_EARNINGS_PROXY_TICKERS: [&str; 7] =
    ["AAPL", "MSFT", "NVDA", "AMZN", "META", "GOOGL", "TSLA"];

#[derive(Debug, Clone, Copy)]
enum PredictionTargetKind {
    FixedMinutes(u32),
    UsCashClose,
}

#[derive(Debug, Clone, Copy)]
struct PredictionTarget {
    label: &'static str,
    kind: PredictionTargetKind,
}

const MODEL_TARGETS: [PredictionTarget; 3] = [
    PredictionTarget {
        label: "1h",
        kind: PredictionTargetKind::FixedMinutes(60),
    },
    PredictionTarget {
        label: "4h",
        kind: PredictionTargetKind::FixedMinutes(240),
    },
    PredictionTarget {
        label: "next close",
        kind: PredictionTargetKind::UsCashClose,
    },
];

#[derive(Parser, Debug)]
#[command(about = "Serve a local web dashboard for Coinbase INTX perp analytics.")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:3000", help = "Bind address for the local dashboard")]
    bind: SocketAddr,
    #[arg(long, help = "Optional explicit INTX portfolio UUID")]
    portfolio: Option<String>,
    #[arg(
        long,
        default_value_t = 15,
        help = "Browser refresh interval in seconds for polling live data"
    )]
    refresh_seconds: u64,
    #[arg(
        long,
        default_value = DEFAULT_HISTORY_FILE,
        help = "Path to the local JSON file used to persist dashboard history"
    )]
    history_file: PathBuf,
}

struct AppState {
    portfolio: Option<String>,
    refresh_ms: u64,
    history_file: PathBuf,
    history: Mutex<HashMap<String, PersistedSymbolHistory>>,
    market_context_cache: Mutex<Option<CachedMarketContext>>,
    prediction_cache: Mutex<HashMap<String, CachedPrediction>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarketContextScope {
    include_equity_earnings_proxy: bool,
}

#[derive(Debug, Clone)]
struct CachedMarketContext {
    loaded_at_ms: u64,
    scope: MarketContextScope,
    context: MarketContext,
}

#[derive(Debug, Clone)]
struct CachedPrediction {
    loaded_at_ms: u64,
    history_signature: u64,
    prediction: ModelPrediction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PositionHistorySample {
    id: String,
    label: String,
    #[serde(default)]
    recorded_at_ms: u64,
    #[serde(default)]
    mark_price: Option<f64>,
    #[serde(default)]
    price_change_24h_pct: Option<f64>,
    #[serde(default)]
    basis_pct: Option<f64>,
    #[serde(default)]
    funding_rate_pct: Option<f64>,
    #[serde(default)]
    open_interest_notional: Option<f64>,
    #[serde(default)]
    event_risk_score: Option<f64>,
    #[serde(default)]
    scheduled_risk_score: Option<f64>,
    #[serde(default)]
    headline_risk_score: Option<f64>,
    spread_bps: Option<f64>,
    top_5_imbalance_pct: Option<f64>,
    buy_10k_bps: Option<f64>,
    buy_40k_bps: Option<f64>,
    sell_10k_bps: Option<f64>,
    sell_40k_bps: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedSymbolHistory {
    #[serde(default)]
    recent: Vec<PositionHistorySample>,
    #[serde(default)]
    rollups: Vec<HistoryRollupBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryRollupBucket {
    bucket_start_ms: u64,
    label: String,
    sample_count: usize,
    #[serde(default)]
    mark_price: RunningMetric,
    #[serde(default)]
    price_change_24h_pct: RunningMetric,
    #[serde(default)]
    basis_pct: RunningMetric,
    #[serde(default)]
    funding_rate_pct: RunningMetric,
    #[serde(default)]
    open_interest_notional: RunningMetric,
    #[serde(default)]
    event_risk_score: RunningMetric,
    #[serde(default)]
    scheduled_risk_score: RunningMetric,
    #[serde(default)]
    headline_risk_score: RunningMetric,
    #[serde(default)]
    spread_bps: RunningMetric,
    #[serde(default)]
    top_5_imbalance_pct: RunningMetric,
    #[serde(default)]
    buy_10k_bps: RunningMetric,
    #[serde(default)]
    buy_40k_bps: RunningMetric,
    #[serde(default)]
    sell_10k_bps: RunningMetric,
    #[serde(default)]
    sell_40k_bps: RunningMetric,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RunningMetric {
    sum: f64,
    count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedHistory {
    #[serde(default = "history_format_version")]
    version: u32,
    #[serde(default)]
    symbols: HashMap<String, PersistedSymbolHistory>,
}

#[derive(Debug, Deserialize)]
struct PersistedHistoryCompat {
    #[serde(default = "history_format_version")]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    symbols: HashMap<String, PersistedSymbolHistoryCompatEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PersistedSymbolHistoryCompatEntry {
    Legacy(Vec<PositionHistorySample>),
    Current(PersistedSymbolHistory),
}

#[derive(Debug, Serialize)]
struct DashboardSnapshot {
    #[serde(flatten)]
    output: Output,
    position_history: HashMap<String, PositionHistorySummary>,
    market_context: MarketContext,
    setup_assessments: HashMap<String, TradeSetupAssessment>,
    watch_assessments: HashMap<String, TradeSetupAssessment>,
    watch_entry_checklists: HashMap<String, EntryChecklist>,
    watch_entry_sizing_plans: HashMap<String, EntrySizingPlan>,
    position_predictions: HashMap<String, ModelPrediction>,
    watch_predictions: HashMap<String, ModelPrediction>,
}

#[derive(Debug, Serialize)]
struct PositionHistorySummary {
    samples: usize,
    approx_window_minutes: f64,
    latest_label: Option<String>,
    insights: Vec<String>,
    spread_bps: Option<MetricHistorySummary>,
    top_5_imbalance_pct: Option<MetricHistorySummary>,
    buy_10k_bps: Option<MetricHistorySummary>,
    buy_40k_bps: Option<MetricHistorySummary>,
    sell_10k_bps: Option<MetricHistorySummary>,
    sell_40k_bps: Option<MetricHistorySummary>,
    long_horizon: Option<LongHorizonSummary>,
}

#[derive(Debug, Serialize)]
struct LongHorizonSummary {
    buckets: usize,
    bucket_minutes: f64,
    approx_window_hours: f64,
    latest_label: Option<String>,
    insights: Vec<String>,
    spread_bps: Option<MetricHistorySummary>,
    top_5_imbalance_pct: Option<MetricHistorySummary>,
    buy_40k_bps: Option<MetricHistorySummary>,
    sell_40k_bps: Option<MetricHistorySummary>,
}

#[derive(Debug, Serialize)]
struct MetricHistorySummary {
    current: f64,
    average: f64,
    min: f64,
    max: f64,
    delta_from_oldest: f64,
    points: Vec<MetricPoint>,
}

#[derive(Debug, Clone, Serialize)]
struct MetricPoint {
    label: String,
    value: f64,
}

#[derive(Debug, Clone, Serialize)]
struct MarketContext {
    headlines: Vec<OfficialHeadline>,
    upcoming_events: Vec<UpcomingEvent>,
    scheduled_risk: String,
    headline_risk: String,
    event_risk: String,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OfficialHeadline {
    source: String,
    title: String,
    published_at: Option<String>,
    link: String,
}

#[derive(Debug, Serialize, Clone)]
struct UpcomingEvent {
    source: String,
    category: String,
    title: String,
    scheduled_for: String,
    days_until: Option<f64>,
    risk: String,
}

#[derive(Debug, Clone, Copy)]
struct ScheduledMacroSpec {
    row_name: &'static str,
    title: &'static str,
    category: &'static str,
    source: &'static str,
    high_window_days: f64,
    medium_window_days: f64,
}

const SCHEDULED_MACRO_SPECS: [ScheduledMacroSpec; 6] = [
    ScheduledMacroSpec {
        row_name: "Consumer Price Index",
        title: "Consumer Price Index (CPI)",
        category: "inflation",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 7.0,
    },
    ScheduledMacroSpec {
        row_name: "The Employment Situation",
        title: "Employment Situation (Jobs)",
        category: "labor",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 7.0,
    },
    ScheduledMacroSpec {
        row_name: "Personal Income and Outlays",
        title: "Personal Income and Outlays (PCE)",
        category: "inflation",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 7.0,
    },
    ScheduledMacroSpec {
        row_name: "Gross Domestic Product",
        title: "Gross Domestic Product (GDP)",
        category: "growth",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 5.0,
    },
    ScheduledMacroSpec {
        row_name: "Advance Monthly Sales for Retail and Food Services",
        title: "Advance Retail Sales",
        category: "consumer",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 3.0,
    },
    ScheduledMacroSpec {
        row_name: "Producer Price Indexes",
        title: "Producer Price Index (PPI)",
        category: "inflation",
        source: "White House / OIRA schedule",
        high_window_days: 1.0,
        medium_window_days: 3.0,
    },
];

#[derive(Debug, Serialize)]
struct TradeSetupAssessment {
    alignment_status: String,
    alignment_confidence: String,
    suggested_max_leverage: f64,
    event_risk: String,
    execution_risk: String,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EntryChecklist {
    overall_status: String,
    passed: usize,
    total: usize,
    summary: String,
    items: Vec<EntryChecklistItem>,
}

#[derive(Debug, Serialize)]
struct EntryChecklistItem {
    label: String,
    passed: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct EntrySizingPlan {
    status: String,
    margin_usage_pct: f64,
    reserve_pct: f64,
    leverage_fraction_pct: f64,
    suggested_actual_leverage: f64,
    summary: String,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ModelPrediction {
    horizon_label: String,
    variant: String,
    status: String,
    horizon_minutes: u32,
    granularity: String,
    evaluation_method: String,
    probability_up: Option<f64>,
    probability_down: Option<f64>,
    model_bias: String,
    confidence: String,
    training_samples: usize,
    test_samples: usize,
    test_accuracy: Option<f64>,
    baseline_accuracy: Option<f64>,
    edge_vs_baseline_pct_points: Option<f64>,
    brier_score: Option<f64>,
    holdout_up_rate: Option<f64>,
    majority_label: Option<String>,
    balanced_accuracy: Option<f64>,
    matthews_corrcoef: Option<f64>,
    readiness_stage: String,
    rollup_buckets_collected: usize,
    rollup_hours_collected: f64,
    buckets_until_activation: usize,
    hours_until_activation: f64,
    summary: String,
    notes: Vec<String>,
    additional_horizons: Vec<ModelPrediction>,
}

#[derive(Debug, Clone)]
struct LabeledFeatureSample {
    source_index: usize,
    target_index: usize,
    features: Vec<f64>,
    label: f64,
}

#[derive(Debug, Deserialize)]
struct CandleResponse {
    #[serde(default)]
    candles: Vec<RawCandle>,
}

#[derive(Debug, Deserialize)]
struct RawCandle {
    start: String,
    low: String,
    high: String,
    open: String,
    close: String,
    volume: String,
}

#[derive(Debug, Clone)]
struct CandleBar {
    start: i64,
    low: f64,
    high: f64,
    open: f64,
    close: f64,
    volume: f64,
}

fn history_format_version() -> u32 {
    3
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn history_signature(history: Option<&PersistedSymbolHistory>) -> u64 {
    let Some(history) = history else {
        return 0;
    };
    let recent_latest = history
        .recent
        .last()
        .map(|item| item.recorded_at_ms)
        .unwrap_or(0);
    let rollup_latest = history
        .rollups
        .last()
        .map(|item| item.bucket_start_ms)
        .unwrap_or(0);
    recent_latest
        ^ rollup_latest.rotate_left(13)
        ^ ((history.recent.len() as u64) << 1)
        ^ ((history.rollups.len() as u64) << 33)
}

fn rollup_buckets_to_hours(bucket_count: usize) -> f64 {
    bucket_count as f64 * (ROLLUP_BUCKET_MS as f64 / 3_600_000.0)
}

fn model_readiness_stage(bucket_count: usize) -> String {
    if bucket_count >= MODEL_SERIOUS_TRUST_ROLLUP_BUCKETS {
        "serious".to_string()
    } else if bucket_count >= MODEL_REVIEW_ROLLUP_BUCKETS {
        "reviewable".to_string()
    } else if bucket_count >= MODEL_ACTIVATION_ROLLUP_BUCKETS {
        "active but immature".to_string()
    } else {
        "collecting".to_string()
    }
}

fn apply_model_readiness(prediction: &mut ModelPrediction, history: Option<&PersistedSymbolHistory>) {
    let bucket_count = history.map(|item| item.rollups.len()).unwrap_or(0);
    let buckets_until_activation = MODEL_ACTIVATION_ROLLUP_BUCKETS.saturating_sub(bucket_count);
    prediction.readiness_stage = model_readiness_stage(bucket_count);
    prediction.rollup_buckets_collected = bucket_count;
    prediction.rollup_hours_collected = rollup_buckets_to_hours(bucket_count);
    prediction.buckets_until_activation = buckets_until_activation;
    prediction.hours_until_activation = rollup_buckets_to_hours(buckets_until_activation);
}

fn target_horizon_minutes(target: PredictionTarget) -> u32 {
    match target.kind {
        PredictionTargetKind::FixedMinutes(minutes) => minutes,
        PredictionTargetKind::UsCashClose => 0,
    }
}

fn next_us_trading_day(mut date: NaiveDate) -> NaiveDate {
    loop {
        date += Duration::days(1);
        match date.weekday() {
            Weekday::Sat | Weekday::Sun => continue,
            _ => return date,
        }
    }
}

fn next_us_cash_close_timestamp(after_ts: i64) -> Option<i64> {
    let current_utc = Utc.timestamp_opt(after_ts, 0).single()?;
    let current_local = current_utc.with_timezone(&New_York);
    let close_time = NaiveTime::from_hms_opt(16, 0, 0)?;
    let current_date = current_local.date_naive();
    let target_date = match current_local.weekday() {
        Weekday::Sat | Weekday::Sun => next_us_trading_day(current_date),
        _ if current_local.time() < close_time => current_date,
        _ => next_us_trading_day(current_date),
    };
    let target_local = New_York
        .from_local_datetime(&NaiveDateTime::new(target_date, close_time))
        .single()?;
    Some(target_local.with_timezone(&Utc).timestamp())
}

fn target_index_for_candle(
    candles: &[CandleBar],
    source_index: usize,
    target: PredictionTarget,
) -> Option<usize> {
    let source = candles.get(source_index)?;
    let target_index = match target.kind {
        PredictionTargetKind::FixedMinutes(minutes) => {
            let bars = (minutes / MODEL_GRANULARITY_MINUTES) as usize;
            source_index.checked_add(bars)?
        }
        PredictionTargetKind::UsCashClose => {
            let target_ts = next_us_cash_close_timestamp(source.start)?;
            candle_index_at_or_before(candles, target_ts)?
        }
    };
    (target_index > source_index && target_index < candles.len()).then_some(target_index)
}

fn empty_prediction_shell(
    target: PredictionTarget,
    variant: &str,
    status: &str,
    summary: String,
    notes: Vec<String>,
) -> ModelPrediction {
    ModelPrediction {
        horizon_label: target.label.to_string(),
        variant: variant.to_string(),
        status: status.to_string(),
        horizon_minutes: target_horizon_minutes(target),
        granularity: MODEL_CANDLE_GRANULARITY.to_string(),
        evaluation_method: "Expanding walk-forward on non-overlapping holdout anchors".to_string(),
        probability_up: None,
        probability_down: None,
        model_bias: "unknown".to_string(),
        confidence: "low".to_string(),
        training_samples: 0,
        test_samples: 0,
        test_accuracy: None,
        baseline_accuracy: None,
        edge_vs_baseline_pct_points: None,
        brier_score: None,
        holdout_up_rate: None,
        majority_label: None,
        balanced_accuracy: None,
        matthews_corrcoef: None,
        readiness_stage: "collecting".to_string(),
        rollup_buckets_collected: 0,
        rollup_hours_collected: 0.0,
        buckets_until_activation: MODEL_ACTIVATION_ROLLUP_BUCKETS,
        hours_until_activation: rollup_buckets_to_hours(MODEL_ACTIVATION_ROLLUP_BUCKETS),
        summary,
        notes,
        additional_horizons: Vec::new(),
    }
}

fn format_pct(value: Option<f64>) -> Option<String> {
    value.map(|item| format!("{item:.2}%"))
}

fn format_bps(value: Option<f64>) -> String {
    value.map(|item| format!("{item:.2} bps"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("coinbase-perps-lab/0.1 (+local dashboard)")
        .build()
        .context("failed to build HTTP client")
}

fn get_text(client: &Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("request failed for GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        anyhow::bail!("remote source returned {status} for GET {url}: {body}");
    }
    response
        .text()
        .with_context(|| format!("failed to read text body for GET {url}"))
}

fn get_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("request failed for GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        anyhow::bail!("remote source returned {status} for GET {url}: {body}");
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .with_context(|| format!("failed to read binary body for GET {url}"))
}

fn get_json<T>(client: &Client, url: &str, no_cache: bool) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut request = client.get(url);
    if no_cache {
        request = request.header("cache-control", "no-cache");
    }
    let response = request
        .send()
        .with_context(|| format!("request failed for GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        anyhow::bail!("remote source returned {status} for GET {url}: {body}");
    }
    response
        .json::<T>()
        .with_context(|| format!("failed to decode JSON body for GET {url}"))
}

fn parse_number(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

fn parse_timestamp(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

fn risk_score(value: &str) -> Option<f64> {
    match value {
        "low" => Some(0.0),
        "medium" => Some(1.0),
        "high" => Some(2.0),
        _ => None,
    }
}

fn fetch_public_product_candles(client: &Client, product_id: &str) -> Result<Vec<CandleBar>> {
    let end = Utc::now().timestamp();
    let span_seconds = MODEL_CANDLE_LIMIT as i64 * MODEL_GRANULARITY_MINUTES as i64 * 60;
    let mut candles = Vec::new();
    for window_index in 0..MODEL_CANDLE_WINDOWS {
        let window_end = end - (window_index as i64 * span_seconds);
        let window_start = window_end - span_seconds;
        let url = format!(
            "https://api.coinbase.com/api/v3/brokerage/market/products/{product_id}/candles?start={window_start}&end={window_end}&granularity={MODEL_CANDLE_GRANULARITY}&limit={MODEL_CANDLE_LIMIT}"
        );
        let response: CandleResponse = get_json(client, &url, true)?;
        candles.extend(response.candles.into_iter().filter_map(|item| {
            Some(CandleBar {
                start: parse_timestamp(&item.start)?,
                low: parse_number(&item.low)?,
                high: parse_number(&item.high)?,
                open: parse_number(&item.open)?,
                close: parse_number(&item.close)?,
                volume: parse_number(&item.volume).unwrap_or(0.0),
            })
        }));
    }
    candles.sort_by_key(|item| item.start);
    candles.dedup_by_key(|item| item.start);
    Ok(candles)
}

fn rolling_mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then_some(values.iter().sum::<f64>() / values.len() as f64)
}

fn rolling_std(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let mean = rolling_mean(values)?;
    let variance = values
        .iter()
        .map(|value| {
            let diff = value - mean;
            diff * diff
        })
        .sum::<f64>()
        / values.len() as f64;
    Some(variance.sqrt())
}

fn sigmoid(value: f64) -> f64 {
    if value >= 0.0 {
        let z = (-value).exp();
        1.0 / (1.0 + z)
    } else {
        let z = value.exp();
        z / (1.0 + z)
    }
}

fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter().zip(right).map(|(a, b)| a * b).sum::<f64>()
}

fn candle_index_at_or_before(candles: &[CandleBar], timestamp: i64) -> Option<usize> {
    match candles.binary_search_by_key(&timestamp, |item| item.start) {
        Ok(index) => Some(index),
        Err(0) => None,
        Err(index) => Some(index.saturating_sub(1)),
    }
}

fn recent_log_returns(candles: &[CandleBar], index: usize, lookback: usize) -> Option<Vec<f64>> {
    let returns = candles
        .get(index + 1 - lookback..=index)?
        .windows(2)
        .filter_map(|window| {
            let left = window.first()?.close;
            let right = window.get(1)?.close;
            (left > 0.0 && right > 0.0).then_some((right / left).ln())
        })
        .collect::<Vec<_>>();
    Some(returns)
}

fn candle_feature_vector(candles: &[CandleBar], index: usize) -> Option<Vec<f64>> {
    if index < 48 || index == 0 {
        return None;
    }
    let close = candles.get(index)?.close;
    let prev_close = candles.get(index - 1)?.close;
    let close_3 = candles.get(index - 3)?.close;
    let close_12 = candles.get(index - 12)?.close;
    let close_48 = candles.get(index - 48)?.close;
    let recent_12 = candles
        .get(index + 1 - 12..=index)?
        .iter()
        .map(|item| item.close)
        .collect::<Vec<_>>();
    let recent_48 = candles
        .get(index + 1 - 48..=index)?
        .iter()
        .map(|item| item.close)
        .collect::<Vec<_>>();
    let returns_12 = recent_log_returns(candles, index, 12)?;
    let returns_48 = recent_log_returns(candles, index, 48)?;
    let candle = candles.get(index)?;
    let sma_12 = rolling_mean(&recent_12)?;
    let sma_48 = rolling_mean(&recent_48)?;
    let vol_12 = rolling_std(&returns_12).unwrap_or(0.0);
    let vol_48 = rolling_std(&returns_48).unwrap_or(0.0);
    let avg_volume_48 = candles
        .get(index + 1 - 48..=index)?
        .iter()
        .map(|item| item.volume)
        .sum::<f64>()
        / 48.0;
    let volume_ratio = if avg_volume_48 > 0.0 {
        candle.volume / avg_volume_48 - 1.0
    } else {
        0.0
    };

    Some(vec![
        close / prev_close - 1.0,
        close / close_3 - 1.0,
        close / close_12 - 1.0,
        close / close_48 - 1.0,
        close / sma_12 - 1.0,
        close / sma_48 - 1.0,
        vol_12,
        vol_48,
        if candle.close > 0.0 {
            (candle.high - candle.low) / candle.close
        } else {
            0.0
        },
        if candle.open > 0.0 {
            candle.close / candle.open - 1.0
        } else {
            0.0
        },
        volume_ratio,
    ])
}

fn rollup_feature_vector(
    history: &PersistedSymbolHistory,
    candles: &[CandleBar],
    index: usize,
) -> Option<(Vec<f64>, usize)> {
    let bucket = history.rollups.get(index)?;
    let candle_index =
        candle_index_at_or_before(candles, (bucket.bucket_start_ms / 1000) as i64)?;
    let mut features = candle_feature_vector(candles, candle_index)?;
    features.push(bucket.spread_bps.average().unwrap_or(0.0));
    features.push(bucket.top_5_imbalance_pct.average().unwrap_or(0.0) / 100.0);
    features.push(bucket.buy_10k_bps.average().unwrap_or(0.0));
    features.push(bucket.buy_40k_bps.average().unwrap_or(0.0));
    features.push(bucket.sell_10k_bps.average().unwrap_or(0.0));
    features.push(bucket.sell_40k_bps.average().unwrap_or(0.0));
    features.push(bucket.basis_pct.average().unwrap_or(0.0) / 100.0);
    features.push(bucket.funding_rate_pct.average().unwrap_or(0.0) / 100.0);
    features.push(bucket.price_change_24h_pct.average().unwrap_or(0.0) / 100.0);
    features.push(bucket.open_interest_notional.average().unwrap_or(0.0).ln_1p());
    features.push(bucket.event_risk_score.average().unwrap_or(0.0) / 2.0);
    features.push(bucket.scheduled_risk_score.average().unwrap_or(0.0) / 2.0);
    features.push(bucket.headline_risk_score.average().unwrap_or(0.0) / 2.0);
    let bucket_mark = bucket.mark_price.average().unwrap_or(0.0);
    let candle_close = candles.get(candle_index)?.close;
    features.push(if bucket_mark > 0.0 {
        candle_close / bucket_mark - 1.0
    } else {
        0.0
    });
    Some((features, candle_index))
}

fn classify_model_confidence(probability_up: f64, test_accuracy: Option<f64>, baseline_accuracy: Option<f64>) -> String {
    let edge = (probability_up - 0.5).abs();
    let accuracy_edge = test_accuracy
        .zip(baseline_accuracy)
        .map(|(test, base)| test - base)
        .unwrap_or(0.0);
    if edge >= 0.12 && accuracy_edge >= 0.04 {
        "high".to_string()
    } else if edge >= 0.07 && accuracy_edge >= 0.02 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn classify_model_bias(probability_up: f64, confidence: &str) -> String {
    if confidence == "low" || (0.47..=0.53).contains(&probability_up) {
        "neutral".to_string()
    } else if probability_up >= 0.53 {
        "bullish".to_string()
    } else {
        "bearish".to_string()
    }
}

#[derive(Debug, Clone)]
struct LogisticModel {
    weights: Vec<f64>,
    bias: f64,
    means: Vec<f64>,
    stds: Vec<f64>,
}

#[derive(Debug, Clone)]
struct ModelEvaluation {
    evaluation_method: String,
    test_samples: usize,
    test_accuracy: Option<f64>,
    baseline_accuracy: Option<f64>,
    edge_vs_baseline_pct_points: Option<f64>,
    brier_score: Option<f64>,
    holdout_up_rate: Option<f64>,
    majority_label: Option<String>,
    balanced_accuracy: Option<f64>,
    matthews_corrcoef: Option<f64>,
}

impl LogisticModel {
    fn normalize_row(&self, row: &[f64]) -> Vec<f64> {
        row.iter()
            .enumerate()
            .map(|(index, value)| (value - self.means[index]) / self.stds[index])
            .collect()
    }

    fn predict_probability(&self, row: &[f64]) -> f64 {
        let normalized = self.normalize_row(row);
        sigmoid(dot(&self.weights, &normalized) + self.bias)
    }
}

fn model_split_index(sample_count: usize) -> Option<usize> {
    if sample_count < 120 {
        return None;
    }
    Some(
        (((sample_count as f64) * 0.8).floor() as usize)
            .clamp(80, sample_count.saturating_sub(24)),
    )
}

fn fit_logistic_model(features: &[Vec<f64>], labels: &[f64]) -> Option<LogisticModel> {
    if features.is_empty() || features.len() != labels.len() {
        return None;
    }
    let feature_count = features.first()?.len();
    if feature_count == 0 {
        return None;
    }

    let mut means = vec![0.0; feature_count];
    let mut stds = vec![1.0; feature_count];
    for feature_index in 0..feature_count {
        let column = features
            .iter()
            .map(|row| row[feature_index])
            .collect::<Vec<_>>();
        means[feature_index] = rolling_mean(&column).unwrap_or(0.0);
        stds[feature_index] = rolling_std(&column)
            .filter(|value| *value > 1e-9)
            .unwrap_or(1.0);
    }

    let normalized_features = features
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, value)| (value - means[index]) / stds[index])
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut weights = vec![0.0; feature_count];
    let mut bias = 0.0;
    let learning_rate = 0.12;
    let l2 = 0.0005;
    for _ in 0..700 {
        let mut grad_w = vec![0.0; feature_count];
        let mut grad_b = 0.0;
        for (row, label) in normalized_features.iter().zip(labels.iter()) {
            let prediction = sigmoid(dot(&weights, row) + bias);
            let error = prediction - label;
            for (index, value) in row.iter().enumerate() {
                grad_w[index] += error * value;
            }
            grad_b += error;
        }
        let n = normalized_features.len() as f64;
        for index in 0..feature_count {
            grad_w[index] = grad_w[index] / n + l2 * weights[index];
            weights[index] -= learning_rate * grad_w[index];
        }
        bias -= learning_rate * (grad_b / n);
    }

    Some(LogisticModel {
        weights,
        bias,
        means,
        stds,
    })
}

fn evaluate_walk_forward(samples: &[LabeledFeatureSample]) -> Option<ModelEvaluation> {
    let split_index = model_split_index(samples.len())?;
    let mut predictions = Vec::new();
    let mut holdout_labels = Vec::new();
    let mut last_target_index = None;
    let mut test_index = split_index;

    while test_index < samples.len() {
        let sample = samples.get(test_index)?;
        if last_target_index
            .map(|last| sample.source_index <= last)
            .unwrap_or(false)
        {
            test_index += 1;
            continue;
        }

        let training = samples[..test_index]
            .iter()
            .filter(|candidate| candidate.target_index < sample.source_index)
            .collect::<Vec<_>>();
        if training.len() < 80 {
            test_index += 1;
            continue;
        }

        let train_features = training
            .iter()
            .map(|item| item.features.clone())
            .collect::<Vec<_>>();
        let train_labels = training
            .iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        let model = fit_logistic_model(&train_features, &train_labels)?;
        predictions.push(model.predict_probability(&sample.features));
        holdout_labels.push(sample.label);
        last_target_index = Some(sample.target_index);
        test_index += 1;
    }

    if predictions.is_empty() || predictions.len() != holdout_labels.len() {
        return None;
    }

    let mut correct = 0usize;
    let mut positives = 0usize;
    let mut squared_error = 0.0;
    let mut tp = 0usize;
    let mut tn = 0usize;
    let mut fp = 0usize;
    let mut fn_ = 0usize;

    for (probability, label) in predictions.iter().zip(holdout_labels.iter()) {
        let predicted_up = *probability >= 0.5;
        let actual_up = *label >= 0.5;
        if predicted_up == actual_up {
            correct += 1;
        }
        if actual_up {
            positives += 1;
        }
        squared_error += (probability - label).powi(2);
        match (predicted_up, actual_up) {
            (true, true) => tp += 1,
            (false, false) => tn += 1,
            (true, false) => fp += 1,
            (false, true) => fn_ += 1,
        }
    }

    let test_samples = holdout_labels.len();
    let positives_f = positives as f64;
    let negatives = test_samples.saturating_sub(positives);
    let negatives_f = negatives as f64;
    let test_accuracy = Some(correct as f64 / test_samples as f64);
    let baseline_accuracy =
        Some((positives.max(negatives) as f64) / test_samples as f64);
    let holdout_up_rate = Some(positives as f64 / test_samples as f64);
    let majority_label = if positives >= negatives {
        Some("up".to_string())
    } else {
        Some("down".to_string())
    };
    let brier_score = Some(squared_error / test_samples as f64);
    let tpr = (positives > 0).then_some(tp as f64 / positives_f);
    let tnr = (negatives > 0).then_some(tn as f64 / negatives_f);
    let balanced_accuracy = tpr.zip(tnr).map(|(up, down)| (up + down) / 2.0);
    let mcc_denominator =
        ((tp + fp) as f64 * (tp + fn_) as f64 * (tn + fp) as f64 * (tn + fn_) as f64).sqrt();
    let matthews_corrcoef = (mcc_denominator > 0.0).then_some(
        ((tp * tn) as f64 - (fp * fn_) as f64) / mcc_denominator,
    );

    Some(ModelEvaluation {
        evaluation_method:
            "Expanding walk-forward on non-overlapping holdout anchors".to_string(),
        test_samples,
        test_accuracy,
        baseline_accuracy,
        edge_vs_baseline_pct_points: test_accuracy
            .zip(baseline_accuracy)
            .map(|(test, base)| (test - base) * 100.0),
        brier_score,
        holdout_up_rate,
        majority_label,
        balanced_accuracy,
        matthews_corrcoef,
    })
}

fn candle_samples_for_target(
    candles: &[CandleBar],
    target: PredictionTarget,
) -> Vec<LabeledFeatureSample> {
    let mut samples = Vec::new();
    for source_index in 48..candles.len() {
        let Some(features) = candle_feature_vector(candles, source_index) else {
            continue;
        };
        let Some(target_index) = target_index_for_candle(candles, source_index, target) else {
            continue;
        };
        let current_close = candles.get(source_index).map(|item| item.close).unwrap_or(0.0);
        let future_close = candles.get(target_index).map(|item| item.close).unwrap_or(0.0);
        if current_close <= 0.0 || future_close <= 0.0 {
            continue;
        }
        samples.push(LabeledFeatureSample {
            source_index,
            target_index,
            features,
            label: (future_close > current_close) as u8 as f64,
        });
    }
    samples
}

fn rollup_samples_for_target(
    history: &PersistedSymbolHistory,
    candles: &[CandleBar],
    target: PredictionTarget,
) -> (Vec<LabeledFeatureSample>, Option<Vec<f64>>) {
    let mut samples = Vec::new();
    let mut latest_features = None;
    for rollup_index in 0..history.rollups.len() {
        let Some((features, source_index)) = rollup_feature_vector(history, candles, rollup_index) else {
            continue;
        };
        latest_features = Some(features.clone());
        let Some(target_index) = target_index_for_candle(candles, source_index, target) else {
            continue;
        };
        let current_close = candles.get(source_index).map(|item| item.close).unwrap_or(0.0);
        let future_close = candles.get(target_index).map(|item| item.close).unwrap_or(0.0);
        if current_close <= 0.0 || future_close <= 0.0 {
            continue;
        }
        samples.push(LabeledFeatureSample {
            source_index,
            target_index,
            features,
            label: (future_close > current_close) as u8 as f64,
        });
    }
    (samples, latest_features)
}

fn build_candle_only_model_prediction(
    candles: &[CandleBar],
    product_id: &str,
    target: PredictionTarget,
) -> Result<ModelPrediction> {
    let samples = candle_samples_for_target(candles, target);
    if samples.len() < 120 {
        return Ok(empty_prediction_shell(
            target,
            "candle_only",
            "not_enough_data",
            format!(
                "Not enough candle-only feature/label examples are available yet for the {} model.",
                target.label
            ),
            vec![
                "The fallback candle-only model needs substantially more 5-minute candles to evaluate this horizon."
                    .to_string(),
            ],
        ));
    }

    let evaluation = evaluate_walk_forward(&samples)
        .context("failed to evaluate candle-only walk-forward metrics")?;
    let latest_features = candle_feature_vector(candles, candles.len() - 1)
        .context("failed to build latest candle-only feature vector")?;
    let latest_model = fit_logistic_model(
        &samples.iter().map(|item| item.features.clone()).collect::<Vec<_>>(),
        &samples.iter().map(|item| item.label).collect::<Vec<_>>(),
    )
    .context("failed to fit latest candle-only model")?;
    let raw_probability_up = latest_model.predict_probability(&latest_features);
    let validation_edge = evaluation
        .test_accuracy
        .zip(evaluation.baseline_accuracy)
        .map(|(test, base)| test - base)
        .unwrap_or(0.0);
    let has_edge = validation_edge > 0.0;
    let probability_up = if has_edge {
        raw_probability_up.clamp(0.05, 0.95)
    } else {
        0.5
    };
    let probability_down = 1.0 - probability_up;
    let confidence = classify_model_confidence(
        probability_up,
        evaluation.test_accuracy,
        evaluation.baseline_accuracy,
    );
    let model_bias = classify_model_bias(probability_up, &confidence);
    let status = if has_edge { "available" } else { "no_edge" }.to_string();

    let mut prediction = empty_prediction_shell(
        target,
        "candle_only",
        &status,
        if has_edge {
            format!(
                "Experimental candle-only baseline estimates a {:.1}% probability that {product_id} is higher by {}.",
                probability_up * 100.0,
                target.label
            )
        } else {
            format!(
                "Experimental candle-only baseline does not currently beat a naive holdout baseline for {product_id} at {}.",
                target.label
            )
        },
        Vec::new(),
    );
    prediction.evaluation_method = evaluation.evaluation_method;
    prediction.probability_up = Some(probability_up);
    prediction.probability_down = Some(probability_down);
    prediction.model_bias = model_bias;
    prediction.confidence = confidence;
    prediction.training_samples = samples.len();
    prediction.test_samples = evaluation.test_samples;
    prediction.test_accuracy = evaluation.test_accuracy;
    prediction.baseline_accuracy = evaluation.baseline_accuracy;
    prediction.edge_vs_baseline_pct_points = evaluation.edge_vs_baseline_pct_points;
    prediction.brier_score = evaluation.brier_score;
    prediction.holdout_up_rate = evaluation.holdout_up_rate;
    prediction.majority_label = evaluation.majority_label;
    prediction.balanced_accuracy = evaluation.balanced_accuracy;
    prediction.matthews_corrcoef = evaluation.matthews_corrcoef;
    prediction.notes = vec![
        format!(
            "Experimental candle-only baseline trained on {} examples for the {} horizon.",
            samples.len(),
            target.label
        ),
        "Features are limited to Coinbase public candle price/volatility/volume.".to_string(),
    ];
    Ok(prediction)
}

fn build_history_augmented_model_prediction(
    history: &PersistedSymbolHistory,
    candles: &[CandleBar],
    product_id: &str,
    target: PredictionTarget,
) -> Result<Option<ModelPrediction>> {
    let (samples, latest_features) = rollup_samples_for_target(history, candles, target);

    let Some(latest_features) = latest_features else {
        return Ok(None);
    };
    if samples.len() < 120 {
        return Ok(None);
    }

    let mut prediction = build_candle_only_model_prediction(candles, product_id, target)?;

    let evaluation = evaluate_walk_forward(&samples)
        .context("failed to evaluate history-augmented walk-forward metrics")?;
    let latest_model = fit_logistic_model(
        &samples.iter().map(|item| item.features.clone()).collect::<Vec<_>>(),
        &samples.iter().map(|item| item.label).collect::<Vec<_>>(),
    )
        .context("failed to fit latest history-augmented model")?;
    let raw_probability_up = latest_model.predict_probability(&latest_features);
    let validation_edge = evaluation
        .test_accuracy
        .zip(evaluation.baseline_accuracy)
        .map(|(test, base)| test - base)
        .unwrap_or(0.0);
    let has_edge = validation_edge > 0.0;
    let probability_up = if has_edge {
        raw_probability_up.clamp(0.05, 0.95)
    } else {
        0.5
    };
    let probability_down = 1.0 - probability_up;
    let confidence = classify_model_confidence(
        probability_up,
        evaluation.test_accuracy,
        evaluation.baseline_accuracy,
    );
    let model_bias = classify_model_bias(probability_up, &confidence);
    let status = if has_edge { "available" } else { "no_edge" }.to_string();

    prediction.status = status;
    prediction.variant = "history_augmented".to_string();
    prediction.evaluation_method = evaluation.evaluation_method;
    prediction.probability_up = Some(probability_up);
    prediction.probability_down = Some(probability_down);
    prediction.model_bias = model_bias;
    prediction.confidence = confidence;
    prediction.training_samples = samples.len();
    prediction.test_samples = evaluation.test_samples;
    prediction.test_accuracy = evaluation.test_accuracy;
    prediction.baseline_accuracy = evaluation.baseline_accuracy;
    prediction.edge_vs_baseline_pct_points = evaluation.edge_vs_baseline_pct_points;
    prediction.brier_score = evaluation.brier_score;
    prediction.holdout_up_rate = evaluation.holdout_up_rate;
    prediction.majority_label = evaluation.majority_label.clone();
    prediction.balanced_accuracy = evaluation.balanced_accuracy;
    prediction.matthews_corrcoef = evaluation.matthews_corrcoef;
    prediction.summary = if has_edge {
        format!(
            "History-augmented baseline estimates a {:.1}% probability that {product_id} is higher by {}.",
            probability_up * 100.0,
            target.label
        )
    } else {
        format!(
            "History-augmented baseline does not currently beat a naive holdout baseline for {product_id} at {}.",
            target.label
        )
    };
    prediction.notes = vec![
        format!(
            "History-augmented baseline trained on {} locally persisted 5-minute rollup examples for the {} horizon.",
            samples.len(),
            target.label
        ),
        format!(
            "Features combine local rollup microstructure, funding, basis, open interest, and market-context risk scores with Coinbase public 5-minute candle momentum/volatility."
        ),
        format!(
            "Evaluation uses expanding walk-forward scoring on non-overlapping {} holdout anchors.",
            target.label
        ),
        format!(
            "Walk-forward holdout accuracy is {:.1}% versus a {:.1}% majority-class baseline.",
            evaluation.test_accuracy.unwrap_or(0.0) * 100.0,
            evaluation.baseline_accuracy.unwrap_or(0.0) * 100.0
        ),
        format!(
            "The non-overlapping holdout slice is {:.1}% up labels, so the naive baseline mostly wins by always predicting {}.",
            evaluation.holdout_up_rate.unwrap_or(0.0) * 100.0,
            evaluation
                .majority_label
                .as_deref()
                .unwrap_or("the majority side")
        ),
        if has_edge {
            format!(
                "The model is ahead of baseline by {:.1} percentage points on the holdout window.",
                validation_edge * 100.0
            )
        } else {
            "The raw model fit does not currently beat a naive holdout baseline, so the directional output is neutralized to 50/50."
                .to_string()
        },
        "This model should improve as the dashboard keeps collecting richer local history across more sessions and regimes."
            .to_string(),
    ];

    Ok(Some(prediction))
}

fn build_model_prediction(
    client: &Client,
    product_id: &str,
    history: Option<&PersistedSymbolHistory>,
) -> Result<ModelPrediction> {
    let candles = fetch_public_product_candles(client, product_id)?;
    let mut built = Vec::new();
    for target in MODEL_TARGETS {
        let mut prediction = if let Some(history) = history {
            if let Some(prediction) =
                build_history_augmented_model_prediction(history, &candles, product_id, target)?
            {
                prediction
            } else {
                build_candle_only_model_prediction(&candles, product_id, target)?
            }
        } else {
            build_candle_only_model_prediction(&candles, product_id, target)?
        };
        apply_model_readiness(&mut prediction, history);
        if prediction.status != "not_enough_data" && prediction.variant == "candle_only" {
            let rollup_count = history.map(|item| item.rollups.len()).unwrap_or(0);
            prediction.notes.push(format!(
                "The richer history-augmented model is not active yet for {} because there are only {rollup_count} persisted local rollup buckets.",
                target.label
            ));
        }
        built.push(prediction);
    }

    let mut built_iter = built.into_iter();
    let mut primary = built_iter
        .next()
        .context("no model targets were configured")?;
    primary.additional_horizons = built_iter.collect();
    Ok(primary)
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#8211;", "–")
}

fn parse_rss_items(xml: &str, default_source: &str, split_title_source: bool) -> Vec<OfficialHeadline> {
    let item_re = Regex::new(r"(?s)<item>(.*?)</item>").unwrap();
    let title_re = Regex::new(r"(?s)<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>").unwrap();
    let link_re = Regex::new(r"(?s)<link>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</link>").unwrap();
    let pub_date_re = Regex::new(r"(?s)<pubDate>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</pubDate>").unwrap();

    item_re
        .captures_iter(xml)
        .filter_map(|caps| {
            let item = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let raw_title = title_re
                .captures(item)
                .and_then(|caps| caps.get(1).map(|m| decode_html_entities(m.as_str())))?;
            let link = link_re
                .captures(item)
                .and_then(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()))?;
            let published_at = pub_date_re
                .captures(item)
                .and_then(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()));

            let (title, source) = if split_title_source {
                split_news_title_source(&raw_title, default_source)
            } else {
                (raw_title, default_source.to_string())
            };

            Some(OfficialHeadline {
                source,
                title,
                published_at,
                link,
            })
        })
        .collect()
}

fn split_news_title_source(raw_title: &str, default_source: &str) -> (String, String) {
    if let Some((title, source)) = raw_title.rsplit_once(" - ") {
        let cleaned_source = source.trim();
        if !cleaned_source.is_empty() && cleaned_source.len() <= 48 {
            return (title.trim().to_string(), cleaned_source.to_string());
        }
    }
    (raw_title.trim().to_string(), default_source.to_string())
}

fn parse_fed_headlines(client: &Client) -> Result<Vec<OfficialHeadline>> {
    let xml = get_text(client, FED_MONETARY_FEED_URL)?;
    Ok(parse_rss_items(&xml, "Federal Reserve", false)
        .into_iter()
        .take(4)
        .collect())
}

fn parse_geopolitical_headlines(client: &Client) -> Result<Vec<OfficialHeadline>> {
    let xml = get_text(client, GOOGLE_NEWS_GEOPOLITICS_RSS_URL)?;
    Ok(parse_rss_items(&xml, "Google News", true)
        .into_iter()
        .take(4)
        .collect())
}

fn month_name_to_number(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

fn date_from_month_and_day(year: i32, month_label: &str, day: u32) -> Option<NaiveDate> {
    let cleaned = month_label.split('/').next().unwrap_or(month_label).trim();
    let month = month_name_to_number(cleaned)?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn classify_event_risk(days_until: f64, high_window_days: f64, medium_window_days: f64) -> String {
    if days_until <= high_window_days {
        "high".to_string()
    } else if days_until <= medium_window_days {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn pfei_pdf_url_for_year(year: i32) -> String {
    OMB_PFEI_PDF_URL_PATTERN
        .replace("{upload_year}", &(year - 1).to_string())
        .replace("{year}", &year.to_string())
}

fn parse_pfei_schedule_text(client: &Client, year: i32) -> Result<String> {
    let url = pfei_pdf_url_for_year(year);
    let bytes = get_bytes(client, &url)?;
    extract_text_from_mem(&bytes).with_context(|| format!("failed to extract text from {url}"))
}

fn extract_indicator_day_tokens(schedule_text: &str, row_name: &str) -> Result<Vec<Option<u32>>> {
    let start = schedule_text
        .find(row_name)
        .with_context(|| format!("failed to locate {row_name} in principal indicators schedule"))?;
    let end = (start + 600).min(schedule_text.len());
    let block = &schedule_text[start..end];
    let quarter_re = Regex::new(r"\b\dQ'\d{2}\b").unwrap();
    let cleaned = quarter_re.replace_all(block, " ");
    let token_re = Regex::new(r"\b\d{1,2}\b|--").unwrap();

    let tokens = token_re
        .find_iter(&cleaned)
        .map(|item| match item.as_str() {
            "--" => None,
            value => value.parse::<u32>().ok(),
        })
        .take(12)
        .collect::<Vec<_>>();

    if tokens.len() < 12 {
        anyhow::bail!(
            "expected 12 month tokens for {row_name}, found {}",
            tokens.len()
        );
    }

    Ok(tokens)
}

fn next_scheduled_release_date(
    year: i32,
    month_tokens: &[Option<u32>],
    today: NaiveDate,
) -> Option<NaiveDate> {
    for month in today.month()..=12 {
        let Some(token) = month_tokens.get(month as usize - 1).copied().flatten() else {
            continue;
        };
        let Some(date) = NaiveDate::from_ymd_opt(year, month, token) else {
            continue;
        };
        if date >= today {
            return Some(date);
        }
    }
    None
}

fn parse_scheduled_macro_events(client: &Client) -> Result<Vec<UpcomingEvent>> {
    let year = Utc::now().year();
    let today = Utc::now().date_naive();
    let schedule_text = parse_pfei_schedule_text(client, year)?;
    let mut events = Vec::new();

    for spec in SCHEDULED_MACRO_SPECS {
        let month_tokens = extract_indicator_day_tokens(&schedule_text, spec.row_name)?;
        let Some(date) = next_scheduled_release_date(year, &month_tokens, today) else {
            continue;
        };
        let days_until = (date - today).num_days() as f64;
        events.push(UpcomingEvent {
            source: spec.source.to_string(),
            category: spec.category.to_string(),
            title: spec.title.to_string(),
            scheduled_for: date.to_string(),
            days_until: Some(days_until),
            risk: classify_event_risk(days_until, spec.high_window_days, spec.medium_window_days),
        });
    }

    Ok(events)
}

fn parse_fomc_events(client: &Client) -> Result<Vec<UpcomingEvent>> {
    let html = get_text(client, FED_FOMC_CALENDAR_URL)?;
    let year_re = Regex::new(r#"<a id="42828">(\d{4}) FOMC Meetings</a>"#).unwrap();
    let year = year_re
        .captures(&html)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<i32>().ok())
        .unwrap_or(Utc::now().year());
    let row_re = Regex::new(
        r#"(?s)fomc-meeting__month[^>]*><strong>([^<]+)</strong>.*?fomc-meeting__date[^>]*>([^<]+)</div>"#,
    )
    .unwrap();

    let today = Utc::now().date_naive();
    let mut events = Vec::new();
    for caps in row_re.captures_iter(&html) {
        let month_label = caps.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
        let raw_date = caps.get(2).map(|m| m.as_str().trim()).unwrap_or_default();
        let first_day = raw_date
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .ok();
        let Some(first_day) = first_day else {
            continue;
        };
        let Some(date) = date_from_month_and_day(year, month_label, first_day) else {
            continue;
        };
        if date < today {
            continue;
        }

        let days_until = (date - today).num_days() as f64;
        let risk = if days_until <= 1.0 {
            "high"
        } else if days_until <= 7.0 {
            "medium"
        } else {
            "low"
        };
        events.push(UpcomingEvent {
            source: "Federal Reserve".to_string(),
            category: "policy".to_string(),
            title: format!("FOMC meeting ({month_label} {raw_date})"),
            scheduled_for: date.to_string(),
            days_until: Some(days_until),
            risk: risk.to_string(),
        });
    }

    Ok(events.into_iter().take(3).collect())
}

fn parse_csv_row(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

fn parse_equity_earnings_proxy_events(client: &Client) -> Result<Vec<UpcomingEvent>> {
    let api_key = env::var("ALPHA_VANTAGE_API_KEY")
        .context("ALPHA_VANTAGE_API_KEY is not set for earnings proxy coverage")?;
    let url = format!(
        "https://www.alphavantage.co/query?function=EARNINGS_CALENDAR&horizon=3month&apikey={api_key}"
    );
    let csv = get_text(client, &url)?;
    let today = Utc::now().date_naive();
    let watchlist = US_EQUITY_EARNINGS_PROXY_TICKERS
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let mut selected = HashMap::<String, UpcomingEvent>::new();

    for line in csv.lines().skip(1) {
        let fields = parse_csv_row(line);
        if fields.len() < 3 {
            continue;
        }
        if fields[0] == "Information" || fields[0] == "Note" || fields[0].is_empty() {
            anyhow::bail!("unexpected Alpha Vantage earnings calendar response");
        }
        let symbol = fields[0].trim();
        if !watchlist.contains(symbol) {
            continue;
        }
        if selected.contains_key(symbol) {
            continue;
        }
        let Ok(report_date) = NaiveDate::parse_from_str(fields[2].trim(), "%Y-%m-%d") else {
            continue;
        };
        if report_date < today {
            continue;
        }
        let days_until = (report_date - today).num_days() as f64;
        let company_name = fields
            .get(1)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(symbol);
        selected.insert(
            symbol.to_string(),
            UpcomingEvent {
                source: "Alpha Vantage earnings proxy".to_string(),
                category: "earnings".to_string(),
                title: format!("{symbol} earnings ({company_name})"),
                scheduled_for: report_date.to_string(),
                days_until: Some(days_until),
                risk: classify_event_risk(days_until, 1.0, 7.0),
            },
        );
    }

    let mut events = selected.into_values().collect::<Vec<_>>();
    events.sort_by(|left, right| left.scheduled_for.cmp(&right.scheduled_for));
    events.truncate(6);
    Ok(events)
}

fn derive_risk_level(values: &[String]) -> String {
    if values.iter().any(|value| value == "high") {
        "high".to_string()
    } else if values.iter().any(|value| value == "medium") {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn derive_event_risk(upcoming_events: &[UpcomingEvent]) -> String {
    derive_risk_level(
        &upcoming_events
            .iter()
            .map(|event| event.risk.clone())
            .collect::<Vec<_>>(),
    )
}

fn geopolitical_keyword_score(title: &str) -> usize {
    let lower = title.to_ascii_lowercase();
    let mut score = 0usize;
    for keyword in [
        "war",
        "missile",
        "strike",
        "attack",
        "invasion",
        "nuclear",
        "bomb",
        "blockade",
        "retaliat",
    ] {
        if lower.contains(keyword) {
            score += 2;
        }
    }
    for keyword in [
        "iran",
        "israel",
        "ukraine",
        "russia",
        "china",
        "taiwan",
        "sanction",
        "tariff",
        "oil",
        "shipping",
        "troop",
        "military",
        "conflict",
    ] {
        if lower.contains(keyword) {
            score += 1;
        }
    }
    score
}

fn derive_headline_risk(headlines: &[OfficialHeadline]) -> String {
    let mut high_hits = 0usize;
    let mut medium_hits = 0usize;
    for headline in headlines.iter().take(4) {
        let score = geopolitical_keyword_score(&headline.title);
        if score >= 4 {
            high_hits += 1;
        } else if score >= 2 {
            medium_hits += 1;
        }
    }
    if high_hits >= 2 {
        "high".to_string()
    } else if high_hits >= 1 || medium_hits >= 2 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn build_market_context_scope(output: &Output) -> MarketContextScope {
    let include_equity_earnings_proxy = output
        .positions
        .iter()
        .filter_map(|position| position.underlying_type.as_deref())
        .chain(
            output
                .watch_markets
                .iter()
                .filter_map(|watch| watch.underlying_type.as_deref()),
        )
        .any(|underlying| underlying == "EQUITY_ETF" || underlying == "EQUITY");

    MarketContextScope {
        include_equity_earnings_proxy,
    }
}

fn load_prediction_for_symbol(
    cache: &Mutex<HashMap<String, CachedPrediction>>,
    symbol: &str,
    history: Option<&PersistedSymbolHistory>,
) -> Result<ModelPrediction> {
    let history_signature = history_signature(history);
    if let Ok(guard) = cache.lock() {
        if let Some(entry) = guard.get(symbol) {
            if entry.history_signature == history_signature
                && now_millis().saturating_sub(entry.loaded_at_ms) <= MODEL_PREDICTION_TTL_MS
            {
                return Ok(entry.prediction.clone());
            }
        }
    }

    let client = build_http_client()?;
    let prediction = build_model_prediction(&client, symbol, history).unwrap_or_else(|error| ModelPrediction {
        horizon_label: "1h".to_string(),
        variant: "error".to_string(),
        status: "error".to_string(),
        horizon_minutes: MODEL_HORIZON_MINUTES,
        granularity: MODEL_CANDLE_GRANULARITY.to_string(),
        evaluation_method: "Expanding walk-forward on non-overlapping 60-minute holdout anchors".to_string(),
        probability_up: None,
        probability_down: None,
        model_bias: "unknown".to_string(),
        confidence: "low".to_string(),
        training_samples: 0,
        test_samples: 0,
        test_accuracy: None,
        baseline_accuracy: None,
        edge_vs_baseline_pct_points: None,
        brier_score: None,
        holdout_up_rate: None,
        majority_label: None,
        balanced_accuracy: None,
        matthews_corrcoef: None,
        readiness_stage: "collecting".to_string(),
        rollup_buckets_collected: 0,
        rollup_hours_collected: 0.0,
        buckets_until_activation: MODEL_ACTIVATION_ROLLUP_BUCKETS,
        hours_until_activation: rollup_buckets_to_hours(MODEL_ACTIVATION_ROLLUP_BUCKETS),
        summary: "The experimental model could not be computed for this symbol.".to_string(),
        notes: vec![format!("Prediction build failed: {error:#}")],
        additional_horizons: Vec::new(),
    });

    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            symbol.to_string(),
            CachedPrediction {
                loaded_at_ms: now_millis(),
                history_signature,
                prediction: prediction.clone(),
            },
        );
    }

    Ok(prediction)
}

fn load_market_context(client: &Client, scope: &MarketContextScope) -> MarketContext {
    let mut notes = Vec::new();
    let mut headlines = match parse_fed_headlines(client) {
        Ok(items) => items,
        Err(error) => {
            notes.push(format!("Fed headline fetch failed: {error:#}"));
            Vec::new()
        }
    };
    match parse_geopolitical_headlines(client) {
        Ok(mut items) => headlines.append(&mut items),
        Err(error) => {
            notes.push(format!("Geopolitical headline fetch failed: {error:#}"));
        }
    }
    let mut upcoming_events = match parse_fomc_events(client) {
        Ok(items) => items,
        Err(error) => {
            notes.push(format!("FOMC calendar fetch failed: {error:#}"));
            Vec::new()
        }
    };
    match parse_scheduled_macro_events(client) {
        Ok(mut items) => upcoming_events.append(&mut items),
        Err(error) => {
            notes.push(format!(
                "Principal indicators schedule fetch failed: {error:#}"
            ));
        }
    }
    if scope.include_equity_earnings_proxy {
        match parse_equity_earnings_proxy_events(client) {
            Ok(mut items) => upcoming_events.append(&mut items),
            Err(error) => {
                notes.push(format!("Earnings proxy fetch failed: {error:#}"));
            }
        }
    }
    upcoming_events.sort_by(|left, right| left.scheduled_for.cmp(&right.scheduled_for));
    upcoming_events.truncate(12);
    let scheduled_risk = derive_event_risk(&upcoming_events);
    let headline_risk = derive_headline_risk(&headlines);
    let event_risk = derive_risk_level(&[scheduled_risk.clone(), headline_risk.clone()]);
    if notes.is_empty() {
        notes.push(
            "Scheduled macro risk is currently derived from the official FOMC calendar and the White House / OIRA principal economic indicators schedule."
                .to_string(),
        );
        notes.push(
            "This now covers policy plus scheduled CPI, jobs, PCE, GDP, retail sales, and PPI releases."
                .to_string(),
        );
        notes.push(
            "Geopolitical headline risk is derived from a Google News RSS search over a fixed market-relevant keyword set. This is heuristic and not exhaustive."
                .to_string(),
        );
        if scope.include_equity_earnings_proxy {
            notes.push(
                "US equity earnings coverage is proxied from Alpha Vantage's earnings calendar for a fixed large-cap watchlist (AAPL, MSFT, NVDA, AMZN, META, GOOGL, TSLA). This is a market-impact proxy, not a full SPY holdings calendar."
                    .to_string(),
            );
        }
    }

    MarketContext {
        scheduled_risk,
        headline_risk,
        event_risk,
        headlines,
        upcoming_events,
        notes,
    }
}

fn trim_history(history: &mut HashMap<String, PersistedSymbolHistory>) {
    for symbol_history in history.values_mut() {
        if symbol_history.recent.len() > MAX_HISTORY_POINTS {
            let overflow = symbol_history.recent.len() - MAX_HISTORY_POINTS;
            symbol_history.recent.drain(0..overflow);
        }
        if symbol_history.rollups.len() > MAX_ROLLUP_BUCKETS {
            let overflow = symbol_history.rollups.len() - MAX_ROLLUP_BUCKETS;
            symbol_history.rollups.drain(0..overflow);
        }
    }
}

impl RunningMetric {
    fn push(&mut self, value: Option<f64>) {
        if let Some(value) = value {
            self.sum += value;
            self.count += 1;
        }
    }

    fn average(&self) -> Option<f64> {
        (self.count > 0).then_some(self.sum / self.count as f64)
    }
}

fn rollup_label(sample: &PositionHistorySample) -> String {
    let iso = sample.id.as_str();
    if iso.len() >= 16 && iso.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &iso[..10], &iso[11..16])
    } else {
        sample.label.clone()
    }
}

fn bucket_start_ms(recorded_at_ms: u64) -> u64 {
    recorded_at_ms - (recorded_at_ms % ROLLUP_BUCKET_MS)
}

fn push_sample_into_rollups(rollups: &mut Vec<HistoryRollupBucket>, sample: &PositionHistorySample) {
    let bucket_start = bucket_start_ms(sample.recorded_at_ms);
    if rollups
        .last()
        .map(|bucket| bucket.bucket_start_ms == bucket_start)
        .unwrap_or(false)
    {
        if let Some(bucket) = rollups.last_mut() {
            bucket.sample_count += 1;
            bucket.mark_price.push(sample.mark_price);
            bucket.price_change_24h_pct.push(sample.price_change_24h_pct);
            bucket.basis_pct.push(sample.basis_pct);
            bucket.funding_rate_pct.push(sample.funding_rate_pct);
            bucket.open_interest_notional.push(sample.open_interest_notional);
            bucket.event_risk_score.push(sample.event_risk_score);
            bucket.scheduled_risk_score.push(sample.scheduled_risk_score);
            bucket.headline_risk_score.push(sample.headline_risk_score);
            bucket.spread_bps.push(sample.spread_bps);
            bucket.top_5_imbalance_pct.push(sample.top_5_imbalance_pct);
            bucket.buy_10k_bps.push(sample.buy_10k_bps);
            bucket.buy_40k_bps.push(sample.buy_40k_bps);
            bucket.sell_10k_bps.push(sample.sell_10k_bps);
            bucket.sell_40k_bps.push(sample.sell_40k_bps);
        }
        return;
    }

    let mut bucket = HistoryRollupBucket {
        bucket_start_ms: bucket_start,
        label: rollup_label(sample),
        sample_count: 0,
        mark_price: RunningMetric::default(),
        price_change_24h_pct: RunningMetric::default(),
        basis_pct: RunningMetric::default(),
        funding_rate_pct: RunningMetric::default(),
        open_interest_notional: RunningMetric::default(),
        event_risk_score: RunningMetric::default(),
        scheduled_risk_score: RunningMetric::default(),
        headline_risk_score: RunningMetric::default(),
        spread_bps: RunningMetric::default(),
        top_5_imbalance_pct: RunningMetric::default(),
        buy_10k_bps: RunningMetric::default(),
        buy_40k_bps: RunningMetric::default(),
        sell_10k_bps: RunningMetric::default(),
        sell_40k_bps: RunningMetric::default(),
    };
    bucket.sample_count += 1;
    bucket.mark_price.push(sample.mark_price);
    bucket.price_change_24h_pct.push(sample.price_change_24h_pct);
    bucket.basis_pct.push(sample.basis_pct);
    bucket.funding_rate_pct.push(sample.funding_rate_pct);
    bucket.open_interest_notional.push(sample.open_interest_notional);
    bucket.event_risk_score.push(sample.event_risk_score);
    bucket.scheduled_risk_score.push(sample.scheduled_risk_score);
    bucket.headline_risk_score.push(sample.headline_risk_score);
    bucket.spread_bps.push(sample.spread_bps);
    bucket.top_5_imbalance_pct.push(sample.top_5_imbalance_pct);
    bucket.buy_10k_bps.push(sample.buy_10k_bps);
    bucket.buy_40k_bps.push(sample.buy_40k_bps);
    bucket.sell_10k_bps.push(sample.sell_10k_bps);
    bucket.sell_40k_bps.push(sample.sell_40k_bps);
    rollups.push(bucket);
}

fn rebuild_rollups(samples: &[PositionHistorySample]) -> Vec<HistoryRollupBucket> {
    let mut rollups = Vec::new();
    for sample in samples {
        push_sample_into_rollups(&mut rollups, sample);
    }
    if rollups.len() > MAX_ROLLUP_BUCKETS {
        let overflow = rollups.len() - MAX_ROLLUP_BUCKETS;
        rollups.drain(0..overflow);
    }
    rollups
}

fn load_history_file(path: &PathBuf) -> Result<HashMap<String, PersistedSymbolHistory>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let raw = fs::read(path)
        .with_context(|| format!("failed to read history file {}", path.display()))?;
    let compat: PersistedHistoryCompat = serde_json::from_slice(&raw)
        .with_context(|| format!("failed to parse history file {}", path.display()))?;
    let mut symbols = compat
        .symbols
        .into_iter()
        .map(|(symbol, entry)| {
            let history = match entry {
                PersistedSymbolHistoryCompatEntry::Legacy(recent) => PersistedSymbolHistory {
                    rollups: rebuild_rollups(&recent),
                    recent,
                },
                PersistedSymbolHistoryCompatEntry::Current(mut current) => {
                    if current.rollups.is_empty() && !current.recent.is_empty() {
                        current.rollups = rebuild_rollups(&current.recent);
                    }
                    current
                }
            };
            (symbol, history)
        })
        .collect::<HashMap<_, _>>();
    trim_history(&mut symbols);
    Ok(symbols)
}

fn save_history_file(
    path: &PathBuf,
    history: &HashMap<String, PersistedSymbolHistory>,
) -> Result<()> {
    let persisted = PersistedHistory {
        version: history_format_version(),
        symbols: history.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&persisted).context("failed to encode history JSON")?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create history directory {}", parent.display())
        })?;
    }

    let mut temp_path = path.clone();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!("{value}.tmp"))
        .unwrap_or_else(|| "tmp".to_string());
    temp_path.set_extension(extension);

    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write history temp file {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to atomically replace history file {}",
            path.display()
        )
    })?;

    Ok(())
}

fn find_slippage_bps(estimates: &[SlippageEstimate], target_quote: f64) -> Option<f64> {
    estimates
        .iter()
        .find(|estimate| (estimate.quote_notional - target_quote).abs() < 0.5)
        .and_then(|estimate| estimate.slippage_bps)
}

fn sample_label(book: Option<&OrderBookSummary>) -> String {
    let raw = book
        .and_then(|order_book| order_book.book_time.as_deref())
        .unwrap_or("unknown");

    raw.split('T')
        .nth(1)
        .and_then(|tail| tail.split('.').next())
        .unwrap_or(raw)
        .to_string()
}

fn sample_id(book: Option<&OrderBookSummary>) -> String {
    book.and_then(|order_book| order_book.book_time.clone())
        .unwrap_or_else(|| sample_label(book))
}

fn history_sample_for_symbol(
    book: &OrderBookSummary,
    mark_price: Option<f64>,
    price_change_24h_pct: Option<f64>,
    basis_pct: Option<f64>,
    funding_rate_pct: Option<f64>,
    open_interest_notional: Option<f64>,
    market_context: &MarketContext,
) -> PositionHistorySample {
    PositionHistorySample {
        id: sample_id(Some(book)),
        label: sample_label(Some(book)),
        recorded_at_ms: now_millis(),
        mark_price,
        price_change_24h_pct,
        basis_pct,
        funding_rate_pct,
        open_interest_notional,
        event_risk_score: risk_score(&market_context.event_risk),
        scheduled_risk_score: risk_score(&market_context.scheduled_risk),
        headline_risk_score: risk_score(&market_context.headline_risk),
        spread_bps: book.spread_bps,
        top_5_imbalance_pct: book.top_5_imbalance_pct,
        buy_10k_bps: find_slippage_bps(&book.buy_slippage, 10_000.0),
        buy_40k_bps: find_slippage_bps(&book.buy_slippage, 40_000.0),
        sell_10k_bps: find_slippage_bps(&book.sell_slippage, 10_000.0),
        sell_40k_bps: find_slippage_bps(&book.sell_slippage, 40_000.0),
    }
}

fn upsert_history_sample(
    history: &mut HashMap<String, PersistedSymbolHistory>,
    symbol: &str,
    sample: PositionHistorySample,
) {
    let symbol_history = history.entry(symbol.to_string()).or_default();
    if symbol_history
        .recent
        .last()
        .map(|existing| existing.id == sample.id)
        .unwrap_or(false)
    {
        if let Some(last) = symbol_history.recent.last_mut() {
            *last = sample;
        }
    } else {
        symbol_history.recent.push(sample.clone());
        push_sample_into_rollups(&mut symbol_history.rollups, &sample);
    }
}

fn history_sample(
    position: &PositionSummary,
    market_context: &MarketContext,
) -> Option<PositionHistorySample> {
    let book = position.order_book.as_ref()?;
    Some(history_sample_for_symbol(
        book,
        position.mark_price.as_deref().and_then(parse_number),
        position.price_change_24h_pct,
        position.basis_pct,
        position.funding_rate_pct,
        position.open_interest_notional,
        market_context,
    ))
}

fn watch_history_sample(
    watch: &WatchMarketSummary,
    market_context: &MarketContext,
) -> Option<PositionHistorySample> {
    let book = watch.order_book.as_ref()?;
    Some(history_sample_for_symbol(
        book,
        watch.mark_price.as_deref().and_then(parse_number),
        watch.price_change_24h_pct,
        watch.basis_pct,
        watch.funding_rate_pct,
        watch.open_interest_notional,
        market_context,
    ))
}

fn upsert_history(
    history: &mut HashMap<String, PersistedSymbolHistory>,
    output: &Output,
    market_context: &MarketContext,
) -> HashMap<String, PositionHistorySummary> {
    for position in &output.positions {
        let Some(sample) = history_sample(position, market_context) else {
            continue;
        };
        upsert_history_sample(history, &position.symbol, sample);
    }
    for watch in &output.watch_markets {
        let Some(sample) = watch_history_sample(watch, market_context) else {
            continue;
        };
        upsert_history_sample(history, &watch.symbol, sample);
    }
    trim_history(history);

    history
        .iter()
        .map(|(symbol, history)| (symbol.clone(), summarize_position_history(history)))
        .collect()
}

fn downsample_points(points: Vec<MetricPoint>, max_points: usize) -> Vec<MetricPoint> {
    if points.len() <= max_points || max_points == 0 {
        return points;
    }

    let step = (points.len() - 1) as f64 / (max_points - 1) as f64;
    (0..max_points)
        .filter_map(|index| {
            let point_index = (index as f64 * step).round() as usize;
            points.get(point_index).cloned()
        })
        .collect()
}

fn metric_summary(points: Vec<MetricPoint>) -> Option<MetricHistorySummary> {
    let first = points.first()?;
    let current = points.last()?.value;
    let min = points
        .iter()
        .map(|point| point.value)
        .fold(f64::INFINITY, f64::min);
    let max = points
        .iter()
        .map(|point| point.value)
        .fold(f64::NEG_INFINITY, f64::max);
    let average = points.iter().map(|point| point.value).sum::<f64>() / points.len() as f64;

    Some(MetricHistorySummary {
        current,
        average,
        min,
        max,
        delta_from_oldest: current - first.value,
        points: downsample_points(points, MAX_POINTS_PER_SERIES),
    })
}

fn raw_metric_summary<F>(
    samples: &[PositionHistorySample],
    extractor: F,
) -> Option<MetricHistorySummary>
where
    F: Fn(&PositionHistorySample) -> Option<f64>,
{
    let points = samples
        .iter()
        .filter_map(|sample| extractor(sample).map(|value| MetricPoint {
            label: sample.label.clone(),
            value,
        }))
        .collect::<Vec<_>>();
    metric_summary(points)
}

fn rollup_metric_summary<F>(
    rollups: &[HistoryRollupBucket],
    extractor: F,
) -> Option<MetricHistorySummary>
where
    F: Fn(&HistoryRollupBucket) -> Option<f64>,
{
    let points = rollups
        .iter()
        .filter_map(|bucket| extractor(bucket).map(|value| MetricPoint {
            label: bucket.label.clone(),
            value,
        }))
        .collect::<Vec<_>>();
    metric_summary(points)
}

fn summarize_long_horizon(history: &PersistedSymbolHistory) -> Option<LongHorizonSummary> {
    if history.rollups.is_empty() {
        return None;
    }

    let spread_bps = rollup_metric_summary(&history.rollups, |bucket| bucket.spread_bps.average());
    let top_5_imbalance_pct =
        rollup_metric_summary(&history.rollups, |bucket| bucket.top_5_imbalance_pct.average());
    let buy_40k_bps = rollup_metric_summary(&history.rollups, |bucket| bucket.buy_40k_bps.average());
    let sell_40k_bps =
        rollup_metric_summary(&history.rollups, |bucket| bucket.sell_40k_bps.average());

    let mut insights = Vec::new();
    if let Some(spread) = spread_bps.as_ref() {
        if spread.current > spread.average + 1.5 {
            insights.push(format!(
                "Spread is currently {delta:.2} bps wider than the long-horizon average.",
                delta = spread.current - spread.average
            ));
        } else if spread.current < spread.average - 1.5 {
            insights.push(format!(
                "Spread is currently {delta:.2} bps tighter than the long-horizon average.",
                delta = spread.average - spread.current
            ));
        }
    }
    if let Some(imbalance) = top_5_imbalance_pct.as_ref() {
        if imbalance.current <= -15.0 && imbalance.average <= -10.0 {
            insights.push(format!(
                "Ask-heavy depth is persistent: current {current:.2}% vs long-horizon average {average:.2}%.",
                current = imbalance.current,
                average = imbalance.average
            ));
        } else if imbalance.current >= 15.0 && imbalance.average >= 10.0 {
            insights.push(format!(
                "Bid-heavy depth is persistent: current {current:.2}% vs long-horizon average {average:.2}%.",
                current = imbalance.current,
                average = imbalance.average
            ));
        }
    }
    if let (Some(buy), Some(sell)) = (buy_40k_bps.as_ref(), sell_40k_bps.as_ref()) {
        let current_gap = buy.current - sell.current;
        let average_gap = buy.average - sell.average;
        if current_gap >= 5.0 && average_gap >= 3.0 {
            insights.push(format!(
                "Upward aggression remains more expensive than selling: current $40k buy/sell gap {current_gap:.2} bps, long-horizon average gap {average_gap:.2} bps."
            ));
        } else if current_gap <= -5.0 && average_gap <= -3.0 {
            insights.push(format!(
                "Downward aggression remains more expensive than buying: current $40k sell/buy gap {gap:.2} bps in favor of the offer side.",
                gap = current_gap.abs()
            ));
        }
    }
    if insights.is_empty() {
        insights.push(
            "Long-horizon rollups are still building. Leave the dashboard running across more sessions for stronger persistence tests."
                .to_string(),
        );
    }

    Some(LongHorizonSummary {
        buckets: history.rollups.len(),
        bucket_minutes: ROLLUP_BUCKET_MS as f64 / 60_000.0,
        approx_window_hours: history
            .rollups
            .first()
            .zip(history.rollups.last())
            .map(|(first, last)| {
                last.bucket_start_ms
                    .saturating_sub(first.bucket_start_ms) as f64
                    / 3_600_000.0
            })
            .unwrap_or(0.0),
        latest_label: history.rollups.last().map(|bucket| bucket.label.clone()),
        insights,
        spread_bps,
        top_5_imbalance_pct,
        buy_40k_bps,
        sell_40k_bps,
    })
}

fn summarize_position_history(history: &PersistedSymbolHistory) -> PositionHistorySummary {
    let spread_bps = raw_metric_summary(&history.recent, |sample| sample.spread_bps);
    let top_5_imbalance_pct =
        raw_metric_summary(&history.recent, |sample| sample.top_5_imbalance_pct);
    let buy_10k_bps = raw_metric_summary(&history.recent, |sample| sample.buy_10k_bps);
    let buy_40k_bps = raw_metric_summary(&history.recent, |sample| sample.buy_40k_bps);
    let sell_10k_bps = raw_metric_summary(&history.recent, |sample| sample.sell_10k_bps);
    let sell_40k_bps = raw_metric_summary(&history.recent, |sample| sample.sell_40k_bps);

    let mut insights = Vec::new();
    if let Some(spread) = spread_bps.as_ref() {
        let tightening = spread.max - spread.current;
        if tightening >= 2.0 {
            insights.push(format!(
                "Spread has tightened by {tightening:.2} bps from the widest level in the current window."
            ));
        } else if spread.current - spread.min >= 2.0 {
            insights.push(format!(
                "Spread is {delta:.2} bps wider than the tightest level in the current window.",
                delta = spread.current - spread.min
            ));
        }
    }
    if let Some(buy_40k) = buy_40k_bps.as_ref() {
        let recovery = buy_40k.max - buy_40k.current;
        if recovery >= 5.0 {
            insights.push(format!(
                "Buy-side depth recovered by {recovery:.2} bps versus the worst $40k sweep cost in the current window."
            ));
        } else if buy_40k.current - buy_40k.min >= 5.0 {
            insights.push(format!(
                "Buy-side depth is {delta:.2} bps thinner than the best $40k sweep cost in the current window.",
                delta = buy_40k.current - buy_40k.min
            ));
        }
    }
    if let Some(sell_40k) = sell_40k_bps.as_ref() {
        let recovery = sell_40k.max - sell_40k.current;
        if recovery >= 5.0 {
            insights.push(format!(
                "Sell-side depth recovered by {recovery:.2} bps versus the worst $40k sweep cost in the current window."
            ));
        } else if sell_40k.current - sell_40k.min >= 5.0 {
            insights.push(format!(
                "Sell-side depth is {delta:.2} bps thinner than the best $40k sweep cost in the current window.",
                delta = sell_40k.current - sell_40k.min
            ));
        }
    }
    if let Some(imbalance) = top_5_imbalance_pct.as_ref() {
        if imbalance.current >= 15.0 {
            insights.push(format!(
                "Top-5 depth is currently bid-heavy by {value:.2}%.",
                value = imbalance.current
            ));
        } else if imbalance.current <= -15.0 {
            insights.push(format!(
                "Top-5 depth is currently ask-heavy by {value:.2}%.",
                value = imbalance.current.abs()
            ));
        }
    }
    if insights.is_empty() {
        insights.push(
            "History is still building. Leave the dashboard running to compare spread, imbalance, and sweep costs over time."
                .to_string(),
        );
    }

    PositionHistorySummary {
        samples: history.recent.len(),
        approx_window_minutes: history
            .recent
            .first()
            .zip(history.recent.last())
            .map(|(first, last)| {
                last.recorded_at_ms
                    .saturating_sub(first.recorded_at_ms) as f64
                    / 60_000.0
            })
            .unwrap_or(0.0),
        latest_label: history.recent.last().map(|sample| sample.label.clone()),
        insights,
        spread_bps,
        top_5_imbalance_pct,
        buy_10k_bps,
        buy_40k_bps,
        sell_10k_bps,
        sell_40k_bps,
        long_horizon: summarize_long_horizon(history),
    }
}

fn assess_directional_setup(
    side: &str,
    market_bias: &str,
    funding_rate_pct: Option<f64>,
    funding_direction: Option<&str>,
    order_book: Option<&OrderBookSummary>,
    history: Option<&PositionHistorySummary>,
    market_context: &MarketContext,
) -> TradeSetupAssessment {
    let mut suggested_max_leverage: f64 = 5.0;
    let mut notes = Vec::new();
    let mut penalties = 0usize;

    let event_risk = market_context.event_risk.clone();
    match event_risk.as_str() {
        "high" => {
            suggested_max_leverage = suggested_max_leverage.min(2.0);
            penalties += 2;
            notes.push(
                "Current market-context risk is high enough that event or headline risk should dominate leverage decisions."
                    .to_string(),
            );
        }
        "medium" => {
            suggested_max_leverage = suggested_max_leverage.min(3.0);
            penalties += 1;
            notes.push(
                "Current market-context risk is elevated, so leverage should stay moderate."
                    .to_string(),
            );
        }
        _ => {}
    }

    let (slip_5k, slip_10k) = order_book
        .map(|book| {
            let estimates = if side == "short" {
                &book.sell_slippage
            } else {
                &book.buy_slippage
            };
            (
                find_slippage_bps(estimates, 5_000.0),
                find_slippage_bps(estimates, 10_000.0),
            )
        })
        .unwrap_or((None, None));

    let execution_risk = if slip_10k.unwrap_or(99.0) <= 3.0 {
        "low"
    } else if slip_10k.unwrap_or(99.0) <= 8.0 {
        suggested_max_leverage = suggested_max_leverage.min(5.0);
        "medium"
    } else {
        suggested_max_leverage = suggested_max_leverage.min(3.0);
        penalties += 1;
        notes.push(
            "The visible book starts charging meaningful slippage by $10k notional, so size and leverage should stay restrained."
                .to_string(),
        );
        "high"
    }
    .to_string();

    if let Some(imbalance) = order_book.and_then(|book| book.top_5_imbalance_pct) {
        let adverse = match side {
            "long" => imbalance <= -15.0,
            "short" => imbalance >= 15.0,
            _ => false,
        };
        if adverse {
            suggested_max_leverage = suggested_max_leverage.min(3.0);
            penalties += 1;
            notes.push(format!(
                "Near-touch depth is leaning against the intended {side} side at {imbalance:.2}% imbalance."
            ));
        }
    }

    if let Some(long_horizon) = history.and_then(|summary| summary.long_horizon.as_ref()) {
        if let Some(imbalance) = long_horizon.top_5_imbalance_pct.as_ref() {
            let adverse = match side {
                "long" => imbalance.average <= -10.0,
                "short" => imbalance.average >= 10.0,
                _ => false,
            };
            if adverse {
                suggested_max_leverage = suggested_max_leverage.min(3.0);
                penalties += 1;
                notes.push(format!(
                    "The longer-horizon depth average is also leaning against the {side} side ({:.2}%).",
                    imbalance.average
                ));
            }
        }
    }

    let bias_against_trade = matches!(
        (side, market_bias),
        ("long", "mildly bearish" | "bearish")
            | ("short", "mildly bullish" | "bullish")
    );
    if bias_against_trade {
        suggested_max_leverage = suggested_max_leverage.min(2.0);
        penalties += 1;
        notes.push(format!(
            "The current heuristic market bias ({}) is working against a {side} trade.",
            market_bias
        ));
    }

    if let (Some(funding), Some(direction)) = (funding_rate_pct, funding_direction) {
        let adverse = (side == "long" && direction == "longs paying shorts" && funding.abs() >= 0.02)
            || (side == "short" && direction == "shorts paying longs" && funding.abs() >= 0.02);
        if adverse {
            suggested_max_leverage = suggested_max_leverage.min(3.0);
            penalties += 1;
            notes.push(
                "Funding is materially charging the intended side, which makes high leverage less forgiving."
                    .to_string(),
            );
        }
    }

    if notes.is_empty() {
        notes.push(
            "Execution costs, book skew, and scheduled event risk are not currently flagging an obvious reason to exceed a conservative leverage cap."
                .to_string(),
        );
    }

    let alignment_status = if penalties == 0 && event_risk == "low" && execution_risk == "low" {
        "aligned"
    } else if penalties <= 1 && event_risk != "high" {
        "mixed"
    } else {
        "avoid aggression"
    }
    .to_string();

    let alignment_confidence = if history
        .map(|summary| summary.samples >= 30 || summary.long_horizon.as_ref().map(|roll| roll.buckets >= 6).unwrap_or(false))
        .unwrap_or(false)
    {
        "medium"
    } else {
        "low"
    }
    .to_string();

    let suggested_max_leverage = match suggested_max_leverage {
        x if x <= 1.5 => 1.0,
        x if x <= 2.5 => 2.0,
        x if x <= 3.5 => 3.0,
        _ => 5.0,
    };

    if let Some(slip_5k) = slip_5k {
        notes.push(format!(
            "Visible-book slippage for a $5k {} is about {:.2} bps in the current snapshot.",
            if side == "short" { "sell" } else { "buy" },
            slip_5k
        ));
    }

    TradeSetupAssessment {
        alignment_status,
        alignment_confidence,
        suggested_max_leverage,
        event_risk,
        execution_risk,
        notes,
    }
}

fn assess_trade_setup(
    position: &PositionSummary,
    history: Option<&PositionHistorySummary>,
    market_context: &MarketContext,
) -> TradeSetupAssessment {
    assess_directional_setup(
        position.side.as_deref().unwrap_or("long"),
        &position.market_bias,
        position.funding_rate_pct,
        position.funding_direction.as_deref(),
        position.order_book.as_ref(),
        history,
        market_context,
    )
}

fn assess_watch_setup(
    watch: &WatchMarketSummary,
    history: Option<&PositionHistorySummary>,
    market_context: &MarketContext,
) -> TradeSetupAssessment {
    let mut assessment = assess_directional_setup(
        "long",
        &watch.market_bias,
        watch.funding_rate_pct,
        watch.funding_direction.as_deref(),
        watch.order_book.as_ref(),
        history,
        market_context,
    );
    assessment.notes.insert(
        0,
        "You are flat. Treat this as a long re-entry watch, not a requirement to trade."
            .to_string(),
    );
    assessment
}

fn entry_gate_item(label: &str, passed: bool, detail: String) -> EntryChecklistItem {
    EntryChecklistItem {
        label: label.to_string(),
        passed,
        detail,
    }
}

fn build_watch_entry_checklist(
    watch: &WatchMarketSummary,
    history: Option<&PositionHistorySummary>,
    assessment: &TradeSetupAssessment,
) -> EntryChecklist {
    let spread_bps = watch.order_book.as_ref().and_then(|book| book.spread_bps);
    let buy_10k_bps = watch.order_book.as_ref().and_then(|book| {
        book.buy_slippage
            .iter()
            .find(|estimate| (estimate.quote_notional - 10_000.0).abs() < 0.5)
            .and_then(|estimate| estimate.slippage_bps)
    });
    let top_5_imbalance_pct = watch
        .order_book
        .as_ref()
        .and_then(|book| book.top_5_imbalance_pct);
    let long_horizon_imbalance_avg = history
        .and_then(|summary| summary.long_horizon.as_ref())
        .and_then(|summary| summary.top_5_imbalance_pct.as_ref())
        .map(|summary| summary.average);
    let history_mature = history
        .map(|summary| {
            summary.samples >= 30
                && summary
                    .long_horizon
                    .as_ref()
                    .map(|rollup| rollup.buckets >= 6)
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    let bias_ok = !matches!(watch.market_bias.as_str(), "mildly bearish" | "bearish");
    let funding_ok = match (
        watch.funding_rate_pct,
        watch.funding_direction.as_deref(),
    ) {
        (Some(rate), Some("longs paying shorts")) => rate.abs() < 0.02,
        _ => true,
    };

    let items = vec![
        entry_gate_item(
            "Macro risk is low",
            assessment.event_risk == "low",
            format!(
                "Current combined market-context risk is {}.",
                assessment.event_risk
            ),
        ),
        entry_gate_item(
            "Spread is tight",
            spread_bps.map(|value| value <= 5.0).unwrap_or(false),
            format!(
                "Current top-of-book spread is {}.",
                format_bps(spread_bps)
            ),
        ),
        entry_gate_item(
            "$10k buy slippage is cheap",
            buy_10k_bps.map(|value| value <= 3.0).unwrap_or(false),
            format!(
                "Current $10k aggressive buy slippage is {}.",
                format_bps(buy_10k_bps)
            ),
        ),
        entry_gate_item(
            "Near-touch depth is not leaning against longs",
            top_5_imbalance_pct.map(|value| value > -10.0).unwrap_or(false),
            format!(
                "Current top-5 imbalance is {}.",
                format_pct(top_5_imbalance_pct)
                    .unwrap_or_else(|| "unknown".to_string())
            ),
        ),
        entry_gate_item(
            "Long-horizon depth is not leaning against longs",
            long_horizon_imbalance_avg
                .map(|value| value > -10.0)
                .unwrap_or(false),
            format!(
                "Long-horizon average imbalance is {}.",
                format_pct(long_horizon_imbalance_avg)
                    .unwrap_or_else(|| "unknown".to_string())
            ),
        ),
        entry_gate_item(
            "Bias is not bearish",
            bias_ok,
            format!("Current heuristic market bias is {}.", watch.market_bias),
        ),
        entry_gate_item(
            "Funding is not materially charging longs",
            funding_ok,
            format!(
                "Funding is {} ({}).",
                format_pct(watch.funding_rate_pct)
                    .unwrap_or_else(|| "unknown".to_string()),
                watch.funding_direction
                    .clone()
                    .unwrap_or_else(|| "unknown direction".to_string())
            ),
        ),
        entry_gate_item(
            "History is mature enough",
            history_mature,
            match history {
                Some(summary) => format!(
                    "History has {} raw samples and {} long-horizon buckets.",
                    summary.samples,
                    summary
                        .long_horizon
                        .as_ref()
                        .map(|rollup| rollup.buckets)
                        .unwrap_or(0)
                ),
                None => "No persisted history is available yet.".to_string(),
            },
        ),
    ];

    let passed = items.iter().filter(|item| item.passed).count();
    let total = items.len();
    let overall_status = if passed == total { "ready" } else { "wait" }.to_string();
    let summary = if passed == total {
        "All conservative long-entry gates are passing."
    } else {
        "One or more conservative long-entry gates are failing. Wait for better alignment."
    }
    .to_string();

    EntryChecklist {
        overall_status,
        passed,
        total,
        summary,
        items,
    }
}

fn build_watch_entry_sizing_plan(
    assessment: &TradeSetupAssessment,
    checklist: &EntryChecklist,
) -> EntrySizingPlan {
    let (status, margin_usage_pct, reserve_pct, leverage_fraction_pct, summary, notes) =
        if checklist.overall_status == "ready" {
            (
                "ready".to_string(),
                60.0,
                40.0,
                100.0,
                "Use 60% of available INTX margin for position collateral and keep 40% in reserve."
                    .to_string(),
                vec![
                    "This plan is only active when the long entry gate is fully passing."
                        .to_string(),
                    "Suggested actual leverage can use the full current dashboard leverage cap."
                        .to_string(),
                ],
            )
        } else {
            match assessment.alignment_status.as_str() {
                "aligned" => (
                    "starter".to_string(),
                    40.0,
                    60.0,
                    75.0,
                    "Conditions are improving, but the gate is not fully ready. Size as a starter only."
                        .to_string(),
                    vec![
                        "Use less than half of available INTX margin until the remaining gate failures clear."
                            .to_string(),
                        "Keep a large reserve buffer and stay below the full leverage cap."
                            .to_string(),
                    ],
                ),
                "mixed" => (
                    "probe".to_string(),
                    25.0,
                    75.0,
                    50.0,
                    "Setup is mixed. If you insist on entering early, keep it to a small probe."
                        .to_string(),
                    vec![
                        "Most available margin should remain unused while the setup is still mixed."
                            .to_string(),
                        "Use about half of the current dashboard leverage cap at most."
                            .to_string(),
                    ],
                ),
                _ => (
                    "stand aside".to_string(),
                    0.0,
                    100.0,
                    0.0,
                    "Do not allocate new margin while the dashboard is still in avoid-aggression mode."
                        .to_string(),
                    vec![
                        "Wait for the entry gate to improve before deploying new collateral."
                            .to_string(),
                    ],
                ),
            }
        };

    let suggested_actual_leverage = if leverage_fraction_pct <= 0.0 {
        0.0
    } else {
        assessment.suggested_max_leverage * (leverage_fraction_pct / 100.0)
    };

    EntrySizingPlan {
        status,
        margin_usage_pct,
        reserve_pct,
        leverage_fraction_pct,
        suggested_actual_leverage,
        summary,
        notes,
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Coinbase Perps Dashboard</title>
  <style>
    :root {
      --bg: #f3efe7;
      --panel: rgba(255, 252, 247, 0.82);
      --panel-strong: rgba(255, 250, 242, 0.96);
      --ink: #1d232b;
      --muted: #5e6773;
      --line: rgba(33, 42, 51, 0.12);
      --accent: #0d9488;
      --accent-soft: rgba(13, 148, 136, 0.12);
      --bull: #0f766e;
      --bear: #b45309;
      --danger: #b91c1c;
      --shadow: 0 24px 70px rgba(50, 42, 30, 0.12);
      --radius: 22px;
    }
    * { box-sizing: border-box; }
    html, body { margin: 0; min-height: 100%; }
    body {
      font-family: "Avenir Next", "Helvetica Neue", "Segoe UI", sans-serif;
      color: var(--ink);
      background:
        radial-gradient(circle at top left, rgba(15, 118, 110, 0.18), transparent 24rem),
        radial-gradient(circle at top right, rgba(191, 83, 9, 0.15), transparent 24rem),
        linear-gradient(180deg, #f8f4ed 0%, #ece5d8 100%);
    }
    .shell {
      width: min(1200px, calc(100vw - 32px));
      margin: 28px auto 40px;
    }
    .hero {
      background: linear-gradient(140deg, rgba(255, 251, 245, 0.92), rgba(245, 239, 231, 0.92));
      border: 1px solid var(--line);
      border-radius: 28px;
      box-shadow: var(--shadow);
      padding: 28px;
      display: grid;
      gap: 18px;
    }
    .hero-top {
      display: flex;
      justify-content: space-between;
      gap: 16px;
      align-items: flex-start;
      flex-wrap: wrap;
    }
    h1 {
      margin: 0;
      font-size: clamp(2rem, 4vw, 3.4rem);
      line-height: 0.95;
      letter-spacing: -0.04em;
      font-weight: 760;
    }
    .subtext, .status, .footnote, .metric-label, .stat-label, .signal-note, .empty-copy {
      color: var(--muted);
    }
    .toolbar {
      display: flex;
      gap: 10px;
      align-items: center;
      flex-wrap: wrap;
    }
    button {
      appearance: none;
      border: 0;
      border-radius: 999px;
      padding: 12px 18px;
      background: var(--ink);
      color: #fff;
      font-weight: 650;
      cursor: pointer;
    }
    button.secondary {
      background: rgba(29, 35, 43, 0.08);
      color: var(--ink);
    }
    .hero-grid, .stats-grid, .scenario-grid, .execution-grid, .history-grid {
      display: grid;
      gap: 14px;
    }
    .hero-grid {
      grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
    }
    .metric, .card, .scenario {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: var(--radius);
      box-shadow: 0 10px 30px rgba(33, 42, 51, 0.06);
      backdrop-filter: blur(10px);
    }
    .metric {
      padding: 16px 18px;
      min-height: 168px;
      display: flex;
      flex-direction: column;
      justify-content: flex-start;
    }
    .metric-label {
      font-size: 0.8rem;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      margin-bottom: 8px;
      line-height: 1.22;
      min-height: 2.2em;
      display: flex;
      align-items: flex-start;
    }
    .metric-value {
      font-size: clamp(1.18rem, 2.1vw, 1.65rem);
      font-weight: 720;
      line-height: 1.05;
      overflow-wrap: anywhere;
      word-break: break-word;
    }
    .metric-value.compact {
      font-size: clamp(1.05rem, 1.7vw, 1.35rem);
      line-height: 1.12;
    }
    .metric-note {
      margin-top: auto;
      padding-top: 12px;
      font-size: 0.86rem;
      color: var(--muted);
      overflow-wrap: anywhere;
    }
    .cards {
      margin-top: 18px;
      display: grid;
      gap: 18px;
    }
    .card {
      padding: 22px;
    }
    .card-header {
      display: flex;
      justify-content: space-between;
      gap: 14px;
      align-items: flex-start;
      flex-wrap: wrap;
      margin-bottom: 18px;
    }
    .card-title {
      margin: 0;
      font-size: 1.6rem;
      font-weight: 760;
      letter-spacing: -0.03em;
    }
    .badges {
      display: flex;
      gap: 8px;
      flex-wrap: wrap;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      border-radius: 999px;
      padding: 7px 12px;
      border: 1px solid transparent;
      font-size: 0.9rem;
      font-weight: 700;
    }
    .badge.neutral { background: rgba(29, 35, 43, 0.08); color: var(--ink); }
    .badge.good { background: rgba(15, 118, 110, 0.12); color: var(--bull); border-color: rgba(15, 118, 110, 0.18); }
    .badge.warn { background: rgba(180, 83, 9, 0.12); color: var(--bear); border-color: rgba(180, 83, 9, 0.2); }
    .badge.bad { background: rgba(185, 28, 28, 0.11); color: var(--danger); border-color: rgba(185, 28, 28, 0.16); }
    .stats-grid {
      grid-template-columns: repeat(auto-fit, minmax(145px, 1fr));
      margin-bottom: 16px;
    }
    .stat {
      border: 1px solid var(--line);
      border-radius: 18px;
      background: var(--panel-strong);
      padding: 14px;
      min-height: 86px;
    }
    .stat-label {
      font-size: 0.78rem;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      margin-bottom: 10px;
    }
    .stat-value {
      font-size: 1.12rem;
      font-weight: 720;
      line-height: 1.2;
      word-break: break-word;
    }
    .scenario-grid {
      grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
      margin: 16px 0;
    }
    .execution-grid {
      grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
      margin: 16px 0;
    }
    .history-grid {
      grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
      margin: 16px 0;
    }
    .execution-panel {
      border: 1px solid var(--line);
      border-radius: 20px;
      background: var(--panel-strong);
      padding: 16px;
    }
    .history-panel {
      border: 1px solid var(--line);
      border-radius: 20px;
      background: var(--panel-strong);
      padding: 16px;
      min-height: 190px;
    }
    .section-title {
      font-size: 0.82rem;
      text-transform: uppercase;
      letter-spacing: 0.09em;
      color: var(--muted);
      margin: 4px 0 10px;
    }
    .history-value {
      font-size: 1.5rem;
      font-weight: 760;
      line-height: 1;
      margin-bottom: 10px;
    }
    .history-range {
      font-size: 0.92rem;
      color: var(--muted);
      margin-bottom: 12px;
    }
    .sparkline {
      width: 100%;
      height: 68px;
      display: block;
      margin-bottom: 10px;
    }
    .sparkline polyline {
      fill: none;
      stroke: var(--accent);
      stroke-width: 2.4;
      stroke-linecap: round;
      stroke-linejoin: round;
    }
    .chart-placeholder {
      height: 68px;
      display: grid;
      place-items: center;
      border-radius: 14px;
      background: rgba(29, 35, 43, 0.04);
      color: var(--muted);
      font-size: 0.9rem;
      margin-bottom: 10px;
    }
    .execution-panel .scenario-grid {
      margin: 0;
    }
    .execution-panel .scenario {
      padding: 14px;
    }
    .execution-meta {
      margin-top: 8px;
      font-size: 0.92rem;
      color: var(--muted);
    }
    .history-meta {
      margin-top: 4px;
      font-size: 0.9rem;
      color: var(--muted);
    }
    .history-insights {
      margin-top: 10px;
      padding-left: 18px;
    }
    .history-insights li {
      margin-bottom: 8px;
      line-height: 1.4;
    }
    .context-grid {
      display: grid;
      gap: 14px;
      grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
      margin-top: 18px;
    }
    .context-panel {
      border: 1px solid var(--line);
      border-radius: 20px;
      background: var(--panel-strong);
      padding: 16px;
    }
    .headline-list, .context-list {
      margin: 10px 0 0;
      padding-left: 18px;
    }
    .headline-list li, .context-list li {
      margin-bottom: 8px;
      line-height: 1.4;
    }
    .headline-list a {
      color: inherit;
      text-decoration: none;
      border-bottom: 1px solid rgba(29, 35, 43, 0.18);
    }
    .headline-list a:hover {
      border-bottom-color: rgba(29, 35, 43, 0.4);
    }
    .scenario {
      padding: 16px;
    }
    .scenario-value {
      font-size: 1.35rem;
      font-weight: 760;
      letter-spacing: -0.02em;
    }
    .positive { color: var(--bull); }
    .negative { color: var(--danger); }
    .neutral-text { color: var(--ink); }
    .signals {
      margin: 14px 0 0;
      padding-left: 18px;
    }
    .signals li {
      margin-bottom: 10px;
      line-height: 1.45;
    }
    .footer {
      margin-top: 16px;
      font-size: 0.94rem;
      color: var(--muted);
    }
    .empty {
      text-align: center;
      padding: 42px 20px;
      border: 1px dashed rgba(33, 42, 51, 0.18);
      border-radius: 24px;
      background: rgba(255, 255, 255, 0.5);
    }
    .error {
      border-radius: 22px;
      border: 1px solid rgba(185, 28, 28, 0.18);
      background: rgba(185, 28, 28, 0.08);
      color: #701a1a;
      padding: 16px 18px;
      margin-top: 16px;
      display: none;
    }
    code {
      font-family: "SFMono-Regular", Consolas, monospace;
      font-size: 0.92em;
    }
    @media (max-width: 720px) {
      .shell { width: min(100vw - 20px, 1200px); margin-top: 16px; }
      .hero, .card { padding: 18px; }
      .card-title { font-size: 1.35rem; }
    }
  </style>
</head>
<body>
  <main class="shell">
    <section class="hero">
      <div class="hero-top">
        <div>
          <div class="subtext">Local read-only Coinbase INTX analytics dashboard</div>
          <h1>Perps Dashboard</h1>
          <div class="subtext" id="portfolioHint">Portfolio selection: __PORTFOLIO_TEXT__</div>
        </div>
        <div class="toolbar">
          <button id="refreshBtn">Refresh now</button>
          <button id="copyJsonBtn" class="secondary">Copy JSON</button>
          <div class="status" id="statusText">Waiting for first snapshot...</div>
        </div>
      </div>
      <div class="hero-grid" id="heroGrid"></div>
      <div class="footer" id="analysisBasis"></div>
    </section>
    <section id="errorBox" class="error"></section>
    <section id="cards" class="cards"></section>
  </main>
  <script>
    const refreshMs = __REFRESH_MS__;
    let latestSnapshot = null;
    let isLoading = false;

    function escapeHtml(value) {
      return String(value ?? "")
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;")
        .replaceAll("'", "&#39;");
    }

    function formatMaybe(value, digits = 2) {
      if (value === null || value === undefined || value === "") return "unknown";
      const num = Number(value);
      if (Number.isFinite(num)) return num.toFixed(digits);
      return escapeHtml(value);
    }

    function formatPct(value) {
      if (value === null || value === undefined) return "unknown";
      return `${Number(value).toFixed(2)}%`;
    }

    function formatBps(value, digits = 2) {
      if (value === null || value === undefined) return "unknown";
      const num = Number(value);
      if (!Number.isFinite(num)) return "unknown";
      return `${num.toFixed(digits)} bps`;
    }

    function formatQuoteNotional(value) {
      if (value === null || value === undefined) return "unknown";
      const num = Number(value);
      if (!Number.isFinite(num)) return "unknown";
      if (Math.abs(num) >= 1000 && Math.abs(num % 1000) < 1e-9) {
        return `$${(num / 1000).toFixed(0)}k`;
      }
      return `$${num.toFixed(0)}`;
    }

    function formatSigned(value, digits = 2) {
      if (value === null || value === undefined) return "unknown";
      const num = Number(value);
      if (!Number.isFinite(num)) return escapeHtml(value);
      const prefix = num > 0 ? "+" : "";
      return `${prefix}${num.toFixed(digits)}`;
    }

    function formatMetric(value, suffix = "", digits = 2) {
      if (value === null || value === undefined) return "unknown";
      const num = Number(value);
      if (!Number.isFinite(num)) return "unknown";
      return `${num.toFixed(digits)}${suffix}`;
    }

    function toneClass(value) {
      if (value === null || value === undefined) return "neutral-text";
      const num = Number(value);
      if (!Number.isFinite(num)) return "neutral-text";
      if (num > 0) return "positive";
      if (num < 0) return "negative";
      return "neutral-text";
    }

    function badgeClass(label) {
      const lower = String(label || "").toLowerCase();
      if (lower.includes("bull") || lower.includes("favor") || lower.includes("construct")) return "good";
      if (lower.includes("aligned") || lower === "low") return "good";
      if (lower.includes("mixed") || lower === "medium") return "warn";
      if (lower.includes("avoid") || lower === "high") return "bad";
      if (lower.includes("bear") || lower.includes("risk")) return "bad";
      if (lower.includes("caut")) return "warn";
      return "neutral";
    }

    function shortId(value, head = 8, tail = 6) {
      const text = String(value ?? "");
      if (text.length <= head + tail + 3) return text;
      return `${text.slice(0, head)}...${text.slice(-tail)}`;
    }

    function metricCard(label, value, extraClass = "", note = "") {
      const noteHtml = note ? `<div class="metric-note">${escapeHtml(note)}</div>` : "";
      return `<article class="metric"><div class="metric-label">${escapeHtml(label)}</div><div class="metric-value ${extraClass}">${escapeHtml(value)}</div>${noteHtml}</article>`;
    }

    function statCard(label, value, extraClass = "") {
      return `<div class="stat"><div class="stat-label">${escapeHtml(label)}</div><div class="stat-value ${extraClass}">${escapeHtml(value)}</div></div>`;
    }

    function scenarioCard(label, value) {
      return `<div class="scenario"><div class="stat-label">${escapeHtml(label)}</div><div class="scenario-value ${toneClass(value)}">${escapeHtml(formatSigned(value, 2))}</div></div>`;
    }

    function sparkline(points) {
      const values = (points || [])
        .map((point) => Number(point?.value))
        .filter((value) => Number.isFinite(value));
      if (values.length < 2) {
        return `<div class="chart-placeholder">Need more samples</div>`;
      }

      const min = Math.min(...values);
      const max = Math.max(...values);
      const span = max - min || 1;
      const coords = values
        .map((value, index) => {
          const x = (index / (values.length - 1)) * 100;
          const y = 100 - ((value - min) / span) * 100;
          return `${x.toFixed(2)},${y.toFixed(2)}`;
        })
        .join(" ");

      return `<svg viewBox="0 0 100 100" preserveAspectRatio="none" class="sparkline" aria-hidden="true"><polyline points="${coords}" /></svg>`;
    }

    function historyPanel(label, summary, suffix = "", digits = 2) {
      if (!summary) {
        return `
          <section class="history-panel">
            <div class="section-title">${escapeHtml(label)}</div>
            <div class="chart-placeholder">No history yet</div>
            <div class="history-meta">Leave the dashboard open to build this series.</div>
          </section>
        `;
      }

      return `
        <section class="history-panel">
          <div class="section-title">${escapeHtml(label)}</div>
          <div class="history-value">${escapeHtml(formatMetric(summary.current, suffix, digits))}</div>
          ${sparkline(summary.points)}
          <div class="history-range">Range ${escapeHtml(formatMetric(summary.min, suffix, digits))} to ${escapeHtml(formatMetric(summary.max, suffix, digits))}</div>
          <div class="history-meta">Average ${escapeHtml(formatMetric(summary.average, suffix, digits))}</div>
          <div class="history-meta">Delta vs oldest ${escapeHtml(formatSigned(summary.delta_from_oldest, digits))}${escapeHtml(suffix)}</div>
        </section>
      `;
    }

    function slippageCard(estimate, sideLabel) {
      const fillStatus = estimate?.complete === false
        ? `Partial fill ${formatPct(estimate.fill_pct)}`
        : "Complete ladder";
      const worst = estimate?.worst_price != null ? `Worst ${formatMaybe(estimate.worst_price, 2)}` : "Worst unknown";
      return `
        <div class="scenario">
          <div class="stat-label">${escapeHtml(formatQuoteNotional(estimate?.quote_notional))} ${escapeHtml(sideLabel)}</div>
          <div class="scenario-value neutral-text">${escapeHtml(formatBps(estimate?.slippage_bps))}</div>
          <div class="execution-meta">Avg ${escapeHtml(formatMaybe(estimate?.average_price, 2))} | ${escapeHtml(worst)} | ${escapeHtml(fillStatus)}</div>
        </div>
      `;
    }

    function executionPanel(label, estimates, sideLabel) {
      if (!(estimates || []).length) {
        return `
          <section class="execution-panel">
            <div class="section-title">${escapeHtml(label)}</div>
            <div class="execution-meta">No book-based execution estimate available.</div>
          </section>
        `;
      }

      return `
        <section class="execution-panel">
          <div class="section-title">${escapeHtml(label)}</div>
          <div class="scenario-grid">${estimates.map((estimate) => slippageCard(estimate, sideLabel)).join("")}</div>
        </section>
      `;
    }

    function marketContextPanels(context) {
      const headlines = (context?.headlines || [])
        .map((item) => `<li><a href="${escapeHtml(item.link)}" target="_blank" rel="noreferrer">${escapeHtml(item.title)}</a><br><span class="history-meta">${escapeHtml(item.source)}${item.published_at ? ` · ${escapeHtml(item.published_at)}` : ""}</span></li>`)
        .join("");
      const events = (context?.upcoming_events || [])
        .map((item) => `<li><strong>${escapeHtml(item.title)}</strong><br><span class="history-meta">${escapeHtml(item.scheduled_for)}${item.days_until != null ? ` (${formatMaybe(item.days_until, 1)} days)` : ""} · ${escapeHtml(item.risk)} risk · ${escapeHtml(item.category)} · ${escapeHtml(item.source)}</span></li>`)
        .join("");
      const notes = (context?.notes || [])
        .map((item) => `<li>${escapeHtml(item)}</li>`)
        .join("");

      return `
        <div class="context-grid">
          <section class="context-panel">
            <div class="section-title">Combined Market Context</div>
            <div class="history-value">${escapeHtml(context?.event_risk || "unknown")} combined risk</div>
            <div class="history-meta">scheduled: ${escapeHtml(context?.scheduled_risk || "unknown")} · headlines: ${escapeHtml(context?.headline_risk || "unknown")}</div>
            <ul class="context-list">${notes}</ul>
          </section>
          <section class="context-panel">
            <div class="section-title">Policy + Geopolitics Headlines</div>
            <ul class="headline-list">${headlines || "<li>No headline context loaded.</li>"}</ul>
          </section>
          <section class="context-panel">
            <div class="section-title">Upcoming Scheduled Events + Earnings</div>
            <ul class="context-list">${events || "<li>No upcoming scheduled events loaded.</li>"}</ul>
          </section>
        </div>
      `;
    }

    function openOrdersPanel(orders) {
      const stale = (orders || []).filter((order) => order.cleanup_candidate);
      const staleBanner = stale.length
        ? `<div class="signal-note negative">Stale reduce-only cleanup review: ${stale.length} open order(s) appear tied to no live position.</div>`
        : `<div class="signal-note">No obvious stale reduce-only cleanup candidates were detected.</div>`;
      const cards = (orders || []).map((order) => {
        const meta = [
          order.order_type || "unknown",
          order.configuration_label || "unknown config",
          order.reduce_only === true ? "reduce-only" : (order.reduce_only === false ? "not reduce-only" : "reduce-only unknown"),
        ].join(" | ");
        const reason = order.cleanup_reason ? `<div class="signal-note negative">${escapeHtml(order.cleanup_reason)}</div>` : "";
        return `
          <article class="card">
            <div class="card-header">
              <div>
                <h2 class="card-title">${escapeHtml(order.product_id)}</h2>
                <div class="subtext">${escapeHtml(order.side || "unknown")} | ${escapeHtml(order.status || "unknown")} | ${escapeHtml(meta)}</div>
              </div>
              <div class="badges">
                <span class="badge ${badgeClass(order.cleanup_candidate ? "high" : "low")}">${order.cleanup_candidate ? "cleanup review" : "open order"}</span>
              </div>
            </div>
            <div class="stats-grid">
              ${statCard("Base Size", order.base_size || "unknown")}
              ${statCard("Filled", order.filled_size || "unknown")}
              ${statCard("Complete", order.completion_percentage ? `${escapeHtml(order.completion_percentage)}%` : "unknown")}
              ${statCard("Limit", order.limit_price || "n/a")}
              ${statCard("Stop", order.stop_price || "n/a")}
              ${statCard("Trigger", order.stop_trigger_price || "n/a")}
              ${statCard("Avg Fill", order.average_filled_price || "unknown")}
              ${statCard("Fees", order.total_fees || "unknown")}
              ${statCard("Created", order.created_time ? shortId(order.created_time, 16, 8) : "unknown")}
              ${statCard("Updated", order.last_update_time ? shortId(order.last_update_time, 16, 8) : "unknown")}
            </div>
            ${reason}
          </article>
        `;
      }).join("");

      return `
        <section class="card">
          <div class="card-header">
            <div>
              <h2 class="card-title">Open Orders</h2>
              <div class="subtext">Live Advanced Trade future/perpetual orders visible to this key.</div>
            </div>
          </div>
          ${staleBanner}
        </section>
        ${cards || `<div class="empty"><h2>No open orders</h2><div class="empty-copy">No active future/perpetual orders are currently visible for this portfolio.</div></div>`}
      `;
    }

    function entryChecklistPanel(checklist) {
      if (!checklist) {
        return `
          <section class="card">
            <div class="card-header">
              <div>
                <h2 class="card-title">Entry Gate</h2>
                <div class="subtext">No checklist is available yet.</div>
              </div>
            </div>
          </section>
        `;
      }

      const items = (checklist.items || []).map((item) => `
        <section class="history-panel">
          <div class="section-title">${escapeHtml(item.label)}</div>
          <div class="history-value ${item.passed ? "positive" : "negative"}">${item.passed ? "Pass" : "Wait"}</div>
          <div class="history-meta">${escapeHtml(item.detail)}</div>
        </section>
      `).join("");

      return `
        <section class="card">
          <div class="card-header">
            <div>
              <h2 class="card-title">Entry Gate</h2>
              <div class="subtext">${escapeHtml(checklist.summary || "")}</div>
            </div>
            <div class="badges">
              <span class="badge ${badgeClass(checklist.overall_status)}">${escapeHtml(checklist.overall_status)}</span>
              <span class="badge neutral">${escapeHtml(`${checklist.passed}/${checklist.total} passed`)}</span>
            </div>
          </div>
          <div class="history-grid">${items}</div>
        </section>
      `;
    }

    function entrySizingPanel(plan) {
      if (!plan) {
        return `
          <section class="card">
            <div class="card-header">
              <div>
                <h2 class="card-title">Entry Sizing</h2>
                <div class="subtext">No sizing plan is available yet.</div>
              </div>
            </div>
          </section>
        `;
      }

      const notes = (plan.notes || []).map((note) => `<li>${escapeHtml(note)}</li>`).join("");
      return `
        <section class="card">
          <div class="card-header">
            <div>
              <h2 class="card-title">Entry Sizing</h2>
              <div class="subtext">${escapeHtml(plan.summary || "")}</div>
            </div>
            <div class="badges">
              <span class="badge ${badgeClass(plan.status)}">${escapeHtml(plan.status)}</span>
            </div>
          </div>
          <div class="stats-grid">
            ${statCard("Margin Use", `${formatMaybe(plan.margin_usage_pct, 0)}%`)}
            ${statCard("Reserve", `${formatMaybe(plan.reserve_pct, 0)}%`)}
            ${statCard("Lev Use of Cap", `${formatMaybe(plan.leverage_fraction_pct, 0)}%`)}
            ${statCard("Suggested Actual Lev", plan.suggested_actual_leverage > 0 ? `${formatMaybe(plan.suggested_actual_leverage, 1)}x` : "wait")}
          </div>
          <div class="signal-note">Percentages refer to currently available INTX margin, not total account equity.</div>
          <ul class="history-insights">${notes}</ul>
        </section>
      `;
    }

    function predictionPanel(prediction) {
      if (!prediction) {
        return `
          <section class="card">
            <div class="card-header">
              <div>
                <h2 class="card-title">Experimental Model</h2>
                <div class="subtext">No model output is available yet.</div>
              </div>
            </div>
          </section>
        `;
      }

      function horizonSection(item, isPrimary) {
        const up = item.probability_up != null ? `${formatMaybe(item.probability_up * 100, 1)}%` : "unknown";
        const down = item.probability_down != null ? `${formatMaybe(item.probability_down * 100, 1)}%` : "unknown";
        const testAcc = item.test_accuracy != null ? `${formatMaybe(item.test_accuracy * 100, 1)}%` : "unknown";
        const baseAcc = item.baseline_accuracy != null ? `${formatMaybe(item.baseline_accuracy * 100, 1)}%` : "unknown";
        const edgeVsBaseline = item.edge_vs_baseline_pct_points != null ? `${formatSigned(item.edge_vs_baseline_pct_points, 1)} pts` : "unknown";
        const holdoutUpRate = item.holdout_up_rate != null ? `${formatMaybe(item.holdout_up_rate * 100, 1)}%` : "unknown";
        const oneClassHoldout = item.holdout_up_rate === 0 || item.holdout_up_rate === 1;
        const balancedAcc = item.balanced_accuracy != null
          ? `${formatMaybe(item.balanced_accuracy * 100, 1)}%`
          : (oneClassHoldout ? "N/A (one-class)" : "unknown");
        const mcc = item.matthews_corrcoef != null
          ? formatMaybe(item.matthews_corrcoef, 3)
          : (oneClassHoldout ? "N/A (one-class)" : "unknown");
        const brier = item.brier_score != null ? formatMaybe(item.brier_score, 3) : "unknown";
        const lowSampleWarning = Number(item.test_samples || 0) < 12
          ? `<span class="badge warn">thin test window</span>`
          : "";
        const sampleNote = Number(item.test_samples || 0) < 12
          ? "The current walk-forward holdout is still very small, so treat these metrics as preliminary."
          : "";

        return `
          <section class="${isPrimary ? "card" : "execution-panel"}">
            <div class="card-header">
              <div>
                <h2 class="card-title">${isPrimary ? "Experimental Model" : `Horizon ${escapeHtml(item.horizon_label || "unknown")}`}</h2>
                <div class="subtext">${escapeHtml(item.summary || "")}</div>
              </div>
              <div class="badges">
                <span class="badge ${badgeClass(item.model_bias)}">${escapeHtml(item.model_bias || "unknown")}</span>
                <span class="badge neutral">${escapeHtml(item.confidence || "low")} confidence</span>
                ${isPrimary ? `<span class="badge ${badgeClass(item.readiness_stage || "neutral")}">${escapeHtml(item.readiness_stage || "collecting")}</span>` : ""}
                ${lowSampleWarning}
              </div>
            </div>
            <div class="stats-grid">
              ${statCard("Horizon", item.horizon_label || "unknown")}
              ${statCard("P Up", up, toneClass(item.probability_up != null ? item.probability_up - 0.5 : null))}
              ${statCard("P Down", down, toneClass(item.probability_down != null ? item.probability_down - 0.5 : null))}
              ${statCard("Train Samples", String(item.training_samples || 0))}
              ${statCard("Test Samples", String(item.test_samples || 0))}
              ${statCard("Holdout Up", holdoutUpRate)}
              ${statCard("Majority Side", item.majority_label || "unknown")}
              ${statCard("Test Accuracy", testAcc)}
              ${statCard("Baseline", baseAcc)}
              ${statCard("Balanced Acc", balancedAcc)}
              ${statCard("MCC", mcc, toneClass(item.matthews_corrcoef))}
              ${statCard("Edge vs Base", edgeVsBaseline, toneClass(item.edge_vs_baseline_pct_points))}
              ${statCard("Brier", brier)}
              ${statCard("Variant", item.variant === "history_augmented" ? "history-augmented" : (item.variant === "candle_only" ? "candle-only" : (item.variant || "unknown")))}
              ${statCard("Status", item.status || "unknown")}
              ${isPrimary ? statCard("History Collected", `${formatMaybe(item.rollup_hours_collected, 1)}h`) : ""}
              ${isPrimary ? statCard("Augmented Model", item.buckets_until_activation > 0 ? `${formatMaybe(item.hours_until_activation, 1)}h left` : "active") : ""}
            </div>
            <div class="signal-note">This model is experimental. It is separate from the execution gate and does not override risk controls.</div>
            <div class="signal-note">${escapeHtml(item.evaluation_method || "Evaluation method unknown.")}</div>
            ${sampleNote ? `<div class="signal-note">${escapeHtml(sampleNote)}</div>` : ""}
            ${isPrimary ? `<div class="signal-note">Thresholds: activate at 120 buckets (~10.0h), first review at 300 buckets (~25.0h), serious trust at 960 buckets (~80.0h).</div>` : ""}
            <ul class="history-insights">${(item.notes || []).map((note) => `<li>${escapeHtml(note)}</li>`).join("")}</ul>
          </section>
        `;
      }

      const extraHorizons = (prediction.additional_horizons || []).map((item) => horizonSection(item, false)).join("");

      return `
        ${horizonSection(prediction, true)}
        ${extraHorizons}
      `;
    }

    function watchCard(watch, history, assessment, checklist, sizingPlan, prediction) {
      const displayName = watch.display_name ? ` (${escapeHtml(watch.display_name)})` : "";
      const spreadValue = watch.order_book?.spread_absolute != null || watch.order_book?.spread_bps != null
        ? `${formatMaybe(watch.order_book?.spread_absolute, 4)} | ${formatBps(watch.order_book?.spread_bps)}`
        : "unknown";
      const historyInsights = (history?.insights || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const longInsights = (history?.long_horizon?.insights || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const watchNotes = (assessment?.notes || []).map((note) => `<li>${escapeHtml(note)}</li>`).join("");
      const signals = (watch.signals || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");

      return `
        <article class="card">
          <div class="card-header">
            <div>
              <h2 class="card-title">${escapeHtml(watch.symbol)}${displayName}</h2>
              <div class="subtext">Flat-mode re-entry watch | Underlying: ${escapeHtml(watch.underlying_type || "unknown")}</div>
            </div>
            <div class="badges">
              <span class="badge ${badgeClass(assessment?.alignment_status || watch.market_bias)}">${escapeHtml(assessment?.alignment_status || watch.market_bias)}</span>
              <span class="badge neutral">${escapeHtml((assessment?.alignment_confidence || "low") + " confidence")}</span>
            </div>
          </div>

          <div class="stats-grid">
            ${statCard("Mark", watch.mark_price || "unknown")}
            ${statCard("Index", watch.index_price || "unknown")}
            ${statCard("24h Change", formatPct(watch.price_change_24h_pct), toneClass(watch.price_change_24h_pct))}
            ${statCard("Basis", formatPct(watch.basis_pct), toneClass(watch.basis_pct))}
            ${statCard("Funding", watch.funding_rate_pct != null ? `${formatMaybe(watch.funding_rate_pct, 4)}%` : "unknown", toneClass(watch.funding_rate_pct))}
            ${statCard("Funding Context", watch.funding_direction && watch.funding_intensity ? `${watch.funding_direction} (${watch.funding_intensity})` : (watch.funding_direction || watch.funding_intensity || "unknown"))}
            ${statCard("Open Interest", watch.open_interest || "unknown")}
            ${statCard("OI Notional", watch.open_interest_notional != null ? formatMaybe(watch.open_interest_notional, 2) : "unknown")}
            ${statCard("Spread", spreadValue)}
            ${statCard("Top 5 Imbalance", watch.order_book?.top_5_imbalance_pct != null ? formatPct(watch.order_book.top_5_imbalance_pct) : "unknown", toneClass(watch.order_book?.top_5_imbalance_pct))}
            ${statCard("Macro Risk", assessment?.event_risk || "unknown", badgeClass(assessment?.event_risk || ""))}
            ${statCard("Execution Risk", assessment?.execution_risk || "unknown", badgeClass(assessment?.execution_risk || ""))}
            ${statCard("Suggested Max Lev", assessment?.suggested_max_leverage != null ? `${formatMaybe(assessment.suggested_max_leverage, 0)}x` : "unknown")}
            ${statCard("Margin Use", sizingPlan?.margin_usage_pct != null ? `${formatMaybe(sizingPlan.margin_usage_pct, 0)}%` : "unknown")}
            ${statCard("Reserve", sizingPlan?.reserve_pct != null ? `${formatMaybe(sizingPlan.reserve_pct, 0)}%` : "unknown")}
            ${statCard("Actual Lev", sizingPlan?.suggested_actual_leverage > 0 ? `${formatMaybe(sizingPlan.suggested_actual_leverage, 1)}x` : "wait")}
            ${statCard("Model Bias", prediction?.model_bias || "unknown", badgeClass(prediction?.model_bias || ""))}
            ${statCard("P Up 1h", prediction?.probability_up != null ? `${formatMaybe(prediction.probability_up * 100, 1)}%` : "unknown", toneClass(prediction?.probability_up != null ? prediction.probability_up - 0.5 : null))}
          </div>

          ${entryChecklistPanel(checklist)}
          ${entrySizingPanel(sizingPlan)}
          ${predictionPanel(prediction)}

          <div class="execution-grid">
            ${executionPanel("Buy Slippage vs Best Ask", watch.order_book?.buy_slippage, "buy")}
            ${executionPanel("Sell Slippage vs Best Bid", watch.order_book?.sell_slippage, "sell")}
          </div>

          <div class="history-grid">
            ${historyPanel("Spread History", history?.spread_bps, " bps", 2)}
            ${historyPanel("Top 5 Imbalance History", history?.top_5_imbalance_pct, "%", 2)}
            ${historyPanel("Buy $10k Slip History", history?.buy_10k_bps, " bps", 2)}
            ${historyPanel("Buy $40k Slip History", history?.buy_40k_bps, " bps", 2)}
          </div>

          <div class="history-meta">History window: ${history ? `${history.samples} samples, ~${formatMaybe(history.approx_window_minutes, 1)} min, latest ${history.latest_label || "unknown"}` : "building from the first snapshot"}</div>
          <ul class="history-insights">${historyInsights}</ul>

          <div class="history-grid">
            ${historyPanel("Long Spread Trend", history?.long_horizon?.spread_bps, " bps", 2)}
            ${historyPanel("Long Imbalance Trend", history?.long_horizon?.top_5_imbalance_pct, "%", 2)}
            ${historyPanel("Long Buy $40k Slip", history?.long_horizon?.buy_40k_bps, " bps", 2)}
            ${historyPanel("Long Sell $40k Slip", history?.long_horizon?.sell_40k_bps, " bps", 2)}
          </div>

          <div class="history-meta">Robust window: ${history?.long_horizon ? `${history.long_horizon.buckets} buckets x ${formatMaybe(history.long_horizon.bucket_minutes, 0)} min, ~${formatMaybe(history.long_horizon.approx_window_hours, 1)} h, latest ${history.long_horizon.latest_label || "unknown"}` : "building from rollups"}</div>
          <ul class="history-insights">${longInsights}</ul>

          <div class="signal-note">Re-entry watch is a conservative heuristic. It is not a signal to trade by itself.</div>
          <ul class="history-insights">${watchNotes}</ul>

          <div class="signal-note">Watch signals are derived from live product and product-book fields while you are flat.</div>
          <ul class="signals">${signals}</ul>
        </article>
      `;
    }

    function positionCard(pos, history, assessment, prediction) {
      const displayName = pos.display_name ? ` (${escapeHtml(pos.display_name)})` : "";
      const signals = (pos.signals || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const historyInsights = (history?.insights || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const longInsights = (history?.long_horizon?.insights || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const setupNotes = (assessment?.notes || []).map((note) => `<li>${escapeHtml(note)}</li>`).join("");
      const spreadValue = pos.order_book?.spread_absolute != null || pos.order_book?.spread_bps != null
        ? `${formatMaybe(pos.order_book?.spread_absolute, 4)} | ${formatBps(pos.order_book?.spread_bps)}`
        : "unknown";
      return `
        <article class="card">
          <div class="card-header">
            <div>
              <h2 class="card-title">${escapeHtml(pos.symbol)}${displayName}</h2>
              <div class="subtext">Underlying: ${escapeHtml(pos.underlying_type || "unknown")} | Side: ${escapeHtml(pos.side || "unknown")} | Margin: ${escapeHtml(pos.margin_mode || "unknown")}</div>
            </div>
            <div class="badges">
              <span class="badge ${badgeClass(assessment?.alignment_status || pos.market_bias)}">${escapeHtml(assessment?.alignment_status || pos.market_bias)}</span>
              <span class="badge ${badgeClass(pos.position_outlook)}">${escapeHtml(pos.position_outlook)}</span>
              <span class="badge neutral">${escapeHtml((assessment?.alignment_confidence || pos.outlook_confidence) + " confidence")}</span>
            </div>
          </div>

          <div class="stats-grid">
            ${statCard("Contracts", pos.contracts || "unknown")}
            ${statCard("Notional", pos.notional || "unknown")}
            ${statCard("Entry", pos.entry_price || "unknown")}
            ${statCard("Mark", pos.mark_price || "unknown")}
            ${statCard("Index", pos.index_price || "unknown")}
            ${statCard("Agg PnL", pos.aggregated_pnl || "unknown", toneClass(pos.aggregated_pnl))}
            ${statCard("Effective Lev", pos.effective_leverage != null ? `${formatMaybe(pos.effective_leverage, 2)}x` : "unknown")}
            ${statCard("API Lev", pos.api_leverage ? `${escapeHtml(pos.api_leverage)}x` : "unknown")}
            ${statCard("Funding", pos.funding_rate_pct != null ? `${formatMaybe(pos.funding_rate_pct, 4)}%` : "unknown", toneClass(pos.funding_rate_pct))}
            ${statCard("Funding Context", pos.funding_direction && pos.funding_intensity ? `${pos.funding_direction} (${pos.funding_intensity})` : (pos.funding_direction || pos.funding_intensity || "unknown"))}
            ${statCard("Open Interest", pos.open_interest || "unknown")}
            ${statCard("OI Notional", pos.open_interest_notional != null ? formatMaybe(pos.open_interest_notional, 2) : "unknown")}
            ${statCard("Position Share of OI", pos.position_share_of_open_interest_pct != null ? `${formatMaybe(pos.position_share_of_open_interest_pct, 2)}%` : "unknown")}
            ${statCard("Best Bid", pos.order_book?.best_bid != null ? formatMaybe(pos.order_book.best_bid, 2) : "unknown")}
            ${statCard("Best Ask", pos.order_book?.best_ask != null ? formatMaybe(pos.order_book.best_ask, 2) : "unknown")}
            ${statCard("Spread", spreadValue)}
            ${statCard("Book Levels", pos.order_book ? `${pos.order_book.bid_levels}/${pos.order_book.ask_levels}` : "unknown")}
            ${statCard("Top 5 Bid Depth", pos.order_book?.top_5_bid_notional != null ? formatMaybe(pos.order_book.top_5_bid_notional, 0) : "unknown")}
            ${statCard("Top 5 Ask Depth", pos.order_book?.top_5_ask_notional != null ? formatMaybe(pos.order_book.top_5_ask_notional, 0) : "unknown")}
            ${statCard("Top 5 Imbalance", pos.order_book?.top_5_imbalance_pct != null ? formatPct(pos.order_book.top_5_imbalance_pct) : "unknown", toneClass(pos.order_book?.top_5_imbalance_pct))}
            ${statCard("Basis", formatPct(pos.basis_pct), toneClass(pos.basis_pct))}
            ${statCard("24h Change", formatPct(pos.price_change_24h_pct), toneClass(pos.price_change_24h_pct))}
            ${statCard("Liq Distance", formatPct(pos.distance_to_liquidation_pct))}
            ${statCard("Liq Price", pos.liquidation_price || "unknown")}
            ${statCard("Collateral", pos.collateral || "unknown")}
            ${statCard("Liq Buffer", pos.liquidation_buffer || "unknown")}
            ${statCard("Max Leverage", pos.max_leverage ? `${escapeHtml(pos.max_leverage)}x` : "unknown")}
            ${statCard("Setup Status", assessment?.alignment_status || "unknown")}
            ${statCard("Macro Risk", assessment?.event_risk || "unknown", badgeClass(assessment?.event_risk || ""))}
            ${statCard("Execution Risk", assessment?.execution_risk || "unknown", badgeClass(assessment?.execution_risk || ""))}
            ${statCard("Suggested Max Lev", assessment?.suggested_max_leverage != null ? `${formatMaybe(assessment.suggested_max_leverage, 0)}x` : "unknown")}
            ${statCard("Model Bias", prediction?.model_bias || "unknown", badgeClass(prediction?.model_bias || ""))}
            ${statCard("P Up 1h", prediction?.probability_up != null ? `${formatMaybe(prediction.probability_up * 100, 1)}%` : "unknown", toneClass(prediction?.probability_up != null ? prediction.probability_up - 0.5 : null))}
          </div>

          <div class="scenario-grid">
            ${scenarioCard("+1% move", pos.projections?.up_1pct_pnl)}
            ${scenarioCard("+3% move", pos.projections?.up_3pct_pnl)}
            ${scenarioCard("-1% move", pos.projections?.down_1pct_pnl)}
            ${scenarioCard("-3% move", pos.projections?.down_3pct_pnl)}
          </div>

          ${predictionPanel(prediction)}

          <div class="execution-grid">
            ${executionPanel("Buy Slippage vs Best Ask", pos.order_book?.buy_slippage, "buy")}
            ${executionPanel("Sell Slippage vs Best Bid", pos.order_book?.sell_slippage, "sell")}
          </div>

          <div class="history-grid">
            ${historyPanel("Spread History", history?.spread_bps, " bps", 2)}
            ${historyPanel("Top 5 Imbalance History", history?.top_5_imbalance_pct, "%", 2)}
            ${historyPanel("Buy $10k Slip History", history?.buy_10k_bps, " bps", 2)}
            ${historyPanel("Buy $40k Slip History", history?.buy_40k_bps, " bps", 2)}
            ${historyPanel("Sell $10k Slip History", history?.sell_10k_bps, " bps", 2)}
            ${historyPanel("Sell $40k Slip History", history?.sell_40k_bps, " bps", 2)}
          </div>

          <div class="history-meta">History window: ${history ? `${history.samples} samples, ~${formatMaybe(history.approx_window_minutes, 1)} min, latest ${history.latest_label || "unknown"}` : "building from the first snapshot"}</div>
          <ul class="history-insights">${historyInsights}</ul>

          <div class="history-grid">
            ${historyPanel("Long Spread Trend", history?.long_horizon?.spread_bps, " bps", 2)}
            ${historyPanel("Long Imbalance Trend", history?.long_horizon?.top_5_imbalance_pct, "%", 2)}
            ${historyPanel("Long Buy $40k Slip", history?.long_horizon?.buy_40k_bps, " bps", 2)}
            ${historyPanel("Long Sell $40k Slip", history?.long_horizon?.sell_40k_bps, " bps", 2)}
          </div>

          <div class="history-meta">Robust window: ${history?.long_horizon ? `${history.long_horizon.buckets} buckets x ${formatMaybe(history.long_horizon.bucket_minutes, 0)} min, ~${formatMaybe(history.long_horizon.approx_window_hours, 1)} h, latest ${history.long_horizon.latest_label || "unknown"}` : "building from rollups"}</div>
          <ul class="history-insights">${longInsights}</ul>

          <div class="signal-note">Setup assessment is a conservative heuristic. It is not financial advice or an execution guarantee.</div>
          <ul class="history-insights">${setupNotes}</ul>

          <div class="signal-note">Signals are heuristic summaries derived from Coinbase position, product, portfolio summary, and product book fields.</div>
          <ul class="signals">${signals}</ul>
        </article>
      `;
    }

    function render(snapshot) {
      latestSnapshot = snapshot;
      const first = snapshot.positions[0];
      const firstWatch = snapshot.watch_markets?.[0];
      const firstSetup = first
        ? snapshot.setup_assessments?.[first.symbol]
        : (firstWatch ? snapshot.watch_assessments?.[firstWatch.symbol] : null);
      const firstChecklist = firstWatch ? snapshot.watch_entry_checklists?.[firstWatch.symbol] : null;
      const firstSizingPlan = firstWatch ? snapshot.watch_entry_sizing_plans?.[firstWatch.symbol] : null;
      const firstPrediction = first
        ? snapshot.position_predictions?.[first.symbol]
        : (firstWatch ? snapshot.watch_predictions?.[firstWatch.symbol] : null);
      const staleCount = (snapshot.open_orders || []).filter((order) => order.cleanup_candidate).length;
      document.getElementById("analysisBasis").textContent = snapshot.analysis_basis || "";
      document.getElementById("heroGrid").innerHTML = [
        metricCard("Positions", String(snapshot.positions.length)),
        metricCard("Open Orders", String((snapshot.open_orders || []).length)),
        metricCard(
          "Portfolio",
          snapshot.portfolio?.portfolio_type || "unknown",
          "",
          snapshot.portfolio?.id ? shortId(snapshot.portfolio.id) : ""
        ),
        metricCard("Credential Source", snapshot.credential_source || "unknown", "compact"),
        metricCard("Setup Status", firstSetup?.alignment_status || "no position"),
        metricCard("Entry Gate", firstChecklist?.overall_status || "n/a"),
        metricCard("Macro Risk", snapshot.market_context?.event_risk || "unknown"),
        metricCard("Suggested Max Lev", firstSetup?.suggested_max_leverage != null ? `${formatMaybe(firstSetup.suggested_max_leverage, 0)}x` : "unknown"),
        metricCard("Margin Use", firstSizingPlan?.margin_usage_pct != null ? `${formatMaybe(firstSizingPlan.margin_usage_pct, 0)}%` : "n/a"),
        metricCard("Model Bias", firstPrediction?.model_bias || "n/a"),
        metricCard("Stale Cleanup", String(staleCount)),
        metricCard("Effective Leverage", first?.effective_leverage != null ? `${formatMaybe(first.effective_leverage, 2)}x` : "flat"),
      ].join("");

      const cards = document.getElementById("cards");
      if (!snapshot.positions.length) {
        const watchHtml = (snapshot.watch_markets || []).length
          ? snapshot.watch_markets.map((watch) => watchCard(watch, snapshot.position_history?.[watch.symbol], snapshot.watch_assessments?.[watch.symbol], snapshot.watch_entry_checklists?.[watch.symbol], snapshot.watch_entry_sizing_plans?.[watch.symbol], snapshot.watch_predictions?.[watch.symbol])).join("")
          : `<div class="empty"><h2>No watch markets yet</h2><div class="empty-copy">Build history on a symbol or leave a related order open to keep a live flat-mode watch here.</div></div>`;
        cards.innerHTML = `${marketContextPanels(snapshot.market_context)}${openOrdersPanel(snapshot.open_orders)}<div class="empty"><h2>No open positions</h2><div class="empty-copy">You are flat. The dashboard is now showing order visibility and re-entry watch conditions instead of a blank state.</div></div>${watchHtml}`;
      } else {
        cards.innerHTML = `${marketContextPanels(snapshot.market_context)}${openOrdersPanel(snapshot.open_orders)}${snapshot.positions.map((position) => positionCard(position, snapshot.position_history?.[position.symbol], snapshot.setup_assessments?.[position.symbol], snapshot.position_predictions?.[position.symbol])).join("")}`;
      }
    }

    function setStatus(text) {
      document.getElementById("statusText").textContent = text;
    }

    function showError(text) {
      const box = document.getElementById("errorBox");
      box.style.display = "block";
      box.textContent = text;
    }

    function clearError() {
      const box = document.getElementById("errorBox");
      box.style.display = "none";
      box.textContent = "";
    }

    async function loadSnapshot() {
      if (isLoading) return;
      isLoading = true;
      setStatus("Refreshing...");
      try {
        const res = await fetch("/api/snapshot", { cache: "no-store" });
        if (!res.ok) {
          throw new Error(await res.text());
        }
        const snapshot = await res.json();
        render(snapshot);
        clearError();
        setStatus(`Last updated ${new Date().toLocaleTimeString()}`);
      } catch (error) {
        showError(`Snapshot refresh failed: ${error.message}`);
        setStatus("Refresh failed");
      } finally {
        isLoading = false;
      }
    }

    document.getElementById("refreshBtn").addEventListener("click", loadSnapshot);
    document.getElementById("copyJsonBtn").addEventListener("click", async () => {
      if (!latestSnapshot) return;
      await navigator.clipboard.writeText(JSON.stringify(latestSnapshot, null, 2));
      setStatus(`Copied JSON at ${new Date().toLocaleTimeString()}`);
    });

    loadSnapshot();
    setInterval(loadSnapshot, refreshMs);
  </script>
</body>
</html>
"#;

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    let portfolio_text = state
        .portfolio
        .as_deref()
        .unwrap_or("auto-select first INTX portfolio");

    let html = INDEX_HTML
        .replace("__REFRESH_MS__", &state.refresh_ms.to_string())
        .replace("__PORTFOLIO_TEXT__", &escape_html_text(portfolio_text));

    Html(html)
}

fn derive_watch_symbols(history: &HashMap<String, PersistedSymbolHistory>) -> Vec<String> {
    let mut ranked = history
        .iter()
        .filter_map(|(symbol, item)| {
            let latest = item
                .recent
                .last()
                .map(|sample| sample.recorded_at_ms)
                .or_else(|| item.rollups.last().map(|bucket| bucket.bucket_start_ms))?;
            Some((symbol.clone(), latest))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1));
    ranked.into_iter().map(|(symbol, _)| symbol).take(3).collect()
}

async fn snapshot(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let portfolio = state.portfolio.clone();
    let watch_symbols = match state.history.lock() {
        Ok(history) => derive_watch_symbols(&history),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Dashboard history lock is poisoned".to_string(),
            )
                .into_response()
        }
    };

    match tokio::task::spawn_blocking(move || load_output_with_watch(portfolio.as_deref(), &watch_symbols))
        .await
    {
        Ok(Ok(output)) => {
            let scope = build_market_context_scope(&output);
            let market_context = {
                let cached = match state.market_context_cache.lock() {
                    Ok(cache) => cache.clone(),
                    Err(_) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Market context cache lock is poisoned".to_string(),
                        )
                            .into_response()
                    }
                };

                let maybe_cached = cached.filter(|entry| {
                    entry.scope == scope
                        && now_millis().saturating_sub(entry.loaded_at_ms) <= MARKET_CONTEXT_TTL_MS
                });

                if let Some(entry) = maybe_cached {
                    entry.context
                } else {
                    let scope_for_load = scope.clone();
                    let loaded_context = match tokio::task::spawn_blocking(move || -> Result<MarketContext> {
                        let client = build_http_client()?;
                        Ok(load_market_context(&client, &scope_for_load))
                    })
                    .await
                    {
                        Ok(Ok(context)) => context,
                        Ok(Err(error)) => {
                            return (
                                StatusCode::BAD_GATEWAY,
                                format!("Failed to load market context: {error:#}"),
                            )
                                .into_response()
                        }
                        Err(error) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Market context worker failed: {error}"),
                            )
                                .into_response()
                        }
                    };

                    match state.market_context_cache.lock() {
                        Ok(mut cache) => {
                            *cache = Some(CachedMarketContext {
                                loaded_at_ms: now_millis(),
                                scope,
                                context: loaded_context.clone(),
                            });
                        }
                        Err(_) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Market context cache lock is poisoned".to_string(),
                            )
                                .into_response()
                        }
                    }

                    loaded_context
                }
            };
            let (position_history, prediction_inputs) = match state.history.lock() {
                Ok(mut history) => {
                    let summaries = upsert_history(&mut history, &output, &market_context);
                    let prediction_inputs = output
                        .positions
                        .iter()
                        .map(|position| position.symbol.clone())
                        .chain(output.watch_markets.iter().map(|watch| watch.symbol.clone()))
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .map(|symbol| (symbol.clone(), history.get(&symbol).cloned()))
                        .collect::<Vec<_>>();
                    if let Err(error) = save_history_file(&state.history_file, &history) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to persist dashboard history: {error:#}"),
                        )
                        .into_response();
                    }
                    (summaries, prediction_inputs)
                }
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Dashboard history lock is poisoned".to_string(),
                    )
                        .into_response()
                }
            };
            let state_for_predictions = Arc::clone(&state);
            let predictions = match tokio::task::spawn_blocking(move || -> Result<HashMap<String, ModelPrediction>> {
                let mut results = HashMap::new();
                for (symbol, symbol_history) in prediction_inputs {
                    results.insert(
                        symbol.clone(),
                        load_prediction_for_symbol(
                            &state_for_predictions.prediction_cache,
                            &symbol,
                            symbol_history.as_ref(),
                        )?,
                    );
                }
                Ok(results)
            })
            .await
            {
                Ok(Ok(items)) => items,
                Ok(Err(error)) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        format!("Failed to load model predictions: {error:#}"),
                    )
                        .into_response()
                }
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Prediction worker failed: {error}"),
                    )
                        .into_response()
                }
            };
            let setup_assessments = output
                .positions
                .iter()
                .map(|position| {
                    let history = position_history.get(&position.symbol);
                    (
                        position.symbol.clone(),
                        assess_trade_setup(position, history, &market_context),
                    )
                })
                .collect::<HashMap<_, _>>();
            let watch_assessments = output
                .watch_markets
                .iter()
                .map(|watch| {
                    let history = position_history.get(&watch.symbol);
                    (
                        watch.symbol.clone(),
                        assess_watch_setup(watch, history, &market_context),
                    )
                })
                .collect::<HashMap<_, _>>();
            let watch_entry_checklists = output
                .watch_markets
                .iter()
                .filter_map(|watch| {
                    let history = position_history.get(&watch.symbol);
                    let assessment = watch_assessments.get(&watch.symbol)?;
                    Some((
                        watch.symbol.clone(),
                        build_watch_entry_checklist(watch, history, assessment),
                    ))
                })
                .collect::<HashMap<_, _>>();
            let watch_entry_sizing_plans = output
                .watch_markets
                .iter()
                .filter_map(|watch| {
                    let assessment = watch_assessments.get(&watch.symbol)?;
                    let checklist = watch_entry_checklists.get(&watch.symbol)?;
                    Some((
                        watch.symbol.clone(),
                        build_watch_entry_sizing_plan(assessment, checklist),
                    ))
                })
                .collect::<HashMap<_, _>>();
            let position_predictions = output
                .positions
                .iter()
                .filter_map(|position| {
                    predictions
                        .get(&position.symbol)
                        .cloned()
                        .map(|prediction| (position.symbol.clone(), prediction))
                })
                .collect::<HashMap<_, _>>();
            let watch_predictions = output
                .watch_markets
                .iter()
                .filter_map(|watch| {
                    predictions
                        .get(&watch.symbol)
                        .cloned()
                        .map(|prediction| (watch.symbol.clone(), prediction))
                })
                .collect::<HashMap<_, _>>();

            let payload = DashboardSnapshot {
                output,
                position_history,
                market_context,
                setup_assessments,
                watch_assessments,
                watch_entry_checklists,
                watch_entry_sizing_plans,
                position_predictions,
                watch_predictions,
            };
            (StatusCode::OK, Json(payload)).into_response()
        }
        Ok(Err(error)) => (
            StatusCode::BAD_GATEWAY,
            format!("Failed to load Coinbase snapshot: {error:#}"),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Dashboard worker failed: {error}"),
        )
            .into_response(),
    }
}

fn escape_html_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let history = load_history_file(&args.history_file)?;
    let state = Arc::new(AppState {
        portfolio: args.portfolio,
        refresh_ms: args.refresh_seconds.saturating_mul(1000),
        history_file: args.history_file,
        history: Mutex::new(history),
        market_context_cache: Mutex::new(None),
        prediction_cache: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/snapshot", get(snapshot))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("failed to bind dashboard to {}", args.bind))?;

    println!("Dashboard listening on http://{}", args.bind);
    axum::serve(listener, app)
        .await
        .context("dashboard server exited unexpectedly")?;

    Ok(())
}
