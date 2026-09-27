use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Transaction, TransactionKind};

#[async_trait]
pub trait TransactionRepository: Send + Sync {
    async fn create(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction>;

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Transaction>>;
    async fn list(&self, filter: &crate::models::TransactionFilter) -> AppResult<Vec<Transaction>>;
    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<Transaction>>;
    async fn update(&self, tx: &Transaction, actor: i64) -> AppResult<Transaction>;
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn balance_for_account(&self, account_id: i64) -> AppResult<Decimal>;
    async fn sync_cached_balance(&self, account_id: i64) -> AppResult<()>;
}

#[derive(Clone)]
pub struct SqliteTransactionRepository {
    pub pool: SqlitePool,
}

impl SqliteTransactionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// helpers

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn kind_from_str(s: &str) -> TransactionKind {
    match s {
        "Income" => TransactionKind::Income,
        "Expense" => TransactionKind::Expense,
        _ => TransactionKind::Expense,
    }
}

fn row_to_tx(row: sqlx::sqlite::SqliteRow) -> Transaction {
    let kind_str: String = row.get("kind");
    let amt_str: String = row.get("amount");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
    Transaction {
        id: row.get("id"),
        account_id: row.get("account_id"),
        kind: kind_from_str(&kind_str),
        amount: parse_decimal(&amt_str),
        description: row.get("description"),
        reference: row.get("reference"),
        date: row.get("date"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at,
        updated_at,
    }
}

async fn balance_for_account_raw(pool: &SqlitePool, account_id: i64) -> AppResult<Decimal> {
    // `ORDER BY id` is load-bearing, not decoration. The check below is on the
    // RUNNING sum, so the order it folds in decides which prefixes it sees, and
    // without an `ORDER BY` that order is a planner decision — the
    // `idx_transactions_account_date` index on `(account_id, date)` can hand the
    // rows back in DATE order while the write pre-check in
    // `TransactionService` folds them in INSERTION order, and the two disagree.
    //
    // They can disagree about the ANSWER too, because addition is not
    // associative over a range that runs out: insert `+0.05e28`, `-0.05e28`,
    // `+0.05e28` and a date-ordered fold sees `0.05 / 0 / 0.05` where an
    // id-ordered one sees `0.05 / 0 / 0.05` — but reorder the dates and a
    // date-ordered fold walks `0.05 / 0.05 → 0.1e28` prefixes an id-ordered fold
    // never sees, and can refuse a balance the pre-check approved. `id` order IS
    // the pre-check's order, because every write appends. It is a covering-index
    // friendly sort on a column the planner already filters and sorts on, so it
    // costs one sort of the account's own rows and nothing else.
    let rows =
        sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ? ORDER BY id"#)
            .bind(account_id)
            .fetch_all(pool)
            .await?;
    // Signed, then summed ONCE through the checked fold: an expense contributes
    // `-amount`, so a set of incomes and expenses is one sum and one bound.
    // Folding `+=` and `-=` per row was the same arithmetic with two raw
    // operators and none. `AppError` rather than sqlx's error, so a refused sum
    // reaches the operator as the same sentence every other refusal answers with.
    //
    // THE FOLD IS THE GUARANTEE, not an induction over the write pre-check: the
    // pre-check in `TransactionService` validates `current + delta` where
    // `current` is this very fold, so it is a check on the whole sum, not on the
    // prefixes of the fold's own iteration — what it buys is that the common case
    // refuses EARLY, with a useful message, instead of waiting for a read to
    // discover it. The two agree on the total precisely because both are sums of
    // the same rows, and the row order is now pinned so their prefixes agree too.
    let signed = signed_amounts(&rows);
    Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
}

/// The rows of a balance query as SIGNED amounts: an income is `+amount`, an
/// expense `-amount`. One helper for every copy of this fold in this layer, so
/// they cannot drift on how a row contributes.
fn signed_amounts(rows: &[sqlx::sqlite::SqliteRow]) -> Vec<Decimal> {
    rows.iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let amt = parse_decimal(&row.get::<String, _>("amount"));
            if kind == "Income" {
                amt
            } else {
                -amt
            }
        })
        .collect()
}

