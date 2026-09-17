use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{Transaction, TransactionFilter, TransactionKind};
use crate::repositories::{AccountRepository, TransactionRepository};

#[derive(Clone)]
pub struct TransactionService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub accounts: A,
    pub transactions: T,
    pub allow_negative: bool,
}

impl<A, T> TransactionService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub fn new(accounts: A, transactions: T, allow_negative: bool) -> Self {
        Self {
            accounts,
            transactions,
            allow_negative,
        }
    }

    fn validate_amount(&self, amount: Decimal) -> AppResult<()> {
        if amount <= Decimal::ZERO {
            return Err(AppError::Validation("amount must be > 0".into()));
        }
        if amount.scale() > 2 {
            // Allow up to 2 decimals for finance; could be relaxed but we validate.
            // rust_decimal supports more; we just warn? For now allow any but log.
        }
        Ok(())
    }

    fn validate_date(&self, date: NaiveDate) -> AppResult<()> {
        // No future restriction by default, but could enforce. We'll allow any.
        let _ = date;
        Ok(())
    }

    /// Manual transaction entry point (no document reference). Kept as the
    /// 5-argument form so manual callers keep working; delegates to
    /// `create_with_reference`.
    pub async fn create(
        &self,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.create_with_reference(account_id, kind, amount, description, None, date)
            .await
    }

    /// Create a transaction, optionally stamped with an opaque source `reference`
    /// (the document number written by sales/purchases). Finance only stores the
    /// string; it never knows about the document that produced it.
    pub async fn create_with_reference(
        &self,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        reference: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.validate_amount(amount)?;
        self.validate_date(date)?;

        if !self.accounts.exists(account_id).await? {
            return Err(AppError::NotFound(format!("account {account_id} not found")));
        }

        // If Expense and negative not allowed, check resulting balance
        if !self.allow_negative && kind == TransactionKind::Expense {
            let current = self.transactions.balance_for_account(account_id).await?;
            let next = current - amount;
            if next < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "insufficient funds: balance {current} would become {next}"
                )));
            }
        }

        let desc = description.unwrap_or_default();
        if desc.len() > 256 {
            return Err(AppError::Validation(
                "description must be <= 256 chars".into(),
            ));
        }

        self.transactions
            .create(account_id, kind, amount, &desc, reference.as_deref(), date)
            .await
    }

    pub async fn list(&self, filter: TransactionFilter) -> AppResult<Vec<Transaction>> {
        if let Some(aid) = filter.account_id {
            if !self.accounts.exists(aid).await? {
                return Err(AppError::NotFound(format!("account {aid} not found")));
            }
        }
        if let (Some(from), Some(to)) = (filter.from, filter.to) {
            if from > to {
                return Err(AppError::Validation("'from' must be <= 'to'".into()));
            }
        }
        self.transactions.list(&filter).await
    }

    pub async fn update(
        &self,
        id: i64,
        kind: Option<TransactionKind>,
        amount: Option<Decimal>,
        description: Option<String>,
        date: Option<NaiveDate>,
    ) -> AppResult<Transaction> {
        let mut existing = self
            .transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))?;

        if let Some(a) = amount {
            self.validate_amount(a)?;
            existing.amount = a;
        }
        if let Some(k) = kind {
            existing.kind = k;
        }
        if let Some(d) = description {
            if d.len() > 256 {
                return Err(AppError::Validation(
                    "description must be <= 256 chars".into(),
                ));
            }
            existing.description = d;
        }
        if let Some(dt) = date {
            self.validate_date(dt)?;
            existing.date = dt;
        }

        // Negative-balance guard: compute projected balance after edit.
        if !self.allow_negative {
            // `existing` holds the mutated values, but DB still has the original.
            // current_balance is derived from DB (original included).
            let current_balance = self
                .transactions
                .balance_for_account(existing.account_id)
                .await?;
            let orig = self.transactions.find_by_id(id).await?.unwrap();
            let orig_signed = match orig.kind {
                TransactionKind::Income => orig.amount,
                TransactionKind::Expense => -orig.amount,
            };
            let new_signed = match existing.kind {
                TransactionKind::Income => existing.amount,
                TransactionKind::Expense => -existing.amount,
            };
            let projected = current_balance - orig_signed + new_signed;
            if projected < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "update would cause negative balance: projected {projected}"
                )));
            }
        }

        self.transactions.update(&existing).await
    }

    pub async fn delete(&self, id: i64) -> AppResult<()> {
        let tx = self
            .transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))?;

        if !self.allow_negative && tx.kind == TransactionKind::Income {
            // Removing income could make balance negative
            let current = self.transactions.balance_for_account(tx.account_id).await?;
            let projected = current - tx.amount; // Income removal subtracts
            if projected < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "deleting this income would cause negative balance: projected {projected}"
                )));
            }
        }

        let deleted = self.transactions.delete(id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!("transaction {id} not found")));
        }
        Ok(())
    }

    pub async fn get(&self, id: i64) -> AppResult<Transaction> {
        self.transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::{SqliteAccountRepository, SqliteTransactionRepository};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    // -- migration backfill ----------------------------------------------------

    #[tokio::test]
    async fn backfill_sets_reference_only_for_document_shaped_descriptions() {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        let migrator = sqlx::migrate!("./migrations");
        // Stop before the traceability migration so the rows below look exactly
        // like data written by M2/M3 before this change.
        migrator.run_to(20240101000018, &pool).await.unwrap();

        let account_id: (i64,) = sqlx::query_as(
            "INSERT INTO accounts (name, cached_balance) VALUES ('legacy', '0') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let sale_id: (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_name, sale_date) \
             VALUES ('Confirmed', 'Cash', 'legacy', '2024-01-01') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date) \
             VALUES (?, ?, 1, '10', '2024-01-01')",
        )
        .bind(sale_id.0)
        .bind(account_id.0)
        .execute(&pool)
        .await
        .unwrap();

        // Document-shaped descriptions written by M2/M3 before this change, plus
        // lookalikes that must never be matched ("do not invent matches").
        let legacy_descriptions = [
            "2024-SALE-000012",
            "2024-PURCH-000007",
            "Salary",
            "2024-SALE-12",
            "2024-SALE-000012 extra",
            "2024-sale-000012",
            "2024-PURCH-0000078",
            "",
        ];
        for description in legacy_descriptions {
            sqlx::query(
                "INSERT INTO transactions (account_id, kind, amount, description, date) \
                 VALUES (?, 'Income', '10', ?, '2024-01-01')",
            )
            .bind(account_id.0)
            .bind(description)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrator.run(&pool).await.unwrap();

        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT description, reference FROM transactions ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let reference_for = |description: &str| {
            rows.iter()
                .find(|(d, _)| d == description)
                .and_then(|(_, r)| r.clone())
        };
        assert_eq!(
            reference_for("2024-SALE-000012").as_deref(),
            Some("2024-SALE-000012")
        );
        assert_eq!(
            reference_for("2024-PURCH-000007").as_deref(),
            Some("2024-PURCH-000007")
        );
        for unrelated in [
            "Salary",
            "2024-SALE-12",
            "2024-SALE-000012 extra",
            "2024-sale-000012",
            "2024-PURCH-0000078",
            "",
        ] {
            assert_eq!(
                reference_for(unrelated),
                None,
                "must not invent a reference for {unrelated:?}"
            );
        }

        // Historical payments stay unlinked: there is no deterministic way to know
        // which transaction a pre-existing payment created, but the transaction is
        // still traceable through its `reference`.
        let legacy_payment: (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT transaction_id, refund_transaction_id FROM sale_payments LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy_payment, (None, None));
    }

    // -- RESTRICT delete quality -----------------------------------------------

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

    async fn svc() -> (
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
        sqlx::SqlitePool,
    ) {
        let pool = test_pool().await;
        let s = TransactionService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
            true,
        );
        (s, pool)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d() -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 5, 1).unwrap()
    }

    /// Raw fixture: link a transaction to a synthetic sale payment. Finance
    /// tests stay domain-agnostic and never import sales/purchases types.
    /// `customer_id` is NOT NULL by design and resolves the seeded walk-in
    /// instead of hardcoding its id.
    async fn link_sale_payment(pool: &sqlx::SqlitePool, tx_id: i64, account_id: i64) {
        let sale_id: (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date) \
             VALUES ('Confirmed', 'Cash', \
                     (SELECT id FROM customers WHERE is_walkin = 1), 'fixture', '2024-05-01') \
             RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments \
             (sale_id, account_id, method_id, amount, date, transaction_id) \
             VALUES (?, ?, 1, '10', '2024-05-01', ?)",
        )
        .bind(sale_id.0)
        .bind(account_id)
        .bind(tx_id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Raw fixture: link a transaction to a synthetic purchase payment.
    async fn link_purchase_payment(pool: &sqlx::SqlitePool, tx_id: i64, account_id: i64) {
        let supplier_id: (i64,) =
            sqlx::query_as("INSERT INTO suppliers (name) VALUES ('fixture supplier') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        let purchase_id: (i64,) = sqlx::query_as(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date) \
             VALUES (?, 'Confirmed', 'Credit', '2024-05-01') RETURNING id",
        )
        .bind(supplier_id.0)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_payments \
             (purchase_id, account_id, method_id, amount, date, transaction_id) \
             VALUES (?, ?, 1, '20', '2024-05-01', ?)",
        )
        .bind(purchase_id.0)
        .bind(account_id)
        .bind(tx_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_linked_transaction_is_conflict_and_keeps_row_and_payment() {
        let (s, pool) = svc().await;
        let acc = s.accounts.create("Caja").await.unwrap();
        let sale_tx = s
            .create_with_reference(
                acc.id,
                TransactionKind::Expense,
                dec("10"),
                Some("sale payment".into()),
                None,
                d(),
            )
            .await
            .unwrap();
        let purchase_tx = s
            .create_with_reference(
                acc.id,
                TransactionKind::Expense,
                dec("20"),
                Some("purchase payment".into()),
                None,
                d(),
            )
            .await
            .unwrap();
        link_sale_payment(&pool, sale_tx.id, acc.id).await;
        link_purchase_payment(&pool, purchase_tx.id, acc.id).await;

        for tx in [&sale_tx, &purchase_tx] {
            let err = s.delete(tx.id).await.unwrap_err();
            assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains("cancel the document"),
                "message must be actionable, got: {msg}"
            );
            assert!(
                s.transactions.find_by_id(tx.id).await.unwrap().is_some(),
                "the linked transaction row must survive"
            );
        }

        let sale_links: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE transaction_id = ?")
                .bind(sale_tx.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sale_links.0, 1, "sale payment must stay linked");
        let purchase_links: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchase_payments WHERE transaction_id = ?")
                .bind(purchase_tx.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(purchase_links.0, 1, "purchase payment must stay linked");
    }

    #[tokio::test]
    async fn delete_unlinked_transaction_still_works() {
        let (s, _pool) = svc().await;
        let acc = s.accounts.create("Caja").await.unwrap();
        let tx = s
            .create_with_reference(
                acc.id,
                TransactionKind::Expense,
                dec("5"),
                Some("manual".into()),
                None,
                d(),
            )
            .await
            .unwrap();

        s.delete(tx.id).await.unwrap();
        assert!(s.transactions.find_by_id(tx.id).await.unwrap().is_none());
    }
}
