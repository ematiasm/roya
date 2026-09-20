use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{PaymentMethod, PaymentMethodWithAccount};

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PaymentMethodRepository: Send + Sync {
    async fn list_methods(&self) -> AppResult<Vec<PaymentMethod>>;
    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>>;
    /// First row with this name by id. Names repeat across accounts, so this is
    /// only a seed-order convenience; account-scoped reads use
    /// `find_method_in_account` / `find_unassigned_by_name`.
    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>>;
    async fn find_method_in_account(
        &self,
        account_id: i64,
        name: &str,
    ) -> AppResult<Option<PaymentMethod>>;
    async fn find_unassigned_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>>;
    /// The account's own methods (ownership, not an allowlist).
    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>>;
    async fn list_unassigned(&self) -> AppResult<Vec<PaymentMethod>>;
    /// Every method with its owning account resolved, for method-only selects.
    async fn list_with_accounts(&self) -> AppResult<Vec<PaymentMethodWithAccount>>;
    /// Assign a method to an account, or unassign it with `None`. Unknown
    /// method ids 404; unknown accounts 404 via the FK; assigning a name the
    /// account already owns 409 via UNIQUE(account_id, name). `actor` is the
    /// audit actor: the reassignment (including an unassign) is an edit the
    /// audit records.
    async fn set_method_account(
        &self,
        actor: i64,
        method_id: i64,
        account_id: Option<i64>,
    ) -> AppResult<()>;
    /// Create a fresh method row owned by `account_id` (duplicates of a
    /// same-named method on another account are allowed by design).
    async fn create_in_account(&self, actor: i64, name: &str, account_id: i64) -> AppResult<PaymentMethod>;
    /// Account ids with no owned methods (self-diagnosing UI warning).
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
        account_id: row.get("account_id"),
        is_active: active_int != 0,
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
    }
}

