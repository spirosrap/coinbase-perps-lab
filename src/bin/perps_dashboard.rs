use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use coinbase_perps_lab::{load_output, OrderBookSummary, Output, PositionSummary, SlippageEstimate};
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const MAX_HISTORY_POINTS: usize = 240;

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
}

struct AppState {
    portfolio: Option<String>,
    refresh_ms: u64,
    history: Mutex<HashMap<String, Vec<PositionHistorySample>>>,
}

#[derive(Debug, Clone)]
struct PositionHistorySample {
    id: String,
    label: String,
    spread_bps: Option<f64>,
    top_5_imbalance_pct: Option<f64>,
    buy_10k_bps: Option<f64>,
    buy_40k_bps: Option<f64>,
    sell_10k_bps: Option<f64>,
    sell_40k_bps: Option<f64>,
}

#[derive(Debug, Serialize)]
struct DashboardSnapshot {
    #[serde(flatten)]
    output: Output,
    position_history: HashMap<String, PositionHistorySummary>,
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
}

#[derive(Debug, Serialize)]
struct MetricHistorySummary {
    current: f64,
    min: f64,
    max: f64,
    delta_from_oldest: f64,
    points: Vec<MetricPoint>,
}

#[derive(Debug, Serialize)]
struct MetricPoint {
    label: String,
    value: f64,
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

fn history_sample(position: &PositionSummary) -> Option<PositionHistorySample> {
    let book = position.order_book.as_ref()?;
    Some(PositionHistorySample {
        id: sample_id(Some(book)),
        label: sample_label(Some(book)),
        spread_bps: book.spread_bps,
        top_5_imbalance_pct: book.top_5_imbalance_pct,
        buy_10k_bps: find_slippage_bps(&book.buy_slippage, 10_000.0),
        buy_40k_bps: find_slippage_bps(&book.buy_slippage, 40_000.0),
        sell_10k_bps: find_slippage_bps(&book.sell_slippage, 10_000.0),
        sell_40k_bps: find_slippage_bps(&book.sell_slippage, 40_000.0),
    })
}

fn upsert_history(
    history: &mut HashMap<String, Vec<PositionHistorySample>>,
    output: &Output,
    refresh_ms: u64,
) -> HashMap<String, PositionHistorySummary> {
    for position in &output.positions {
        let Some(sample) = history_sample(position) else {
            continue;
        };

        let series = history.entry(position.symbol.clone()).or_default();
        if series
            .last()
            .map(|existing| existing.id == sample.id)
            .unwrap_or(false)
        {
            if let Some(last) = series.last_mut() {
                *last = sample;
            }
        } else {
            series.push(sample);
            if series.len() > MAX_HISTORY_POINTS {
                let overflow = series.len() - MAX_HISTORY_POINTS;
                series.drain(0..overflow);
            }
        }
    }

    history
        .iter()
        .map(|(symbol, samples)| {
            (
                symbol.clone(),
                summarize_position_history(samples, refresh_ms),
            )
        })
        .collect()
}

fn metric_summary<F>(
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

    Some(MetricHistorySummary {
        current,
        min,
        max,
        delta_from_oldest: current - first.value,
        points,
    })
}

fn summarize_position_history(samples: &[PositionHistorySample], refresh_ms: u64) -> PositionHistorySummary {
    let spread_bps = metric_summary(samples, |sample| sample.spread_bps);
    let top_5_imbalance_pct = metric_summary(samples, |sample| sample.top_5_imbalance_pct);
    let buy_10k_bps = metric_summary(samples, |sample| sample.buy_10k_bps);
    let buy_40k_bps = metric_summary(samples, |sample| sample.buy_40k_bps);
    let sell_10k_bps = metric_summary(samples, |sample| sample.sell_10k_bps);
    let sell_40k_bps = metric_summary(samples, |sample| sample.sell_40k_bps);

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
        samples: samples.len(),
        approx_window_minutes: ((samples.len().saturating_sub(1)) as f64 * refresh_ms as f64) / 60_000.0,
        latest_label: samples.last().map(|sample| sample.label.clone()),
        insights,
        spread_bps,
        top_5_imbalance_pct,
        buy_10k_bps,
        buy_40k_bps,
        sell_10k_bps,
        sell_40k_bps,
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
    }
    .metric-label {
      font-size: 0.8rem;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      margin-bottom: 8px;
    }
    .metric-value {
      font-size: 1.45rem;
      font-weight: 720;
      line-height: 1.1;
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
      if (lower.includes("bear") || lower.includes("risk")) return "bad";
      if (lower.includes("caut")) return "warn";
      return "neutral";
    }

