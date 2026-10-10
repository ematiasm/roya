use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{PaymentMethod, PaymentMethodWithAccount};

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PaymentMethodRepository: Send + Sync {
    async fn list_methods(&self) -> AppResult<Vec<PaymentMethod>>;
    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>>;

    /// [`Self::find_method`] inside a transaction the CALLER owns, and the
    /// WEAKEST reason in the closure — stated that way on purpose, because a doc
    /// comment that borrowed `stock_for_product_in`'s argument would be borrowing
    /// a claim this method cannot make.
    ///
    /// **The deadlock alone, not correctness.** `confirm` writes
    /// `doc_sequences`, `stock_movements`, `transactions`, `sale_payments`,
    /// `sales`, `purchase_*` and `product_supplier_costs` — it never writes
    /// `payment_methods`, and that is traced rather than assumed: the only
    /// production writers of this table are `set_method_account` and
    /// `create_in_account` in this file, the three `routes/*_api.rs` HTTP
    /// handlers, and the deactivate at `src/services/finance_methods.rs:453`.
    /// None of them is on the confirm path. So nothing this read validates is
    /// ever written by the transaction that will hold it, and reading it from
    /// the caller's connection buys no fresher truth than reading it from the
    /// pool. Contrast `stock_for_product_in`, which folds rows the same document
    /// is still writing and genuinely cannot answer correctly from a snapshot.
    ///
    /// What this read does need is a CONNECTION it was handed, and where it is
    /// reached is narrower than a casual grep suggests:
    ///
    /// * `SalesService::confirm`, the `PaymentType::Cash` arm —
    ///   `src/services/sales.rs:1281` → `resolve_method_account` → this;
    /// * `PurchasesService::confirm`, the `PaymentType::Cash` arm —
    ///   `src/services/purchases.rs:1092` → `resolve_account` → this.
    ///
    /// The `Credit` arm of both returns `None` for `cash_account_id` without
    /// resolving anything, so this is a Cash-path read and only a Cash-path
    /// read. That cuts both ways for a test suite: a test that builds a Credit
    /// document never reaches this door at all, which is why the sibling
    /// customer-side `find_by_id_in` needs its own coverage rather than being
    /// assumed covered by these two.
    ///
    /// The production failure is worth stating honestly, because the single
    /// connection these tests use would make it look like a timeout. The pool is
    /// `max_connections(5)` (`src/db.rs`), not 1, so a pool-reaching read there
    /// is not a `PoolTimedOut` at all: it takes a SECOND connection and answers
    /// from a snapshot while the unit holds the first. That is the silent
    /// split-brain, and it is the whole argument. This read is a door, not a
    /// correctness fix.
    ///
    /// Three callers reach it OUTSIDE `confirm` and are the reason the deadlock
    /// argument does not depend on where Phase B opens its BEGIN:
    /// `record_payment_with_receipt` (`sales.rs:1482`), `record_payment`
    /// (`purchases.rs:1236`) and its own guard at `purchases.rs:1301` all
    /// collect against an already-**Confirmed** document, and
    /// `src/services/customer_receipts.rs:224` resolves a receipt's method
    /// while writing the receipt. Those are separate units today and stay
    /// separate; this door does not close them.
    ///
    /// One copy of the SQL, on one executor: the public twin is nothing but
    /// BEGIN/delegate/COMMIT around this method, so there is no `find_method_raw`
    /// free function here the way `purchase_repo` has `find_purchase_raw`. There,
    /// `find_purchase` is a method in its own right with its own many callers
    /// and has to keep running on the pool, so the statement genuinely had to
    /// exist on two executors. Here it does not.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn find_method_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
    ) -> AppResult<Option<PaymentMethod>>;
    /// First row with this name by id. Names repeat across accounts, so this is
    /// only a seed-order convenience; account-scoped reads use
    /// `find_method_in_account`.
    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>>;
    async fn find_method_in_account(
        &self,
        account_id: i64,
        name: &str,
    ) -> AppResult<Option<PaymentMethod>>;
    /// The account's own methods (ownership, not an allowlist).
    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>>;
    /// Every method with its owning account resolved, for method-only selects.
    async fn list_with_accounts(&self) -> AppResult<Vec<PaymentMethodWithAccount>>;
    /// Move a method to `account_id`. Unknown method ids 404; unknown accounts
    /// 404 via the FK; a name the target account already owns is a 409 via
    /// UNIQUE(account_id, name). `actor` is the audit actor the move records.
    ///
    /// The parameter is `i64` and not `Option<i64>` because migration 45 made
    /// `account_id` NOT NULL: an unowned method is no longer a state this schema
    /// can hold, so it is not a state this type offers. "Take a method out of
    /// service" is [`Self::set_active`], which keeps the owner and makes the
    /// method unusable — the two facts an operator actually needs to distinguish
    /// (still owned here, or gone from this account) stay separate columns
    /// instead of collapsing into one NULL.
    async fn set_method_account(
        &self,
        actor: i64,
        method_id: i64,
        account_id: i64,
    ) -> AppResult<()>;
    /// Turn a method selectable (`true`) or not (`false`). This is what "remove a
    /// method from an account" means: the owner is kept, because the stored
    /// account is the historical fact of where a payment went (a refund reads it
    /// back), while `is_active` is the switch `resolve_account_for` checks before
    /// a method can be chosen. Unknown ids 404.
    async fn set_active(&self, actor: i64, method_id: i64, active: bool) -> AppResult<()>;
    /// Create a fresh method row owned by `account_id` (duplicates of a
    /// same-named method on another account are allowed by design).
    async fn create_in_account(
        &self,
        actor: i64,
        name: &str,
        account_id: i64,
    ) -> AppResult<PaymentMethod>;
    /// Account ids with no ACTIVE method (self-diagnosing UI warning).
    ///
    /// "Active" and not "has a row": since migration 45 an unticked method is
    /// deactivated rather than unowned, so an account whose every method was
    /// unticked still HAS rows while being exactly the account this warning is
    /// about — one that cannot record a payment. Counting rows here would have
    /// silenced the warning at the moment it became true.
    async fn list_accounts_without_methods(&self) -> AppResult<Vec<i64>>;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_method(row: sqlx::sqlite::SqliteRow) -> PaymentMethod {
    let active_int: i64 = row.get("is_active");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
    PaymentMethod {
        id: row.get("id"),
        name: row.get("name"),
        account_id: row.get("account_id"),
        is_active: active_int != 0,
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at,
        updated_at,
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
        let rows = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at FROM payment_methods ORDER BY id"#)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query and the projection are `find_method_in`'s to inherit unchanged; all
    /// this adds is the BEGIN/COMMIT that it deliberately leaves to someone else.
    /// A read that opens a transaction is not a write's privilege — the caller
    /// that owns the larger unit is the only one who can see what is in it.
    async fn find_method(&self, id: i64) -> AppResult<Option<PaymentMethod>> {
        let mut tx = self.pool.begin().await?;
        let found = self.find_method_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(found)
    }

    async fn find_method_in(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
    ) -> AppResult<Option<PaymentMethod>> {
        // The executor is the caller's connection and nothing here opens a unit
        // of its own, so this read joins the caller's unit instead of ending
        // one. The SQL, the bind and the `Ok(row.map(row_to_method))` mapping are
        // byte-for-byte what `find_method` always ran — including the
        // unfiltered projection, because `resolve_account_for` branches on
        // `is_active` and on `account_id` being NULL and must be handed the row
        // as it stands, not a row this statement pre-judged.
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at FROM payment_methods WHERE id = ?"#)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn find_method_by_name(&self, name: &str) -> AppResult<Option<PaymentMethod>> {
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at FROM payment_methods WHERE name = ? ORDER BY id LIMIT 1"#)
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
        let row = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at FROM payment_methods WHERE account_id = ? AND name = ?"#)
        .bind(account_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_method))
    }

    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>> {
        let rows = sqlx::query(r#"SELECT id, name, account_id, is_active, created_by, updated_by, created_at, updated_at FROM payment_methods WHERE account_id = ? ORDER BY id"#)
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_method).collect())
    }

    async fn list_with_accounts(&self) -> AppResult<Vec<PaymentMethodWithAccount>> {
        // INNER JOIN, not LEFT: `account_id` is NOT NULL and a FK to `accounts`,
        // so there is no method row without an owner for a LEFT JOIN to keep.
        let rows = sqlx::query(
            r#"SELECT m.id, m.name, m.account_id, a.name AS account_name, m.is_active
               FROM payment_methods m
               JOIN accounts a ON a.id = m.account_id
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
        account_id: i64,
    ) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE payment_methods
             SET account_id = ?, updated_by = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
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

    async fn set_active(&self, actor: i64, method_id: i64, active: bool) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE payment_methods
             SET is_active = ?, updated_by = ?,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(if active { 1_i64 } else { 0_i64 })
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

    async fn create_in_account(
        &self,
        actor: i64,
        name: &str,
        account_id: i64,
    ) -> AppResult<PaymentMethod> {
        let row = sqlx::query(r#"INSERT INTO payment_methods (name, account_id, is_active, created_by)
               VALUES (?, ?, 1, ?)
               RETURNING id, name, account_id, is_active, created_by, updated_by, created_at, updated_at"#)
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
                   SELECT 1 FROM payment_methods pm
                    WHERE pm.account_id = a.id AND pm.is_active = 1
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
    use std::time::{Duration, Instant};

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
                sqlx::raw_sql(sqlx::AssertSqlSafe(stmt))
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
        // The current repository also returns the technical update timestamp
        // introduced by migration 36. This focused migration-24 fixture does
        // not need the rest of that migration, only the matching column shape.
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "ALTER TABLE payment_methods ADD COLUMN updated_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00.000Z'",
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    /// `apply_migration_file` lands migrations 24 and 30 only, so this fixture
    /// is still the PRE-45 shape: `payment_methods` carries no `account_id`
    /// column and the allowlist is out of reach. It therefore seeds its own
    /// account and a method owned by it, because the module under test reads
    /// ownership from a column that does not exist here and a test that reached
    /// `list_unassigned` (a read migration 45 deleted) would be asserting a
    /// shape the product no longer has.
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
            sqlx::query(
                "INSERT INTO account_payment_methods (account_id, method_id) VALUES (?, ?)",
            )
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
        assert_eq!(
            rows.len(),
            4,
            "one duplicate row for the shared method: {rows:?}"
        );
        assert_eq!(rows[0], (shared.0, "Shared".into(), Some(a.0)));
        assert_eq!(rows[1], (solo.0, "Solo".into(), Some(a.0)));
        assert_eq!(rows[2], (orphan.0, "Orphan".into(), None));
        assert_eq!(rows[3].1, "Shared");
        assert_eq!(rows[3].2, Some(b.0));
        assert!(rows[3].0 > orphan.0, "the duplicate gets a new id");

        // Same name on two accounts is allowed; twice on one account is not.
        repo.create_in_account(audit_actor(&repo).await, "Solo", b.0)
            .await
            .unwrap();
        let dup = repo
            .create_in_account(audit_actor(&repo).await, "Solo", b.0)
            .await
            .unwrap_err();
        assert!(matches!(dup, AppError::Conflict(_)), "got {dup:?}");

        // The allowlist table is gone.
        let gone = sqlx::query("SELECT COUNT(*) FROM account_payment_methods")
            .fetch_one(&pool)
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("no such table"), "got {gone}");

        // Account-scoped reads see the split. `Orphan` is the pre-45 allowance
        // this fixture's own migrations leave behind: it survives here because
        // migration 45 is deliberately not applied, and the repository only
        // ever lists by an account it is given.
        let a_methods = repo.list_by_account(a.0).await.unwrap();
        assert_eq!(a_methods.len(), 2);
        assert!(repo
            .list_accounts_without_methods()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn set_method_account_moves_between_accounts_and_rejects_unknowns() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        // The fixture account is system-planted data: the actor is the
        // migration's sentinel account.
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let acc: (i64,) =
            sqlx::query_as("INSERT INTO accounts (name, created_by) VALUES ('A', ?) RETURNING id")
                .bind(actor)
                .fetch_one(&pool)
                .await
                .unwrap();
        let other: (i64,) =
            sqlx::query_as("INSERT INTO accounts (name, created_by) VALUES ('B', ?) RETURNING id")
                .bind(actor)
                .fetch_one(&pool)
                .await
                .unwrap();
        // Migration 45 seeds Cash owned by Caja, so a fresh database has no
        // method to move and this test creates one to have a subject.
        let cash = repo
            .create_in_account(actor, "Movable", acc.0)
            .await
            .unwrap();
        assert_eq!(cash.account_id, acc.0);

        repo.set_method_account(actor, cash.id, other.0)
            .await
            .unwrap();
        assert_eq!(
            repo.find_method(cash.id).await.unwrap().unwrap().account_id,
            other.0
        );

        // The same name twice in one account is a conflict, not a second row.
        repo.create_in_account(actor, "Movable", acc.0)
            .await
            .unwrap();
        let clash = repo
            .set_method_account(actor, cash.id, acc.0)
            .await
            .unwrap_err();
        assert!(matches!(clash, AppError::Conflict(_)), "got {clash:?}");

        let err = repo
            .set_method_account(actor, 999_999, acc.0)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        let err = repo
            .set_method_account(actor, cash.id, 999_999)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        assert_eq!(
            repo.find_method(cash.id).await.unwrap().unwrap().account_id,
            other.0,
            "a refused move must leave the owner where it was"
        );
    }

    /// Migration 45 turned "take a method out of service" into `set_active`: the
    /// owner survives, because the stored account is the historical fact a
    /// refund reads back, and only the selectability flips.
    #[tokio::test]
    async fn set_active_deactivates_and_reactivates_without_touching_the_owner() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cash = repo.find_method_by_name("Cash").await.unwrap().unwrap();
        assert_eq!(cash.account_id, caja);

        repo.set_active(actor, cash.id, false).await.unwrap();
        let off = repo.find_method(cash.id).await.unwrap().unwrap();
        assert!(!off.is_active, "deactivation must land");
        assert_eq!(off.account_id, caja, "deactivation must keep the owner");
        assert_eq!(off.updated_by, Some(actor), "the change is attributed");

        repo.set_active(actor, cash.id, true).await.unwrap();
        assert!(repo.find_method(cash.id).await.unwrap().unwrap().is_active);

        let err = repo.set_active(actor, 999_999, false).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // ONE method of the eleven here gets an `_in` twin, and the reason it is
    // this one is a reachability fact rather than a judgement about importance.
    // `SalesService::confirm` and `PurchasesService::confirm` reach it from the
    // `PaymentType::Cash` arm and nowhere else — `sales.rs:1281`,
    // `purchases.rs:1092`. The `Credit` arm returns `None` for
    // `cash_account_id` without resolving anything, so a test that reaches this
    // door by building a Credit document never touches it, and a test that
    // reaches it by building a Cash document never touches the customer-side
    // `find_by_id`. Half the suite therefore proves nothing about either unless
    // the `_in` form is exercised DIRECTLY, which is what the three tests below
    // do. The door is covered whichever branch a future test happens to take.
    //
    // Why it moves is the DEADLOCK ALONE, and the trait doc says so plainly
    // rather than borrowing the correctness argument that `find_by_id_in` and
    // `stock_for_product_in` legitimately make. `confirm` writes
    // `doc_sequences`, `stock_movements`, `transactions`, `sale_payments`,
    // `sales`, `purchase_*` and `product_supplier_costs` — traced, not assumed:
    // the only production writers of `payment_methods` are `set_method_account`
    // and `create_in_account` in this file, the three `routes/*_api.rs` HTTP
    // handlers, and the deactivate at `finance_methods.rs:453`. None of them is
    // on the confirm path.
    //
    // Every other method in this file is left alone, and the audit is in the
    // commit message: the ten siblings are reached only by the read-only GET
    // surface (`list`, `methods_with_accounts`, `unassigned`,
    // `accounts_without_methods`) or by the account-setup flow
    // (`ensure_defaults_for_account`, `replace_account_methods`), plus
    // `customer_receipts`. None is reached by `confirm`.
    //
    // Nothing here opens a transaction across a service call. Phase A installs
    // the door; `confirm` does not walk through it until a later commit, and
    // the last test pins that the public `find_method` is untouched meanwhile.

    /// One account, created committed, so both the method and the account it
    /// points at exist before any transaction is opened.
    async fn seed_account(repo: &SqlitePaymentMethodRepository, name: &str, actor: i64) -> i64 {
        sqlx::query_scalar("INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id")
            .bind(name)
            .bind(actor)
            .fetch_one(&repo.pool)
            .await
            .unwrap()
    }

    /// `find_method_in` must answer from the connection it was HANDED, and the
    /// two fields it must see move are the two `resolve_account_for` branches
    /// on: `account_id` (this account vs another) and `is_active`
    /// (`finance_methods.rs`). A read that answered from a snapshot would hand
    /// the Cash path a different DECISION than the one the transaction has
    /// already committed to.
    ///
    /// The fixture is the file's own `migrated_pool()`, so `max_connections(1)`
    /// — the lever the next test needs — is inherited rather than restated.
    #[tokio::test]
    async fn find_method_in_reads_the_callers_uncommitted_method_and_a_rollback_hides_it_again() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let acc = seed_account(&repo, "PM-PH-A Cash", actor).await;
        // Migration 45 seeds Cash owned by Caja and active; this fixture starts
        // by moving it to ITS account, so "the unit changed the owner" has an
        // owner to change away from and back to.
        let cash = repo.find_method_by_name("Cash").await.unwrap().unwrap();
        let caja = cash.account_id;
        assert!(
            cash.is_active,
            "the fixture must start active, or this test proves nothing"
        );
        assert_ne!(
            caja, acc,
            "the fixture account must differ from the seeded one"
        );

        let mut tx = pool.begin().await.unwrap();
        // Both halves of "uncommitted": a row the unit CREATES, and the two
        // columns the unit CHANGES on a committed row. Neither is visible to
        // anything outside this connection.
        let inserted: i64 = sqlx::query_scalar(
            "INSERT INTO payment_methods (name, account_id, is_active, created_by) \
             VALUES ('Created Inside', ?, 1, ?) RETURNING id",
        )
        .bind(acc)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE payment_methods SET account_id = ?, is_active = 0 WHERE id = ?")
            .bind(acc)
            .bind(cash.id)
            .execute(&mut *tx)
            .await
            .unwrap();

        let seen_inserted = repo
            .find_method_in(&mut tx, inserted)
            .await
            .unwrap()
            .expect("the row this transaction created is invisible to it");
        assert_eq!(seen_inserted.name, "Created Inside");
        assert_eq!(seen_inserted.account_id, acc);
        assert!(seen_inserted.is_active);

        let seen_update = repo
            .find_method_in(&mut tx, cash.id)
            .await
            .unwrap()
            .expect("the row this transaction changed is invisible to it");
        assert_eq!(
            seen_update.account_id, acc,
            "the read did not see the caller's own uncommitted writes, so it would resolve the Cash account from a different moment than the one the transaction holds"
        );
        assert!(
            !seen_update.is_active,
            "the read missed the caller's own deactivation: the inactive-method refusal is one of the two branches this row drives"
        );
        // And it is still a plain read by id: an unknown id is a VALUE, not an
        // error, which is the branch `resolve_account_for` turns into
        // `NotFound`.
        assert!(repo
            .find_method_in(&mut tx, 9_999_999)
            .await
            .unwrap()
            .is_none());
        tx.rollback().await.unwrap();

        // The rollback took both with it, which is the other half: a
        // `find_method_in` that could not see the rollback was reading
        // something other than the caller's transaction.
        assert!(
            repo.find_method(inserted).await.unwrap().is_none(),
            "the payment method survived a rollback of the transaction that created it"
        );
        let restored = repo.find_method(cash.id).await.unwrap().unwrap();
        assert_eq!(
            (restored.account_id, restored.is_active),
            (caja, true),
            "the reassignment or the deactivation survived a rollback of the transaction that made it"
        );
    }

    /// THE test of this commit: `find_method_in` must not reach for the pool AT
    /// ALL, and the assertion is the pairing itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on
    /// this pool at all, ever: it would sit on sqlx's 30s acquire timeout and
    /// come back as `PoolTimedOut`. This test therefore cannot pass by being
    /// slow, and the timing bound below is corroboration rather than the proof.
    #[tokio::test]
    async fn find_method_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let acc = seed_account(&repo, "PM-PH-B Cash", actor).await;
        let cash = repo.find_method_by_name("Cash").await.unwrap().unwrap();
        repo.set_method_account(actor, cash.id, acc).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a
        // read right now, and that is a fact about the pool, not about this
        // test's patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let found = repo.find_method_in(&mut tx, cash.id).await;
        let elapsed = started.elapsed();
        let found = found
            .expect(
                "find_method_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
            )
            .expect("the committed row is visible to a transaction opened after it");
        // MEASURED, not assumed: the pairing above already decides it. Five
        // seconds sits far above what a query on a held connection costs and far
        // below the 30s acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "find_method_in took {elapsed:?}; that is a read stalling for a connection, not one on the connection it was handed"
        );
        assert_eq!(found.name, "Cash");
        assert_eq!(found.account_id, acc);
        // The caller's transaction is still ALIVE and still holds its lock: a
        // second statement on the same connection answers. A `find_method_in`
        // that had ended, committed or rolled back the unit it was given could
        // not leave this true.
        assert!(repo
            .find_method_in(&mut tx, cash.id)
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .find_method_in(&mut tx, 9_999_999)
            .await
            .unwrap()
            .is_none());
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert!(repo.find_method(cash.id).await.unwrap().is_some());
    }

    /// The additive claim, proved rather than asserted: the public `find_method`
    /// still answers exactly what it always answered, in every branch its callers
    /// actually distinguish. `resolve_account_for` (`finance_methods.rs`) draws
    /// TWO outcomes from this one read — unknown id becomes `NotFound`, and
    /// `!is_active` becomes a `Validation` — plus the owner it returns on
    /// success. Migration 45 removed the third (unassigned) branch; the test
    /// below pins that a refusal is a VALUE the read reports, not a row it
    /// filters away.
    #[tokio::test]
    async fn the_public_find_method_answers_every_value_the_service_branches_on() {
        let pool = migrated_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let acc = seed_account(&repo, "PM-PH-C Cash", actor).await;
        let cash = repo.find_method_by_name("Cash").await.unwrap().unwrap();
        // A SECOND method of the same database: migration 45 deleted the
        // history-less `Transfer`, so the fixture creates the second subject the
        // way the product does.
        let other = repo
            .create_in_account(actor, "Transfer", acc)
            .await
            .unwrap();

        // The seeded owner is itself a value the read reports.
        let seeded = repo.find_method(cash.id).await.unwrap().unwrap();
        assert_eq!(seeded.name, "Cash");
        assert!(seeded.account_id > 0);
        assert!(seeded.is_active);

        // A move lands immediately: the wrapper's own unit is invisible, so the
        // read is correct right after it.
        repo.set_method_account(actor, cash.id, acc).await.unwrap();
        let moved = repo.find_method(cash.id).await.unwrap().unwrap();
        assert_eq!(moved.account_id, acc);
        assert_eq!(moved.name, "Cash");

        // Deactivated is a THIRD value: the read does not filter, so `is_active`
        // reaches the service as `false` rather than as a missing row.
        sqlx::query("UPDATE payment_methods SET is_active = 0 WHERE id = ?")
            .bind(cash.id)
            .execute(&pool)
            .await
            .unwrap();
        let inactive = repo.find_method(cash.id).await.unwrap().unwrap();
        assert!(
            !inactive.is_active,
            "the read filtered the deactivated method out instead of reporting it"
        );
        assert_eq!(
            inactive.account_id, acc,
            "the wrapper lost the ownership it had committed a moment earlier"
        );

        // Per id, not "is the table non-empty": a wrapper that dropped its bind
        // would answer the second read with the first.
        assert_eq!(
            repo.find_method(other.id).await.unwrap().unwrap().name,
            "Transfer"
        );
        // The unknown-id branch is a VALUE, not an error — and it stays one.
        assert!(matches!(repo.find_method(9_999_999).await, Ok(None)));
        assert!(matches!(repo.find_method(0).await, Ok(None)));
        // And the wrapper leaves no unit behind: it is answerable again
        // immediately, and the row it read is still the row it found.
        let again = repo.find_method(cash.id).await.unwrap().unwrap();
        assert_eq!(again.account_id, acc);
        assert!(!again.is_active);
    }
}
