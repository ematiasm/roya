use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewSale, PaymentType, Sale, SaleLine, SaleListFilter, SalePayment, SaleStatus,
    UpdateSaleDraft,
};

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

/// A `%…%` LIKE needle whose literal `%`, `_` and `\` are escaped, so the SQL
/// matches the same partial substring the retired in-memory filter did. Callers
/// compare it to `LOWER(column) ... ESCAPE '\'`; case folding is ASCII, like
/// SQLite's `LOWER`, because the engine ships no Unicode collation.
fn like_needle(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('%');
    for ch in raw.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
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
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
        receipt_id: row.get("receipt_id"),
        // Only the receipt-allocation query selects `sale_number`; every other
        // payment read leaves it `None`.
        sale_number: row.try_get::<Option<String>, _>("sale_number").unwrap_or(None),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
        AppError::NotFound("referenced sale/product/account/receipt not found".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait SaleRepository: Send + Sync {
    /// `customer_name` is the snapshot resolved by the service through
    /// `CustomerService`; this layer never reads the `customers` table.
    async fn create_sale(&self, actor: i64, input: &NewSale, customer_name: &str) -> AppResult<Sale>;
    async fn find_sale(&self, id: i64) -> AppResult<Option<Sale>>;
    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>>;
    async fn list_sales(&self) -> AppResult<Vec<Sale>>;
    /// The same rows narrowed by the list filter, inside the repository query so
    /// only matching documents have their lines and payments loaded. Party
    /// matching uses the frozen `customer_name` snapshot the list already shows.
    async fn list_sales_filtered(&self, filter: &SaleListFilter) -> AppResult<Vec<Sale>>;
    /// Confirmed credit sales of one customer, oldest first. Feeds the derived
    /// receivable used by the credit-limit check; cancelled sales never count.
    async fn list_confirmed_credit_sales(&self, customer_id: i64) -> AppResult<Vec<Sale>>;

    /// Confirmed credit sales of one customer with their lines and payments,
    /// oldest first, for the derived receivable reads (balance, ageing, statement).
    /// Cancelled and cash sales never appear. The service folds these rows into the
    /// `SaleDetail` shape `outstanding_debt` uses, so every total is computed in
    /// Rust (`total - paid`), never with SQL `SUM`.
    async fn list_customer_credit_ledger(
        &self,
        customer_id: i64,
    ) -> AppResult<Vec<(Sale, Vec<SaleLine>, Vec<SalePayment>)>>;
    /// Every customer's confirmed credit ledger in three batched reads, oldest due
    /// first. The debt banner uses this so its query count stays constant as the
    /// shop's history grows; the service derives each total in Rust.
    async fn list_confirmed_credit_ledger_all(
        &self,
    ) -> AppResult<Vec<(Sale, Vec<SaleLine>, Vec<SalePayment>)>>;
    /// Update Draft header fields (service guarantees Draft status).
    async fn update_draft(&self, id: i64, actor: i64, patch: &UpdateSaleDraft) -> AppResult<Sale>;
    /// Transition Draft -> Confirmed with assigned number.
    async fn set_confirmed(&self, id: i64, actor: i64, sale_number: &str) -> AppResult<Sale>;
    /// Transition Draft/Confirmed -> Cancelled.
    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>) -> AppResult<Sale>;

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
    /// (`transaction_id`) and to the receipt that groups it (`receipt_id`); both
    /// are NULL for a direct payment on a single sale. A receipt-grouped payment
    /// still belongs to its sale and keeps its own transaction link.
    async fn create_payment(
        &self,
        actor: i64,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
        receipt_id: Option<i64>,
    ) -> AppResult<SalePayment>;
    /// Link the refund transaction created by cancelling the sale to the payment
    /// row it refunds. The original `transaction_id` is left untouched.
    async fn set_payment_refund_transaction(
        &self,
        actor: i64,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<SalePayment>;
    async fn list_payments(&self, sale_id: i64) -> AppResult<Vec<SalePayment>>;
    /// The payments one customer receipt groups (its allocations), by id. The SQL
    /// for `sale_payments` stays here, in the sales module that owns the table, so
    /// the receipt repository can expose the read without querying a sales table.
    async fn list_payments_by_receipt(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>>;
}

#[derive(Clone)]
pub struct SqliteSaleRepository {
    pub pool: SqlitePool,
    /// Test-only read counter: proves the filtered list reads scale with the
    /// result set, not the shop's history. Absent from production builds.
    #[cfg(test)]
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SqliteSaleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            #[cfg(test)]
            reads: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Count one repository read (test builds only).
    #[cfg(test)]
    fn tick(&self) {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reset and read the test-only read counter.
    #[cfg(test)]
    pub(crate) fn reset_reads(&self) {
        self.reads.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl SaleRepository for SqliteSaleRepository {
    async fn create_sale(&self, actor: i64, input: &NewSale, customer_name: &str) -> AppResult<Sale> {
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
               (status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, created_by)
               VALUES ('Draft', ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(input.payment_type.to_string())
        .bind(input.customer_id)
        .bind(customer_name)
        .bind(input.sale_date)
        .bind(input.due_date)
        .bind(receipt)
        .bind(notes)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn find_sale(&self, id: i64) -> AppResult<Option<Sale>> {
        let row = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn find_sale_by_number(&self, number: &str) -> AppResult<Option<Sale>> {
        let row = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE sale_number = ?"#,
        )
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_sale))
    }

    async fn list_sales(&self) -> AppResult<Vec<Sale>> {
        #[cfg(test)]
        self.tick();
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn list_sales_filtered(&self, filter: &SaleListFilter) -> AppResult<Vec<Sale>> {
        #[cfg(test)]
        self.tick();
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales",
        );
        let has_filter = filter.status.is_some()
            || filter.customer_ids.is_some()
            || filter.number.is_some()
            || filter.from.is_some()
            || filter.to.is_some();
        if has_filter {
            qb.push(" WHERE 1 = 1");
        }
        if let Some(status) = filter.status {
            qb.push(" AND status = ").push_bind(status.to_string());
        }
        if let Some(ids) = &filter.customer_ids {
            if ids.is_empty() {
                // The party filter matched no customer, so no document can match.
                return Ok(Vec::new());
            }
            qb.push(" AND customer_id IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(number) = &filter.number {
            qb.push(" AND sale_number IS NOT NULL AND LOWER(sale_number) LIKE LOWER(")
                .push_bind(like_needle(number))
                .push(") ESCAPE '\\'");
        }
        if let Some(from) = filter.from {
            qb.push(" AND sale_date >= ").push_bind(from);
        }
        if let Some(to) = filter.to {
            qb.push(" AND sale_date <= ").push_bind(to);
        }
        qb.push(" ORDER BY id");
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn list_confirmed_credit_sales(&self, customer_id: i64) -> AppResult<Vec<Sale>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at
               FROM sales
               WHERE customer_id = ? AND status = 'Confirmed' AND payment_type = 'Credit'
               ORDER BY id"#,
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale).collect())
    }

    async fn list_customer_credit_ledger(
        &self,
        customer_id: i64,
    ) -> AppResult<Vec<(Sale, Vec<SaleLine>, Vec<SalePayment>)>> {
        let sales = self.list_confirmed_credit_sales(customer_id).await?;
        let mut out = Vec::with_capacity(sales.len());
        for sale in sales {
            let lines = self.list_lines(sale.id).await?;
            let payments = self.list_payments(sale.id).await?;
            out.push((sale, lines, payments));
        }
        Ok(out)
    }

    async fn list_confirmed_credit_ledger_all(
        &self,
    ) -> AppResult<Vec<(Sale, Vec<SaleLine>, Vec<SalePayment>)>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at
               FROM sales
               WHERE status = 'Confirmed' AND payment_type = 'Credit'
               ORDER BY due_date, sale_date, id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        #[cfg(test)]
        self.tick();
        let sales: Vec<Sale> = rows.into_iter().map(row_to_sale).collect();
        if sales.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<i64> = sales.iter().map(|sale| sale.id).collect();

        let mut lines_qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, sale_id, product_id, qty, unit_price, created_at FROM sale_lines WHERE sale_id IN (",
        );
        {
            let mut separated = lines_qb.separated(", ");
            for id in &ids {
                separated.push_bind(*id);
            }
            separated.push_unseparated(") ORDER BY id");
        }
        let line_rows = lines_qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        let mut payments_qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at FROM sale_payments WHERE sale_id IN (",
        );
        {
            let mut separated = payments_qb.separated(", ");
            for id in &ids {
                separated.push_bind(*id);
            }
            separated.push_unseparated(") ORDER BY id");
        }
        let payment_rows = payments_qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        let mut lines_by_sale: std::collections::BTreeMap<i64, Vec<SaleLine>> =
            std::collections::BTreeMap::new();
        for row in line_rows {
            let line = row_to_line(row);
            lines_by_sale.entry(line.sale_id).or_default().push(line);
        }
        let mut payments_by_sale: std::collections::BTreeMap<i64, Vec<SalePayment>> =
            std::collections::BTreeMap::new();
        for row in payment_rows {
            let payment = row_to_payment(row);
            payments_by_sale
                .entry(payment.sale_id)
                .or_default()
                .push(payment);
        }

        Ok(sales
            .into_iter()
            .map(|sale| {
                let lines = lines_by_sale.remove(&sale.id).unwrap_or_default();
                let payments = payments_by_sale.remove(&sale.id).unwrap_or_default();
                (sale, lines, payments)
            })
            .collect())
    }

    async fn update_draft(&self, id: i64, actor: i64, patch: &UpdateSaleDraft) -> AppResult<Sale> {
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
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(sale_date)
        .bind(due_date)
        .bind(receipt_no)
        .bind(notes)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn set_confirmed(&self, id: i64, actor: i64, sale_number: &str) -> AppResult<Sale> {
        let row = sqlx::query(
            r#"UPDATE sales
               SET sale_number = ?, status = 'Confirmed',
                   updated_by = ?,
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(sale_number)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_sale(row))
    }

    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>) -> AppResult<Sale> {
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
                   updated_by = ?,
                   cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(clean)
        .bind(actor)
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
        #[cfg(test)]
        self.tick();
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
        actor: i64,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
        receipt_id: Option<i64>,
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, transaction_id, receipt_id, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at"#,
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .bind(transaction_id)
        .bind(receipt_id)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn set_payment_refund_transaction(
        &self,
        actor: i64,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"UPDATE sale_payments SET refund_transaction_id = ?, updated_by = ? WHERE id = ?
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at"#,
        )
        .bind(refund_transaction_id)
        .bind(actor)
        .bind(payment_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn list_payments(&self, sale_id: i64) -> AppResult<Vec<SalePayment>> {
        #[cfg(test)]
        self.tick();
        let rows = sqlx::query(
            r#"SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at
               FROM sale_payments WHERE sale_id = ? ORDER BY id"#,
        )
        .bind(sale_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_payment).collect())
    }

    async fn list_payments_by_receipt(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>> {
        let rows = sqlx::query(
            r#"SELECT sp.id, sp.sale_id, sp.account_id, sp.method_id, sp.amount, sp.date, sp.transaction_id, sp.refund_transaction_id, sp.receipt_id, sp.created_by, sp.updated_by, sp.created_at, s.sale_number
               FROM sale_payments sp
               JOIN sales s ON s.id = sp.sale_id
               WHERE sp.receipt_id = ? ORDER BY sp.id"#,
        )
        .bind(receipt_id)
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

        // Replay the pre-K2 migration range so the legacy schema is real. The
        // later migrations depend on K2's `sales.customer_id` (migration 23's
        // receipt triggers reference it), so they are applied after the rebuild
        // below instead of against a schema their SQL cannot compile on.
        let migrator = sqlx::migrate!("./migrations");
        let mut applied: Vec<String> = Vec::new();
        for migration in migrator.iter() {
            if migration.version >= 20240101000021 {
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

        // K2 owns `sales.customer_id`; only now can the migrations that depend on
        // it (customer receipts and the receipt-link triggers) be replayed.
        for migration in migrator.iter() {
            if migration.version <= 20240101000021 {
                continue;
            }
            sqlx::raw_sql(migration.sql.clone())
                .execute(&pool)
                .await
                .unwrap();
            applied.push(migration.description.to_string());
        }

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
