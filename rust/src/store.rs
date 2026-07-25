//! The edge-payments store: a local, append-only sales ledger with a Square
//! sync outbox, all in one local libsql database.
//!
//! Design notes
//! - **Sales are recorded fully offline.** [`Store::record_sale`] writes one
//!   immutable row with `synced = 0`. No network is touched at sale time, so a
//!   Pi at a venue with no connectivity still takes money reliably.
//! - **The ledger is append-only.** Sales are never updated except to flip
//!   `synced 0 -> 1` and stamp the returned `square_id`; nothing is deleted.
//!   A `void_sale` should be a *compensating* row, not a mutation (see TASKS).
//! - **Money is integer cents.** `amount_cents: i64` — never a float, so totals
//!   are exact.
//! - **Outbox / sync state machine.** A sale is `synced = 0` (queued) until
//!   [`Store::square_sync`] pushes it. If `SQUARE_ACCESS_TOKEN` is unset the
//!   sync is a no-op that reports the queue depth (fail-safe), so going offline
//!   never raises an error.

use std::time::{SystemTime, UNIX_EPOCH};

use libsql::{params_from_iter, Builder, Connection, Database, Value};

use crate::error::{Error, Result};

fn db_err(e: libsql::Error) -> Error {
    Error::Db(e.to_string())
}

/// Wall-clock epoch milliseconds. Single source of time for the whole store.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One recorded sale in the local ledger.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Transaction {
    /// Public receipt id handed back at sale time (a uuid v4). The thing a fan
    /// or auditor quotes to prove a sale happened.
    pub receipt_id: String,
    pub amount_cents: i64,
    pub currency: String,
    pub item: String,
    /// Payment method, e.g. "cash" / "card".
    pub method: String,
    pub note: Option<String>,
    /// `false` while queued in the outbox, `true` once pushed to Square.
    pub synced: bool,
    /// Square's id for the synced payment; `None` until synced.
    pub square_id: Option<String>,
    pub created_at: i64,
    /// When the row was pushed to Square; `None` until synced.
    pub synced_at: Option<i64>,
}

/// A summed-up slice of the ledger, grouped one way (by currency or by method).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Tally {
    pub key: String,
    pub total_cents: i64,
    pub count: i64,
}

/// The result of a [`Store::reconcile`] pass: anomalies that need a human.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReconcileReport {
    /// Receipt ids that are still unsynced and older than the threshold.
    pub stuck_unsynced: Vec<String>,
    /// Receipt ids marked synced but missing a square_id (an impossible state
    /// that means a bug or a partial write — flag loudly).
    pub synced_without_square_id: Vec<String>,
    pub threshold_ms: i64,
    pub checked_at: i64,
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS transactions (
        receipt_id   TEXT PRIMARY KEY,
        amount_cents INTEGER NOT NULL,
        currency     TEXT NOT NULL DEFAULT 'USD',
        item         TEXT NOT NULL,
        method       TEXT NOT NULL,
        note         TEXT,
        synced       INTEGER NOT NULL DEFAULT 0,
        square_id    TEXT,
        created_at   INTEGER NOT NULL,
        synced_at    INTEGER
    )",
    "CREATE INDEX IF NOT EXISTS idx_tx_synced ON transactions(synced)",
    "CREATE INDEX IF NOT EXISTS idx_tx_created ON transactions(created_at)",
];

const SELECT_COLS: &str = "receipt_id, amount_cents, currency, item, method, note, \
                           synced, square_id, created_at, synced_at";

/// The edge-payments store. Wraps one local libsql database.
pub struct Store {
    _db: Database,
    conn: Connection,
}

impl Store {
    /// Open (or create) the ledger database at `path`.
    pub async fn open(path: &str) -> Result<Self> {
        let db = Builder::new_local(path).build().await.map_err(db_err)?;
        let conn = db.connect().map_err(db_err)?;
        let store = Store { _db: db, conn };
        store.init().await?;
        Ok(store)
    }

    async fn init(&self) -> Result<()> {
        for stmt in SCHEMA {
            self.conn.execute(stmt, ()).await.map_err(db_err)?;
        }
        Ok(())
    }

