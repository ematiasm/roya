use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::AppResult;
use crate::models::{Account, AccountWithBalance};

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait AccountRepository: Send + Sync {
    async fn create(&self, name: &str) -> AppResult<Account>;
    async fn list(&self) -> AppResult<Vec<Account>>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>>;
    async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>>;
    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>>;
    async fn total_balance(&self) -> AppResult<Decimal>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

// ---------------------------------------------------------------------------
// Helpers: row mapping (Decimal stored as TEXT for SQLite precision)
// ---------------------------------------------------------------------------

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_account(row: sqlx::sqlite::SqliteRow) -> Account {
    let cached_str: String = row.get("cached_balance");
    Account {
        id: row.get("id"),
        name: row.get("name"),
        cached_balance: parse_decimal(&cached_str),
        created_at: row.get("created_at"),
    }
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SqliteAccountRepository {
    pub pool: SqlitePool,
}

impl SqliteAccountRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountRepository for SqliteAccountRepository {
    async fn create(&self, name: &str) -> AppResult<Account> {
        let row = sqlx::query(
            r#"INSERT INTO accounts (name) VALUES (?) RETURNING id, name, cached_balance, created_at"#,
        )
        .bind(name.trim())
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_account(row))
    }

    async fn list(&self) -> AppResult<Vec<Account>> {
        let rows = sqlx::query(r#"SELECT id, name, cached_balance, created_at FROM accounts ORDER BY id"#)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_account).collect())
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>> {
        let row = sqlx::query(r#"SELECT id, name, cached_balance, created_at FROM accounts WHERE id = ?"#)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_account))
    }

    /// Balance is always derived from transactions SUM (cached_balance is kept in sync but not trusted).
    /// For SQLite we compute in Rust to preserve Decimal precision (TEXT -> Decimal sum).
    async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>> {
        let accounts = self.list().await?;
        let mut out = Vec::with_capacity(accounts.len());
        for acc in accounts {
            let balance = balance_for_account(&self.pool, acc.id).await?;
            out.push(AccountWithBalance {
                id: acc.id,
                name: acc.name,
                balance,
                cached_balance: acc.cached_balance,
                created_at: acc.created_at,
            });
        }
        Ok(out)
    }

    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>> {
        let acc = self.find_by_id(id).await?;
        let Some(acc) = acc else { return Ok(None) };
        let balance = balance_for_account(&self.pool, acc.id).await?;
        Ok(Some(AccountWithBalance {
            id: acc.id,
            name: acc.name,
            balance,
            cached_balance: acc.cached_balance,
            created_at: acc.created_at,
        }))
    }

    async fn total_balance(&self) -> AppResult<Decimal> {
        // Sum all transactions in Rust for precision
        let rows = sqlx::query(r#"SELECT kind, amount FROM transactions"#)
            .fetch_all(&self.pool)
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

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM accounts WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}

async fn balance_for_account(pool: &SqlitePool, account_id: i64) -> Result<Decimal, sqlx::Error> {
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
