use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    DocumentKind, DocumentQuery, DocumentRow, NewSale, PaymentType, Sale, SaleLine,
    SaleListFilter, SalePayment, SaleStatus, UpdateSaleDraft,
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

    /// Delete a DRAFT sale and let its lines die by CASCADE. The `status =
    /// 'Draft'` in the WHERE is the load-bearing backstop: even if a caller
    /// ever relaxed the service's state guard, a Confirmed or Cancelled row
    /// cannot be removed by this statement — it answers `false` instead, so
    /// the caller can refuse honestly. A draft is the only deletable state by
    /// construction (no payments — `record_payment` refuses anything not
    /// Confirmed — no stock movement, no ledger entry, no customer debt), so
    /// nothing dangles. This is a WRITE, not a read: the test read counter
    /// stays untouched.
    async fn delete_draft(&self, id: i64) -> AppResult<bool>;

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
    /// One payment by id — the documents drawer's per-payment read. `None`
    /// for an id that does not exist; the service decides what that means.
    async fn find_payment(&self, id: i64) -> AppResult<Option<SalePayment>>;
    /// The payments one customer receipt groups (its allocations), by id. The SQL
    /// for `sale_payments` stays here, in the sales module that owns the table, so
    /// the receipt repository can expose the read without querying a sales table.
    async fn list_payments_by_receipt(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>>;

    /// The SALES family of the documents index (documents-index): the stored
    /// sale projected to the feed's facts, with its derived total summed in Rust
    /// over ONE batched lines read — `SaleLine::subtotal`, the same definition
    /// the sale record page uses, never SQL `SUM` over a TEXT column.
    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>>;

    /// The SALE-PAYMENTS family of the documents index: the payment joined to
    /// its sale so the row names the sale the way the operator does (number, or
    /// `Draft #id`) and shows the frozen customer name.
    async fn list_payment_document_rows(&self, query: &DocumentQuery)
        -> AppResult<Vec<DocumentRow>>;

    /// `receipt_id -> Σ amount` for every payment grouped under the given
    /// receipts. The receipt family's total comes from here so
    /// `customer_receipt_repo` keeps its rule of never querying a sales table.
    /// An empty id list answers an empty map without touching the database.
    async fn receipt_allocations(
        &self,
        receipt_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, Decimal>>;
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

    async fn delete_draft(&self, id: i64) -> AppResult<bool> {
        // `AND status = 'Draft'` is the backstop that makes deleting a
        // confirmed (or cancelled) document impossible even if the service
        // check were relaxed: the WHERE simply matches nothing and the answer
        // is `false`.
        let res = sqlx::query(r#"DELETE FROM sales WHERE id = ? AND status = 'Draft'"#)
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

    async fn find_payment(&self, id: i64) -> AppResult<Option<SalePayment>> {
        #[cfg(test)]
        self.tick();
        let row = sqlx::query(
            r#"SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at
               FROM sale_payments WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_payment))
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

    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>> {
        // `Some(empty)` matched no actor, so no document can match: answer
        // without querying, like every other `Some(empty)` id filter here.
        if let Some(ids) = &query.actor_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND sale_date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND sale_date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // NULL-safe on every nullable identifier; the frozen customer name
            // is NOT NULL, so it needs no COALESCE. ESCAPE is per-comparison
            // SQLite syntax, so the needle repeats three times.
            let needle = like_needle(search);
            qb.push(" AND (LOWER(COALESCE(sale_number, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(COALESCE(receipt_no, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(customer_name) LIKE LOWER(")
                .push_bind(needle)
                .push(") ESCAPE '\\')");
        }
        // Newest first: date descending, then id as the stable tiebreak.
        qb.push(" ORDER BY sale_date DESC, id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();
        let sales: Vec<Sale> = rows.into_iter().map(row_to_sale).collect();
        if sales.is_empty() {
            return Ok(Vec::new());
        }

        // ONE batched lines read for the whole page: the query count stays
        // constant no matter how many documents matched. The total is folded
        // in Rust over `SaleLine::subtotal`, never with SQL SUM over TEXT.
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

        let mut totals: std::collections::BTreeMap<i64, Decimal> =
            std::collections::BTreeMap::new();
        for row in line_rows {
            let line = row_to_line(row);
            *totals.entry(line.sale_id).or_insert_with(|| Decimal::ZERO) += line.subtotal();
        }

        Ok(sales
            .into_iter()
            .map(|sale| DocumentRow {
                kind: DocumentKind::Sale,
                id: sale.id,
                owner_id: sale.id,
                reference: sale
                    .sale_number
                    .clone()
                    .unwrap_or_else(|| format!("Draft #{}", sale.id)),
                party: sale.customer_name.clone(),
                date: sale.sale_date,
                detail: sale.status.to_string(),
                amount: Some(totals.remove(&sale.id).unwrap_or_default()),
                quantity: None,
                created_by: sale.created_by,
            })
            .collect())
    }

    async fn list_payment_document_rows(
        &self,
        query: &DocumentQuery,
    ) -> AppResult<Vec<DocumentRow>> {
        // `Some(empty)` matched no actor, so no payment can match.
        if let Some(ids) = &query.actor_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }
        // The JOIN is the whole point of this family read: the payment names
        // its sale (number or Draft #id) and the frozen customer snapshot, in
        // the same single query that reads the payment.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT sp.id, sp.sale_id, sp.amount, sp.date, sp.created_by, s.sale_number, s.customer_name FROM sale_payments sp JOIN sales s ON s.id = sp.sale_id",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND sp.created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND sp.date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND sp.date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // The operator searches a payment by the sale it belongs to.
            let needle = like_needle(search);
            qb.push(" AND (LOWER(COALESCE(s.sale_number, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(COALESCE(s.receipt_no, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(s.customer_name) LIKE LOWER(")
                .push_bind(needle)
                .push(") ESCAPE '\\')");
        }
        qb.push(" ORDER BY sp.date DESC, sp.id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        Ok(rows
            .into_iter()
            .map(|row| {
                let amount_str: String = row.get("amount");
                let sale_id: i64 = row.get("sale_id");
                let sale_number: Option<String> = row.get("sale_number");
                DocumentRow {
                    kind: DocumentKind::SalePayment,
                    id: row.get("id"),
                    owner_id: sale_id,
                    reference: sale_number.unwrap_or_else(|| format!("Draft #{sale_id}")),
                    party: row.get("customer_name"),
                    date: row.get("date"),
                    detail: "Pago".to_string(),
                    amount: Some(parse_decimal(&amount_str)),
                    quantity: None,
                    created_by: row.get("created_by"),
                }
            })
            .collect())
    }

    async fn receipt_allocations(
        &self,
        receipt_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, Decimal>> {
        if receipt_ids.is_empty() {
            // Nothing was asked: no query, no rows, an empty map.
            return Ok(std::collections::BTreeMap::new());
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT receipt_id, amount FROM sale_payments WHERE receipt_id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for id in receipt_ids {
                separated.push_bind(*id);
            }
            separated.push_unseparated(")");
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        let mut out: std::collections::BTreeMap<i64, Decimal> =
            std::collections::BTreeMap::new();
        for row in rows {
            let receipt_id: i64 = row.get("receipt_id");
            let amount_str: String = row.get("amount");
            *out.entry(receipt_id).or_insert_with(|| Decimal::ZERO) += parse_decimal(&amount_str);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use chrono::NaiveDate;
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


    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn documents_pool() -> SqlitePool {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn product_id(pool: &SqlitePool, actor: i64) -> i64 {
        // The test seeds several lines per document; one product row per
        // database is enough, and the sku is UNIQUE so reuse it.
        match sqlx::query_scalar("SELECT id FROM products WHERE sku = 'DOC-P'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                       VALUES ('DOC-P', 'doc prod', 'Product', 'un', '10', 1, ?)
                       RETURNING id"#,
                )
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap()
            }
        }
    }

    /// One confirmed sale through raw SQL (the projection tests seed shapes the
    /// repository API cannot build: arbitrary numbers, dates and actors).
    async fn seed_sale(
        pool: &SqlitePool,
        number: Option<&str>,
        customer: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (walkin,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(pool)
                .await
                .unwrap();
        sqlx::query_scalar(
            r#"INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, created_by)
               VALUES (?, 'Confirmed', 'Credit', ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(number)
        .bind(walkin)
        .bind(customer)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_line(pool: &SqlitePool, sale_id: i64, qty: &str, price: &str) {
        let (actor,): (i64,) = sqlx::query_as("SELECT created_by FROM sales WHERE id = ?")
            .bind(sale_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let product = product_id(pool, actor).await;
        sqlx::query("INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, ?, ?)")
            .bind(sale_id)
            .bind(product)
            .bind(qty)
            .bind(price)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn account_and_method(pool: &SqlitePool, actor: i64) -> (i64, i64) {
        // One wallet per test database: the name is UNIQUE, so reuse it when a
        // second payment in the same test needs the pair.
        let account: i64 = match sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'doc wallet'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO accounts (name, created_by) VALUES ('doc wallet', ?) RETURNING id",
                )
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap()
            }
        };
        let (method,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(pool)
                .await
                .unwrap();
        (account, method)
    }

    async fn seed_payment(
        pool: &SqlitePool,
        sale_id: i64,
        amount: &str,
        date: NaiveDate,
        actor: i64,
        receipt_id: Option<i64>,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(sale_id)
        .bind(account)
        .bind(method)
        .bind(amount)
        .bind(date)
        .bind(receipt_id)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The projection: number (or Draft #id), frozen customer, status pill,
    /// owner id, the exact Rust-summed line total, no quantity, the actor.
    #[tokio::test]
    async fn sale_document_rows_project_the_feed_facts() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();

        let confirmed = seed_sale(&pool, Some("2024-SALE-000001"), "Pérez", d(2024, 5, 2), actor).await;
        seed_line(&pool, confirmed, "2", "10").await;
        seed_line(&pool, confirmed, "3", "2.5").await; // Σ = 20 + 7.5 = 27.5
        let draft = seed_sale(&pool, None, "Díaz", d(2024, 5, 3), actor).await;
        seed_line(&pool, draft, "1", "5").await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        let confirmed_row = rows
            .iter()
            .find(|r| r.id == confirmed)
            .expect("confirmed sale row");
        assert_eq!(confirmed_row.kind, DocumentKind::Sale);
        assert_eq!(confirmed_row.reference, "2024-SALE-000001");
        assert_eq!(confirmed_row.party, "Pérez");
        assert_eq!(confirmed_row.date, d(2024, 5, 2));
        assert_eq!(confirmed_row.detail, "Confirmed");
        assert_eq!(confirmed_row.owner_id, confirmed);
        assert_eq!(confirmed_row.amount, Some(dec("27.5")));
        assert_eq!(confirmed_row.quantity, None);
        assert_eq!(confirmed_row.created_by, actor);

        let draft_row = rows.iter().find(|r| r.id == draft).expect("draft row");
        assert_eq!(draft_row.reference, format!("Draft #{draft}"));
        assert_eq!(draft_row.detail, "Confirmed");
    }

    /// The audit-actor filter narrows the family read to the given ids, and
    /// `Some(empty)` matches nothing without touching the database.
    #[tokio::test]
    async fn sale_document_rows_filter_by_actor_and_empty_actor_set() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let sistema = test_support::audit_actor_id(&pool).await.unwrap();
        let other = test_support::seed_audit_user(&pool, "doc-actor-2", "Doc Actor 2")
            .await
            .unwrap();
        assert_ne!(sistema, other);

        let mine = seed_sale(&pool, Some("2024-SALE-000001"), "Pérez", d(2024, 5, 2), sistema).await;
        let theirs = seed_sale(&pool, Some("2024-SALE-000002"), "Díaz", d(2024, 5, 3), other).await;

        let only_sistema = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![sistema]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(
            only_sistema.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![mine]
        );

        let only_other = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![other]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(
            only_other.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![theirs]
        );

        let none = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    /// Both date bounds are inclusive and exclude the neighbours.
    #[tokio::test]
    async fn sale_document_rows_date_range_is_inclusive() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let early = seed_sale(&pool, Some("2024-SALE-000001"), "Early", d(2024, 5, 1), actor).await;
        let first = seed_sale(&pool, Some("2024-SALE-000002"), "First", d(2024, 5, 2), actor).await;
        let last = seed_sale(&pool, Some("2024-SALE-000003"), "Last", d(2024, 5, 4), actor).await;
        let late = seed_sale(&pool, Some("2024-SALE-000004"), "Late", d(2024, 5, 5), actor).await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: Some(d(2024, 5, 2)),
                to: Some(d(2024, 5, 4)),
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        let ids = rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert!(!ids.contains(&early) && !ids.contains(&late));
        assert!(ids.contains(&first) && ids.contains(&last));
        assert_eq!(ids.len(), 2);
    }

    /// The search matches number and counterpart partially and
    /// case-insensitively, and a search matching nothing is an empty list.
    #[tokio::test]
    async fn sale_document_rows_search_number_and_counterpart() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        seed_sale(&pool, Some("2024-SALE-000001"), "González", d(2024, 5, 2), actor).await;
        seed_sale(&pool, Some("2024-SALE-000002"), "Pérez", d(2024, 5, 3), actor).await;

        // Partial, case-insensitive number match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("sale-000002".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "2024-SALE-000002");

        // Counterpart match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("gonzález".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].party, "González");

        // Matching nothing is empty, never an error.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("zzz-nothing".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    /// The family read returns at most `limit` newest rows, date then id
    /// descending — the feed's newest-first contract.
    #[tokio::test]
    async fn sale_document_rows_limit_returns_newest_first() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let a = seed_sale(&pool, Some("2024-SALE-000001"), "A", d(2024, 5, 1), actor).await;
        let b = seed_sale(&pool, Some("2024-SALE-000002"), "B", d(2024, 5, 2), actor).await;
        let c = seed_sale(&pool, Some("2024-SALE-000003"), "C", d(2024, 5, 2), actor).await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 2,
            })
            .await
            .unwrap();
        // Same date falls back to id descending, so c beats b.
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![c, b]);
        assert!(!rows.iter().any(|r| r.id == a));
    }

    /// The bounded-reads contract: 20 documents cost exactly two queries (the
    /// rows query plus ONE batched lines read), never one read per row.
    #[tokio::test]
    async fn sale_document_rows_read_count_stays_bounded() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        for i in 1..=20 {
            let id = seed_sale(
                &pool,
                Some(&format!("2024-SALE-{i:06}")),
                "Bulk",
                d(2024, 5, 1),
                actor,
            )
            .await;
            seed_line(&pool, id, "1", "1").await;
        }

        repo.reset_reads();
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 200,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 20);
        assert_eq!(repo.read_count(), 2, "rows query + one batched lines read");

        // A filter matching nothing stops after the rows query.
        repo.reset_reads();
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("zzz-nothing".into()),
                limit: 200,
            })
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert_eq!(repo.read_count(), 1, "no lines batch for an empty result");
    }

    /// The payment projection names the sale the way the operator does and
    /// carries the frozen customer name; one query total.
    #[tokio::test]
    async fn sale_payment_document_rows_project_the_feed_facts() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();

        let confirmed = seed_sale(&pool, Some("2024-SALE-000001"), "Pérez", d(2024, 5, 2), actor).await;
        let draft = seed_sale(&pool, None, "Díaz", d(2024, 5, 3), actor).await;
        seed_payment(&pool, confirmed, "10", d(2024, 5, 10), actor, None).await;
        seed_payment(&pool, draft, "5", d(2024, 5, 11), actor, None).await;

        repo.reset_reads();
        let rows = repo
            .list_payment_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(repo.read_count(), 1, "the joined read is one query");
        assert_eq!(rows.len(), 2);

        let confirmed_payment = rows
            .iter()
            .find(|r| r.owner_id == confirmed)
            .expect("payment of the confirmed sale");
        assert_eq!(confirmed_payment.kind, DocumentKind::SalePayment);
        assert_eq!(confirmed_payment.reference, "2024-SALE-000001");
        assert_eq!(confirmed_payment.party, "Pérez");
        assert_eq!(confirmed_payment.date, d(2024, 5, 10));
        assert_eq!(confirmed_payment.detail, "Pago");
        assert_eq!(confirmed_payment.amount, Some(dec("10")));
        assert_eq!(confirmed_payment.quantity, None);
        assert_eq!(confirmed_payment.created_by, actor);

        let draft_payment = rows
            .iter()
            .find(|r| r.owner_id == draft)
            .expect("payment of the draft");
        assert_eq!(draft_payment.reference, format!("Draft #{draft}"));
        assert_eq!(draft_payment.party, "Díaz");
    }

    /// The first read-by-id of one sale payment: found carries every stored
    /// column back, absent is `None` — never an error — and the whole read is
    /// ONE query (the drawer's per-payment read must stay that cheap).
    #[tokio::test]
    async fn find_payment_reads_one_payment_in_one_query() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, Some("2024-SALE-000001"), "Pérez", d(2024, 5, 2), actor).await;
        let payment_id = seed_payment(&pool, sale, "10", d(2024, 5, 10), actor, None).await;

        repo.reset_reads();
        let found = repo.find_payment(payment_id).await.unwrap().expect("payment exists");
        assert_eq!(found.id, payment_id);
        assert_eq!(found.sale_id, sale);
        assert_eq!(found.amount, dec("10"));
        assert_eq!(found.date, d(2024, 5, 10));
        assert_eq!(found.created_by, actor);
        assert_eq!(found.refund_transaction_id, None);
        assert_eq!(found.receipt_id, None);
        assert_eq!(repo.read_count(), 1);

        // An unknown id is `None`, and it still costs exactly one query.
        repo.reset_reads();
        assert!(repo.find_payment(999_999).await.unwrap().is_none());
        assert_eq!(repo.read_count(), 1);
    }

    /// The receipt allocations fold: one receipt's total is the exact sum of
    /// the payments grouped under it, and the whole read is one query.
    #[tokio::test]
    async fn receipt_allocations_sum_grouped_payments_in_one_read() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (account, method) = account_and_method(&pool, actor).await;
        let (walkin,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let receipt_a: i64 = sqlx::query_scalar(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, created_by)
               VALUES (?, ?, ?, '2024-06-01', ?) RETURNING id"#,
        )
        .bind(walkin)
        .bind(account)
        .bind(method)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        let receipt_b: i64 = sqlx::query_scalar(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, created_by)
               VALUES (?, ?, ?, '2024-06-02', ?) RETURNING id"#,
        )
        .bind(walkin)
        .bind(account)
        .bind(method)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        let sale = seed_sale(&pool, Some("2024-SALE-000001"), "Pérez", d(2024, 5, 2), actor).await;
        seed_payment(&pool, sale, "10", d(2024, 6, 1), actor, Some(receipt_a)).await;
        seed_payment(&pool, sale, "2.5", d(2024, 6, 1), actor, Some(receipt_a)).await;
        seed_payment(&pool, sale, "7", d(2024, 6, 2), actor, Some(receipt_b)).await;

        repo.reset_reads();
        let map = repo
            .receipt_allocations(&[receipt_a, receipt_b])
            .await
            .unwrap();
        assert_eq!(repo.read_count(), 1);
        assert_eq!(map.get(&receipt_a), Some(&dec("12.5")));
        assert_eq!(map.get(&receipt_b), Some(&dec("7")));

        // An empty id list answers an empty map without touching the database.
        repo.reset_reads();
        let map = repo.receipt_allocations(&[]).await.unwrap();
        assert!(map.is_empty());
        assert_eq!(repo.read_count(), 0);
    }

    // -- delete_draft (the documents drawer's draft delete) --------------------

    /// Unlike [`memory_pool`], this one carries the FULL current schema: the
    /// AC16 test replays migrations by hand against a bare pool, but the
    /// delete tests need today's tables (users, customers, payment methods)
    /// seeded exactly as production has them.
    async fn migrated_pool() -> SqlitePool {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// One sale with an EXPLICIT status, seeded through raw SQL: the delete
    /// tests must be able to pin a Confirmed row WITHOUT the service's guard,
    /// because the point is proving the SQL backstop (`WHERE status = 'Draft'`)
    /// is load-bearing on its own, not that the service refuses politely.
    async fn seed_sale_with_status(
        pool: &SqlitePool,
        status: &str,
        customer: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (walkin,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(pool)
                .await
                .unwrap();
        sqlx::query_scalar(
            r#"INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
               VALUES (?, 'Cash', ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(status)
        .bind(walkin)
        .bind(customer)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_delete_product(pool: &SqlitePool, actor: i64) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
               VALUES ('DEL-P', 'delete prod', 'Product', 'un', '10', 1, ?)
               RETURNING id"#,
        )
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn count(pool: &SqlitePool, sql: &'static str, id: i64) -> i64 {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A draft delete removes the draft and its lines (CASCADE) and NOTHING
    /// else: another draft seeded beside it keeps its row and its line.
    #[tokio::test]
    async fn delete_draft_deletes_a_draft_with_its_lines_and_leaves_others_alive() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let product = seed_delete_product(&pool, actor).await;

        let draft =
            seed_sale_with_status(&pool, "Draft", "Delete Buyer", d(2024, 5, 2), actor).await;
        for _ in 0..2 {
            sqlx::query(
                "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '1', '10')",
            )
            .bind(draft)
            .bind(product)
            .execute(&pool)
            .await
            .unwrap();
        }
        let other =
            seed_sale_with_status(&pool, "Draft", "Keep Buyer", d(2024, 5, 3), actor).await;
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '3', '10')",
        )
        .bind(other)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        assert!(repo.delete_draft(draft).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", draft).await,
            0,
            "the draft row must be gone"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?", draft).await,
            0,
            "the draft's lines must be gone with it"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", other).await,
            1,
            "the other document must survive"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?", other).await,
            1,
            "the other document's lines must survive"
        );
    }

    /// THE backstop proof: the repository is called DIRECTLY on a Confirmed
    /// sale — no service guard in the way — and still refuses, because the
    /// `WHERE status = 'Draft'` in the statement is what makes deleting a
    /// confirmed document impossible even if the service check were relaxed.
    #[tokio::test]
    async fn delete_draft_called_directly_on_a_confirmed_sale_returns_false_and_the_row_survives() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let product = seed_delete_product(&pool, actor).await;

        let confirmed = seed_sale_with_status(
            &pool,
            "Confirmed",
            "Confirmed Buyer",
            d(2024, 5, 2),
            actor,
        )
        .await;
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '1', '10')",
        )
        .bind(confirmed)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        assert!(!repo.delete_draft(confirmed).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", confirmed).await,
            1,
            "a confirmed sale must survive a direct repository delete attempt"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?", confirmed).await,
            1,
            "the confirmed sale's lines must survive too"
        );
    }

    #[tokio::test]
    async fn delete_draft_on_a_cancelled_sale_returns_false_and_the_row_survives() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let cancelled = seed_sale_with_status(
            &pool,
            "Cancelled",
            "Cancelled Buyer",
            d(2024, 5, 2),
            actor,
        )
        .await;

        assert!(!repo.delete_draft(cancelled).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", cancelled).await,
            1
        );
    }

    #[tokio::test]
    async fn delete_draft_on_an_unknown_id_returns_false() {
        let pool = migrated_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        assert!(!repo.delete_draft(999_999).await.unwrap());
    }
}
