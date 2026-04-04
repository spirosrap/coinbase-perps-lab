use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use coinbase_perps_lab::load_output;
use std::net::SocketAddr;
use std::sync::Arc;

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

#[derive(Clone)]
struct AppState {
    portfolio: Option<String>,
    refresh_ms: u64,
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
    .hero-grid, .stats-grid, .scenario-grid {
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

    function formatSigned(value, digits = 2) {
      if (value === null || value === undefined) return "unknown";
      const num = Number(value);
      if (!Number.isFinite(num)) return escapeHtml(value);
      const prefix = num > 0 ? "+" : "";
      return `${prefix}${num.toFixed(digits)}`;
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

    function positionCard(pos) {
      const displayName = pos.display_name ? ` (${escapeHtml(pos.display_name)})` : "";
      const signals = (pos.signals || []).map((signal) => `<li>${escapeHtml(signal)}</li>`).join("");
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
            ${statCard("Funding Direction", pos.funding_direction || "unknown")}
            ${statCard("Basis", formatPct(pos.basis_pct), toneClass(pos.basis_pct))}
            ${statCard("24h Change", formatPct(pos.price_change_24h_pct), toneClass(pos.price_change_24h_pct))}
            ${statCard("Liq Distance", formatPct(pos.distance_to_liquidation_pct))}
            ${statCard("Liq Price", pos.liquidation_price || "unknown")}
            ${statCard("Collateral", pos.collateral || "unknown")}
            ${statCard("Liq Buffer", pos.liquidation_buffer || "unknown")}
            ${statCard("Open Interest", pos.open_interest || "unknown")}
            ${statCard("Max Leverage", pos.max_leverage ? `${escapeHtml(pos.max_leverage)}x` : "unknown")}
          </div>

          <div class="scenario-grid">
            ${scenarioCard("+1% move", pos.projections?.up_1pct_pnl)}
            ${scenarioCard("+3% move", pos.projections?.up_3pct_pnl)}
            ${scenarioCard("-1% move", pos.projections?.down_1pct_pnl)}
            ${scenarioCard("-3% move", pos.projections?.down_3pct_pnl)}
          </div>

          <div class="signal-note">Signals are heuristic summaries derived from Coinbase position, product, and portfolio summary fields.</div>
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
        cards.innerHTML = snapshot.positions.map(positionCard).join("");
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

    match tokio::task::spawn_blocking(move || load_output(portfolio.as_deref())).await {
        Ok(Ok(output)) => (StatusCode::OK, Json(output)).into_response(),
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
