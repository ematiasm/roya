use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    DocumentKind, DocumentQuery, DocumentRow, NewPurchase, PaymentType, Purchase, PurchaseLine,
    PurchaseListFilter, PurchasePayment, PurchaseStatus, UpdatePurchaseDraft,
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
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
    /// `actor` is the acting user's id the service resolved from its request;
    /// it becomes the row's `created_by` and nothing the request itself can
    /// supply names it.
    async fn create_purchase(&self, actor: i64, input: &NewPurchase) -> AppResult<Purchase>;
    async fn find_purchase(&self, id: i64) -> AppResult<Option<Purchase>>;
    async fn find_purchase_by_number(&self, number: &str) -> AppResult<Option<Purchase>>;
    async fn list_purchases(&self) -> AppResult<Vec<Purchase>>;
    /// The same rows narrowed by the list filter, inside the repository query so
    /// only matching documents have their lines and payments loaded. The supplier
    /// filter joins the suppliers table for the name the list shows.
    async fn list_purchases_filtered(
        &self,
        filter: &PurchaseListFilter,
    ) -> AppResult<Vec<Purchase>>;
    /// Update Draft header fields (service guarantees Draft status); the edit
    /// stamps `updated_by` with the acting user.
    async fn update_draft(&self, id: i64, actor: i64, patch: &UpdatePurchaseDraft) -> AppResult<Purchase>;
    /// Transition Draft -> Confirmed with assigned number; the confirming
    /// request is an edit of the document and stamps `updated_by`.
    async fn set_confirmed(&self, id: i64, actor: i64, purchase_number: &str) -> AppResult<Purchase>;
    /// Transition Draft/Confirmed -> Cancelled; the cancelling request stamps
    /// `updated_by`.
    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>) -> AppResult<Purchase>;
    /// Stamp a purchase's `updated_by`/`updated_at` after a line change: the
    /// line inherits the purchase's actor (no columns of its own), but the
    /// document was just edited and the edit is attributed to the request.
    /// The statement updates whatever id it is given, so the restriction to
    /// drafts lives in the caller — stated here rather than enforced in SQL,
    /// because a `WHERE status = 'Draft'` would turn a future misuse into a
    /// silent no-op instead of a visible edit on the wrong document.
    async fn touch_draft(&self, id: i64, actor: i64) -> AppResult<Purchase>;

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
    /// (`transaction_id`); NULL only for historical rows. The payment carries
    /// the acting user of the request that produced it (AC18).
    async fn create_payment(
        &self,
        actor: i64,
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
        actor: i64,
        payment_id: i64,
        refund_transaction_id: i64,
    ) -> AppResult<PurchasePayment>;
    async fn list_payments(&self, purchase_id: i64) -> AppResult<Vec<PurchasePayment>>;

    /// The PURCHASES family of the documents index: the stored purchase
    /// projected to the feed's facts, with its derived total summed in Rust
    /// over ONE batched lines read — `PurchaseLine::subtotal`, the same
    /// definition the purchase record page uses, never SQL `SUM` over a TEXT
    /// column.
    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>>;

    /// The PURCHASE-PAYMENTS family of the documents index: the payment joined
    /// to its purchase so the row names the document the way the operator does
    /// (number, or `Draft #id`) and shows the supplier's name.
    async fn list_payment_document_rows(&self, query: &DocumentQuery)
        -> AppResult<Vec<DocumentRow>>;
}

#[derive(Clone)]
pub struct SqlitePurchaseRepository {
    pub pool: SqlitePool,
    /// Test-only read counter: proves the filtered list reads scale with the
    /// result set, not the shop's history. Absent from production builds.
    #[cfg(test)]
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SqlitePurchaseRepository {
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
impl PurchaseRepository for SqlitePurchaseRepository {
    async fn create_purchase(&self, actor: i64, input: &NewPurchase) -> AppResult<Purchase> {
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
               (supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, created_by)
               VALUES (?, 'Draft', ?, ?, ?, ?, ?, ?)
               RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(input.supplier_id)
        .bind(input.payment_type.to_string())
        .bind(input.purchase_date)
        .bind(input.due_date)
        .bind(invoice)
        .bind(notes)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn find_purchase(&self, id: i64) -> AppResult<Option<Purchase>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM purchases WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_purchase))
    }

