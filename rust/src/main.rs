//! edge-revenue-mcp server — stdio transport over the offline-first sales ledger.
//!
//! Six tools let an agent run payments at a venue with no reliable network:
//!   sale_record    — record a sale OFFLINE into the local append-only ledger
//!   receipt_verify — look up a sale by its receipt id (proof it happened)
//!   square_sync    — push queued sales to Square (no-op + report when offline)
//!   sync_status    — outbox counts: total / synced / queued / last_synced_at
//!   daily_tally    — sum sales by currency and by method over a time window
//!   reconcile      — flag anomalies (stuck-unsynced / synced-without-square-id)
//!
//! Environment:
//!   EDGE_REVENUE_DB_PATH   optional — defaults to ~/.edge-revenue/ledger.db
//!   SQUARE_ACCESS_TOKEN    optional — if UNSET, square_sync runs in offline
//!                          mode (queues, never errors). The actual Square HTTP
//!                          call is stubbed (see store::push_to_square TODO).
//!   RUST_LOG               optional — tracing filter (logs go to stderr).

use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use tracing_subscriber::EnvFilter;

use edge_revenue_mcp::{Store, Tally, Transaction};

/// Default "stuck unsynced" threshold for `reconcile`: 1 hour in millis.
const DEFAULT_RECONCILE_THRESHOLD_MS: i64 = 60 * 60 * 1000;

