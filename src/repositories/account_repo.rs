use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Account, AccountWithBalance, SetMoney};

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait AccountRepository: Send + Sync {
    async fn create(&self, actor: i64, name: &str) -> AppResult<Account>;
    async fn list(&self) -> AppResult<Vec<Account>>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>>;
    /// Every account with its derived balance, TOLERANT: an account whose
    /// transactions cannot be added up keeps its place in the list carrying the
    /// rule, so one unmeasurable account never empties the finance page around it.
    async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>>;
    /// One account's derived balance, tolerant for the same reason — a detail page
    /// is a list of one.
    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>>;
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
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
    async fn create(&self, actor: i64, name: &str) -> AppResult<Account> {
        let row = sqlx::query(
            r#"INSERT INTO accounts (name, created_by) VALUES (?, ?)
               RETURNING id, name, cached_balance, created_by, updated_by, created_at"#,
        )
        .bind(name.trim())
        .bind(actor)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_account(row))
    }

    async fn list(&self) -> AppResult<Vec<Account>> {
        let rows = sqlx::query(
            r#"SELECT id, name, cached_balance, created_by, updated_by, created_at FROM accounts ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_account).collect())
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>> {
        let row = sqlx::query(
            r#"SELECT id, name, cached_balance, created_by, updated_by, created_at FROM accounts WHERE id = ?"#,
        )
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
            out.push(with_balance(&self.pool, acc).await?);
        }
        Ok(out)
    }

    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>> {
        match self.find_by_id(id).await? {
            None => Ok(None),
            Some(acc) => Ok(Some(with_balance(&self.pool, acc).await?)),
        }
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM accounts WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}

/// One account with its derived balance, TOLERANT.
///
/// The fold below is the checked one, and a refusal is not an error here: the
/// account keeps its place in the list and the row states the rule. The CACHED
/// balance goes with it only when the derived figure carried — a cache of a sum
/// that cannot be made is a stale number in the place of the number.
async fn with_balance(pool: &SqlitePool, acc: Account) -> AppResult<AccountWithBalance> {
    let balance = match balance_for_account(pool, acc.id).await {
        Ok(amount) => SetMoney::amount(amount),
        Err(AppError::PriceRefused(refusal)) => SetMoney::refused(refusal),
        Err(other) => return Err(other),
    };
    let cached_balance = balance.amount.map(|_| acc.cached_balance);
    Ok(AccountWithBalance {
        id: acc.id,
        name: acc.name,
        balance,
        cached_balance,
        created_by: acc.created_by,
        updated_by: acc.updated_by,
        created_at: acc.created_at,
    })
}

async fn balance_for_account(pool: &SqlitePool, account_id: i64) -> AppResult<Decimal> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    let signed: Vec<Decimal> = rows
        .iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let amt = parse_decimal(&row.get::<String, _>("amount"));
            if kind == "Income" {
                amt
            } else {
                -amt
            }
        })
        .collect();
    // `AppError` rather than sqlx's error, so a refused sum reaches the operator
    // as the same sentence every other refusal answers with, instead of as a
    // driver error. `AppError: From<sqlx::Error>` still covers the query.
    Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
}
