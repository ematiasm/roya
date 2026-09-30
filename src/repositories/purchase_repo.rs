use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    DocumentKind, DocumentQuery, DocumentRow, NewLineTax, NewPurchase, PaymentType, PriceRefusal,
    Purchase, PurchaseLine, PurchaseListFilter, PurchasePayment, PurchaseStatus,
    UpdatePurchaseDraft,
};
use crate::repositories::tax_repo::active_taxes_for_product;
use crate::repositories::tax_snapshot_repo::replace_purchase_line_taxes;
use crate::services::line_taxes::{calculate_line_taxes, line_net_amount, tax_inclusive_total};

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
    let tax_str: String = row.get("tax_total");
    PurchaseLine {
        id: row.get("id"),
        purchase_id: row.get("purchase_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&qty_str),
        unit_cost: parse_decimal(&cost_str),
        tax_total: parse_decimal(&tax_str),
        created_at: row.get("created_at"),
    }
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> PurchasePayment {
    let amt_str: String = row.get("amount");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
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
        created_at,
        updated_at,
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

/// One purchase by id, over whichever connection the caller offers.
///
/// `set_confirmed` reads its own row back after the UPDATE, so this statement
/// runs on two executors the moment a transaction-joining form of that write
/// exists: the caller's `&mut SqliteConnection` when a unit holds the confirm,
/// and the pool when the public wrapper owns the unit. ONE copy of the SQL,
/// generic over the executor — the same shape and the same reason as
/// `transaction_repo::balance_for_account_raw`. Two copies would be able to
/// drift on the projection, and a drift here is the worst kind: the read-back
/// would answer about a different row shape than the write it is confirming.
async fn find_purchase_raw<'e, E>(executor: E, id: i64) -> AppResult<Option<Purchase>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        r#"SELECT id, purchase_number, supplier_id, status, payment_type, purchase_date, due_date, supplier_invoice_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM purchases WHERE id = ?"#,
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(row_to_purchase))
}

#[async_trait]
pub trait PurchaseRepository: Send + Sync {
    /// `actor` is the acting user's id the service resolved from its request;
    /// it becomes the row's `created_by` and nothing the request itself can
    /// supply names it.
    async fn create_purchase(&self, actor: i64, input: &NewPurchase) -> AppResult<Purchase>;
    async fn find_purchase(&self, id: i64) -> AppResult<Option<Purchase>>;
    async fn find_purchase_by_number(&self, number: &str) -> AppResult<Option<Purchase>>;
    /// The supplier of the most recently created purchase (T3's creation
    /// dialog default), or None when no purchase exists yet — an empty
    /// database has no default, and the dialog must open empty rather than
    /// guess. `id DESC` matches the repo's id-ordered conventions.
    async fn last_used_supplier_id(&self) -> AppResult<Option<i64>>;
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
    async fn update_draft(
        &self,
        id: i64,
        actor: i64,
        patch: &UpdatePurchaseDraft,
    ) -> AppResult<Purchase>;
    /// Transition Draft -> Confirmed with assigned number; the confirming
    /// request is an edit of the document and stamps `updated_by`.
    ///
    /// The Draft precondition is this statement's own WHERE, not the service's
    /// up-front read: a document that is not a Draft is refused with an
    /// [`AppError::Validation`] naming the state it was found in. So a duplicate
    /// submission of an already-Confirmed purchase cannot stamp a second number,
    /// even if the caller's status check were relaxed or raced.
    ///
    /// It does NOT make a failed confirmation retryable. A confirm writes on
    /// several autocommit connections — and on this side `record_cost` runs
    /// AFTER this write, so a failure there leaves a Confirmed purchase with
    /// only some of its costs recorded — and a retry re-passes this predicate
    /// and writes again. Only a shared transaction removes that.
    async fn set_confirmed(
        &self,
        id: i64,
        actor: i64,
        purchase_number: &str,
    ) -> AppResult<Purchase>;

