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
    async fn update(&self, tx: &Transaction) -> AppResult<Transaction>;
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
    Transaction {
        id: row.get("id"),
        account_id: row.get("account_id"),
        kind: kind_from_str(&kind_str),
        amount: parse_decimal(&amt_str),
        description: row.get("description"),
        reference: row.get("reference"),
        date: row.get("date"),
        created_at: row.get("created_at"),
    }
}

async fn balance_for_account_raw(pool: &SqlitePool, account_id: i64) -> Result<Decimal, sqlx::Error> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    let mut total = Decimal::ZERO;
    for row in rows {
        let kind: String = row.get("kind");
        let amt_str: String = row.get("amount");
        let amt = parse_decimal(&amt_str);
        if kind == "Income" {
            total += amt;
        } else {
            total -= amt;
        }
    }
    Ok(total)
}

#[async_trait]
impl TransactionRepository for SqliteTransactionRepository {
    async fn create(
        &self,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query(
            r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, account_id, kind, amount, description, reference, date, created_at"#,
        )
        .bind(account_id)
        .bind(kind.to_string())
        .bind(amount.to_string())
        .bind(description)
        .bind(reference)
        .bind(date)
        .fetch_one(&mut *tx)
        .await?;

        let rec = row_to_tx(row);
        sync_cached(&mut *tx, account_id).await?;
        tx.commit().await?;
        Ok(rec)
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Transaction>> {
        let row = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE id = ?"#,
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
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions ORDER BY date DESC, id DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC",
                )
                .bind(aid)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE account_id = ? AND date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE account_id = ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE date >= ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(from).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE account_id = ? AND date >= ? AND date <= ? ORDER BY date DESC, id DESC",
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
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_tx).collect())
    }

    async fn update(&self, tx_rec: &Transaction) -> AppResult<Transaction> {
        let mut conn = self.pool.begin().await?;
        let row = sqlx::query(
            r#"UPDATE transactions SET kind = ?, amount = ?, description = ?, date = ? WHERE id = ?
               RETURNING id, account_id, kind, amount, description, reference, date, created_at"#,
        )
        .bind(tx_rec.kind.to_string())
        .bind(tx_rec.amount.to_string())
        .bind(&tx_rec.description)
        .bind(tx_rec.date)
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
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_at FROM transactions WHERE id = ?"#,
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
        Ok(balance_for_account_raw(&self.pool, account_id).await?)
    }

    async fn sync_cached_balance(&self, account_id: i64) -> AppResult<()> {
        let mut conn = self.pool.begin().await?;
        sync_cached(&mut *conn, account_id).await?;
        conn.commit().await?;
        Ok(())
    }
}

async fn sync_cached(
    conn: &mut sqlx::SqliteConnection,
    account_id: i64,
) -> Result<(), sqlx::Error> {
    let balance = balance_for_account_raw_pool(conn, account_id).await?;
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
) -> Result<Decimal, sqlx::Error> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(&mut *conn)
        .await?;
    let mut total = Decimal::ZERO;
    for row in rows {
        let kind: String = row.get("kind");
        let amt_str: String = row.get("amount");
        let amt = parse_decimal(&amt_str);
        if kind == "Income" {
            total += amt;
        } else {
            total -= amt;
        }
    }
    Ok(total)
}
