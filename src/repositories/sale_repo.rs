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
    async fn create_sale(&self, input: &NewSale) -> AppResult<Sale>;
    async fn find_sale(&self, id: i64) -> AppResult<Option<Sale>>;
    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>>;
    async fn list_sales(&self) -> AppResult<Vec<Sale>>;
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

    async fn create_payment(
        &self,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
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
    async fn create_sale(&self, input: &NewSale) -> AppResult<Sale> {
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
               (status, payment_type, customer_name, sale_date, due_date, receipt_no, notes)
               VALUES ('Draft', ?, ?, ?, ?, ?, ?)
               RETURNING id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(input.payment_type.to_string())
        .bind(input.customer_name.clone())
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
            r#"SELECT id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>> {
        let row = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE sale_number = ?"#,
        )
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn list_sales(&self) -> AppResult<Vec<Sale>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM sales ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn update_draft(&self, id: i64, patch: &UpdateSaleDraft) -> AppResult<Sale> {
        let existing = self
            .find_sale(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))?;

        let customer_name = patch.customer_name.clone().unwrap_or(existing.customer_name);
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
               SET customer_name = ?, sale_date = ?, due_date = ?, receipt_no = ?, notes = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(customer_name)
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
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
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
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
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
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, sale_id, account_id, method_id, amount, date, created_at"#,
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn list_payments(&self, sale_id: i64) -> AppResult<Vec<SalePayment>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_id, account_id, method_id, amount, date, created_at
               FROM sale_payments WHERE sale_id = ? ORDER BY id"#,
        )
        .bind(sale_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_payment).collect())
    }
}