fn row_to_method_with_account(row: sqlx::sqlite::SqliteRow) -> PaymentMethodWithAccount {
    let active_int: i64 = row.get("is_active");
    PaymentMethodWithAccount {
        id: row.get("id"),
        name: row.get("name"),
        account_id: row.get("account_id"),
        account_name: row.get("account_name"),
        is_active: active_int != 0,
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
        let rows = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods ORDER BY id"#)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE id = ?"#)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE name = ? ORDER BY id LIMIT 1"#)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn find_method_in_account(
        &self,
        account_id: i64,
        name: &str,
    ) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE account_id = ? AND name = ?"#)
        .bind(account_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn find_unassigned_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE account_id IS NULL AND name = ? ORDER BY id LIMIT 1"#)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>> {
        let rows = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE account_id = ? ORDER BY id"#)
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn list_unassigned(&self) -> AppResult<Vec<PaymentMethod>> {
        let rows = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at FROM payment_methods WHERE account_id IS NULL ORDER BY id"#)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn list_with_accounts(&self) -> AppResult<Vec<PaymentMethodWithAccount>> {
        let rows = sqlx::query(
            r#"SELECT m.id, m.name, m.account_id, a.name AS account_name, m.is_active
               FROM payment_methods m
               LEFT JOIN accounts a ON a.id = m.account_id
               ORDER BY m.id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method_with_account).collect())
    }

    async fn set_method_account(
        &self,
        actor: i64,
        method_id: i64,
        account_id: Option<i64>,
    ) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE payment_methods SET account_id = ?, updated_by = ? WHERE id = ?",
        )
        .bind(account_id)
        .bind(actor)
        .bind(method_id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!("method {method_id} not found")));
        }
        Ok(())
    }

    async fn create_in_account(&self, actor: i64, name: &str, account_id: i64) -> AppResult<PaymentMethod> {
        let row = sqlx::query(r#"INSERT INTO payment_methods (name, account_id, is_active, created_by)
               VALUES (?, ?, 1, ?)
               RETURNING id, name, account_id, is_active, created_by, updated_by, created_at"#)
        .bind(name)
        .bind(account_id)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_method(row))
    }

    async fn list_accounts_without_methods(&self) -> AppResult<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            r#"SELECT a.id FROM accounts a
               WHERE NOT EXISTS (
                   SELECT 1 FROM payment_methods pm WHERE pm.account_id = a.id
               ) ORDER BY a.id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A valid acting user for the repo-level fixture calls: the migration's
    /// sentinel account. The audit-attribution tests live in the services.
    async fn audit_actor(repo: &SqlitePaymentMethodRepository) -> i64 {
        test_support::audit_actor_id(&repo.pool).await.unwrap()
    }

    async fn migrated_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Recreate the pre-migration shape (migration 12): global UNIQUE(name)
    /// plus the `account_payment_methods` allowlist. The fixture also carries
    /// the pieces the audit rebuild (migration 30) needs when this test applies
    /// it after migration 24 — a `users` table, and the audit columns on
    /// `accounts` — because the repository under test writes those columns and
    /// only the post-30 tables satisfy its SQL. The split behaviour migration 24
    /// protects is untouched by either extra piece.
    async fn pre_migration_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, cached_balance TEXT NOT NULL DEFAULT '0', created_at TEXT NOT NULL DEFAULT '2024-01-01T00:00:00Z', \
             created_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT, \
             updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, username TEXT NOT NULL, \
             display_name TEXT NOT NULL, password_hash TEXT NOT NULL, is_active INTEGER NOT NULL DEFAULT 1, \
             must_change_password INTEGER NOT NULL DEFAULT 0, last_login_at TEXT NULL, \
             created_at TEXT NOT NULL DEFAULT '2024-01-01T00:00:00Z', \
             updated_at TEXT NOT NULL DEFAULT '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE transactions (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, kind TEXT NOT NULL, \
             amount TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', reference TEXT NULL, date TEXT NOT NULL, \
             created_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT, \
             updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT, \
             created_at TEXT NOT NULL DEFAULT '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE payment_methods (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, \
             is_active INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL DEFAULT '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE account_payment_methods (account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT, \
             method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT, \
             PRIMARY KEY (account_id, method_id))",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn split_statements(sql: &str) -> Vec<String> {
        let code: Vec<&str> = sql
            .lines()
            .filter(|line| {
                let t = line.trim();
                !(t.is_empty() || t.starts_with("--"))
            })
            .collect();
        code.join("\n")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// Apply migration 24 (the payment-method split) and then the audit rebuild
    /// (migration 30): the repository under test writes the audit columns only
    /// the post-30 tables carry.
    async fn apply_migration_file(pool: &SqlitePool) {
        for file in [
            "migrations/20240101000024_payment_methods_single_account.sql",
            "migrations/20240101000030_add_audit_finance.sql",
        ] {
            let sql = std::fs::read_to_string(file).unwrap();
            for stmt in split_statements(&sql) {
                if stmt.trim().is_empty() {
                    continue;
                }
                sqlx::raw_sql(sqlx::AssertSqlSafe(stmt)).execute(pool).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn migration_splits_shared_methods_keeps_ids_and_drops_allowlist() {
        let pool = pre_migration_pool().await;
        // Accounts A (lowest id) and B; Shared allowed on both, Solo on A, Orphan nowhere.
        let a: (i64,) = sqlx::query_as("INSERT INTO accounts (name) VALUES ('A') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
        let b: (i64,) = sqlx::query_as("INSERT INTO accounts (name) VALUES ('B') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
        let shared: (i64,) =
            sqlx::query_as("INSERT INTO payment_methods (name) VALUES ('Shared') RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();
        let solo: (i64,) =
            sqlx::query_as("INSERT INTO payment_methods (name) VALUES ('Solo') RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();
        let orphan: (i64,) =
            sqlx::query_as("INSERT INTO payment_methods (name) VALUES ('Orphan') RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();
        for (acc, m) in [(a.0, shared.0), (b.0, shared.0), (a.0, solo.0)] {
            sqlx::query("INSERT INTO account_payment_methods (account_id, method_id) VALUES (?, ?)")
                .bind(acc)
                .bind(m)
                .execute(&pool)
                .await
                .unwrap();
        }

        apply_migration_file(&pool).await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());

        // Shared splits into two rows: the original id stays on the lowest account.
        let rows: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT id, name, account_id FROM payment_methods ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 4, "one duplicate row for the shared method: {rows:?}");
        assert_eq!(rows[0], (shared.0, "Shared".into(), Some(a.0)));
        assert_eq!(rows[1], (solo.0, "Solo".into(), Some(a.0)));
        assert_eq!(rows[2], (orphan.0, "Orphan".into(), None));
        assert_eq!(rows[3].1, "Shared");
        assert_eq!(rows[3].2, Some(b.0));
        assert!(rows[3].0 > orphan.0, "the duplicate gets a new id");

        // Same name on two accounts is allowed; twice on one account is not.
        repo.create_in_account(audit_actor(&repo).await, "Solo", b.0).await.unwrap();
        let dup = repo.create_in_account(audit_actor(&repo).await, "Solo", b.0).await.unwrap_err();
        assert!(matches!(dup, AppError::Conflict(_)), "got {dup:?}");

        // The allowlist table is gone.
        let gone = sqlx::query("SELECT COUNT(*) FROM account_payment_methods")
            .fetch_one(&pool)
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("no such table"), "got {gone}");

        // Account-scoped reads see the split.
        let a_methods = repo.list_by_account(a.0).await.unwrap();
        assert_eq!(a_methods.len(), 2);
        let unassigned = repo.list_unassigned().await.unwrap();
        assert_eq!(unassigned.iter().map(|m| m.name.clone()).collect::<Vec<_>>(), vec!["Orphan"]);
        assert!(repo.list_accounts_without_methods().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_method_account_assigns_unassigns_and_rejects_unknowns() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        // The fixture account is system-planted data: the actor is the
        // migration's sentinel account.
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let acc: (i64,) = sqlx::query_as("INSERT INTO accounts (name, created_by) VALUES ('A', ?) RETURNING id")
            .bind(actor)
            .fetch_one(&pool)
            .await
            .unwrap();
        // Seeded methods are unassigned on a fresh DB.
        let cash = repo.find_method_by_name("Cash").await.unwrap().unwrap();
        assert_eq!(cash.account_id, None);

        repo.set_method_account(audit_actor(&repo).await, cash.id, Some(acc.0)).await.unwrap();
        assert_eq!(repo.find_method(cash.id).await.unwrap().unwrap().account_id, Some(acc.0));
        repo.set_method_account(audit_actor(&repo).await, cash.id, None).await.unwrap();
        assert_eq!(repo.find_method(cash.id).await.unwrap().unwrap().account_id, None);

        let err = repo.set_method_account(audit_actor(&repo).await, 999_999, Some(acc.0)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        let err = repo.set_method_account(audit_actor(&repo).await, cash.id, Some(999_999)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }
}