#[async_trait]
impl TransactionRepository for SqliteTransactionRepository {
    async fn create(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query(
            r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(account_id)
        .bind(kind.to_string())
        .bind(amount.to_string())
        .bind(description)
        .bind(reference)
        .bind(date)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;

        let rec = row_to_tx(row);
        sync_cached(&mut *tx, account_id).await?;
        tx.commit().await?;
        Ok(rec)
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Transaction>> {
        let row = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_tx))
    }

    async fn list(&self, filter: &crate::models::TransactionFilter) -> AppResult<Vec<Transaction>> {
        // Build dynamic query with binds - using match for type safety
        let rows = match (filter.account_id, filter.from, filter.to) {
            (None, None, None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions ORDER BY date DESC, id DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC",
                )
                .bind(aid)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date >= ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(from).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date >= ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(from).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(row_to_tx).collect())
    }

    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<Transaction>> {
        let rows = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_tx).collect())
    }

    async fn update(&self, tx_rec: &Transaction, actor: i64) -> AppResult<Transaction> {
        let mut conn = self.pool.begin().await?;
        let row = sqlx::query(
            r#"UPDATE transactions
               SET kind = ?, amount = ?, description = ?, date = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(tx_rec.kind.to_string())
        .bind(tx_rec.amount.to_string())
        .bind(&tx_rec.description)
        .bind(tx_rec.date)
        .bind(actor)
        .bind(tx_rec.id)
        .fetch_one(&mut *conn)
        .await?;

        let rec = row_to_tx(row);
        sync_cached(&mut *conn, rec.account_id).await?;
        conn.commit().await?;
        Ok(rec)
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let existing = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = existing else {
            return Ok(false);
        };
        let rec = row_to_tx(row);

        let mut tx = self.pool.begin().await?;
        sqlx::query(r#"DELETE FROM transactions WHERE id = ?"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                // The FK from payment rows (sale_payments/purchase_payments) is
                // RESTRICT and finance deliberately knows nothing about those
                // modules; the constraint failure is the only signal. A user
                // action that is not allowed must be a 409 with an actionable
                // message, never a 500.
                if e.to_string().contains("FOREIGN KEY constraint failed") {
                    AppError::Conflict(format!(
                        "transaction {id} belongs to a document payment; cancel the document instead of deleting its money entry"
                    ))
                } else {
                    AppError::Database(e)
                }
            })?;
        sync_cached(&mut *tx, rec.account_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn balance_for_account(&self, account_id: i64) -> AppResult<Decimal> {
        balance_for_account_raw(&self.pool, account_id).await
    }

    async fn sync_cached_balance(&self, account_id: i64) -> AppResult<()> {
        let mut conn = self.pool.begin().await?;
        sync_cached(&mut *conn, account_id).await?;
        conn.commit().await?;
        Ok(())
    }
}

/// The cached balance of an account, refreshed inside the caller's transaction.
///
/// The error channel is sqlx's because this runs mid-transaction, where every
/// other statement's channel is sqlx's too. A refused sum therefore travels as
/// `Error::Protocol` carrying the rule's own text — the same bytes
/// `PriceRefusal::as_str` is, so the sentence an operator reads is the sentence
/// the catalog holds and not a driver string.
async fn sync_cached(
    conn: &mut sqlx::SqliteConnection,
    account_id: i64,
) -> Result<(), sqlx::Error> {
    let balance = balance_for_account_raw_pool(conn, account_id)
        .await
        .map_err(|error| match error {
            AppError::PriceRefused(refusal) => sqlx::Error::Protocol(refusal.as_str().to_string()),
            other => sqlx::Error::Protocol(other.to_string()),
        })?;
    sqlx::query(r#"UPDATE accounts SET cached_balance = ? WHERE id = ?"#)
        .bind(balance.to_string())
        .bind(account_id)
        .execute(conn)
        .await?;
    Ok(())
}

async fn balance_for_account_raw_pool(
    conn: &mut sqlx::SqliteConnection,
    account_id: i64,
) -> AppResult<Decimal> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(&mut *conn)
        .await?;
    // Signed, then summed ONCE through the checked fold: an expense contributes
    // `-amount`, so a set of incomes and expenses is one sum and one bound.
    // Folding `+=` and `-=` per row was the same arithmetic with two raw
    // operators and none. `AppError` rather than sqlx's error, so a refused sum
    // reaches the operator as the same sentence every other refusal answers with.
    let signed = signed_amounts(&rows);
    Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
}