    // --- recording sales (offline) -------------------------------------------

    /// Record a sale fully offline. Writes one immutable `synced = 0` row and
    /// returns the new [`Transaction`] (carrying its receipt id). No network is
    /// touched — this is safe to call on a Pi with no connectivity.
    pub async fn record_sale(
        &self,
        amount_cents: i64,
        currency: &str,
        item: &str,
        method: &str,
        note: Option<&str>,
    ) -> Result<Transaction> {
        if amount_cents <= 0 {
            return Err(Error::Invalid("amount_cents must be positive".into()));
        }
        if item.trim().is_empty() {
            return Err(Error::Invalid("item cannot be empty".into()));
        }
        if method.trim().is_empty() {
            return Err(Error::Invalid("method cannot be empty".into()));
        }
        let currency = if currency.trim().is_empty() {
            "USD".to_string()
        } else {
            currency.trim().to_uppercase()
        };
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        self.conn
            .execute(
                "INSERT INTO transactions \
                 (receipt_id, amount_cents, currency, item, method, note, synced, square_id, created_at, synced_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, NULL, ?7, NULL)",
                params_from_iter([
                    Value::Text(receipt_id.clone()),
                    Value::Integer(amount_cents),
                    Value::Text(currency.clone()),
                    Value::Text(item.to_string()),
                    Value::Text(method.to_string()),
                    note.map(|s| Value::Text(s.to_string())).unwrap_or(Value::Null),
                    Value::Integer(now),
                ]),
            )
            .await
            .map_err(db_err)?;
        Ok(Transaction {
            receipt_id,
            amount_cents,
            currency,
            item: item.to_string(),
            method: method.to_string(),
            note: note.map(|s| s.to_string()),
            synced: false,
            square_id: None,
            created_at: now,
            synced_at: None,
        })
    }

    /// Look up a single transaction by its receipt id (proof a sale happened).
    pub async fn get_transaction(&self, receipt_id: &str) -> Result<Option<Transaction>> {
        let sql = format!("SELECT {SELECT_COLS} FROM transactions WHERE receipt_id = ?1");
        let mut rows = self
            .conn
            .query(&sql, params_from_iter([Value::Text(receipt_id.to_string())]))
            .await
            .map_err(db_err)?;
        match rows.next().await.map_err(db_err)? {
            Some(row) => Ok(Some(row_to_tx(&row)?)),
            None => Ok(None),
        }
    }

