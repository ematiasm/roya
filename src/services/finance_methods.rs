// M0 payment-method allowlist service (finance-owned).
// Methods seeded Cash/Transfer/Debit/CreditCard/QR (no Other) via migration.
// account_payment_methods PK(both) RESTRICT both. Sensible defaults:
// Caja->[Cash], Banco->[Transfer,Debit,CreditCard], MP->[QR,Transfer].
use crate::error::{AppError, AppResult};
use crate::models::PaymentMethod;
use crate::repositories::PaymentMethodRepository;
use serde::Serialize;
use std::collections::HashSet;

/// A catalog entry plus whether the account currently accepts it. Serialized by
/// `GET /api/accounts/{id}/payment-methods` and rendered by the web matrix.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentMethodOption {
    pub id: i64,
    pub name: String,
    pub is_active: bool,
    pub allowed: bool,
}

#[derive(Clone)]
pub struct PaymentMethodService<PM>
where
    PM: PaymentMethodRepository,
{
    pub methods: PM,
}

impl<PM> PaymentMethodService<PM>
where
    PM: PaymentMethodRepository,
{
    pub fn new(methods: PM) -> Self {
        Self { methods }
    }

    pub async fn list(&self) -> AppResult<Vec<PaymentMethod>> {
        self.methods.list_methods().await
    }

    /// Canonical method names in seed order.
    pub fn seeded_names() -> Vec<&'static str> {
        vec!["Cash", "Transfer", "Debit", "CreditCard", "QR"]
    }

    /// Sensible default method names for a well-known account name.
    /// Matching is exact (Caja/Banco/MP); unknown names get no defaults.
    pub fn default_method_names_for_account_name(name: &str) -> Vec<&'static str> {
        match name.trim() {
            "Caja" => vec!["Cash"],
            "Banco" => vec!["Transfer", "Debit", "CreditCard"],
            "MP" => vec!["QR", "Transfer"],
            _ => vec![],
        }
    }

    /// Insert allowlist rows for the well-known defaults (idempotent).
    pub async fn ensure_defaults_for_account(
        &self,
        account_id: i64,
        account_name: &str,
    ) -> AppResult<()> {
        for method_name in Self::default_method_names_for_account_name(account_name) {
            if let Some(m) = self.methods.find_method_by_name(method_name).await? {
                self.methods.allow(account_id, m.id).await?;
            }
        }
        Ok(())
    }

    /// Full catalog for one account with an `allowed` flag per method.
    pub async fn catalog_for_account(
        &self,
        account_id: i64,
    ) -> AppResult<Vec<PaymentMethodOption>> {
        let methods = self.methods.list_methods().await?;
        let allowed = self.methods.list_allowed(account_id).await?;
        let allowed_ids: HashSet<i64> = allowed.into_iter().map(|m| m.id).collect();
        Ok(methods
            .into_iter()
            .map(|m| PaymentMethodOption {
                id: m.id,
                name: m.name,
                is_active: m.is_active,
                allowed: allowed_ids.contains(&m.id),
            })
            .collect())
    }

    /// Account ids whose allowlist is empty; drives the self-diagnosing warning.
    pub async fn accounts_without_methods(&self) -> AppResult<Vec<i64>> {
        self.methods.list_accounts_without_methods().await
    }

    /// Validate a requested allowlist: non-empty, deduped, every id exists.
    /// An empty list is a 400: removing every method leaves the account unable
    /// to record any payment, which is the bug this configuration surface fixes.
    pub async fn validated_method_ids(&self, method_ids: &[i64]) -> AppResult<Vec<i64>> {
        if method_ids.is_empty() {
            return Err(AppError::Validation(
                "at least one payment method is required; an empty allowlist rejects every payment"
                    .into(),
            ));
        }
        let mut seen = HashSet::new();
        let mut unique = Vec::with_capacity(method_ids.len());
        for id in method_ids {
            if seen.insert(*id) {
                unique.push(*id);
            }
        }
        for id in &unique {
            if self.methods.find_method(*id).await?.is_none() {
                return Err(AppError::NotFound(format!("method {id} not found")));
            }
        }
        Ok(unique)
    }

    /// Replace the account's allowlist with `method_ids` (no merge). Unknown ids
    /// 404 and empty lists 400 before any write, so a rejected request never
    /// clears the existing set. Returns the updated catalog.
    pub async fn replace_allowed(
        &self,
        account_id: i64,
        method_ids: &[i64],
    ) -> AppResult<Vec<PaymentMethodOption>> {
        let unique = self.validated_method_ids(method_ids).await?;
        self.methods.replace_allowed(account_id, &unique).await?;
        self.catalog_for_account(account_id).await
    }

    /// Validate (account, method) pair for sales: 404 unknown method,
    /// 400 inactive or not allowlisted. No stock/finance side effects.
    pub async fn require_allowed(
        &self,
        account_id: i64,
        method_id: i64,
    ) -> AppResult<PaymentMethod> {
        let method = self
            .methods
            .find_method(method_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("method {method_id} not found")))?;
        if !method.is_active {
            return Err(AppError::Validation(format!(
                "method {} is inactive",
                method.name
            )));
        }
        if !self.methods.is_allowed(account_id, method_id).await? {
            return Err(AppError::Validation(format!(
                "method {} is not allowed for account {account_id}; configure the account's payment methods and try again",
                method.name
            )));
        }
        Ok(method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::SqlitePaymentMethodRepository;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn test_pool() -> sqlx::SqlitePool {
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

    #[tokio::test]
    async fn seeded_methods_without_other() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool);
        let svc = PaymentMethodService::new(repo);
        let methods = svc.list().await.unwrap();
        let names: Vec<String> = methods.into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["Cash", "Transfer", "Debit", "CreditCard", "QR"]);
        assert!(!names.iter().any(|n| n == "Other"));
    }

    #[tokio::test]
    async fn sensible_defaults_for_caja_banco_mp() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo);
        async fn acc_id(pool: &sqlx::SqlitePool, name: &str) -> i64 {
            sqlx::query("INSERT OR IGNORE INTO accounts (name) VALUES (?)")
                .bind(name)
                .execute(pool)
                .await
                .unwrap();
            let row: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = ?")
                .bind(name)
                .fetch_one(pool)
                .await
                .unwrap();
            row.0
        }
        async fn method_names(pool: &sqlx::SqlitePool, acc: i64) -> Vec<String> {
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT m.name FROM payment_methods m JOIN account_payment_methods a ON a.method_id = m.id WHERE a.account_id = ? ORDER BY m.name",
            )
            .bind(acc)
            .fetch_all(pool)
            .await
            .unwrap();
            rows.into_iter().map(|r| r.0).collect()
        }
        for name in ["Caja", "Banco", "MP"] {
            let id = acc_id(&pool, name).await;
            svc.ensure_defaults_for_account(id, name).await.unwrap();
        }
        assert_eq!(method_names(&pool, acc_id(&pool, "Caja").await).await, vec!["Cash"]);
        assert_eq!(
            method_names(&pool, acc_id(&pool, "Banco").await).await,
            vec!["CreditCard", "Debit", "Transfer"]
        );
        assert_eq!(
            method_names(&pool, acc_id(&pool, "MP").await).await,
            vec!["QR", "Transfer"]
        );
    }

    #[tokio::test]
    async fn require_allowed_rejects_unknown_pair() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo);
        sqlx::query("INSERT OR IGNORE INTO accounts (name) VALUES ('Banco')")
            .execute(&pool)
            .await
            .unwrap();
        let acc: (i64,) =
            sqlx::query_as("SELECT id FROM accounts WHERE name = 'Banco'")
                .fetch_one(&pool)
                .await
                .unwrap();
        svc.ensure_defaults_for_account(acc.0, "Banco").await.unwrap();
        let cash = svc
            .methods
            .find_method_by_name("Cash")
            .await
            .unwrap()
            .unwrap();
        // Banco allows Transfer/Debit/CreditCard, not Cash.
        let err = svc.require_allowed(acc.0, cash.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    // -- explicit allowlist configuration (bug: accounts created in-app had no methods) --

    async fn seed_account(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        sqlx::query("INSERT OR IGNORE INTO accounts (name) VALUES (?)")
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
        let row: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    async fn method_id(
        svc: &PaymentMethodService<SqlitePaymentMethodRepository>,
        name: &str,
    ) -> i64 {
        svc.methods
            .find_method_by_name(name)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    fn allowed_names(catalog: &[PaymentMethodOption]) -> Vec<String> {
        catalog
            .iter()
            .filter(|m| m.allowed)
            .map(|m| m.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn catalog_marks_allowed_methods() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Catalog").await;
        let cash = method_id(&svc, "Cash").await;

        let empty = svc.catalog_for_account(acc).await.unwrap();
        assert_eq!(empty.len(), 5, "full catalog is returned");
        assert!(allowed_names(&empty).is_empty(), "nothing allowed yet");

        svc.replace_allowed(acc, &[cash]).await.unwrap();
        let catalog = svc.catalog_for_account(acc).await.unwrap();
        assert_eq!(allowed_names(&catalog), vec!["Cash"]);
        assert_eq!(catalog.iter().filter(|m| m.allowed).count(), 1);
        assert!(catalog.iter().any(|m| m.name == "QR" && !m.allowed));
    }

    #[tokio::test]
    async fn replace_allowed_replaces_previous_set() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Replace").await;
        let cash = method_id(&svc, "Cash").await;
        let transfer = method_id(&svc, "Transfer").await;

        svc.replace_allowed(acc, &[cash]).await.unwrap();
        assert_eq!(
            allowed_names(&svc.catalog_for_account(acc).await.unwrap()),
            vec!["Cash"]
        );

        svc.replace_allowed(acc, &[transfer]).await.unwrap();
        assert_eq!(
            allowed_names(&svc.catalog_for_account(acc).await.unwrap()),
            vec!["Transfer"],
            "replace must remove Cash, not merge"
        );
        let rows = svc.methods.list_allowed(acc).await.unwrap();
        assert!(rows.iter().all(|m| m.id != cash), "Cash row must be gone");
    }

    #[tokio::test]
    async fn replace_allowed_rejects_empty_list_with_clear_message() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Empty").await;

        let err = svc.replace_allowed(acc, &[]).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("at least one"), "got {msg}");
        assert!(svc.methods.list_allowed(acc).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replace_allowed_rejects_unknown_method_id_without_clearing_existing_set() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Unknown").await;
        let cash = method_id(&svc, "Cash").await;
        let transfer = method_id(&svc, "Transfer").await;

        svc.replace_allowed(acc, &[cash]).await.unwrap();
        let err = svc
            .replace_allowed(acc, &[transfer, 999_999])
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        assert_eq!(
            allowed_names(&svc.catalog_for_account(acc).await.unwrap()),
            vec!["Cash"],
            "a rejected replacement must not clear the existing allowlist"
        );
    }

    #[tokio::test]
    async fn accounts_without_methods_lists_only_unconfigured_accounts() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let bare = seed_account(&pool, "Bare").await;
        let configured = seed_account(&pool, "Configured").await;
        let cash = method_id(&svc, "Cash").await;
        svc.replace_allowed(configured, &[cash]).await.unwrap();

        let missing = svc.accounts_without_methods().await.unwrap();
        assert!(missing.contains(&bare), "unconfigured account must be listed");
        assert!(!missing.contains(&configured), "configured account is not listed");
    }

    #[tokio::test]
    async fn require_allowed_message_tells_user_to_configure_methods() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "NoMethods").await;
        let cash = method_id(&svc, "Cash").await;

        let err = svc.require_allowed(acc, cash).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("configure the account's payment methods"),
            "message must tell the user what to do, got {msg}"
        );
    }
}
