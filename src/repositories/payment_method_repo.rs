use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::PaymentMethod;

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PaymentMethodRepository: Send + Sync {
    async fn list_methods(&self) -> AppResult<Vec<PaymentMethod>>;
    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>>;
    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>>;
    async fn is_allowed(&self, account_id: i64, method_id: i64) -> AppResult<bool>;
    async fn allow(&self, account_id: i64, method_id: i64) -> AppResult<()>;
    /// Replace the account's allowlist atomically (delete + insert).
    async fn replace_allowed(&self, account_id: i64, method_ids: &[i64]) -> AppResult<()>;
    async fn list_allowed(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>>;
    /// Account ids whose allowlist is empty (self-diagnosing UI warning).
    async fn list_accounts_without_methods(&self) -> AppResult<Vec<i64>>;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_method(row: sqlx::sqlite::SqliteRow) -> PaymentMethod {
    let active_int: i64 = row.get("is_active");
    PaymentMethod {
        id: row.get("id"),
        name: row.get("name"),
        is_active: active_int != 0,
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("FOREIGN KEY constraint failed") {
        AppError::NotFound("referenced account/method not found".into())
    } else if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("payment method already exists".into())
    } else {
        AppError::Database(e)
    }
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SqlitePaymentMethodRepository {
    pub pool: SqlitePool,
}

impl SqlitePaymentMethodRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PaymentMethodRepository for SqlitePaymentMethodRepository {
    async fn list_methods(&self) -> AppResult<Vec<PaymentMethod>> {
        let rows = sqlx::query(
            r#"SELECT id, name, is_active, created_at FROM payment_methods ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(
            r#"SELECT id, name, is_active, created_at FROM payment_methods WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(
            r#"SELECT id, name, is_active, created_at FROM payment_methods WHERE name = ?"#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn is_allowed(&self, account_id: i64, method_id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM account_payment_methods WHERE account_id = ? AND method_id = ?"#,
        )
        .bind(account_id)
        .bind(method_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    async fn allow(&self, account_id: i64, method_id: i64) -> AppResult<()> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO account_payment_methods (account_id, method_id) VALUES (?, ?)"#,
        )
        .bind(account_id)
        .bind(method_id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        // INSERT OR IGNORE hides FK violations (rows_affected 0); verify pair when missing.
        if !self.is_allowed(account_id, method_id).await? {
            return Err(AppError::NotFound(
                "referenced account/method not found".into(),
            ));
        }
        Ok(())
    }

    async fn replace_allowed(&self, account_id: i64, method_ids: &[i64]) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_payment_methods WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        for method_id in method_ids {
            sqlx::query(
                r#"INSERT INTO account_payment_methods (account_id, method_id) VALUES (?, ?)"#,
            )
            .bind(account_id)
            .bind(method_id)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn list_allowed(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>> {
        let rows = sqlx::query(
            r#"SELECT m.id, m.name, m.is_active, m.created_at
               FROM payment_methods m
               JOIN account_payment_methods a ON a.method_id = m.id
               WHERE a.account_id = ? ORDER BY m.id"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn list_accounts_without_methods(&self) -> AppResult<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            r#"SELECT a.id FROM accounts a
               WHERE NOT EXISTS (
                   SELECT 1 FROM account_payment_methods apm WHERE apm.account_id = a.id
               ) ORDER BY a.id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }
}
