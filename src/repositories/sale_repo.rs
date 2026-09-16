use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewSale, PaymentType, Sale, SaleLine, SalePayment, SaleStatus, UpdateSaleDraft,
};

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn status_from_str(s: &str) -> SaleStatus {
    s.parse().unwrap_or(SaleStatus::Draft)
}

fn payment_type_from_str(s: &str) -> PaymentType {
    s.parse().unwrap_or(PaymentType::Cash)
}

fn row_to_sale(row: sqlx::sqlite::SqliteRow) -> Sale {
    let status_str: String = row.get("status");
    let payment_str: String = row.get("payment_type");
    Sale {
        id: row.get("id"),
        sale_number: row.get("sale_number"),
        status: status_from_str(&status_str),
        payment_type: payment_type_from_str(&payment_str),
        customer_id: row.get("customer_id"),
        customer_name: row.get("customer_name"),
        sale_date: row.get("sale_date"),
        due_date: row.get("due_date"),
        receipt_no: row.get("receipt_no"),
        notes: row.get("notes"),
        cancel_reason: row.get("cancel_reason"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        confirmed_at: row.get("confirmed_at"),
        cancelled_at: row.get("cancelled_at"),
    }
}

fn row_to_line(row: sqlx::sqlite::SqliteRow) -> SaleLine {
    let qty_str: String = row.get("qty");
    let price_str: String = row.get("unit_price");
    SaleLine {
        id: row.get("id"),
        sale_id: row.get("sale_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&qty_str),
        unit_price: parse_decimal(&price_str),
        created_at: row.get("created_at"),
    }
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> SalePayment {
    let amt_str: String = row.get("amount");
    SalePayment {
        id: row.get("id"),
        sale_id: row.get("sale_id"),
        account_id: row.get("account_id"),
        method_id: row.get("method_id"),
        amount: parse_decimal(&amt_str),
        date: row.get("date"),
        transaction_id: row.get("transaction_id"),
        refund_transaction_id: row.get("refund_transaction_id"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains("sales.sale_number") || s.contains(".sale_number") {
            AppError::Conflict("sale_number already exists".into())
        } else if s.contains("receipt_no") {
            AppError::Conflict("receipt_no already exists".into())
        } else {
            AppError::Conflict("sale already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::NotFound("referenced sale/product/account not found".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait SaleRepository: Send + Sync {
    /// `customer_name` is the snapshot resolved by the service through
    /// `CustomerService`; this layer never reads the `customers` table.
    async fn create_sale(&self, input: &NewSale, customer_name: &str) -> AppResult<Sale>;
    async fn find_sale(&self, id: i64) -> AppResult<Option<Sale>>;
    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>>;
    async fn list_sales(&self) -> AppResult<Vec<Sale>>;
    /// Confirmed credit sales of one customer, oldest first. Feeds the derived
    /// receivable used by the credit-limit check; cancelled sales never count.
    async fn list_confirmed_credit_sales(&self, customer_id: i64) -> AppResult<Vec<Sale>>;
    /// Update Draft header fields (service guarantees Draft status).
    async fn update_draft(&self, id: i64, patch: &UpdateSaleDraft) -> AppResult<Sale>;
    /// Transition Draft -> Confirmed with assigned number.
    async fn set_confirmed(&self, id: i64, sale_number: &str) -> AppResult<Sale>;
    /// Transition Draft/Confirmed -> Cancelled.
    async fn set_cancelled(&self, id: i64, reason: Option<&str>) -> AppResult<Sale>;

    async fn create_line(
        &self,
        sale_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine>;
    async fn find_line(&self, id: i64) -> AppResult<Option<SaleLine>>;
    async fn list_lines(&self, sale_id: i64) -> AppResult<Vec<SaleLine>>;
    async fn update_line(&self, id: i64, qty: Decimal, unit_price: Decimal)
        -> AppResult<SaleLine>;
    async fn delete_line(&self, id: i64) -> AppResult<bool>;

    /// Create the payment row and link it to the finance transaction it produced
    /// (`transaction_id`); NULL only for historical rows.
    async fn create_payment(
        &self,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<SalePayment>;
    /// Link the refund transaction created by cancelling the sale to the payment
    /// row it refunds. The original `transaction_id` is left untouched.
    async fn set_payment_refund_transaction(
        &self,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<SalePayment>;
    async fn list_payments(&self, sale_id: i64) -> AppResult<Vec<SalePayment>>;
}

#[derive(Clone)]
pub struct SqliteSaleRepository {
    pub pool: SqlitePool,
}

impl SqliteSaleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SaleRepository for SqliteSaleRepository {
    async fn create_sale(&self, input: &NewSale, customer_name: &str) -> AppResult<Sale> {
        let receipt = input.receipt_no.clone().and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        let notes = input.notes.clone().unwrap_or_default();
        let row = sqlx::query(
            r#"INSERT INTO sales
               (status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes)
               VALUES ('Draft', ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(input.payment_type.to_string())
        .bind(input.customer_id)
        .bind(customer_name)
        .bind(input.sale_date)
        .bind(input.due_date)
        .bind(receipt)
        .bind(notes)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn find_sale(&self, id: i64) -> AppResult<Option<Sale>> {
        let row = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>> {
        let row = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE sale_number = ?"#,
        )
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn list_sales(&self) -> AppResult<Vec<Sale>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn list_confirmed_credit_sales(&self, customer_id: i64) -> AppResult<Vec<Sale>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at
               FROM sales
               WHERE customer_id = ? AND status = 'Confirmed' AND payment_type = 'Credit'
               ORDER BY id"#,
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn update_draft(&self, id: i64, patch: &UpdateSaleDraft) -> AppResult<Sale> {
        let existing = self
            .find_sale(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))?;

        let sale_date = patch.sale_date.unwrap_or(existing.sale_date);
        let due_date = match &patch.due_date {
            Some(inner) => *inner,
            None => existing.due_date,
        };
        let receipt_no = match &patch.receipt_no {
            Some(inner) => inner.clone().and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }),
            None => existing.receipt_no,
        };
        let notes = patch.notes.clone().unwrap_or(existing.notes);

        let row = sqlx::query(
            r#"UPDATE sales
               SET sale_date = ?, due_date = ?, receipt_no = ?, notes = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(sale_date)
        .bind(due_date)
        .bind(receipt_no)
        .bind(notes)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn set_confirmed(&self, id: i64, sale_number: &str) -> AppResult<Sale> {
        let row = sqlx::query(
            r#"UPDATE sales
               SET sale_number = ?, status = 'Confirmed',
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(sale_number)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn set_cancelled(&self, id: i64, reason: Option<&str>) -> AppResult<Sale> {
        let clean = reason.and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        });
        let row = sqlx::query(
            r#"UPDATE sales
               SET status = 'Cancelled', cancel_reason = ?,
                   cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(clean)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn create_line(
        &self,
        sale_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine> {
        let row = sqlx::query(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
               VALUES (?, ?, ?, ?)
               RETURNING id, sale_id, product_id, qty, unit_price, created_at"#,
        )
        .bind(sale_id)
        .bind(product_id)
        .bind(qty.to_string())
        .bind(unit_price.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_line(row))
    }

    async fn find_line(&self, id: i64) -> AppResult<Option<SaleLine>> {
        let row = sqlx::query(
            r#"SELECT id, sale_id, product_id, qty, unit_price, created_at
               FROM sale_lines WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_line))
    }

    async fn list_lines(&self, sale_id: i64) -> AppResult<Vec<SaleLine>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_id, product_id, qty, unit_price, created_at
               FROM sale_lines WHERE sale_id = ? ORDER BY id"#,
        )
        .bind(sale_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_line).collect())
    }

    async fn update_line(
        &self,
        id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine> {
        let row = sqlx::query(
            r#"UPDATE sale_lines SET qty = ?, unit_price = ? WHERE id = ?
               RETURNING id, sale_id, product_id, qty, unit_price, created_at"#,
        )
        .bind(qty.to_string())
        .bind(unit_price.to_string())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_line(row))
    }

    async fn delete_line(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM sale_lines WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn create_payment(
        &self,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, transaction_id)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at"#,
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .bind(transaction_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn set_payment_refund_transaction(
        &self,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"UPDATE sale_payments SET refund_transaction_id = ? WHERE id = ?
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at"#,
        )
        .bind(refund_transaction_id)
        .bind(payment_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn list_payments(&self, sale_id: i64) -> AppResult<Vec<SalePayment>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at
               FROM sale_payments WHERE sale_id = ? ORDER BY id"#,
        )
        .bind(sale_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_payment).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool so the walk-in backstops fire
            // exactly as they do in production.
            .pragma("recursive_triggers", "1");
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    }

    /// AC16: the K2 migration runs against a database that already has sales,
    /// lines and payments. It must backfill every sale to the seeded walk-in and
    /// enforce `NOT NULL` without losing a single child row (the parent swap
    /// would otherwise cascade-delete them).
    #[tokio::test]
    async fn ac16_add_sales_customer_backfills_walkin_and_preserves_rows() {
        let pool = memory_pool().await;

        // Replay every migration except the K2 one so the legacy schema is real.
        let migrator = sqlx::migrate!("./migrations");
        let mut applied: Vec<String> = Vec::new();
        for migration in migrator.iter() {
            if migration.description.contains("add sales customer") {
                continue;
            }
            sqlx::raw_sql(migration.sql.clone())
                .execute(&pool)
                .await
                .unwrap();
            applied.push(migration.description.to_string());
        }
        assert!(
            applied.iter().any(|d| d.contains("create sales")),
            "legacy sales schema must exist before K2: {applied:?}"
        );
        assert!(
            applied.iter().any(|d| d.contains("create customers")),
            "K1 customers schema must exist before K2: {applied:?}"
        );

        // Legacy data going through the rebuild: sale + line + payment.
        let (walkin_id,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (account_id,): (i64,) = sqlx::query_as(
            "INSERT INTO accounts (name) VALUES ('legacy wallet') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (method_id,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (product_id,): (i64,) = sqlx::query_as(
            r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock)
               VALUES ('LEGACY-P', 'legacy prod', 'Product', 'un', '10', 1)
               RETURNING id"#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (legacy_sale_id,): (i64,) = sqlx::query_as(
            r#"INSERT INTO sales (sale_number, status, payment_type, customer_name, sale_date, due_date)
               VALUES ('2024-SALE-000001', 'Confirmed', 'Credit', 'Legacy buyer', '2024-05-02', '2024-06-01')
               RETURNING id"#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
               VALUES (?, ?, '2', '10')"#,
        )
        .bind(legacy_sale_id)
        .bind(product_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date)
               VALUES (?, ?, ?, '5', '2024-05-10')"#,
        )
        .bind(legacy_sale_id)
        .bind(account_id)
        .bind(method_id)
        .execute(&pool)
        .await
        .unwrap();

        // Apply the K2 migration to the populated database.
        let k2 = migrator
            .iter()
            .find(|m| m.description.contains("add sales customer"))
            .expect("add_sales_customer migration is missing");
        sqlx::raw_sql(k2.sql.clone())
            .execute(&pool)
            .await
            .unwrap();

        // Backfill: the legacy sale belongs to the walk-in and keeps its snapshot.
        let (customer_id, customer_name): (i64, String) =
            sqlx::query_as("SELECT customer_id, customer_name FROM sales WHERE id = ?")
                .bind(legacy_sale_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            customer_id, walkin_id,
            "legacy sales must be backfilled to the seeded walk-in"
        );
        assert_eq!(
            customer_name, "Legacy buyer",
            "the legacy customer_name snapshot must not be rewritten"
        );

        // Child rows survived the parent rebuild.
        let (lines,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?")
                .bind(legacy_sale_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let (payments,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?")
                .bind(legacy_sale_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lines, 1, "the rebuild must preserve sale lines");
        assert_eq!(payments, 1, "the rebuild must preserve sale payments");

        // NOT NULL and FK enforcement on the rebuilt column.
        assert!(
            sqlx::query("INSERT INTO sales (status, payment_type, sale_date) VALUES ('Draft', 'Cash', '2024-05-03')")
                .execute(&pool)
                .await
                .is_err(),
            "customer_id must be NOT NULL"
        );
        assert!(
            sqlx::query(
                "INSERT INTO sales (status, payment_type, customer_id, sale_date) VALUES ('Draft', 'Cash', 99999, '2024-05-03')"
            )
            .execute(&pool)
            .await
            .is_err(),
            "unknown customers must be rejected by the foreign key"
        );

        // Indexes recreated (the old ones die with the dropped table).
        let (customer_idx,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_sales_customer_id'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(customer_idx, 1, "customer_id must be indexed");
        let (kept_idx,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN ('idx_sales_status', 'idx_sales_sale_date')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept_idx, 2, "pre-existing sales indexes must survive");
        assert!(
            sqlx::query("PRAGMA foreign_key_check").execute(&pool).await.is_ok(),
            "the rebuilt database must pass the foreign key check"
        );
    }

    /// AC17: the sales storage layer never reads another module's tables, so
    /// `customers` is reached exclusively through `CustomerService`. The needles
    /// are assembled at runtime so this assertion cannot match its own source.
    #[test]
    fn ac17_sale_repository_never_reads_the_customers_table() {
        let source = include_str!("sale_repo.rs");
        // Only the production half; the migration fixture below reconstructs the
        // pre-K2 schema and would otherwise match its own needles.
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("source must have a production half");
        assert!(
            production.len() < source.len(),
            "the test module marker must split the source"
        );
        let table = "customers";
        for verb in ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"] {
            let needle = format!("{verb} {table}");
            assert!(
                !production.contains(&needle),
                "sale_repo must not run `{needle}` SQL; sales reach customers through CustomerService"
            );
        }
    }
}