    /// All transactions still in the outbox (`synced = 0`), oldest first.
    pub async fn queued(&self) -> Result<Vec<Transaction>> {
        let sql = format!(
            "SELECT {SELECT_COLS} FROM transactions WHERE synced = 0 ORDER BY created_at ASC, rowid ASC"
        );
        let mut rows = self.conn.query(&sql, ()).await.map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.push(row_to_tx(&row)?);
        }
        Ok(out)
    }

    // --- Square sync outbox ---------------------------------------------------

    /// Push every queued (`synced = 0`) transaction to Square and mark it
    /// `synced = 1` with the returned Square id.
    ///
    /// **Fail-safe:** if `token` is `None` (i.e. `SQUARE_ACCESS_TOKEN` is unset)
    /// this does NOT error. It returns `(0, queued_count)` so the caller can
    /// report "offline mode: N queued, not synced" and keep taking sales.
    ///
    /// Returns `(synced_now, still_queued_after)`.
    pub async fn square_sync(&self, token: Option<&str>) -> Result<(usize, usize)> {
        let pending = self.queued().await?;
        let token = match token {
            Some(t) if !t.is_empty() => t,
            // Offline mode: nothing synced, everything stays queued. No error.
            _ => return Ok((0, pending.len())),
        };

        let mut synced_now = 0usize;
        for tx in &pending {
            // The one network call. Stubbed today (see push_to_square TODO).
            let square_id = push_to_square(token, tx)?;
            let now = now_millis();
            self.conn
                .execute(
                    "UPDATE transactions SET synced = 1, square_id = ?2, synced_at = ?3 \
                     WHERE receipt_id = ?1 AND synced = 0",
                    params_from_iter([
                        Value::Text(tx.receipt_id.clone()),
                        Value::Text(square_id),
                        Value::Integer(now),
                    ]),
                )
                .await
                .map_err(db_err)?;
            synced_now += 1;
        }
        let still_queued = self.queued().await?.len();
        Ok((synced_now, still_queued))
    }

    // --- reporting ------------------------------------------------------------

    /// Outbox counts: total rows, synced, queued (unsynced), and the most
    /// recent `synced_at` timestamp (`None` if nothing has ever synced).
    pub async fn sync_status(&self) -> Result<(i64, i64, i64, Option<i64>)> {
        let mut rows = self
            .conn
            .query(
                "SELECT \
                   COUNT(*), \
                   COALESCE(SUM(synced), 0), \
                   COALESCE(SUM(CASE WHEN synced = 0 THEN 1 ELSE 0 END), 0), \
                   MAX(synced_at) \
                 FROM transactions",
                (),
            )
            .await
            .map_err(db_err)?;
        match rows.next().await.map_err(db_err)? {
            Some(row) => {
                let total: i64 = row.get(0).map_err(db_err)?;
                let synced: i64 = row.get(1).map_err(db_err)?;
                let queued: i64 = row.get(2).map_err(db_err)?;
                // MAX over an all-NULL / empty column comes back as NULL.
                let last_synced_at: Option<i64> = row.get(3).map_err(db_err)?;
                Ok((total, synced, queued, last_synced_at))
            }
            None => Ok((0, 0, 0, None)),
        }
    }

    /// Sum `amount_cents` over an optional time window, grouped by `column`
    /// (`currency` or `method`). Internal helper for [`Store::daily_tally`].
    async fn tally_by(
        &self,
        column: &str,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    ) -> Result<Vec<Tally>> {
        // `column` is never user input — it's a fixed literal from daily_tally.
        let mut sql = format!(
            "SELECT {column} AS k, COALESCE(SUM(amount_cents), 0), COUNT(*) \
             FROM transactions WHERE 1 = 1"
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(s) = since_ms {
            sql.push_str(" AND created_at >= ?");
            args.push(Value::Integer(s));
        }
        if let Some(u) = until_ms {
            sql.push_str(" AND created_at <= ?");
            args.push(Value::Integer(u));
        }
        sql.push_str(" GROUP BY k ORDER BY k ASC");

        let mut rows = self.conn.query(&sql, params_from_iter(args)).await.map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.push(Tally {
                key: row.get(0).map_err(db_err)?,
                total_cents: row.get(1).map_err(db_err)?,
                count: row.get(2).map_err(db_err)?,
            });
        }
        Ok(out)
    }

    /// Daily (or any-window) tally: returns `(by_currency, by_method, count)`
    /// over the optional `[since_ms, until_ms]` window (inclusive).
    pub async fn daily_tally(
        &self,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    ) -> Result<(Vec<Tally>, Vec<Tally>, i64)> {
        let by_currency = self.tally_by("currency", since_ms, until_ms).await?;
        let by_method = self.tally_by("method", since_ms, until_ms).await?;
        let count = by_currency.iter().map(|t| t.count).sum();
        Ok((by_currency, by_method, count))
    }

    /// Flag anomalies for a human to chase:
    /// - transactions still unsynced and older than `threshold_ms`
    ///   (the Pi has likely been offline too long — go check connectivity);
    /// - rows marked synced but missing a square_id (a should-be-impossible
    ///   state pointing at a bug or partial write).
    pub async fn reconcile(&self, threshold_ms: i64) -> Result<ReconcileReport> {
        let now = now_millis();
        let cutoff = now - threshold_ms.max(0);

        let mut stuck_unsynced = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT receipt_id FROM transactions \
                 WHERE synced = 0 AND created_at < ?1 ORDER BY created_at ASC",
                params_from_iter([Value::Integer(cutoff)]),
            )
            .await
            .map_err(db_err)?;
        while let Some(row) = rows.next().await.map_err(db_err)? {
            stuck_unsynced.push(row.get(0).map_err(db_err)?);
        }

        let mut synced_without_square_id = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT receipt_id FROM transactions \
                 WHERE synced = 1 AND (square_id IS NULL OR square_id = '') \
                 ORDER BY created_at ASC",
                (),
            )
            .await
            .map_err(db_err)?;
        while let Some(row) = rows.next().await.map_err(db_err)? {
            synced_without_square_id.push(row.get(0).map_err(db_err)?);
        }

        Ok(ReconcileReport {
            stuck_unsynced,
            synced_without_square_id,
            threshold_ms,
            checked_at: now,
        })
    }
}