    function metricCard(label, value, extraClass = "") {
      return `<article class="metric"><div class="metric-label">${escapeHtml(label)}</div><div class="metric-value ${extraClass}">${escapeHtml(value)}</div></article>`;
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

    function positionCard(pos, history) {
      const displayName = pos.display_name ? ` (${escapeHtml(pos.display_name)})` : "";
      const signals = (pos.signals || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
      const historyInsights = (history?.insights || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
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
              <span class="badge ${badgeClass(pos.market_bias)}">${escapeHtml(pos.market_bias)}</span>
              <span class="badge ${badgeClass(pos.position_outlook)}">${escapeHtml(pos.position_outlook)}</span>
              <span class="badge neutral">${escapeHtml(pos.outlook_confidence)} confidence</span>
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
          </div>

          <div class="scenario-grid">
            ${scenarioCard("+1% move", pos.projections?.up_1pct_pnl)}
            ${scenarioCard("+3% move", pos.projections?.up_3pct_pnl)}
            ${scenarioCard("-1% move", pos.projections?.down_1pct_pnl)}
            ${scenarioCard("-3% move", pos.projections?.down_3pct_pnl)}
          </div>

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

          <div class="signal-note">Signals are heuristic summaries derived from Coinbase position, product, portfolio summary, and product book fields.</div>
          <ul class="signals">${signals}</ul>
        </article>
      `;
    }

    function render(snapshot) {
      latestSnapshot = snapshot;
      const first = snapshot.positions[0];
      document.getElementById("analysisBasis").textContent = snapshot.analysis_basis || "";
      document.getElementById("heroGrid").innerHTML = [
        metricCard("Positions", String(snapshot.positions.length)),
        metricCard("Portfolio", snapshot.portfolio?.portfolio_type ? `${snapshot.portfolio.id} (${snapshot.portfolio.portfolio_type})` : "unknown"),
        metricCard("Credential Source", snapshot.credential_source || "unknown"),
        metricCard("Primary Bias", first?.market_bias || "no position"),
        metricCard("Primary Outlook", first?.position_outlook || "no position"),
        metricCard("Effective Leverage", first?.effective_leverage != null ? `${formatMaybe(first.effective_leverage, 2)}x` : "unknown"),
      ].join("");

      const cards = document.getElementById("cards");
      if (!snapshot.positions.length) {
        cards.innerHTML = `<div class="empty"><h2>No open positions</h2><div class="empty-copy">The dashboard is live, but Coinbase returned no open INTX perpetual positions.</div></div>`;
      } else {
        cards.innerHTML = snapshot.positions.map((position) => positionCard(position, snapshot.position_history?.[position.symbol])).join("");
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

async fn snapshot(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let portfolio = state.portfolio.clone();
    let refresh_ms = state.refresh_ms;

    match tokio::task::spawn_blocking(move || load_output(portfolio.as_deref())).await {
        Ok(Ok(output)) => {
            let position_history = match state.history.lock() {
                Ok(mut history) => upsert_history(&mut history, &output, refresh_ms),
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Dashboard history lock is poisoned".to_string(),
                    )
                        .into_response()
                }
            };

            let payload = DashboardSnapshot {
                output,
                position_history,
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
    let state = Arc::new(AppState {
        portfolio: args.portfolio,
        refresh_ms: args.refresh_seconds.saturating_mul(1000),
        history: Mutex::new(HashMap::new()),
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