#[derive(Clone)]
struct EdgeRevenueServer {
    store: Arc<Store>,
    /// Square access token; `None` => offline mode (square_sync never errors).
    square_token: Option<String>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SaleRecordReq {
    #[schemars(description = "Sale amount in integer cents (e.g. 1299 = $12.99). Must be positive.")]
    amount_cents: i64,
    #[serde(default)]
    #[schemars(description = "ISO currency code; defaults to USD if empty")]
    currency: String,
    #[schemars(description = "What was sold (e.g. 'T-shirt', 'cover charge')")]
    item: String,
    #[schemars(description = "Payment method, e.g. 'cash' or 'card'")]
    method: String,
    #[schemars(description = "Optional note recorded with the sale")]
    note: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReceiptVerifyReq {
    #[schemars(description = "The receipt id returned by sale_record")]
    receipt_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DailyTallyReq {
    #[schemars(description = "Optional window start, epoch millis (inclusive)")]
    since_ms: Option<i64>,
    #[schemars(description = "Optional window end, epoch millis (inclusive)")]
    until_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReconcileReq {
    #[schemars(description = "How old (in millis) an unsynced sale must be to count as 'stuck'. Default 1h.")]
    threshold_ms: Option<i64>,
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn money(cents: i64, currency: &str) -> String {
    format!("{}.{:02} {}", cents / 100, (cents % 100).abs(), currency)
}

fn render_tally(label: &str, rows: &[Tally]) -> String {
    if rows.is_empty() {
        return format!("  {label}: (none)\n");
    }
    let mut out = format!("  {label}:\n");
    for t in rows {
        out.push_str(&format!(
            "    {} — {} ({} sale(s))\n",
            t.key,
            money(t.total_cents, ""),
            t.count
        ));
    }
    out
}

fn render_tx(tx: &Transaction) -> String {
    let synced = if tx.synced {
        format!("synced (square_id {})", tx.square_id.as_deref().unwrap_or("?"))
    } else {
        "queued (offline, not yet synced)".to_string()
    };
    let note = tx.note.as_deref().map(|n| format!(" note: {n}")).unwrap_or_default();
    format!(
        "Receipt {}: {} for '{}' via {} — {}.{}",
        tx.receipt_id,
        money(tx.amount_cents, &tx.currency),
        tx.item,
        tx.method,
        synced,
        note
    )
}

#[tool_router]
impl EdgeRevenueServer {
    fn new(store: Arc<Store>, square_token: Option<String>) -> Self {
        Self {
            store,
            square_token,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Record a sale OFFLINE into the local append-only ledger. Touches no network — safe with no connectivity. Returns the receipt id; the sale is queued for Square sync.")]
    async fn sale_record(&self, Parameters(r): Parameters<SaleRecordReq>) -> Result<String, McpError> {
        let tx = self
            .store
            .record_sale(r.amount_cents, &r.currency, &r.item, &r.method, r.note.as_deref())
            .await
            .map_err(internal)?;
        Ok(format!(
            "Recorded sale: {} for '{}' via {}. Receipt id: {} (queued offline, not yet synced to Square).",
            money(tx.amount_cents, &tx.currency),
            tx.item,
            tx.method,
            tx.receipt_id
        ))
    }

    #[tool(description = "Verify a sale by its receipt id — returns the transaction if found (proof the sale happened), else not-found.")]
    async fn receipt_verify(&self, Parameters(r): Parameters<ReceiptVerifyReq>) -> Result<String, McpError> {
        match self.store.get_transaction(&r.receipt_id).await.map_err(internal)? {
            Some(tx) => Ok(render_tx(&tx)),
            None => Ok(format!("No sale found for receipt id {}.", r.receipt_id)),
        }
    }

    #[tool(description = "Push all queued (unsynced) sales to Square and mark them synced. If SQUARE_ACCESS_TOKEN is unset, runs in offline mode: reports the queue depth and syncs nothing (never errors).")]
    async fn square_sync(&self, Parameters(_): Parameters<EmptyReq>) -> Result<String, McpError> {
        let (synced_now, still_queued) = self
            .store
            .square_sync(self.square_token.as_deref())
            .await
            .map_err(internal)?;
        if self.square_token.is_none() {
            Ok(format!(
                "Offline mode: {still_queued} transaction(s) queued, not synced (SQUARE_ACCESS_TOKEN unset)."
            ))
        } else {
            Ok(format!(
                "Synced {synced_now} transaction(s) to Square. {still_queued} still queued."
            ))
        }
    }

    #[tool(description = "Outbox status: total transactions, synced, queued (unsynced), and the last sync time (epoch millis).")]
    async fn sync_status(&self, Parameters(_): Parameters<EmptyReq>) -> Result<String, McpError> {
        let (total, synced, queued, last) = self.store.sync_status().await.map_err(internal)?;
        let last = last.map(|ms| ms.to_string()).unwrap_or_else(|| "never".to_string());
        Ok(format!(
            "total={total}, synced={synced}, queued={queued}, last_synced_at={last}"
        ))
    }

    #[tool(description = "Sum sales by currency and by method over an optional [since_ms, until_ms] window. Returns totals in cents plus sale counts.")]
    async fn daily_tally(&self, Parameters(r): Parameters<DailyTallyReq>) -> Result<String, McpError> {
        let (by_currency, by_method, count) = self
            .store
            .daily_tally(r.since_ms, r.until_ms)
            .await
            .map_err(internal)?;
        let mut out = format!("Tally over {count} sale(s):\n");
        out.push_str(&render_tally("by currency", &by_currency));
        out.push_str(&render_tally("by method", &by_method));
        Ok(out)
    }

    #[tool(description = "Reconcile the ledger: flag transactions stuck unsynced longer than the threshold, and synced rows missing a square_id (a bug indicator).")]
    async fn reconcile(&self, Parameters(r): Parameters<ReconcileReq>) -> Result<String, McpError> {
        let threshold = r.threshold_ms.unwrap_or(DEFAULT_RECONCILE_THRESHOLD_MS);
        let report = self.store.reconcile(threshold).await.map_err(internal)?;
        if report.stuck_unsynced.is_empty() && report.synced_without_square_id.is_empty() {
            return Ok(format!(
                "Reconcile clean (threshold {}ms): no stuck-unsynced and no synced-without-square_id rows.",
                report.threshold_ms
            ));
        }
        let mut out = format!("Reconcile anomalies (threshold {}ms):\n", report.threshold_ms);
        if !report.stuck_unsynced.is_empty() {
            out.push_str(&format!(
                "  stuck unsynced ({}): {}\n",
                report.stuck_unsynced.len(),
                report.stuck_unsynced.join(", ")
            ));
        }
        if !report.synced_without_square_id.is_empty() {
            out.push_str(&format!(
                "  synced without square_id ({}): {}\n",
                report.synced_without_square_id.len(),
                report.synced_without_square_id.join(", ")
            ));
        }
        Ok(out)
    }
}

/// Empty request for the no-argument tools (rmcp needs a Parameters type).
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct EmptyReq {}

#[tool_handler]
impl ServerHandler for EdgeRevenueServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Offline-first edge payments. Record sales with sale_record (works with no \
             network); they queue in a local append-only ledger. square_sync pushes the \
             queue to Square when connectivity returns (no-op + report when offline). \
             receipt_verify proves a sale, sync_status shows the outbox, daily_tally sums \
             takings, reconcile flags anomalies. Money is integer cents."
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "edge-revenue-mcp".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info
    }
}

fn default_db_path() -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    format!("{home}/.edge-revenue/ledger.db")
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the JSON-RPC channel — all logging MUST go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let db_path = std::env::var("EDGE_REVENUE_DB_PATH").unwrap_or_else(|_| default_db_path());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let square_token = std::env::var("SQUARE_ACCESS_TOKEN").ok().filter(|s| !s.is_empty());
    let store = Store::open(&db_path).await?;

    tracing::info!(
        "edge-revenue-mcp v{} ready (db: {db_path}, square sync: {})",
        env!("CARGO_PKG_VERSION"),
        if square_token.is_some() { "enabled" } else { "OFFLINE (no SQUARE_ACCESS_TOKEN)" }
    );

    let service = EdgeRevenueServer::new(Arc::new(store), square_token)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
