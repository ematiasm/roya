// M0 payment-method allowlist service (finance-owned).
// Methods seeded Cash/Transfer/Debit/CreditCard/QR (no Other) via migration.
// account_payment_methods PK(both) RESTRICT both. Sensible defaults:
// Caja->[Cash], Banco->[Transfer,Debit,CreditCard], MP->[QR,Transfer].
use crate::error::{AppError, AppResult};
use crate::models::PaymentMethod;
use crate::repositories::PaymentMethodRepository;

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
                "method {} not allowed for account {account_id}",
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
}
