// M4 customers (Slice L): customer receipts. One receipt groups the payments a
// single handover of money produced, so this repository owns the receipt document
// and nothing else. It never runs SQL against a sales or finance table:
// `list_allocations` delegates the `sale_payments` read to the sales repository
// that owns the table, and the customer/account/method references are validated by
// the service before `create` is called. FK failures surface as `Validation`
// (a RESTRICTed reference), UNIQUE failures as `Conflict`.
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{CustomerReceipt, NewReceipt, SalePayment};
use crate::repositories::sale_repo::{SaleRepository, SqliteSaleRepository};

fn row_to_receipt(row: sqlx::sqlite::SqliteRow) -> CustomerReceipt {
    CustomerReceipt {
        id: row.get("id"),
        customer_id: row.get("customer_id"),
        account_id: row.get("account_id"),
        method_id: row.get("method_id"),
        date: row.get("date"),
        notes: row.get("notes"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("receipt already exists".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for receipt".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait CustomerReceiptRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewReceipt) -> AppResult<CustomerReceipt>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<CustomerReceipt>>;
    /// Receipts of one customer, oldest first (`date`, then id).
    async fn list_by_customer(&self, customer_id: i64) -> AppResult<Vec<CustomerReceipt>>;
    /// The payments this receipt groups (its allocations), by id. The SQL for
    /// `sale_payments` stays in the sales repository; this read only exposes it to
    /// the receipt aggregate that owns the grouping.
    async fn list_allocations(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>>;
    /// DELETE is RESTRICTed by `sale_payments.receipt_id` once a payment references
    /// the receipt; that failure is surfaced as `Validation`, not a database error.
    async fn delete(&self, id: i64) -> AppResult<bool>;
}

#[derive(Clone)]
pub struct SqliteCustomerReceiptRepository {
    pub pool: SqlitePool,
    /// The `sale_payments` read is delegated here so this file never queries a
    /// sales table.
    sales: SqliteSaleRepository,
}

impl SqliteCustomerReceiptRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            sales: SqliteSaleRepository::new(pool.clone()),
            pool,
        }
    }
}

#[async_trait]
impl CustomerReceiptRepository for SqliteCustomerReceiptRepository {
    async fn create(&self, actor: i64, input: &NewReceipt) -> AppResult<CustomerReceipt> {
        let row = sqlx::query(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, notes, created_by)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at"#,
        )
        .bind(input.customer_id)
        .bind(input.account_id)
        .bind(input.method_id)
        .bind(input.date)
        .bind(input.notes.clone())
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_receipt(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<CustomerReceipt>> {
        let row = sqlx::query(
            r#"SELECT id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at
               FROM customer_receipts WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_receipt))
    }

    async fn list_by_customer(&self, customer_id: i64) -> AppResult<Vec<CustomerReceipt>> {
        let rows = sqlx::query(
            r#"SELECT id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at
               FROM customer_receipts WHERE customer_id = ? ORDER BY date, id"#,
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_receipt).collect())
    }

    async fn list_allocations(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>> {
        self.sales.list_payments_by_receipt(receipt_id).await
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM customer_receipts WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(format!(
                        "cannot delete receipt {id}: payments still reference it; the receipt documents what they paid"
                    ))
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use chrono::NaiveDate;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn receipt(customer_id: i64, account_id: i64, method_id: i64) -> NewReceipt {
        NewReceipt {
            customer_id,
            account_id,
            method_id,
            date: NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
            notes: None,
        }
    }

    /// FK failures surface as `Validation` and a referenced receipt cannot be
    /// deleted, so the collection service reports both without translating a raw
    /// database error itself.
    #[tokio::test]
    async fn fk_failures_are_validation_and_a_referenced_receipt_cannot_be_deleted() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let (customer_id,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (account_id,): (i64,) = sqlx::query_as(
            "INSERT INTO accounts (name, created_by) VALUES ('Caja', ?) RETURNING id",
        )
        .bind(test_support::audit_actor_id(&pool).await.unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        let (method_id,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(&pool)
                .await
                .unwrap();

        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let err = repo
            .create(actor, &receipt(999_999, account_id, method_id))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let stored = repo
            .create(actor, &receipt(customer_id, account_id, method_id))
            .await
            .unwrap();
        assert_eq!(repo.list_by_customer(customer_id).await.unwrap().len(), 1);

        let (sale_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_id, sale_date, created_by)\n             VALUES ('Confirmed', 'Credit', ?, '2024-06-01', ?) RETURNING id",
        )
        .bind(customer_id)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by)\n             VALUES (?, ?, ?, '10', '2024-06-01', ?, ?)",
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(stored.id)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();
        let allocations = repo.list_allocations(stored.id).await.unwrap();
        assert_eq!(allocations.len(), 1);
        // The receipt's amount is derived from these payments, not stored.
        let applied: Decimal = allocations.iter().map(|payment| payment.amount).sum();
        assert_eq!(applied, Decimal::from_str("10").unwrap());

        let err = repo.delete(stored.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("payments still reference it"),
            "got {err}"
        );
        assert!(repo.find_by_id(stored.id).await.unwrap().is_some());
    }
}
