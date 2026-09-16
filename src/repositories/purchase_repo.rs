use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewPurchase, PaymentType, Purchase, PurchaseLine, PurchasePayment, PurchaseStatus,
    UpdatePurchaseDraft,
};

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn status_from_str(s: &str) -> PurchaseStatus {
    s.parse().unwrap_or(PurchaseStatus::Draft)
}

fn payment_type_from_str(s: &str) -> PaymentType {
    s.parse().unwrap_or(PaymentType::Cash)
}

fn row_to_purchase(row: sqlx::sqlite::SqliteRow) -> Purchase {
    let status_str: String = row.get("status");
    let payment_str: String = row.get("payment_type");
    Purchase {
        id: row.get("id"),
        purchase_number: row.get("purchase_number"),
        supplier_id: row.get("supplier_id"),
        status: status_from_str(&status_str),
        payment_type: payment_type_from_str(&payment_str),
        purchase_date: row.get("purchase_date"),
        due_date: row.get("due_date"),
        supplier_invoice_no: row.get("supplier_invoice_no"),
        notes: row.get("notes"),
        cancel_reason: row.get("cancel_reason"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        confirmed_at: row.get("confirmed_at"),
        cancelled_at: row.get("cancelled_at"),
    }
}

fn row_to_line(row: sqlx::sqlite::SqliteRow) -> PurchaseLine {
    let qty_str: String = row.get("qty");
    let cost_str: String = row.get("unit_cost");
    PurchaseLine {
        id: row.get("id"),
        purchase_id: row.get("purchase_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&qty_str),
        unit_cost: parse_decimal(&cost_str),
        created_at: row.get("created_at"),
    }
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> PurchasePayment {
    let amt_str: String = row.get("amount");
    PurchasePayment {
        id: row.get("id"),
        purchase_id: row.get("purchase_id"),
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
        if s.contains("purchases.purchase_number") || s.contains(".purchase_number") {
            AppError::Conflict("purchase_number already exists".into())
        } else {
            AppError::Conflict("purchase already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::NotFound("referenced purchase/supplier/product/account not found".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait PurchaseRepository: Send + Sync {
    async fn create_purchase(&self, input: &NewPurchase) -> AppResult<Purchase>;
    async fn find_purchase(&self, id: i64) -> AppResult<Option<Purchase>>;
    async fn find_purchase_by_number(&self, number: &str) -> AppResult<Option<Purchase>>;
    async fn list_purchases(&self) -> AppResult<Vec<Purchase>>;
    /// Update Draft header fields (service guarantees Draft status).
    async fn update_draft(&self, id: i64, patch: &UpdatePurchaseDraft) -> AppResult<Purchase>;
    /// Transition Draft -> Confirmed with assigned number.
    async fn set_confirmed(&self, id: i64, purchase_number: &str) -> AppResult<Purchase>;
    /// Transition Draft/Confirmed -> Cancelled.
    async fn set_cancelled(&self, id: i64, reason: Option<&str>) -> AppResult<Purchase>;

    async fn create_line(
        &self,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine>;
    async fn find_line(&self, id: i64) -> AppResult<Option<PurchaseLine>>;
    async fn list_lines(&self, purchase_id: i64) -> AppResult<Vec<PurchaseLine>>;
    async fn update_line(&self, id: i64, qty: Decimal, unit_cost: Decimal)
        -> AppResult<PurchaseLine>;
    async fn delete_line(&self, id: i64) -> AppResult<bool>;

    /// Create the payment row and link it to the finance transaction it produced
    /// (`transaction_id`); NULL only for historical rows.
    async fn create_payment(
        &self,
        purchase_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<PurchasePayment>;
    /// Link the refund transaction created by cancelling the purchase to the
    /// payment row it refunds. The original `transaction_id` is left untouched.
    async fn set_payment_refund_transaction(
        &self,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<PurchasePayment>;
    async fn list_payments(&self, purchase_id: i64) -> AppResult<Vec<PurchasePayment>>;
}

#[derive(Clone)]
pub struct SqlitePurchaseRepository {
    pub pool: SqlitePool,
}

impl SqlitePurchaseRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PurchaseRepository for SqlitePurchaseRepository {
    async fn create_purchase(&self, input: &NewPurchase) -> AppResult<Purchase> {
        let invoice = input.supplier_invoice_no.clone().and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        let notes = input.notes.clone().unwrap_or_default();
        let row = sqlx::query(
            r#"INSERT INTO purchases
               (supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes)
               VALUES (?, 'Draft', ?, ?, ?, ?, ?)
               RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(input.supplier_id)
        .bind(input.payment_type.to_string())
        .bind(input.purchase_date)
        .bind(input.due_date)
        .bind(invoice)
        .bind(notes)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn find_purchase(&self, id: i64) -> AppResult<Option<Purchase>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM purchases WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_purchase))
    }

    async fn find_purchase_by_number(&self, number: &str) -> AppResult<Option<Purchase>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM purchases WHERE purchase_number = ?"#,
        )
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_purchase))
    }

    async fn list_purchases(&self) -> AppResult<Vec<Purchase>> {
        let rows = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at FROM purchases ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_purchase).collect())
    }

    async fn update_draft(&self, id: i64, patch: &UpdatePurchaseDraft) -> AppResult<Purchase> {
        let existing = self
            .find_purchase(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {id} not found")))?;

        let supplier_id = patch.supplier_id.unwrap_or(existing.supplier_id);
        let payment_type = patch.payment_type.unwrap_or(existing.payment_type);
        let purchase_date = patch.purchase_date.unwrap_or(existing.purchase_date);
        let due_date = match &patch.due_date {
            Some(inner) => *inner,
            None => existing.due_date,
        };
        let supplier_invoice_no = match &patch.supplier_invoice_no {
            Some(inner) => inner.clone().and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }),
            None => existing.supplier_invoice_no,
        };
        let notes = patch.notes.clone().unwrap_or(existing.notes);

        let row = sqlx::query(
            r#"UPDATE purchases
               SET supplier_id = ?, payment_type = ?, purchase_date = ?, due_date = ?,
                   supplier_invoice_no = ?, notes = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(supplier_id)
        .bind(payment_type.to_string())
        .bind(purchase_date)
        .bind(due_date)
        .bind(supplier_invoice_no)
        .bind(notes)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn set_confirmed(&self, id: i64, purchase_number: &str) -> AppResult<Purchase> {
        let row = sqlx::query(
            r#"UPDATE purchases
               SET purchase_number = ?, status = 'Confirmed',
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(purchase_number)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn set_cancelled(&self, id: i64, reason: Option<&str>) -> AppResult<Purchase> {
        let clean = reason.and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        });
        let row = sqlx::query(
            r#"UPDATE purchases
               SET status = 'Cancelled', cancel_reason = ?,
                   cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(clean)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn create_line(
        &self,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine> {
        let row = sqlx::query(
            r#"INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost)
               VALUES (?, ?, ?, ?)
               RETURNING id, purchase_id, product_id, qty, unit_cost, created_at"#,
        )
        .bind(purchase_id)
        .bind(product_id)
        .bind(qty.to_string())
        .bind(unit_cost.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_line(row))
    }

    async fn find_line(&self, id: i64) -> AppResult<Option<PurchaseLine>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_id, product_id, qty, unit_cost, created_at
               FROM purchase_lines WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_line))
    }

    async fn list_lines(&self, purchase_id: i64) -> AppResult<Vec<PurchaseLine>> {
        let rows = sqlx::query(
            r#"SELECT id, purchase_id, product_id, qty, unit_cost, created_at
               FROM purchase_lines WHERE purchase_id = ? ORDER BY id"#,
        )
        .bind(purchase_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_line).collect())
    }

    async fn update_line(
        &self,
        id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine> {
        let row = sqlx::query(
            r#"UPDATE purchase_lines SET qty = ?, unit_cost = ? WHERE id = ?
               RETURNING id, purchase_id, product_id, qty, unit_cost, created_at"#,
        )
        .bind(qty.to_string())
        .bind(unit_cost.to_string())
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_line(row))
    }

    async fn delete_line(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM purchase_lines WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn create_payment(
        &self,
        purchase_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<PurchasePayment> {
        let row = sqlx::query(
            r#"INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, transaction_id)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at"#,
        )
        .bind(purchase_id)
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
    ) -> AppResult<PurchasePayment> {
        let row = sqlx::query(
            r#"UPDATE purchase_payments SET refund_transaction_id = ? WHERE id = ?
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at"#,
        )
        .bind(refund_transaction_id)
        .bind(payment_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn list_payments(&self, purchase_id: i64) -> AppResult<Vec<PurchasePayment>> {
        let rows = sqlx::query(
            r#"SELECT id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_at
               FROM purchase_payments WHERE purchase_id = ? ORDER BY id"#,
        )
        .bind(purchase_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_payment).collect())
    }

}