    async fn find_purchase_by_number(&self, number: &str) -> AppResult<Option<Purchase>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM purchases WHERE purchase_number = ?"#,
        )
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_purchase))
    }

    async fn list_purchases(&self) -> AppResult<Vec<Purchase>> {
        #[cfg(test)]
        self.tick();
        let rows = sqlx::query(
            r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM purchases ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_purchase).collect())
    }

    async fn list_purchases_filtered(
        &self,
        filter: &PurchaseListFilter,
    ) -> AppResult<Vec<Purchase>> {
        #[cfg(test)]
        self.tick();
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT p.id, p.purchase_number, p.supplier_id, p.status, p.payment_type, p.purchase_date, p.due_date, p.supplier_invoice_no, p.notes, p.cancel_reason, p.created_by, p.updated_by, p.created_at, p.updated_at, p.confirmed_at, p.cancelled_at FROM purchases p",
        );
        let has_filter = filter.status.is_some()
            || filter.supplier_ids.is_some()
            || filter.number.is_some()
            || filter.from.is_some()
            || filter.to.is_some();
        if has_filter {
            qb.push(" WHERE 1 = 1");
        }
        if let Some(status) = filter.status {
            qb.push(" AND p.status = ").push_bind(status.to_string());
        }
        if let Some(ids) = &filter.supplier_ids {
            if ids.is_empty() {
                // The party filter matched no supplier, so no document can match.
                return Ok(Vec::new());
            }
            qb.push(" AND p.supplier_id IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(number) = &filter.number {
            qb.push(" AND p.purchase_number IS NOT NULL AND LOWER(p.purchase_number) LIKE LOWER(")
                .push_bind(like_needle(number))
                .push(") ESCAPE '\\'");
        }
        if let Some(from) = filter.from {
            qb.push(" AND p.purchase_date >= ").push_bind(from);
        }
        if let Some(to) = filter.to {
            qb.push(" AND p.purchase_date <= ").push_bind(to);
        }
        qb.push(" ORDER BY p.id");
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(row_to_purchase).collect())
    }

    async fn update_draft(&self, id: i64, actor: i64, patch: &UpdatePurchaseDraft) -> AppResult<Purchase> {
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
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(supplier_id)
        .bind(payment_type.to_string())
        .bind(purchase_date)
        .bind(due_date)
        .bind(supplier_invoice_no)
        .bind(notes)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn set_confirmed(&self, id: i64, actor: i64, purchase_number: &str) -> AppResult<Purchase> {
        let row = sqlx::query(
            r#"UPDATE purchases
               SET purchase_number = ?, status = 'Confirmed',
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(purchase_number)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>) -> AppResult<Purchase> {
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
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(clean)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_purchase(row))
    }

    async fn touch_draft(&self, id: i64, actor: i64) -> AppResult<Purchase> {
        let row = sqlx::query(
            r#"UPDATE purchases
               SET updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(actor)
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
        #[cfg(test)]
        self.tick();
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
        actor: i64,
        purchase_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<PurchasePayment> {
        let row = sqlx::query(
            r#"INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, transaction_id, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at"#,
        )
        .bind(purchase_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .bind(transaction_id)
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
    ) -> AppResult<PurchasePayment> {
        let row = sqlx::query(
            r#"UPDATE purchase_payments
               SET refund_transaction_id = ?, updated_by = ?
               WHERE id = ?
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at"#,
        )
        .bind(refund_transaction_id)
        .bind(actor)
        .bind(payment_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_payment(row))
    }

    async fn list_payments(&self, purchase_id: i64) -> AppResult<Vec<PurchasePayment>> {
        #[cfg(test)]
        self.tick();
        let rows = sqlx::query(
            r#"SELECT id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at FROM purchase_payments WHERE purchase_id = ? ORDER BY id"#,
        )
        .bind(purchase_id)
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
        // Purchases are the only family with no frozen counterpart name: the
        // read-only suppliers JOIN resolves the name the row shows. One JOIN
        // per family read beats N per-row name lookups in the caller.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT p.id, p.purchase_number, p.supplier_id, p.status, p.payment_type, p.purchase_date, p.due_date, p.supplier_invoice_no, p.notes, p.cancel_reason, p.created_by, p.updated_by, p.created_at, p.updated_at, p.confirmed_at, p.cancelled_at, s.name AS supplier_name FROM purchases p JOIN suppliers s ON s.id = p.supplier_id",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND p.created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND p.purchase_date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND p.purchase_date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // The operator searches a purchase by its number, the supplier's
            // own invoice or the supplier's name; every nullable side is
            // COALESCEd so a NULL never drops the row from the OR chain.
            qb.push(" AND (LOWER(COALESCE(p.purchase_number, '')) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\' OR LOWER(COALESCE(p.supplier_invoice_no, '')) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\' OR LOWER(s.name) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\')");
        }
        // Newest first: date descending, then id as the stable tiebreak.
        qb.push(" ORDER BY p.purchase_date DESC, p.id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();
        let purchases: Vec<(Purchase, String)> = rows
            .into_iter()
            .map(|row| {
                let supplier_name: String = row.get("supplier_name");
                (row_to_purchase(row), supplier_name)
            })
            .collect();
        if purchases.is_empty() {
            return Ok(Vec::new());
        }

        // ONE batched lines read for the whole page: the query count stays
        // constant no matter how many documents matched. The total is folded
        // in Rust over `PurchaseLine::subtotal`, never with SQL SUM over TEXT.
        let ids: Vec<i64> = purchases.iter().map(|(p, _)| p.id).collect();
        let mut lines_qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, purchase_id, product_id, qty, unit_cost, created_at FROM purchase_lines WHERE purchase_id IN (",
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
            *totals.entry(line.purchase_id).or_insert_with(|| Decimal::ZERO) += line.subtotal();
        }

        Ok(purchases
            .into_iter()
            .map(|(purchase, supplier_name)| DocumentRow {
                kind: DocumentKind::Purchase,
                id: purchase.id,
                owner_id: purchase.id,
                reference: purchase
                    .purchase_number
                    .clone()
                    .unwrap_or_else(|| format!("Draft #{}", purchase.id)),
                party: supplier_name,
                date: purchase.purchase_date,
                detail: purchase.status.to_string(),
                amount: Some(totals.remove(&purchase.id).unwrap_or_default()),
                quantity: None,
                created_by: purchase.created_by,
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
        // The JOINs are the whole point of this family read: the payment names
        // its purchase (number or Draft #id) and the supplier, in the same
        // single query that reads the payment.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT pp.id, pp.purchase_id, pp.amount, pp.date, pp.created_by, p.purchase_number, p.supplier_invoice_no, s.name AS supplier_name FROM purchase_payments pp JOIN purchases p ON p.id = pp.purchase_id JOIN suppliers s ON s.id = p.supplier_id",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND pp.created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND pp.date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND pp.date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // The operator searches a payment by the purchase it belongs to.
            qb.push(" AND (LOWER(COALESCE(p.purchase_number, '')) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\' OR LOWER(COALESCE(p.supplier_invoice_no, '')) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\' OR LOWER(s.name) LIKE LOWER(")
                .push_bind(like_needle(search))
                .push(") ESCAPE '\\')");
        }
        qb.push(" ORDER BY pp.date DESC, pp.id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        Ok(rows
            .into_iter()
            .map(|row| {
                let amount_str: String = row.get("amount");
                let purchase_id: i64 = row.get("purchase_id");
                let purchase_number: Option<String> = row.get("purchase_number");
                DocumentRow {
                    kind: DocumentKind::PurchasePayment,
                    id: row.get("id"),
                    owner_id: purchase_id,
                    reference: purchase_number.unwrap_or_else(|| format!("Draft #{purchase_id}")),
                    party: row.get("supplier_name"),
                    date: row.get("date"),
                    detail: "Pago".to_string(),
                    amount: Some(parse_decimal(&amount_str)),
                    quantity: None,
                    created_by: row.get("created_by"),
                }
            })
            .collect())
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
            // Same posture as db::create_pool so the walk-in backstops fire
            // exactly as they do in production.
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
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

    async fn seed_supplier(pool: &SqlitePool, name: &str, actor: i64) -> i64 {
        // The supplier name is UNIQUE, so a repeated seed in one database
        // (the bulk read-count test uses one supplier for 20 documents)
        // reuses the existing row.
        match sqlx::query_scalar("SELECT id FROM suppliers WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO suppliers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
                )
                .bind(name)
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap()
            }
        }
    }

    /// One confirmed purchase through raw SQL (the projection tests seed shapes
    /// the repository API cannot build: arbitrary numbers, dates and actors).
    async fn seed_purchase(
        pool: &SqlitePool,
        number: Option<&str>,
        supplier: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let supplier_id = seed_supplier(pool, supplier, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO purchases (purchase_number, supplier_id, status, payment_type, purchase_date, created_by)
               VALUES (?, ?, 'Confirmed', 'Cash', ?, ?)
               RETURNING id"#,
        )
        .bind(number)
        .bind(supplier_id)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_line(pool: &SqlitePool, purchase_id: i64, qty: &str, cost: &str) {
        let (actor,): (i64,) = sqlx::query_as("SELECT created_by FROM purchases WHERE id = ?")
            .bind(purchase_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let product = product_id(pool, actor).await;
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, ?, ?)",
        )
        .bind(purchase_id)
        .bind(product)
        .bind(qty)
        .bind(cost)
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
        purchase_id: i64,
        amount: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, created_by)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(purchase_id)
        .bind(account)
        .bind(method)
        .bind(amount)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The projection: number (or Draft #id), resolved supplier name, status
    /// pill, owner id, the exact Rust-summed line total, no quantity, actor.
    #[tokio::test]
    async fn purchase_document_rows_project_the_feed_facts() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();

        let confirmed =
            seed_purchase(&pool, Some("2024-PURCH-000001"), "Distribuidora Sur", d(2024, 5, 2), actor)
                .await;
        seed_line(&pool, confirmed, "2", "10").await;
        seed_line(&pool, confirmed, "3", "2.5").await; // Σ = 20 + 7.5 = 27.5
        let draft = seed_purchase(&pool, None, "Importadora Norte", d(2024, 5, 3), actor).await;
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
            .expect("confirmed purchase row");
        assert_eq!(confirmed_row.kind, DocumentKind::Purchase);
        assert_eq!(confirmed_row.reference, "2024-PURCH-000001");
        assert_eq!(confirmed_row.party, "Distribuidora Sur");
        assert_eq!(confirmed_row.date, d(2024, 5, 2));
        assert_eq!(confirmed_row.detail, "Confirmed");
        assert_eq!(confirmed_row.owner_id, confirmed);
        assert_eq!(confirmed_row.amount, Some(dec("27.5")));
        assert_eq!(confirmed_row.quantity, None);
        assert_eq!(confirmed_row.created_by, actor);

        let draft_row = rows.iter().find(|r| r.id == draft).expect("draft row");
        assert_eq!(draft_row.reference, format!("Draft #{draft}"));
        assert_eq!(draft_row.party, "Importadora Norte");
    }

    /// The audit-actor filter narrows the family read to the given ids, and
    /// `Some(empty)` matches nothing.
    #[tokio::test]
    async fn purchase_document_rows_filter_by_actor_and_empty_actor_set() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let sistema = test_support::audit_actor_id(&pool).await.unwrap();
        let other = test_support::seed_audit_user(&pool, "doc-actor-2", "Doc Actor 2")
            .await
            .unwrap();
        assert_ne!(sistema, other);

        let mine = seed_purchase(&pool, Some("2024-PURCH-000001"), "Sur", d(2024, 5, 2), sistema).await;
        let theirs =
            seed_purchase(&pool, Some("2024-PURCH-000002"), "Norte", d(2024, 5, 3), other).await;

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
    async fn purchase_document_rows_date_range_is_inclusive() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let early = seed_purchase(&pool, Some("2024-PURCH-000001"), "A", d(2024, 5, 1), actor).await;
        let first = seed_purchase(&pool, Some("2024-PURCH-000002"), "B", d(2024, 5, 2), actor).await;
        let last = seed_purchase(&pool, Some("2024-PURCH-000003"), "C", d(2024, 5, 4), actor).await;
        let late = seed_purchase(&pool, Some("2024-PURCH-000004"), "D", d(2024, 5, 5), actor).await;

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

    /// The search matches number, supplier invoice and supplier name partially
    /// and case-insensitively; matching nothing is empty, never an error.
    #[tokio::test]
    async fn purchase_document_rows_search_number_invoice_and_supplier() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let with_invoice =
            seed_purchase(&pool, Some("2024-PURCH-000001"), "Distribuidora Sur", d(2024, 5, 2), actor)
                .await;
        sqlx::query("UPDATE purchases SET supplier_invoice_no = 'FACT-77' WHERE id = ?")
            .bind(with_invoice)
            .execute(&pool)
            .await
            .unwrap();
        seed_purchase(&pool, Some("2024-PURCH-000002"), "Importadora Norte", d(2024, 5, 3), actor)
            .await;

        // Partial, case-insensitive number match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("purch-000002".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "2024-PURCH-000002");

        // Supplier name match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("norte".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].party, "Importadora Norte");

        // Supplier invoice match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("fact-77".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reference, "2024-PURCH-000001");

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
    async fn purchase_document_rows_limit_returns_newest_first() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let a = seed_purchase(&pool, Some("2024-PURCH-000001"), "A", d(2024, 5, 1), actor).await;
        let b = seed_purchase(&pool, Some("2024-PURCH-000002"), "B", d(2024, 5, 2), actor).await;
        let c = seed_purchase(&pool, Some("2024-PURCH-000003"), "C", d(2024, 5, 2), actor).await;

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
    async fn purchase_document_rows_read_count_stays_bounded() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        for i in 1..=20 {
            let id = seed_purchase(
                &pool,
                Some(&format!("2024-PURCH-{i:06}")),
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

    /// The payment projection names the purchase the way the operator does and
    /// carries the supplier name; one query total.
    #[tokio::test]
    async fn purchase_payment_document_rows_project_the_feed_facts() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();

        let confirmed =
            seed_purchase(&pool, Some("2024-PURCH-000001"), "Distribuidora Sur", d(2024, 5, 2), actor)
                .await;
        let draft = seed_purchase(&pool, None, "Importadora Norte", d(2024, 5, 3), actor).await;
        seed_payment(&pool, confirmed, "10", d(2024, 5, 10), actor).await;
        seed_payment(&pool, draft, "5", d(2024, 5, 11), actor).await;

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
            .expect("payment of the confirmed purchase");
        assert_eq!(confirmed_payment.kind, DocumentKind::PurchasePayment);
        assert_eq!(confirmed_payment.reference, "2024-PURCH-000001");
        assert_eq!(confirmed_payment.party, "Distribuidora Sur");
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
        assert_eq!(draft_payment.party, "Importadora Norte");
    }
}