// --- Square integration -------------------------------------------------------

/// Push a single transaction to Square and return its Square payment id.
///
/// TODO: wire the real Square Payments API here. Replace the stub body
/// with an HTTPS POST to `https://connect.squareup.com/v2/payments` carrying:
///   - header `Authorization: Bearer {token}`
///   - header `Square-Version: 2024-xx-xx`
///   - a JSON body `{ "idempotency_key": <receipt_id>, "amount_money": {
///     "amount": <amount_cents>, "currency": <currency> }, "source_id":
///     "EXTERNAL" or a card nonce, "note": <note> }`
/// Use the transaction's `receipt_id` as the idempotency_key so a retry after a
/// dropped response never double-charges (see TASKS.md). Return the real
/// `payment.id` from the response. On a network failure, return `Err(..)` and
/// the row stays queued (synced = 0) for the next sync pass — that is the whole
/// point of the outbox.
///
/// Until then this returns a deterministic fake id derived from the receipt so
/// the outbox state machine is fully exercised and testable.
pub fn push_to_square(_token: &str, tx: &Transaction) -> Result<String> {
    // STUB — no network. Real call described in the TODO above.
    Ok(format!("sq_{}", tx.receipt_id))
}

// --- helpers ------------------------------------------------------------------

fn row_to_tx(row: &libsql::Row) -> Result<Transaction> {
    let synced: i64 = row.get(6).map_err(db_err)?;
    Ok(Transaction {
        receipt_id: row.get(0).map_err(db_err)?,
        amount_cents: row.get(1).map_err(db_err)?,
        currency: row.get(2).map_err(db_err)?,
        item: row.get(3).map_err(db_err)?,
        method: row.get(4).map_err(db_err)?,
        note: row.get(5).map_err(db_err)?,
        synced: synced != 0,
        square_id: row.get(7).map_err(db_err)?,
        created_at: row.get(8).map_err(db_err)?,
        synced_at: row.get(9).map_err(db_err)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let store = Store::open(path.to_str().unwrap()).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn offline_record_is_queued() {
        let (_d, s) = open_tmp().await;
        let tx = s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();
        assert!(!tx.synced);
        assert!(tx.square_id.is_none());
        assert_eq!(tx.amount_cents, 500);

        // It lands in the outbox.
        let queued = s.queued().await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].receipt_id, tx.receipt_id);

        let (total, synced, q, last) = s.sync_status().await.unwrap();
        assert_eq!((total, synced, q), (1, 0, 1));
        assert!(last.is_none());
    }

    #[tokio::test]
    async fn record_rejects_bad_input() {
        let (_d, s) = open_tmp().await;
        assert!(s.record_sale(0, "USD", "x", "cash", None).await.is_err());
        assert!(s.record_sale(-5, "USD", "x", "cash", None).await.is_err());
        assert!(s.record_sale(100, "USD", "  ", "cash", None).await.is_err());
        assert!(s.record_sale(100, "USD", "x", "", None).await.is_err());
        // empty currency defaults to USD
        let tx = s.record_sale(100, "", "x", "cash", None).await.unwrap();
        assert_eq!(tx.currency, "USD");
    }

    #[tokio::test]
    async fn square_sync_offline_mode_does_not_error() {
        let (_d, s) = open_tmp().await;
        s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();
        s.record_sale(800, "USD", "Hat", "card", None).await.unwrap();

        // No token => fail-safe: nothing synced, both still queued, no error.
        let (synced_now, still_queued) = s.square_sync(None).await.unwrap();
        assert_eq!((synced_now, still_queued), (0, 2));
        let (_t, synced, queued, _l) = s.sync_status().await.unwrap();
        assert_eq!((synced, queued), (0, 2));
    }

    #[tokio::test]
    async fn square_sync_marks_synced_with_token() {
        let (_d, s) = open_tmp().await;
        let a = s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();
        let b = s.record_sale(800, "USD", "Hat", "card", None).await.unwrap();

        let (synced_now, still_queued) = s.square_sync(Some("test-token")).await.unwrap();
        assert_eq!((synced_now, still_queued), (2, 0));

        // Both rows now synced and carry a square_id (from the stub).
        let ta = s.get_transaction(&a.receipt_id).await.unwrap().unwrap();
        let tb = s.get_transaction(&b.receipt_id).await.unwrap().unwrap();
        assert!(ta.synced && tb.synced);
        assert_eq!(ta.square_id.as_deref(), Some(format!("sq_{}", a.receipt_id).as_str()));
        assert!(tb.square_id.is_some());
        assert!(ta.synced_at.is_some());

        let (total, synced, queued, last) = s.sync_status().await.unwrap();
        assert_eq!((total, synced, queued), (2, 2, 0));
        assert!(last.is_some());

        // A second sync with nothing queued is a clean no-op.
        let (n, q) = s.square_sync(Some("test-token")).await.unwrap();
        assert_eq!((n, q), (0, 0));
    }

    #[tokio::test]
    async fn daily_tally_sums_by_currency_and_method() {
        let (_d, s) = open_tmp().await;
        s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();
        s.record_sale(800, "USD", "Hat", "card", None).await.unwrap();
        s.record_sale(200, "USD", "Sticker", "cash", None).await.unwrap();

        let (by_currency, by_method, count) = s.daily_tally(None, None).await.unwrap();
        assert_eq!(count, 3);

        // one currency, summing all three
        assert_eq!(by_currency.len(), 1);
        assert_eq!(by_currency[0].key, "USD");
        assert_eq!(by_currency[0].total_cents, 1500);
        assert_eq!(by_currency[0].count, 3);

        // two methods
        let cash = by_method.iter().find(|t| t.key == "cash").unwrap();
        let card = by_method.iter().find(|t| t.key == "card").unwrap();
        assert_eq!(cash.total_cents, 700);
        assert_eq!(cash.count, 2);
        assert_eq!(card.total_cents, 800);
        assert_eq!(card.count, 1);
    }

    #[tokio::test]
    async fn daily_tally_respects_window() {
        let (_d, s) = open_tmp().await;
        s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();
        // a window entirely in the future excludes everything
        let future = now_millis() + 1_000_000;
        let (by_currency, _by_method, count) =
            s.daily_tally(Some(future), None).await.unwrap();
        assert_eq!(count, 0);
        assert!(by_currency.is_empty());
    }

    #[tokio::test]
    async fn receipt_verify_hit_and_miss() {
        let (_d, s) = open_tmp().await;
        let tx = s.record_sale(500, "USD", "T-shirt", "cash", Some("VIP")).await.unwrap();

        let hit = s.get_transaction(&tx.receipt_id).await.unwrap();
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().note.as_deref(), Some("VIP"));

        let miss = s.get_transaction("does-not-exist").await.unwrap();
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn reconcile_flags_stuck_unsynced() {
        let (_d, s) = open_tmp().await;
        s.record_sale(500, "USD", "T-shirt", "cash", None).await.unwrap();

        // threshold 0 => anything created before "now" counts as stuck
        let report = s.reconcile(0).await.unwrap();
        assert_eq!(report.stuck_unsynced.len(), 1);
        assert!(report.synced_without_square_id.is_empty());

        // after a synced pass, nothing is stuck
        s.square_sync(Some("test-token")).await.unwrap();
        let report = s.reconcile(0).await.unwrap();
        assert!(report.stuck_unsynced.is_empty());
        assert!(report.synced_without_square_id.is_empty());
    }
}