    /// [`Self::set_confirmed`] inside a transaction the CALLER owns.
    ///
    /// This is the write that decides whether a document exists as far as the
    /// shop is concerned, and the last of the five `confirm` performs
    /// (`src/services/purchases.rs:1134-1192`). Everything before it — the
    /// sequence number, a stock movement per tracked line, the `Expense`, the
    /// `purchase_payments` row — is already committed by the time it runs, which
    /// is precisely why a unit that does not include this statement cannot be
    /// atomic.
    ///
    /// The DRAFT predicate is unchanged and is still this statement's own
    /// `WHERE`: a non-Draft matches nothing and the refusal is read back
    /// through the SAME connection, so it names the state the write saw. That
    /// read-back is the trap in this file, and it is called out on
    /// [`SqlitePurchaseRepository::refuse_confirm`] — the helper's executor is
    /// part of what moves, not an implementation detail of it.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn set_confirmed_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        actor: i64,
        purchase_number: &str,
    ) -> AppResult<Purchase>;
    /// Transition Draft/Confirmed -> Cancelled; the cancelling request stamps
    /// `updated_by`.
    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>)
        -> AppResult<Purchase>;
    /// Stamp a purchase's `updated_by`/`updated_at` after a line change: the
    /// line inherits the purchase's actor (no columns of its own), but the
    /// document was just edited and the edit is attributed to the request.
    /// The statement updates whatever id it is given, so the restriction to
    /// drafts lives in the caller — stated here rather than enforced in SQL,
    /// because a `WHERE status = 'Draft'` would turn a future misuse into a
    /// silent no-op instead of a visible edit on the wrong document.
    async fn touch_draft(&self, id: i64, actor: i64) -> AppResult<Purchase>;

    /// Create a line on a DRAFT purchase together with the frozen tax breakdown
    /// and the tax total that summarizes it, in ONE transaction — and like the
    /// sales mirror it IS the tax-aware contract, not a tax-free shortcut
    /// beside one. The DRAFT requirement is in the INSERT's own `WHERE`, the
    /// product's active taxes are resolved inside that same transaction, and
    /// the calculation is the shared contract, so the purchase path cannot
    /// drift from the sale path. `NotFound` for a missing purchase, `Conflict`
    /// for one that is not a Draft.
    async fn create_line(
        &self,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine>;
    async fn find_line(&self, id: i64) -> AppResult<Option<PurchaseLine>>;
    async fn list_lines(&self, purchase_id: i64) -> AppResult<Vec<PurchaseLine>>;
    /// Edit a DRAFT purchase line: new quantity, new cost, the breakdown
    /// REPLACED by the taxes that are active right now, and the tax total
    /// recomputed from all three — atomically, in the same single transaction as
    /// the create. The DRAFT predicate is in the UPDATE's own `WHERE` and the
    /// breakdown is only touched after it matched, so a Confirmed purchase's
    /// line, aggregate and snapshots are provably untouched by a refused call.
    async fn update_line(
        &self,
        id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine>;
    /// Remove a DRAFT purchase line, and with it the tax breakdown the line
    /// CASCADEs. The purchase-line mirror of `SaleRepository::delete_line`:
    /// statement-level DRAFT predicate, `NotFound` for a missing line,
    /// `Conflict` for one whose purchase is no longer a Draft, and a confirmed
    /// purchase's frozen breakdown provably untouched.
    async fn delete_line(&self, id: i64) -> AppResult<()>;

    /// Delete a DRAFT purchase — or a DISCARDED one (Cancelled while never
    /// confirmed: `purchase_number IS NULL`) — and let its lines die by
    /// CASCADE. The predicate in the WHERE is the load-bearing backstop: even
    /// if a caller ever relaxed the service's state guard, a Confirmed row or
    /// a Cancelled row that carries a number cannot be removed by this
    /// statement — it answers `false` instead, so the caller can refuse
    /// honestly. Both deletable states posted nothing by construction (no
    /// payments — nothing but a Confirmed document takes them — no stock
    /// movement, no ledger entry, no supplier debt), so nothing dangles; a
    /// confirmed-then-cancelled document is permanent audit trail and stays
    /// protected. This is a WRITE, not a read: the test read counter
    /// stays untouched.
    async fn delete_draft(&self, id: i64) -> AppResult<bool>;

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

    /// [`Self::create_payment`] inside a transaction the CALLER owns, and the
    /// reason this write belongs to the confirm unit is sharper than for most.
    ///
    /// The `purchase_payments` row is written FOURTH of five, after the
    /// `Expense` it records, and it is the row the paid/unpaid state of the
    /// document is derived from. A confirm that dies between them leaves an
    /// `Expense` with no payment naming it; a confirm that dies AFTER them
    /// leaves a `Draft` that has been paid — measured by
    /// `purchase_confirm_failure_on_set_confirmed_leaves_a_paid_draft`
    /// (`src/services/purchases.rs:5811`), whose own assertion reads *"the
    /// payment was committed: the supplier was paid and no document says so"*.
    /// Neither residue is reachable from a WHERE clause; only the shared
    /// transaction removes it.
    ///
    /// Unlike `set_confirmed_in` this write has no read-back and no refusal
    /// helper, which makes it the straight case: the statement moves to the
    /// caller's executor and `map_db_err` moves with it, unchanged.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn create_payment_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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
    /// One payment by id — the documents drawer's per-payment read. `None`
    /// for an id that does not exist; the service decides what that means.
    async fn find_payment(&self, id: i64) -> AppResult<Option<PurchasePayment>>;

    /// The PURCHASES family of the documents index: the stored purchase
    /// projected to the feed's facts, with its derived total summed in Rust
    /// over ONE batched lines read — `PurchaseLine::subtotal`, the same
    /// definition the purchase record page uses, never SQL `SUM` over a TEXT
    /// column.
    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>>;

    /// The PURCHASE-PAYMENTS family of the documents index: the payment joined
    /// to its purchase so the row names the document the way the operator does
    /// (number, or `Draft #id`) and shows the supplier's name.
    async fn list_payment_document_rows(
        &self,
        query: &DocumentQuery,
    ) -> AppResult<Vec<DocumentRow>>;
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
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    /// The ONE implementation behind `PurchaseRepository::create_line`: insert
    /// the line, snapshot its resolved taxes and store the aggregate that
    /// summarizes them, in a single transaction. Private on purpose — the public
    /// trait method is the contract, so a caller cannot reach a tax-aware path
    /// that behaves differently from the ordinary one.
    async fn write_line_with_taxes(
        &self,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine> {
        let mut tx = self.pool.begin().await?;

        // Resolve, calculate and write inside one transaction: the breakdown and
        // the aggregate that summarizes it can never be committed apart.
        // Resolved through this transaction, like the creation path.
        let taxes = active_taxes_for_product(&mut tx, product_id).await?;
        let calc = line_net_amount(qty, unit_cost)
            .and_then(|net| calculate_line_taxes(net, &taxes))
            .map_err(AppError::PriceRefused)?;

        // The DRAFT predicate is the statement's own, so a Confirmed purchase
        // cannot gain a line even if a caller skipped the service's guard.
        let row = sqlx::query(
            r#"INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost, tax_total)
               SELECT ?, ?, ?, ?, ?
               WHERE EXISTS (SELECT 1 FROM purchases WHERE id = ? AND status = 'Draft')
               RETURNING id, purchase_id, product_id, qty, unit_cost, tax_total, created_at"#,
        )
        .bind(purchase_id)
        .bind(product_id)
        .bind(qty.to_string())
        .bind(unit_cost.to_string())
        .bind(calc.tax_total.to_string())
        .bind(purchase_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, purchase_id).await),
        };

        let taxes: Vec<NewLineTax> = calc.taxes.iter().map(NewLineTax::from).collect();
        replace_purchase_line_taxes(&mut tx, row.get("id"), &taxes).await?;
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    /// The ONE implementation behind `PurchaseRepository::update_line`: guarded
    /// draft edit, breakdown replaced, aggregate recomputed, one transaction.
    async fn rewrite_draft_line_taxes(
        &self,
        id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine> {
        let mut tx = self.pool.begin().await?;

        let current: Option<(i64, i64)> =
            sqlx::query_as("SELECT product_id, purchase_id FROM purchase_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let (product_id, purchase_id) = match current {
            Some(current) => current,
            None => return Err(AppError::NotFound(format!("purchase line {id} not found"))),
        };

        // Resolved through this transaction, like the creation path.
        let taxes = active_taxes_for_product(&mut tx, product_id).await?;
        let calc = line_net_amount(qty, unit_cost)
            .and_then(|net| calculate_line_taxes(net, &taxes))
            .map_err(AppError::PriceRefused)?;

        // Statement-level DRAFT predicate again: when it matches nothing the
        // call is refused BEFORE any snapshot is deleted or inserted.
        let row = sqlx::query(
            r#"UPDATE purchase_lines SET qty = ?, unit_cost = ?, tax_total = ?
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM purchases p WHERE p.id = purchase_lines.purchase_id AND p.status = 'Draft')
               RETURNING id, purchase_id, product_id, qty, unit_cost, tax_total, created_at"#,
        )
        .bind(qty.to_string())
        .bind(unit_cost.to_string())
        .bind(calc.tax_total.to_string())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, purchase_id).await),
        };

        let taxes: Vec<NewLineTax> = calc.taxes.iter().map(NewLineTax::from).collect();
        replace_purchase_line_taxes(&mut tx, id, &taxes).await?;
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    /// Why a tax-aware line write matched no row: either the purchase does not
    /// exist, or it exists and is no longer a Draft. Read through the CALLER'S
    /// transaction, so the refusal describes the same state the write saw, and
    /// the two cases stay distinguishable.
    async fn refuse_line(conn: &mut SqliteConnection, purchase_id: i64) -> AppError {
        let status = sqlx::query_scalar::<_, String>("SELECT status FROM purchases WHERE id = ?")
            .bind(purchase_id)
            .fetch_optional(&mut *conn)
            .await;
        match status {
            Ok(None) => AppError::NotFound(format!("purchase {purchase_id} not found")),
            Ok(Some(status)) => AppError::Conflict(format!(
                "purchase {purchase_id} is {status}: its lines and their taxes are frozen"
            )),
            Err(error) => AppError::Database(error),
        }
    }

    /// Why a confirm write matched no row: either the purchase does not exist,
    /// or it exists and is no longer a Draft. Read back so the refusal names the
    /// state the document actually rests in, and so the two cases stay
    /// distinguishable — a missing document is a 404, a frozen one is a
    /// validation the caller can explain. Same contract as
    /// [`SqlitePurchaseRepository::refuse_line`], and the same executor for the
    /// same reason: the refusal is read through the CALLER'S transaction, so it
    /// describes the same state the write saw.
    ///
    /// This signature used to be hard-coded to `&SqlitePool`, written when
    /// `set_confirmed` ran on autocommit, and it is the trap in this file. The
    /// refusal is reached ONLY when the DRAFT predicate matched no row, so a
    /// migration that moves the UPDATE and forgets this helper passes every
    /// happy-path test and fails only on refusal — where it would stall for
    /// sqlx's 30s acquire timeout inside the caller's transaction and answer
    /// `PoolTimedOut` instead of a `Validation` the operator is waiting for.
    /// The mapping below is unchanged: `NotFound`, `Validation`, `Database`.
    async fn refuse_confirm(conn: &mut SqliteConnection, id: i64) -> AppError {
        match sqlx::query_scalar::<_, String>("SELECT status FROM purchases WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
        {
            Ok(None) => AppError::NotFound(format!("purchase {id} not found")),
            Ok(Some(status)) => AppError::Validation(format!(
                "purchase {id} is {status}: only a Draft purchase can be confirmed"
            )),
            Err(error) => AppError::Database(error),
        }
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
        // Unchanged in every observable way: same SQL, same bind, same
        // projection, same pool. The statement moved into `find_purchase_raw`
        // only because `set_confirmed_in` now has to run this SAME query on a
        // caller's connection, and one copy of a statement is the rule. No read
        // counter lives here — `find_purchase` never carried a `tick()`, so
        // nothing about this refactor has to decide where one goes.
        find_purchase_raw(&self.pool, id).await
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

    async fn last_used_supplier_id(&self) -> AppResult<Option<i64>> {
        #[cfg(test)]
        self.tick();
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT supplier_id FROM purchases ORDER BY id DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
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

    async fn update_draft(
        &self,
        id: i64,
        actor: i64,
        patch: &UpdatePurchaseDraft,
    ) -> AppResult<Purchase> {
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

    async fn set_confirmed(
        &self,
        id: i64,
        actor: i64,
        purchase_number: &str,
    ) -> AppResult<Purchase> {
        let mut tx = self.pool.begin().await?;
        let confirmed = self
            .set_confirmed_in(&mut tx, id, actor, purchase_number)
            .await?;
        tx.commit().await?;
        Ok(confirmed)
    }

    async fn set_confirmed_in(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
        actor: i64,
        purchase_number: &str,
    ) -> AppResult<Purchase> {
        // The DRAFT predicate is this statement's own WHERE, the same backstop
        // `delete_line` and `delete_draft` already carry. A document that is
        // not a Draft matches nothing, and a zero-row match is a refusal that
        // names the state the statement saw — never a `RowNotFound` for
        // `map_db_err` to guess about.
        //
        // WHAT IT REFUSES: a duplicate submission of a document that has already
        // been confirmed (or cancelled). `confirm` reads the status once, up
        // front, and that read is not a lock — between it and this write another
        // writer can confirm the same document, and without the predicate this
        // statement would stamp a second number over the first and report a
        // success the caller must never be told about.
        //
        // WHAT IT DOES NOT DO: it does not make a failed confirmation safe to
        // retry. `confirm` writes the sequence, the stock movements, the finance
        // row and the payment on separate autocommit connections — and then
        // runs `record_cost` for every line in a loop AFTER this write, so a
        // failure in that loop leaves a Confirmed, fully paid purchase with only
        // some of its supplier costs recorded. None of that is reachable from a
        // WHERE clause; only the shared transaction — one unit per user action,
        // the way Odoo holds a cursor for the whole request — removes it.
        let res = sqlx::query(
            r#"UPDATE purchases
               SET purchase_number = ?, status = 'Confirmed',
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
                 AND status = 'Draft'"#,
        )
        .bind(purchase_number)
        .bind(actor)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_confirm(&mut *tx, id).await);
        }
        // The row the UPDATE just wrote, read back through the SAME statement
        // `find_purchase` runs and on the SAME connection — not the pool. This is
        // load-bearing rather than tidy: a read-back that reached for the pool
        // here would not be merely slow, it would be unable to answer at all
        // while the caller holds the only connection, and the error it returned
        // (`PoolTimedOut`) would be a driver failure standing in for a document
        // that had in fact just been confirmed correctly. It cannot return
        // `None` in practice — a just-confirmed row is not deletable — but the
        // branch is written out rather than unwrapped so a future caller never
        // sees a panic from a repository method.
        find_purchase_raw(&mut *tx, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {id} not found")))
    }

    async fn set_cancelled(
        &self,
        id: i64,
        actor: i64,
        reason: Option<&str>,
    ) -> AppResult<Purchase> {
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
        // The public contract and the tax-aware write are the same operation.
        self.write_line_with_taxes(purchase_id, product_id, qty, unit_cost)
            .await
    }

    async fn find_line(&self, id: i64) -> AppResult<Option<PurchaseLine>> {
        let row = sqlx::query(
            r#"SELECT id, purchase_id, product_id, qty, unit_cost, tax_total, created_at
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
            r#"SELECT id, purchase_id, product_id, qty, unit_cost, tax_total, created_at
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
        // Same single contract on the edit path: the breakdown is replaced and
        // the aggregate recomputed, never left behind a stale total.
        self.rewrite_draft_line_taxes(id, qty, unit_cost).await
    }

    async fn delete_line(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;

        let purchase_id: Option<i64> =
            sqlx::query_scalar("SELECT purchase_id FROM purchase_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let purchase_id = match purchase_id {
            Some(purchase_id) => purchase_id,
            None => return Err(AppError::NotFound(format!("purchase line {id} not found"))),
        };

        // Statement-level DRAFT predicate: a closed purchase's line cannot be
        // removed, and removing it would CASCADE away its immutable breakdown.
        let res = sqlx::query(
            r#"DELETE FROM purchase_lines
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM purchases p WHERE p.id = purchase_lines.purchase_id AND p.status = 'Draft')"#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_line(&mut tx, purchase_id).await);
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_draft(&self, id: i64) -> AppResult<bool> {
        // The WHERE clause is the backstop that makes deleting an
        // undeletable document impossible even if the service check were
        // relaxed: the statement simply matches nothing and the answer is
        // `false`. Deletable = a Draft, OR a Cancelled row whose
        // `purchase_number` is NULL. A Cancelled row WITH a number was confirmed
        // first and is permanent audit trail (return movements and refund
        // transactions reference it). That much the predicate decides, and it
        // decides it soundly.
        //
        // WHAT THE PREDICATE DOES NOT ESTABLISH is that the row is clean. This
        // statement used to claim a discarded purchase "never touched stock or
        // finance", and that is false, measured rather than inferred. `confirm`
        // writes, in order: the sequence number, a movement per tracked line, the
        // `Expense`, the `purchase_payments` row, and `set_confirmed` LAST
        // (`src/services/purchases.rs:1134-1192`). The purchase row is only
        // stamped at the very end, so every earlier write is already committed
        // while the document still reads `("Draft", NULL)`.
        //
        // The window that matters is a failure AT `set_confirmed`, which leaves a
        // Draft carrying a committed `purchase_payments` row and an orphan
        // `Expense` whose `reference` is the burned number. The characterization
        // test is `purchase_confirm_failure_on_set_confirmed_leaves_a_paid_draft`
        // (`src/services/purchases.rs:5811`), and its own assertion is the
        // sentence this comment used to contradict: "the payment was committed:
        // the supplier was paid and no document says so".
        //
        // Such a Draft MATCHES the first branch. `purchase_payments.purchase_id`
        // CASCADEs from `purchases`
        // (`migrations/20240101000017_create_purchase_payments.sql:8`), so the
        // delete takes the payment row with it — while the `Expense` has no
        // foreign key to `purchases` at all and survives, pointing at a number no
        // document carries any more. The delete removes the document and keeps
        // the money.
        //
        // Discarding such a residue row does not dodge this. The Draft ->
        // Cancelled step is explicitly a no-op for stock, finance and the cost
        // satellite (`src/services/purchases.rs:1399-1406`), and it does not
        // assign a number, so the row arrives at the SECOND branch still
        // carrying its payment and its `Expense`. The hazard is not confined to
        // Draft. Sales carries the same statement and the same hazard
        // (`src/repositories/sale_repo.rs`).
        //
        // Purchases have one residue window sales does not: `record_cost` runs in
        // a loop AFTER `set_confirmed` succeeded, so a failure there lands on a
        // Confirmed, numbered, fully paid purchase with only some of its supplier
        // costs recorded. That one is a different WHERE branch — the row is not
        // deletable at all — and the `set_confirmed` doc above already states it.
        //
        // So the guard is not this WHERE and never was. Status is not a
        // cleanliness proof: before treating a Draft as safe to delete, check for
        // payments, movements or an `Expense`. This method deliberately keeps
        // answering from the predicate alone, because changing that answer is a
        // behaviour decision with its own test and its own migration story — not
        // a comment's business, and not this commit's.
        let res = sqlx::query(
            r#"DELETE FROM purchases
               WHERE id = ?
                 AND (status = 'Draft'
                      OR (status = 'Cancelled' AND purchase_number IS NULL))"#,
        )
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
        let mut tx = self.pool.begin().await?;
        let payment = self
            .create_payment_in(
                &mut tx,
                actor,
                purchase_id,
                account_id,
                method_id,
                amount,
                date,
                transaction_id,
            )
            .await?;
        tx.commit().await?;
        Ok(payment)
    }

    async fn create_payment_in(
        &self,
        tx: &mut SqliteConnection,
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
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(purchase_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .bind(transaction_id)
        .bind(actor)
        .fetch_one(&mut *tx)
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
               SET refund_transaction_id = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at, updated_at"#,
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
            r#"SELECT id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at, updated_at FROM purchase_payments WHERE purchase_id = ? ORDER BY id"#,
        )
        .bind(purchase_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_payment).collect())
    }

    async fn find_payment(&self, id: i64) -> AppResult<Option<PurchasePayment>> {
        #[cfg(test)]
        self.tick();
        let row = sqlx::query(
            r#"SELECT id, purchase_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, created_by, updated_by, created_at, updated_at FROM purchase_payments WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_payment))
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
        // in Rust with the shared tax-inclusive rule, never with SQL SUM over
        // TEXT: the stored `tax_total` is part of what a document costs.
        let ids: Vec<i64> = purchases.iter().map(|(p, _)| p.id).collect();
        let mut lines_qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, purchase_id, product_id, qty, unit_cost, tax_total, created_at FROM purchase_lines WHERE purchase_id IN (",
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

        // The per-document fold is CHECKED, and a document it cannot carry is
        // recorded as a refusal ON ITS ROW rather than as an error for the read.
        //
        // That is the difference between this surface and the record page: the
        // index holds every document in the shop, so one that cannot be added up
        // must not take the rest of the page down with it. The row keeps its
        // place, states the rule, and shows no amount — publishing a number the
        // arithmetic could not produce is the one thing this work unit exists to
        // prevent. Every other family on this page is untouched.
        let mut totals: std::collections::BTreeMap<i64, Decimal> =
            std::collections::BTreeMap::new();
        let mut refused: std::collections::BTreeMap<i64, PriceRefusal> =
            std::collections::BTreeMap::new();
        for row in line_rows {
            let line = row_to_line(row);
            let running = totals.entry(line.purchase_id).or_default();
            match running.checked_add(tax_inclusive_total(line.subtotal(), line.tax_total)) {
                Some(sum) => *running = sum,
                // The document keeps folding — a later line of the same document
                // cannot make the sum carryable — and the row will show no amount.
                None => {
                    refused.insert(line.purchase_id, PriceRefusal::DocumentTotalTooLarge);
                }
            }
        }

        Ok(purchases
            .into_iter()
            .map(|(purchase, supplier_name)| {
                let total_refusal = refused.remove(&purchase.id);
                DocumentRow {
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
                    amount: if total_refusal.is_some() {
                        None
                    } else {
                        Some(totals.remove(&purchase.id).unwrap_or_default())
                    },
                    total_refusal,
                    quantity: None,
                    created_by: purchase.created_by,
                }
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
                    // A payment row carries its OWN stored amount, never a sum of
                    // a document's lines, so there is no document total here to
                    // refuse.
                    total_refusal: None,
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
    use std::time::{Duration, Instant};

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
            None => sqlx::query_scalar(
                r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                       VALUES ('DOC-P', 'doc prod', 'Product', 'un', '10', 1, ?)
                       RETURNING id"#,
            )
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
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
            None => sqlx::query_scalar(
                "INSERT INTO suppliers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
            )
            .bind(name)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
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
        let account: i64 =
            match sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'doc wallet'")
                .fetch_optional(pool)
                .await
                .unwrap()
            {
                Some(id) => id,
                None => sqlx::query_scalar(
                    "INSERT INTO accounts (name, created_by) VALUES ('doc wallet', ?) RETURNING id",
                )
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap(),
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

        let confirmed = seed_purchase(
            &pool,
            Some("2024-PURCH-000001"),
            "Distribuidora Sur",
            d(2024, 5, 2),
            actor,
        )
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

        let mine = seed_purchase(
            &pool,
            Some("2024-PURCH-000001"),
            "Sur",
            d(2024, 5, 2),
            sistema,
        )
        .await;
        let theirs = seed_purchase(
            &pool,
            Some("2024-PURCH-000002"),
            "Norte",
            d(2024, 5, 3),
            other,
        )
        .await;

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
        let early =
            seed_purchase(&pool, Some("2024-PURCH-000001"), "A", d(2024, 5, 1), actor).await;
        let first =
            seed_purchase(&pool, Some("2024-PURCH-000002"), "B", d(2024, 5, 2), actor).await;
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
        let with_invoice = seed_purchase(
            &pool,
            Some("2024-PURCH-000001"),
            "Distribuidora Sur",
            d(2024, 5, 2),
            actor,
        )
        .await;
        sqlx::query("UPDATE purchases SET supplier_invoice_no = 'FACT-77' WHERE id = ?")
            .bind(with_invoice)
            .execute(&pool)
            .await
            .unwrap();
        seed_purchase(
            &pool,
            Some("2024-PURCH-000002"),
            "Importadora Norte",
            d(2024, 5, 3),
            actor,
        )
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

        let confirmed = seed_purchase(
            &pool,
            Some("2024-PURCH-000001"),
            "Distribuidora Sur",
            d(2024, 5, 2),
            actor,
        )
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

    /// The first read-by-id of one purchase payment: found carries every
    /// stored column back, absent is `None` — never an error — and the whole
    /// read is ONE query (the drawer's per-payment read must stay cheap).
    #[tokio::test]
    async fn find_payment_reads_one_payment_in_one_query() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let purchase = seed_purchase(
            &pool,
            Some("2024-PURCH-000001"),
            "Distribuidora Sur",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let payment_id = seed_payment(&pool, purchase, "10", d(2024, 5, 10), actor).await;

        repo.reset_reads();
        let found = repo
            .find_payment(payment_id)
            .await
            .unwrap()
            .expect("payment exists");
        assert_eq!(found.id, payment_id);
        assert_eq!(found.purchase_id, purchase);
        assert_eq!(found.amount, dec("10"));
        assert_eq!(found.date, d(2024, 5, 10));
        assert_eq!(found.created_by, actor);
        assert_eq!(found.refund_transaction_id, None);
        assert_eq!(repo.read_count(), 1);

        // An unknown id is `None`, and it still costs exactly one query.
        repo.reset_reads();
        assert!(repo.find_payment(999_999).await.unwrap().is_none());
        assert_eq!(repo.read_count(), 1);
    }

    // -- delete_draft (the documents drawer's draft delete) --------------------

    /// One purchase with an EXPLICIT status, seeded through raw SQL: the delete
    /// tests must be able to pin a Confirmed row WITHOUT the service's guard,
    /// because the point is proving the SQL backstop (`WHERE status = 'Draft'`)
    /// is load-bearing on its own, not that the service refuses politely.
    async fn seed_purchase_with_status(
        pool: &SqlitePool,
        status: &str,
        supplier: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let supplier_id = seed_supplier(pool, supplier, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by)
               VALUES (?, ?, 'Cash', ?, ?)
               RETURNING id"#,
        )
        .bind(supplier_id)
        .bind(status)
        .bind(date)
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
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;

        let draft =
            seed_purchase_with_status(&pool, "Draft", "Delete Supplier", d(2024, 5, 2), actor)
                .await;
        for _ in 0..2 {
            sqlx::query(
                "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, '1', '5')",
            )
            .bind(draft)
            .bind(product)
            .execute(&pool)
            .await
            .unwrap();
        }
        let other =
            seed_purchase_with_status(&pool, "Draft", "Keep Supplier", d(2024, 5, 3), actor).await;
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, '3', '5')",
        )
        .bind(other)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        assert!(repo.delete_draft(draft).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM purchases WHERE id = ?", draft).await,
            0,
            "the draft row must be gone"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_lines WHERE purchase_id = ?",
                draft
            )
            .await,
            0,
            "the draft's lines must be gone with it"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM purchases WHERE id = ?", other).await,
            1,
            "the other document must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_lines WHERE purchase_id = ?",
                other
            )
            .await,
            1,
            "the other document's lines must survive"
        );
    }

    /// THE backstop proof: the repository is called DIRECTLY on a Confirmed
    /// purchase — no service guard in the way — and still refuses, because the
    /// `WHERE status = 'Draft'` in the statement is what makes deleting a
    /// confirmed document impossible even if the service check were relaxed.
    #[tokio::test]
    async fn delete_draft_called_directly_on_a_confirmed_purchase_returns_false_and_the_row_survives(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;

        let confirmed = seed_purchase_with_status(
            &pool,
            "Confirmed",
            "Confirmed Supplier",
            d(2024, 5, 2),
            actor,
        )
        .await;
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, '1', '5')",
        )
        .bind(confirmed)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        assert!(!repo.delete_draft(confirmed).await.unwrap());
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchases WHERE id = ?",
                confirmed
            )
            .await,
            1,
            "a confirmed purchase must survive a direct repository delete attempt"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_lines WHERE purchase_id = ?",
                confirmed
            )
            .await,
            1,
            "the confirmed purchase's lines must survive too"
        );
    }

    /// Re-pinned protection test: a purchase that was CONFIRMED (its number was
    /// assigned at confirm and is immutable) and then cancelled stays
    /// undeletable — the audit trail (return movements, refund transactions)
    /// references it. The number is stamped directly, exactly the fact confirm
    /// would have written: `purchase_number IS NOT NULL` is the only marker
    /// that separates this fixture from a discarded draft.
    #[tokio::test]
    async fn delete_draft_on_a_confirmed_then_cancelled_purchase_returns_false_and_the_row_survives(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let cancelled = seed_purchase_with_status(
            &pool,
            "Cancelled",
            "Cancelled Supplier",
            d(2024, 5, 2),
            actor,
        )
        .await;
        sqlx::query("UPDATE purchases SET purchase_number = '2024-PURCH-000001' WHERE id = ?")
            .bind(cancelled)
            .execute(&pool)
            .await
            .unwrap();

        assert!(!repo.delete_draft(cancelled).await.unwrap());
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchases WHERE id = ?",
                cancelled
            )
            .await,
            1,
            "a confirmed-then-cancelled purchase must survive a direct repository delete"
        );
    }

    /// A discarded (never-confirmed) cancelled purchase posts nothing — it is a
    /// garbage row and MUST delete, taking its lines with it (CASCADE), while
    /// a sibling discarded purchase keeps its row.
    #[tokio::test]
    async fn delete_draft_on_a_discarded_cancelled_purchase_deletes_it_with_its_lines() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;

        let discarded = seed_purchase_with_status(
            &pool,
            "Cancelled",
            "Discarded Supplier",
            d(2024, 5, 2),
            actor,
        )
        .await;
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, '1', '5')",
        )
        .bind(discarded)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();
        let other =
            seed_purchase_with_status(&pool, "Cancelled", "Keep Supplier", d(2024, 5, 3), actor)
                .await;

        assert!(
            repo.delete_draft(discarded).await.unwrap(),
            "a never-confirmed cancelled purchase is deletable"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchases WHERE id = ?",
                discarded
            )
            .await,
            0,
            "the discarded row must be gone"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_lines WHERE purchase_id = ?",
                discarded
            )
            .await,
            0,
            "its lines must be gone with it"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM purchases WHERE id = ?", other).await,
            1,
            "the sibling row must survive"
        );
    }

    #[tokio::test]
    async fn delete_draft_on_an_unknown_id_returns_false() {
        let pool = memory_pool().await;
        let repo = SqlitePurchaseRepository::new(pool.clone());
        assert!(!repo.delete_draft(999_999).await.unwrap());
    }

    // -- set_confirmed (the confirm write's own DRAFT predicate) ---------------

    /// THE backstop proof for the confirm write, on the same shape as the
    /// delete proof above: the repository is called DIRECTLY on an already
    /// Confirmed purchase — no service guard in the way — and must refuse,
    /// because `AND status = 'Draft'` is what makes a duplicate confirmation
    /// impossible even if the service's read-then-write check were relaxed or
    /// raced.
    #[tokio::test]
    async fn set_confirmed_called_directly_on_a_confirmed_purchase_is_refused_naming_the_state() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase =
            seed_purchase_with_status(&pool, "Draft", "Confirm Supplier", d(2024, 5, 2), actor)
                .await;

        // The first confirmation is the only one that may write.
        let confirmed = repo
            .set_confirmed(purchase, actor, "2024-PURCH-000001")
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::models::PurchaseStatus::Confirmed);
        assert_eq!(
            confirmed.purchase_number.as_deref(),
            Some("2024-PURCH-000001")
        );

        // The duplicate submission is refused, and the refusal names the state
        // the statement actually saw rather than a generic "already exists".
        let err = repo
            .set_confirmed(purchase, actor, "2024-PURCH-000002")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Confirmed"),
                    "the refusal must name the state the document was found in: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // Nothing was rewritten: the FIRST number survives, so a refused
        // duplicate did not stamp a second one over the confirmed document.
        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(
            after.purchase_number.as_deref(),
            Some("2024-PURCH-000001"),
            "the refused duplicate must leave the confirmed number untouched"
        );
        assert_eq!(after.confirmed_at, confirmed.confirmed_at);
    }

    /// The same refusal for a Cancelled document: only a Draft may be confirmed,
    /// so a cancelled purchase is refused too — and it is named as Cancelled,
    /// because that is the state the row rests in.
    #[tokio::test]
    async fn set_confirmed_called_directly_on_a_cancelled_purchase_is_refused_naming_cancelled() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase = seed_purchase_with_status(
            &pool,
            "Cancelled",
            "Annulled Supplier",
            d(2024, 5, 2),
            actor,
        )
        .await;

        let err = repo
            .set_confirmed(purchase, actor, "2024-PURCH-000003")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Cancelled"),
                    "the refusal must name the state the document was found in: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(
            after.purchase_number, None,
            "the cancelled purchase kept no number"
        );
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // Two WRITES, and the asymmetry between them is the reason this commit is
    // worth a separate slice at all.
    //
    // `create_payment` is the straight pattern: one INSERT, `RETURNING`, no
    // read-back, no refusal helper. Moving it is a copy of the statement onto a
    // different executor and nothing else.
    //
    // `set_confirmed` is the trap, and the trap is NOT in `set_confirmed` — it is
    // in a private helper called only on the refusal path. `refuse_confirm`'s
    // signature was hard-coded to `&SqlitePool` because `set_confirmed` ran on
    // autocommit, exactly as `refuse_line`'s had to be changed when
    // `rewrite_draft_line_taxes` moved. A migration that misses it passes every
    // happy-path test in this file and fails only the refusal ones, which is why
    // there is a test below that drives the refusal by NAME rather than
    // incidentally.
    //
    // Nothing here opens a transaction across a service call. Phase A installs
    // the doors; `confirm` does not walk through them until a later commit, and
    // the last test pins that both public wrappers are untouched in the meantime.

    /// The write must land in the caller's unit, not in one of its own: a payment
    /// created inside a transaction and rolled back with it is GONE, and one that
    /// escaped into a private unit would be visible to the pool the moment it
    /// committed.
    ///
    /// This is the rollback half. A payment that survived the rollback would mean
    /// `create_payment_in` opened and committed a unit behind the caller's back,
    /// and a single-connection pool is what makes that visible instead of merely
    /// likely: there is no spare connection for a nested `begin()` to take.
    #[tokio::test]
    async fn create_payment_in_writes_into_the_callers_transaction_and_a_rollback_takes_it_away() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase =
            seed_purchase_with_status(&pool, "Draft", "In-Tx Supplier", d(2024, 5, 2), actor).await;
        // The account and the method are looked up BEFORE the unit opens: they
        // are fixture reads, and the pool is the only thing that can answer them.
        let (account, method) = account_and_method(&pool, actor).await;
        assert!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_payments WHERE purchase_id = ?",
                purchase
            )
            .await
                == 0,
            "the fixture must start with no payments, or this test proves nothing"
        );

        let mut tx = pool.begin().await.unwrap();
        let payment = repo
            .create_payment_in(
                &mut tx,
                actor,
                purchase,
                account,
                method,
                dec("10"),
                d(2024, 5, 2),
                None,
            )
            .await
            .expect("create_payment_in could not run while it held the caller's connection");
        // The RETURNING projection is the row the INSERT wrote, read inside the
        // same unit, so a payment that answered from a private connection would
        // have had to guess this id.
        assert_eq!(payment.purchase_id, purchase);
        assert_eq!(payment.account_id, account);
        assert_eq!(payment.method_id, method);
        assert_eq!(payment.amount, dec("10"));
        assert_eq!(payment.transaction_id, None);
        assert_eq!(payment.refund_transaction_id, None);
        tx.rollback().await.unwrap();

        // Every assertion that touches the pool is AFTER the rollback, and it
        // says the row is not there. This is the assertion that fails if the
        // `_in` form committed a unit of its own.
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_payments WHERE purchase_id = ?",
                purchase
            )
            .await,
            0,
            "the payment survived a rollback of the transaction that created it, so create_payment_in opened and committed a unit of its own"
        );
        assert!(
            repo.list_payments(purchase).await.unwrap().is_empty(),
            "list_payments still sees a payment the caller's rollback removed"
        );
        // And the unit left no residue on the document it named.
        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(after.status, crate::models::PurchaseStatus::Draft);
    }

    /// `create_payment_in` must not reach for the pool AT ALL, and the assertion
    /// is the pairing itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on this
    /// pool at all, ever: it would sit on sqlx's 30s acquire timeout and come
    /// back as `PoolTimedOut`. The timing bound below is corroboration; the
    /// premise is the proof.
    #[tokio::test]
    async fn create_payment_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase =
            seed_purchase_with_status(&pool, "Draft", "Held-Conn Supplier", d(2024, 5, 3), actor)
                .await;
        let (account, method) = account_and_method(&pool, actor).await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a read
        // right now, and that is a fact about the pool, not about this test's
        // patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let payment = repo
            .create_payment_in(
                &mut tx,
                actor,
                purchase,
                account,
                method,
                dec("25"),
                d(2024, 5, 3),
                None,
            )
            .await;
        let elapsed = started.elapsed();
        let payment = payment.expect(
            "create_payment_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert_eq!(payment.amount, dec("25"));
        // MEASURED, not assumed: the pairing above already decides it, and this
        // bound is the corroboration. Five seconds sits four orders of magnitude
        // above what an INSERT on a held connection costs and six below the 30s
        // acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "create_payment_in took {elapsed:?}; that is a write stalling for a connection, not one on the connection it was handed"
        );
        // The caller's transaction is still ALIVE and still holds its lock: a
        // second write on the same connection answers. A `create_payment_in` that
        // had ended, committed or rolled back the unit it was given could not
        // leave this true.
        let second = repo
            .create_payment_in(
                &mut tx,
                actor,
                purchase,
                account,
                method,
                dec("5"),
                d(2024, 5, 3),
                None,
            )
            .await
            .expect("a second write on the same connection could not run");
        assert_ne!(second.id, payment.id, "both writes returned the same row");
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the premise
        // above was the transaction and not the connection.
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchase_payments WHERE purchase_id = ?",
                purchase
            )
            .await,
            0
        );
        assert!(repo
            .create_payment(
                actor,
                purchase,
                account,
                method,
                dec("7"),
                d(2024, 5, 3),
                None
            )
            .await
            .is_ok());
    }

    /// The confirm write is the one that MATTERS, because it is the statement
    /// that turns a Draft into a numbered document. Inside the caller's unit, a
    /// rollback must leave the document exactly as it found it: still a Draft,
    /// still unnumbered, with no `confirmed_at` to mislead a later read.
    ///
    /// This is also the only test in the commit that would catch a
    /// `set_confirmed_in` which stamped the number through a private committed
    /// unit: the number would be visible on the pool the moment the statement
    /// returned, and the rollback could not take it back.
    #[tokio::test]
    async fn set_confirmed_in_stamps_the_number_in_the_callers_unit_and_a_rollback_leaves_the_draft_untouched(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase =
            seed_purchase_with_status(&pool, "Draft", "Rollback Supplier", d(2024, 5, 4), actor)
                .await;
        let before = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(before.status, crate::models::PurchaseStatus::Draft);
        assert_eq!(before.purchase_number, None);
        assert_eq!(before.confirmed_at, None);

        let mut tx = pool.begin().await.unwrap();
        let confirmed = repo
            .set_confirmed_in(&mut tx, purchase, actor, "2024-PURCH-IN-0001")
            .await
            .expect("set_confirmed_in could not run while it held the caller's connection");
        // The read-back is the row the UPDATE wrote, seen through the SAME unit:
        // the number, the status and the stamp all belong to this transaction.
        assert_eq!(confirmed.status, crate::models::PurchaseStatus::Confirmed);
        assert_eq!(
            confirmed.purchase_number.as_deref(),
            Some("2024-PURCH-IN-0001")
        );
        assert!(
            confirmed.confirmed_at.is_some(),
            "the confirmed document carries no confirmed_at stamp"
        );
        // While the unit is open the pool is still blind to all of it, which is
        // the other half: an answer identical to the committed one would mean the
        // write had already escaped the caller's transaction.
        tx.rollback().await.unwrap();

        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            crate::models::PurchaseStatus::Draft,
            "the document was confirmed by a unit the caller's rollback could not reach"
        );
        assert_eq!(
            after.purchase_number, None,
            "the number survived a rollback of the transaction that stamped it"
        );
        assert_eq!(
            after.confirmed_at, None,
            "the confirmed_at stamp survived a rollback of the transaction that wrote it"
        );
        // And the draft is still confirmable, so the rollback restored the state
        // rather than corrupting the row.
        let again = repo
            .set_confirmed(purchase, actor, "2024-PURCH-IN-0002")
            .await
            .unwrap();
        assert_eq!(again.status, crate::models::PurchaseStatus::Confirmed);
    }

    /// THE test of this commit. `set_confirmed_in` must not reach for the pool on
    /// EITHER of the two paths that run inside the caller's unit — and the
    /// assertion is the pairing itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on this
    /// pool at all, ever: it would sit on sqlx's 30s acquire timeout and come
    /// back as `PoolTimedOut`. This test therefore cannot pass by being slow, and
    /// it cannot pass by accident.
    ///
    /// It is the happy path that makes this interesting rather than the refusal
    /// path, because on the happy path the read-back at the end of `set_confirmed`
    /// runs too. A `set_confirmed_in` whose read-back still went to the pool would
    /// pass a test that only exercised the refusal, and that is precisely the bug
    /// a test that only exercises the refusal cannot see.
    #[tokio::test]
    async fn set_confirmed_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let purchase =
            seed_purchase_with_status(&pool, "Draft", "Held-Conn Supplier", d(2024, 5, 5), actor)
                .await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a read
        // right now, and that is a fact about the pool, not about this test's
        // patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let confirmed = repo
            .set_confirmed_in(&mut tx, purchase, actor, "2024-PURCH-HELD-1")
            .await;
        let elapsed = started.elapsed();
        let confirmed = confirmed.expect(
            "set_confirmed_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert_eq!(confirmed.status, crate::models::PurchaseStatus::Confirmed);
        assert_eq!(
            confirmed.purchase_number.as_deref(),
            Some("2024-PURCH-HELD-1")
        );
        // MEASURED, not assumed: the pairing above already decides it, and this
        // bound is the corroboration. Five seconds sits four orders of magnitude
        // above what an UPDATE plus a read-back on a held connection costs and six
        // below the 30s acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "set_confirmed_in took {elapsed:?}; that is a write stalling for a connection, not one on the connection it was handed"
        );
        // The caller's transaction is still ALIVE and still holds its lock. An
        // `_in` that had ended, committed or rolled back the unit it was given
        // could not leave a second confirm running on it.
        let refused = repo
            .set_confirmed_in(&mut tx, purchase, actor, "2024-PURCH-HELD-2")
            .await;
        assert!(
            matches!(refused, Err(AppError::Validation(_))),
            "the second confirm inside the same unit must be refused by the same DRAFT predicate, and must not be a driver error: {refused:?}"
        );
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM purchases WHERE purchase_number = ?",
                0
            )
            .await,
            0
        );
        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(after.status, crate::models::PurchaseStatus::Draft);
    }

    /// The trap, by name: `set_confirmed_in` on a document that is NOT a Draft
    /// must produce the refusal without the pool ever being involved.
    ///
    /// This is the test that catches a migration which moves the UPDATE and
    /// forgets `refuse_confirm`. The refusal is only reached when
    /// `rows_affected() == 0`, so every happy-path test in this file would stay
    /// green while the helper still held `&SqlitePool` — and the defect would
    /// only surface later, inside `confirm`'s real transaction, as a 30-second
    /// stall on a refusal the operator is waiting for.
    ///
    /// The assertion is the pool's own state, not elapsed time: with
    /// `max_connections(1)` and the unit open, a helper that reached for the pool
    /// could not answer at all.
    #[tokio::test]
    async fn set_confirmed_in_refuses_a_non_draft_document_without_ever_reaching_for_the_pool() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        // A Confirmed row, seeded through raw SQL so the service's guard cannot
        // be what refuses it: the point is that the repository's OWN DRAFT
        // predicate is load-bearing inside the caller's transaction.
        let purchase = seed_purchase_with_status(
            &pool,
            "Confirmed",
            "Already Confirmed",
            d(2024, 5, 6),
            actor,
        )
        .await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let refused = repo
            .set_confirmed_in(&mut tx, purchase, actor, "2024-PURCH-DUP-1")
            .await;
        let elapsed = started.elapsed();

        // The refusal is a VALUE carrying the state it found, and it is still
        // `Validation` — the same variant the public method has always produced.
        let refused = refused.expect_err(
            "confirming a Confirmed document must be refused, and a refusal that reached for the pool would be a 30s PoolTimedOut instead",
        );
        match refused {
            AppError::Validation(msg) => assert!(
                msg.contains("Confirmed"),
                "the refusal must name the state the document was found in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "the refusal path took {elapsed:?}; that is a read stalling for a connection, not one on the connection it was handed"
        );
        tx.rollback().await.unwrap();

        // The refusal is a refusal, not a write: the number was not stamped over
        // the confirmed document, and the pool agrees.
        let after = repo.find_purchase(purchase).await.unwrap().unwrap();
        assert_eq!(
            after.purchase_number, None,
            "a refused confirm stamped a number on a document it did not confirm"
        );
    }

    /// The additive claim, proved rather than asserted: both public wrappers still
    /// answer exactly what they always answered, in every direction — the success
    /// path, the duplicate-submission refusal, the frozen-document refusal, the
    /// missing-document refusal and the payment write.
    ///
    /// The error VARIANTS matter as much as the messages. `refuse_confirm` maps
    /// "no such document" to `NotFound` and "exists but frozen" to `Validation`,
    /// and a rewrite that collapsed them would still refuse — it would just refuse
    /// in a way the HTTP layer maps to a different status.
    #[tokio::test]
    async fn the_public_wrappers_answer_exactly_as_before_including_every_refusal() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqlitePurchaseRepository::new(pool.clone());
        let (account, method) = account_and_method(&pool, actor).await;
        let draft =
            seed_purchase_with_status(&pool, "Draft", "Public Supplier", d(2024, 5, 7), actor)
                .await;

        // -- set_confirmed: the success path -------------------------------
        let confirmed = repo
            .set_confirmed(draft, actor, "2024-PUBLIC-1")
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::models::PurchaseStatus::Confirmed);
        assert_eq!(confirmed.purchase_number.as_deref(), Some("2024-PUBLIC-1"));
        assert!(confirmed.confirmed_at.is_some());

        // -- set_confirmed: the duplicate submission ------------------------
        // `Validation`, and it names the state rather than saying "exists".
        let err = repo
            .set_confirmed(draft, actor, "2024-PUBLIC-2")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(
                msg.contains("Confirmed"),
                "the duplicate refusal must name the state: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        // The first number survives: a refused duplicate stamped nothing.
        assert_eq!(
            repo.find_purchase(draft)
                .await
                .unwrap()
                .unwrap()
                .purchase_number,
            Some("2024-PUBLIC-1".to_string())
        );

        // -- set_confirmed: a frozen document -------------------------------
        let cancelled = seed_purchase_with_status(
            &pool,
            "Cancelled",
            "Cancelled Supplier",
            d(2024, 5, 8),
            actor,
        )
        .await;
        let err = repo
            .set_confirmed(cancelled, actor, "2024-PUBLIC-3")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(
                msg.contains("Cancelled"),
                "the frozen-document refusal must name the state: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        // -- set_confirmed: a document that does not exist -------------------
        // A DIFFERENT variant, and the one a rewrite is most likely to collapse
        // into the other: not found is a 404, not a validation failure.
        let err = repo
            .set_confirmed(999_999, actor, "2024-PUBLIC-4")
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::NotFound(_)),
            "expected NotFound for a missing document, got {err:?}"
        );

        // -- create_payment: the success path --------------------------------
        let paid_draft =
            seed_purchase_with_status(&pool, "Draft", "Paying Supplier", d(2024, 5, 9), actor)
                .await;
        let payment = repo
            .create_payment(
                actor,
                paid_draft,
                account,
                method,
                dec("42.50"),
                d(2024, 5, 9),
                None,
            )
            .await
            .unwrap();
        assert_eq!(payment.purchase_id, paid_draft);
        assert_eq!(payment.account_id, account);
        assert_eq!(payment.method_id, method);
        assert_eq!(payment.amount, dec("42.50"));
        // The wrapper leaves no unit of its own behind: the row is readable
        // immediately, and a second payment on the same document is a different
        // row rather than an overwrite.
        let listed = repo.list_payments(paid_draft).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, payment.id);
        let second = repo
            .create_payment(
                actor,
                paid_draft,
                account,
                method,
                dec("1"),
                d(2024, 5, 9),
                None,
            )
            .await
            .unwrap();
        assert_ne!(second.id, payment.id);
        assert_eq!(repo.list_payments(paid_draft).await.unwrap().len(), 2);
    }
}
