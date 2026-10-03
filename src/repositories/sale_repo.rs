use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    DocumentKind, DocumentQuery, DocumentRow, NewLineTax, NewSale, PaymentType, PriceRefusal, Sale,
    SaleLine, SaleListFilter, SalePayment, SaleStatus, UpdateSaleDraft,
};
use crate::repositories::tax_repo::active_taxes_for_product;
use crate::repositories::tax_snapshot_repo::replace_sale_line_taxes;
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
    let tax_str: String = row.get("tax_total");
    SaleLine {
        id: row.get("id"),
        sale_id: row.get("sale_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&qty_str),
        unit_price: parse_decimal(&price_str),
        tax_total: parse_decimal(&tax_str),
        created_at: row.get("created_at"),
    }
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> SalePayment {
    let amt_str: String = row.get("amount");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
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
        sale_number: row
            .try_get::<Option<String>, _>("sale_number")
            .unwrap_or(None),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at,
        updated_at,
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

/// One sale by id, over whichever connection the caller offers.
///
/// `set_confirmed` reads its own row back after the UPDATE, so this statement
/// runs on two executors the moment a transaction-joining form of that write
/// exists: the caller's `&mut SqliteConnection` when a unit holds the confirm,
/// and the pool when the public wrapper owns the unit. ONE copy of the SQL,
/// generic over the executor — the same shape and the same reason as
/// `transaction_repo::balance_for_account_raw`. Two copies would be able to
/// drift on the projection, and a drift here is the worst kind: the read-back
/// would answer about a different row shape than the write it is confirming.
async fn find_sale_raw<'e, E>(executor: E, id: i64) -> AppResult<Option<Sale>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        r#"SELECT id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM sales WHERE id = ?"#,
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(row_to_sale))
}

#[async_trait]
pub trait SaleRepository: Send + Sync {
    /// The connection pool behind this repository, so a caller that owns a
    /// WORKING UNIT can open the transaction the `_in` methods join. It is the
    /// one non-`async` method here: it hands out the pool rather than borrowing
    /// it, so nothing about it can be awaited and `#[async_trait]` leaves it
    /// alone.
    ///
    /// Why it exists at all. `SalesService` is generic over repository TRAITS and
    /// holds no `SqlitePool` of its own — by design, so the service layer cannot
    /// grow SQL. Until this method the traits were pure data access, and
    /// `confirm` could not open the unit its five writes need. The alternative
    /// was a `pool` field on the service, which would have changed four
    /// construction sites and broken the dependency injection the whole test
    /// suite builds its fixtures through. A field read in one impl line is the
    /// smaller change, and it keeps the service holding no SQL handle.
    ///
    /// It is a WIDENING of the trait's surface and nothing else: the service
    /// that receives it is the caller, and every read and write below still goes
    /// through the repository's own methods.
    fn pool(&self) -> &SqlitePool;

    /// `customer_name` is the snapshot resolved by the service through
    /// `CustomerService`; this layer never reads the `customers` table.
    async fn create_sale(
        &self,
        actor: i64,
        input: &NewSale,
        customer_name: &str,
    ) -> AppResult<Sale>;
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
    ///
    /// The Draft precondition is this statement's own WHERE, not the service's
    /// up-front read: a document that is not a Draft is refused with a
    /// [`AppError::Validation`] naming the state it was found in. So a duplicate
    /// submission of an already-Confirmed sale cannot stamp a second number,
    /// even if the caller's status check were relaxed or raced.
    ///
    /// It does NOT make a failed confirmation retryable BY ITSELF, and that is
    /// still true: a WHERE clause cannot reach residue. What changed is that
    /// there is no residue left to reach. `confirm` now opens ONE transaction
    /// immediately before `next_number` and commits after the last write
    /// (`src/services/sales.rs:1402-1469`), so a failure anywhere in between
    /// rolls back the sequence number, every stock movement, the `Income`, the
    /// `sale_payments` row and the confirmation together. The document is still
    /// `("Draft", None)` — a Draft with no number, no money, and nothing the
    /// predicates below were written to distrust — and once the cause is fixed
    /// the same retry succeeds. The removal of that residue is measured, not
    /// assumed, by the `confirm_failure_*` tests in `src/services/sales.rs`.
    ///
    /// This public twin still opens a unit of its own and delegates, so a caller
    /// that reaches for it directly gets exactly the old behaviour: the single
    /// statement, atomically, with no sibling writes.
    async fn set_confirmed(&self, id: i64, actor: i64, sale_number: &str) -> AppResult<Sale>;

    /// [`Self::set_confirmed`] inside a transaction the CALLER owns.
    ///
    /// This is the write that decides whether a document exists as far as the
    /// shop is concerned, and the last of the five `confirm` performs
    /// (`src/services/sales.rs:1402-1469`). It is also the write that made the
    /// surrounding unit worth opening: when every other statement ran on its own
    /// autocommit connection, a failure HERE left the sequence number spent, the
    /// stock movements committed, the `Income` committed and a `sale_payments`
    /// row committed on a document still reading `("Draft", NULL)` — a paid
    /// draft, and a residue no WHERE clause could reach.
    ///
    /// `confirm` now opens ONE unit immediately before `next_number` and commits
    /// after this statement (`src/services/sales.rs:1402-1469`), so that
    /// residue cannot be produced by a failure at this step: the `?` drops the
    /// `Transaction` and sqlx rolls the whole run back, including the burned
    /// number. The document stays a Draft with no number, `get_detail` derives
    /// it as Unpaid again, and the retry works once the cause is fixed. The
    /// `delete_draft` hazard that residue created — its CASCADE taking the
    /// payment while the unparented `Income` survived — is gone with it.
    ///
    /// The DRAFT predicate is unchanged and is still this statement's own
    /// `WHERE`: a non-Draft matches nothing and the refusal is read back
    /// through the SAME connection, so it names the state the write saw. That
    /// read-back is the trap in this file, and it is called out on
    /// [`SqliteSaleRepository::refuse_confirm`] — the helper's executor is part
    /// of what moves, not an implementation detail of it. Inside the unit, the
    /// predicate's remaining job is that the statement, the read-back and
    /// `refuse_confirm` all have to stay on the connection the caller was
    /// handed; it still refuses a duplicate submission of an already-Confirmed
    /// document, and it does so without the pool ever being involved.
    ///
    /// `confirm` DOES walk through this door now. The public twin still opens a
    /// unit of its own and delegates to this method, so both shapes remain and
    /// the twin's behaviour is unchanged.
    async fn set_confirmed_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        actor: i64,
        sale_number: &str,
    ) -> AppResult<Sale>;
    /// Transition Draft/Confirmed -> Cancelled.
    async fn set_cancelled(&self, id: i64, actor: i64, reason: Option<&str>) -> AppResult<Sale>;
    /// Stamp a sale's `updated_by`/`updated_at` after a line change: the line
    /// inherits the sale's actor (no columns of its own), but the document was
    /// just edited and the edit is attributed to the request. The statement
    /// updates whatever id it is given, so the restriction to drafts lives in
    /// the caller — stated here rather than enforced in SQL, because a
    /// `WHERE status = 'Draft'` would turn a future misuse into a silent no-op
    /// instead of a visible edit on the wrong document.
    async fn touch_draft(&self, id: i64, actor: i64) -> AppResult<Sale>;

    /// Create a line on a DRAFT sale together with everything the feature says
    /// a line must carry: the frozen tax breakdown and the tax total that
    /// summarizes it. ONE transaction, ONE public contract — this method IS the
    /// tax-aware write, so there is no second, tax-free way to add a line.
    ///
    /// The taxes are resolved from the product's ACTIVE links INSIDE that
    /// transaction, so a tax deactivated or re-rated while the line was being
    /// written cannot slip between resolution and persistence. The calculation
    /// is the shared contract (`services::line_taxes`), so the sale, purchase
    /// and product-preview paths cannot disagree by a cent.
    ///
    /// The DRAFT requirement is in the INSERT's own `WHERE` clause, not only in
    /// the service: even a caller that skipped the service's state guard cannot
    /// attach a line to a Confirmed or Cancelled sale. `NotFound` for a sale
    /// that does not exist, `Conflict` for one that is not a Draft.
    async fn create_line(
        &self,
        sale_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine>;
    async fn find_line(&self, id: i64) -> AppResult<Option<SaleLine>>;
    async fn list_lines(&self, sale_id: i64) -> AppResult<Vec<SaleLine>>;
    /// Edit a DRAFT line: new quantity, new price, the breakdown REPLACED by the
    /// taxes that are active right now, and the tax total recomputed from all
    /// three — atomically, in the same single transaction as the create.
    ///
    /// Replacing (never appending) is what keeps a line's breakdown equal to the
    /// catalog state it was last edited against: a tax that stopped applying
    /// disappears from the line instead of surviving as stale money.
    ///
    /// The DRAFT predicate is part of the UPDATE's `WHERE`, and the breakdown is
    /// only touched after that UPDATE has matched, so a Confirmed line's
    /// quantity, price, aggregate and snapshots are all provably untouched by a
    /// refused call. `NotFound` for an unknown line, `Conflict` for one whose
    /// sale is no longer a Draft.
    async fn update_line(&self, id: i64, qty: Decimal, unit_price: Decimal) -> AppResult<SaleLine>;
    /// Remove a DRAFT line, and with it the tax breakdown the line CASCADEs —
    /// the ordinary "the operator changed their mind" case.
    ///
    /// The DRAFT predicate is in the DELETE's own `WHERE` clause, the same
    /// backstop `create_line` and `update_line` carry: a Confirmed or Cancelled
    /// sale's line cannot be removed even by a caller that skipped the service's
    /// state guard, and deleting it would CASCADE away the immutable record of
    /// what the document actually charged. `NotFound` for a line that does not
    /// exist, `Conflict` for one whose sale is no longer a Draft.
    async fn delete_line(&self, id: i64) -> AppResult<()>;

    /// Delete a DRAFT sale — or a DISCARDED one (Cancelled while never
    /// confirmed: `sale_number IS NULL`) — and let its lines die by
    /// CASCADE. The predicate in the WHERE is the load-bearing backstop: even
    /// if a caller ever relaxed the service's state guard, a Confirmed row or
    /// a Cancelled row that carries a number cannot be removed by this
    /// statement — it answers `false` instead, so the caller can refuse
    /// honestly. Both deletable states posted nothing by construction (no
    /// payments — nothing but a Confirmed document takes them — no stock
    /// movement, no ledger entry, no customer debt), so nothing dangles; a
    /// confirmed-then-cancelled document is permanent audit trail and stays
    /// protected. This is a WRITE, not a read: the test read counter
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

    /// [`Self::create_payment`] inside a transaction the CALLER owns, and the
    /// reason this write belongs to the confirm unit is sharper than for most.
    ///
    /// The `sale_payments` row is written FOURTH of five, after the `Income` it
    /// records, and it is the row the paid/unpaid state of the document is
    /// derived from. That made it the hinge of the old residue: a confirm that
    /// died between the two left an `Income` with no payment naming it, and a
    /// confirm that died AFTER them left a Draft that reported itself Paid —
    /// measured, at the time, by what used to be called a "paid draft" and what
    /// its own assertion read as *"the shop has the money and no document"*.
    ///
    /// Neither shape is reachable any more. `confirm` holds ONE unit across both
    /// writes and this statement (`src/services/sales.rs:1402-1469`), so a
    /// failure on either side of the pair rolls back both, and the measurement
    /// is now the ABSENCE: `confirm_failure_on_set_confirmed_leaves_a_clean_draft_that_reports_unpaid`
    /// (`src/services/sales.rs:6461`) asserts zero `Income`, zero payments, an
    /// unspent number, and a `get_detail` that reports `Unpaid` with the whole
    /// total outstanding. The same holds for the window between the two writes.
    ///
    /// Unlike `set_confirmed_in` this write has no read-back and no refusal
    /// helper, which makes it the straight case: the statement moves to the
    /// caller's executor and `map_db_err` moves with it, unchanged.
    ///
    /// `confirm` DOES walk through this door now. The public twin still opens a
    /// unit of its own and delegates, and the two production callers that
    /// collect against an already-Confirmed document
    /// (`record_payment_with_receipt`) are unchanged — this doc describes the
    /// confirm path, not the only two paths to the insert.
    async fn create_payment_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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
    async fn list_payment_document_rows(
        &self,
        query: &DocumentQuery,
    ) -> AppResult<Vec<DocumentRow>>;

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

    /// The ONE implementation behind `SaleRepository::create_line`: insert the
    /// line, snapshot its resolved taxes and store the aggregate that
    /// summarizes them, in a single transaction. Private on purpose — the
    /// public trait method is the contract, so a caller cannot reach a
    /// tax-aware path that behaves differently from the ordinary one.
    async fn write_line_with_taxes(
        &self,
        sale_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine> {
        let mut tx = self.pool.begin().await?;

        // Resolve, calculate, write — all inside this one transaction, so the
        // breakdown and the aggregate that summarizes it can never be committed
        // apart. The resolver takes this transaction's connection, so the taxes
        // a line is snapshotted with are the ones its own transaction saw.
        let taxes = active_taxes_for_product(&mut tx, product_id).await?;
        let calc = line_net_amount(qty, unit_price)
            .and_then(|net| calculate_line_taxes(net, &taxes))
            .map_err(AppError::PriceRefused)?;

        // The DRAFT predicate is the statement's own: a Confirmed sale matches
        // no row and the insert is a no-op, not a line on closed history.
        let row = sqlx::query(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
               SELECT ?, ?, ?, ?, ?
               WHERE EXISTS (SELECT 1 FROM sales WHERE id = ? AND status = 'Draft')
               RETURNING id, sale_id, product_id, qty, unit_price, tax_total, created_at"#,
        )
        .bind(sale_id)
        .bind(product_id)
        .bind(qty.to_string())
        .bind(unit_price.to_string())
        .bind(calc.tax_total.to_string())
        .bind(sale_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, sale_id).await),
        };

        let taxes: Vec<NewLineTax> = calc.taxes.iter().map(NewLineTax::from).collect();
        replace_sale_line_taxes(&mut tx, row.get("id"), &taxes).await?;
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    /// The ONE implementation behind `SaleRepository::update_line`: guarded
    /// draft edit, breakdown replaced, aggregate recomputed, one transaction.
    async fn rewrite_draft_line_taxes(
        &self,
        id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine> {
        let mut tx = self.pool.begin().await?;

        // The line's product and its owning document, read inside the
        // transaction that will write it.
        let current: Option<(i64, i64)> =
            sqlx::query_as("SELECT product_id, sale_id FROM sale_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let (product_id, sale_id) = match current {
            Some(current) => current,
            None => return Err(AppError::NotFound(format!("sale line {id} not found"))),
        };

        // Resolved through this transaction, like the creation path.
        let taxes = active_taxes_for_product(&mut tx, product_id).await?;
        let calc = line_net_amount(qty, unit_price)
            .and_then(|net| calculate_line_taxes(net, &taxes))
            .map_err(AppError::PriceRefused)?;

        // The DRAFT predicate is in the UPDATE's own WHERE. If it matches
        // nothing, the line belongs to a closed document and NOTHING below
        // runs: no snapshot is deleted, none is inserted, and the stored
        // aggregate is left alone.
        let row = sqlx::query(
            r#"UPDATE sale_lines SET qty = ?, unit_price = ?, tax_total = ?
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM sales s WHERE s.id = sale_lines.sale_id AND s.status = 'Draft')
               RETURNING id, sale_id, product_id, qty, unit_price, tax_total, created_at"#,
        )
        .bind(qty.to_string())
        .bind(unit_price.to_string())
        .bind(calc.tax_total.to_string())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, sale_id).await),
        };

        // Reached only for a DRAFT line: swap the whole breakdown, so no
        // snapshot of a tax that stopped applying can survive the edit.
        let taxes: Vec<NewLineTax> = calc.taxes.iter().map(NewLineTax::from).collect();
        replace_sale_line_taxes(&mut tx, id, &taxes).await?;
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    /// Why a tax-aware line write matched no row: either the sale does not
    /// exist, or it exists and is no longer a Draft. Read through the CALLER'S
    /// transaction, so the refusal describes the same state the write saw, and
    /// the two cases stay distinguishable — a missing document is a 404, a
    /// frozen one is a conflict the caller can explain.
    async fn refuse_line(conn: &mut SqliteConnection, sale_id: i64) -> AppError {
        let status = sqlx::query_scalar::<_, String>("SELECT status FROM sales WHERE id = ?")
            .bind(sale_id)
            .fetch_optional(&mut *conn)
            .await;
        match status {
            Ok(None) => AppError::NotFound(format!("sale {sale_id} not found")),
            Ok(Some(status)) => AppError::Conflict(format!(
                "sale {sale_id} is {status}: its lines and their taxes are frozen"
            )),
            Err(error) => AppError::Database(error),
        }
    }

    /// Why a confirm write matched no row: either the sale does not exist, or it
    /// exists and is no longer a Draft. Read back so the refusal names the state
    /// the document actually rests in, and so the two cases stay
    /// distinguishable — a missing document is a 404, a frozen one is a
    /// validation the caller can explain. Same contract as
    /// [`SqliteSaleRepository::refuse_line`], and the same executor for the same
    /// reason: the refusal is read through the CALLER'S transaction, so it
    /// describes the same state the write saw.
    ///
    /// This signature USED to be hard-coded to `&SqlitePool`, written when
    /// `set_confirmed` ran on autocommit. That history is why it reads the way
    /// it does, but it is not a statement about current behaviour: `confirm`
    /// holds ONE unit (`src/services/sales.rs:1402-1469`) and calls this helper
    /// on the connection it was handed, so the connection is now load-bearing
    /// rather than historical.
    ///
    /// The trap is unchanged, and reverting this signature to `&SqlitePool` is
    /// what the test
    /// `set_confirmed_in_refuses_a_non_draft_document_without_ever_reaching_for_the_pool`
    /// catches. The refusal is reached ONLY when the DRAFT predicate matched no
    /// row, so a regression here passes every happy-path test in this file and
    /// fails only on refusal — where it would stall for sqlx's 30s acquire
    /// timeout inside the caller's transaction and answer `PoolTimedOut` instead
    /// of a `Validation` the operator is waiting for. The mapping below is
    /// unchanged: `NotFound`, `Validation`, `Database`.
    async fn refuse_confirm(conn: &mut SqliteConnection, id: i64) -> AppError {
        match sqlx::query_scalar::<_, String>("SELECT status FROM sales WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
        {
            Ok(None) => AppError::NotFound(format!("sale {id} not found")),
            Ok(Some(status)) => AppError::Validation(format!(
                "sale {id} is {status}: only a Draft sale can be confirmed"
            )),
            Err(error) => AppError::Database(error),
        }
    }
}

#[async_trait]
impl SaleRepository for SqliteSaleRepository {
    /// A field read: the struct's `pool` is already `pub` and is already what
    /// every other method on this impl borrows.
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn create_sale(
        &self,
        actor: i64,
        input: &NewSale,
        customer_name: &str,
    ) -> AppResult<Sale> {
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
        // Unchanged in every observable way: same SQL, same bind, same
        // projection, same pool. The statement moved into `find_sale_raw` only
        // because `set_confirmed_in` now has to run this SAME query on a
        // caller's connection, and one copy of a statement is the rule. No
        // read counter lives here, so nothing about this refactor has to decide
        // where a `tick()` goes.
        find_sale_raw(&self.pool, id).await
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
            "SELECT id, sale_id, product_id, qty, unit_price, tax_total, created_at FROM sale_lines WHERE sale_id IN (",
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
            "SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at, updated_at FROM sale_payments WHERE sale_id IN (",
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
        let mut tx = self.pool.begin().await?;
        let confirmed = self
            .set_confirmed_in(&mut tx, id, actor, sale_number)
            .await?;
        tx.commit().await?;
        Ok(confirmed)
    }

    async fn set_confirmed_in(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
        actor: i64,
        sale_number: &str,
    ) -> AppResult<Sale> {
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
        // WHAT IT STILL DOES NOT DO: it does not make a failed confirmation safe
        // to retry ON ITS OWN. A WHERE clause cannot reach residue; that is
        // what the shared unit is for. `confirm` now opens ONE transaction
        // immediately before `next_number` and commits after this statement
        // (`src/services/sales.rs:1402-1469`), so the sequence, the movements,
        // the `Income` and the payment are no longer separate autocommit
        // connections. A failure at this step rolls the whole run back: the
        // number is UNSPENT, no movement survives, no finance row is orphaned,
        // the document is still `("Draft", None)` and the retry re-passes this
        // predicate and succeeds once the cause is fixed.
        //
        // So the predicate is no longer asked to cover that case — the unit is.
        // What it is still asked to cover, and still covers, is the duplicate
        // submission above, and getting THAT refusal out of this statement is
        // why the read-back and `refuse_confirm` below must both stay on
        // `tx`.
        let res = sqlx::query(
            r#"UPDATE sales
               SET sale_number = ?, status = 'Confirmed',
                   updated_by = ?,
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
                 AND status = 'Draft'"#,
        )
        .bind(sale_number)
        .bind(actor)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_confirm(&mut *tx, id).await);
        }
        // The row the UPDATE just wrote, read back through the SAME statement
        // `find_sale` runs and on the SAME connection — not the pool. This is
        // load-bearing rather than tidy: a read-back that reached for the pool
        // here would not be merely slow, it would be unable to answer at all
        // while the caller holds the only connection, and the error it returned
        // (`PoolTimedOut`) would be a driver failure standing in for a document
        // that had in fact just been confirmed correctly. It cannot return
        // `None` in practice — a just-confirmed row is not deletable — but the
        // branch is written out rather than unwrapped so a future caller never
        // sees a panic from a repository method.
        find_sale_raw(&mut *tx, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))
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

    async fn touch_draft(&self, id: i64, actor: i64) -> AppResult<Sale> {
        let row = sqlx::query(
            r#"UPDATE sales
               SET updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, receipt_no, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
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
        // The public contract and the tax-aware write are the same operation.
        self.write_line_with_taxes(sale_id, product_id, qty, unit_price)
            .await
    }

    async fn find_line(&self, id: i64) -> AppResult<Option<SaleLine>> {
        let row = sqlx::query(
            r#"SELECT id, sale_id, product_id, qty, unit_price, tax_total, created_at
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
            r#"SELECT id, sale_id, product_id, qty, unit_price, tax_total, created_at
               FROM sale_lines WHERE sale_id = ? ORDER BY id"#,
        )
        .bind(sale_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_line).collect())
    }

    async fn update_line(&self, id: i64, qty: Decimal, unit_price: Decimal) -> AppResult<SaleLine> {
        // Same single contract on the edit path: the breakdown is replaced and
        // the aggregate recomputed, never left behind a stale total.
        self.rewrite_draft_line_taxes(id, qty, unit_price).await
    }

    async fn delete_line(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;

        // The owning document, read inside the transaction that will delete.
        let sale_id: Option<i64> =
            sqlx::query_scalar("SELECT sale_id FROM sale_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let sale_id = match sale_id {
            Some(sale_id) => sale_id,
            None => return Err(AppError::NotFound(format!("sale line {id} not found"))),
        };

        // The DRAFT predicate is this statement's own WHERE. Zero rows means the
        // line belongs to a closed document, and the refusal is read through the
        // same transaction so it describes the state the delete saw.
        let res = sqlx::query(
            r#"DELETE FROM sale_lines
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM sales s WHERE s.id = sale_lines.sale_id AND s.status = 'Draft')"#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_line(&mut tx, sale_id).await);
        }
        // The line's breakdown went with it by CASCADE, inside this same
        // transaction: a refusal can never leave a half-deleted line.
        tx.commit().await?;
        Ok(())
    }

    async fn delete_draft(&self, id: i64) -> AppResult<bool> {
        // The WHERE clause is the backstop that makes deleting an
        // undeletable document impossible even if the service check were
        // relaxed: the statement simply matches nothing and the answer is
        // `false`. Deletable = a Draft, OR a Cancelled row whose
        // `sale_number` is NULL. A Cancelled row WITH a number was confirmed
        // first and is permanent audit trail (its receipt and ledger history
        // reference it). That much the predicate decides, and it decides it
        // soundly.
        //
        // WHAT THE PREDICATE DOES NOT ESTABLISH, and never did, is integrity.
        // It is a backstop on STATUS, not a cleanliness check: it answers "may
        // this row be removed", and it says nothing about what else is pointing
        // at that row.
        //
        // That distinction mattered most when a Draft could be DIRTY. It used
        // to: `confirm` wrote the sequence number, a movement per tracked line,
        // the `Income`, the `sale_payments` row, and `set_confirmed` LAST, each
        // on its own autocommit connection, so every earlier write was already
        // committed while the document still read `("Draft", NULL)`. A failure
        // AT `set_confirmed` left a Draft carrying a committed payment row and
        // an orphan `Income` whose `reference` was the burned number — and such
        // a Draft MATCHES the first branch. `sale_payments.sale_id` CASCADEs
        // from `sales`, so the delete took the payment row with it, while the
        // `Income` has no foreign key to `sales` at all and survived, pointing
        // at a number no document carried any more: the delete removed the
        // document and kept the money. Discarding such a row did not dodge it
        // either — Draft -> Cancelled is explicitly a no-op for stock and
        // finance (`src/services/sales.rs:1618`), and it assigns no number, so
        // the row arrived at the SECOND branch still carrying both.
        //
        // THAT RESIDUE NO LONGER EXISTS. `confirm` opens one transaction
        // immediately before `next_number` and commits after the last write
        // (`src/services/sales.rs:1402-1469`), so a failure at any step leaves
        // the row a Draft with no number, no payments, no movements and no
        // ledger entry — measured by
        // `confirm_failure_on_set_confirmed_leaves_a_clean_draft_that_reports_unpaid`
        // (`src/services/sales.rs:6461`), whose assertions are the ABSENCE of
        // every shape listed above. The history is kept here because it is why
        // the transaction exists and because the guard below still has to be
        // read as a backstop rather than as a proof.
        //
        // The method keeps answering from the predicate alone. Widening or
        // narrowing what it deletes is a behaviour decision with its own test
        // and its own migration story, and it is not a comment's business. Note
        // the corollary for any future reader tempted to close the window here
        // instead: `create_payment` must NOT grow a `Draft` gate, because two
        // production callers collect against an already-Confirmed document. The
        // fix for "a Draft might be dirty" was `confirm`, and that is where it
        // went.
        let res = sqlx::query(
            r#"DELETE FROM sales
               WHERE id = ?
                 AND (status = 'Draft'
                      OR (status = 'Cancelled' AND sale_number IS NULL))"#,
        )
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
        let mut tx = self.pool.begin().await?;
        let payment = self
            .create_payment_in(
                &mut tx,
                actor,
                sale_id,
                account_id,
                method_id,
                amount,
                date,
                transaction_id,
                receipt_id,
            )
            .await?;
        tx.commit().await?;
        Ok(payment)
    }

    async fn create_payment_in(
        &self,
        tx: &mut SqliteConnection,
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
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(amount.to_string())
        .bind(date)
        .bind(transaction_id)
        .bind(receipt_id)
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
    ) -> AppResult<SalePayment> {
        let row = sqlx::query(
            r#"UPDATE sale_payments
               SET refund_transaction_id = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at, updated_at"#,
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
            r#"SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at, updated_at
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
            r#"SELECT id, sale_id, account_id, method_id, amount, date, transaction_id, refund_transaction_id, receipt_id, created_by, updated_by, created_at, updated_at
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
        // in Rust with the shared tax-inclusive rule, never with SQL SUM over
        // TEXT: the stored `tax_total` is part of what a document costs.
        let ids: Vec<i64> = sales.iter().map(|sale| sale.id).collect();
        let mut lines_qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, sale_id, product_id, qty, unit_price, tax_total, created_at FROM sale_lines WHERE sale_id IN (",
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
        // The purchase twin says why at length; this is the same rule on the
        // sales side of the same index: one document must not take the page down.
        let mut totals: std::collections::BTreeMap<i64, Decimal> =
            std::collections::BTreeMap::new();
        let mut refused: std::collections::BTreeMap<i64, PriceRefusal> =
            std::collections::BTreeMap::new();
        for row in line_rows {
            let line = row_to_line(row);
            let running = totals.entry(line.sale_id).or_default();
            match running.checked_add(tax_inclusive_total(line.subtotal(), line.tax_total)) {
                Some(sum) => *running = sum,
                // The document keeps folding — a later line of the same document
                // cannot make the sum carryable — and the row will show no amount.
                None => {
                    refused.insert(line.sale_id, PriceRefusal::DocumentTotalTooLarge);
                }
            }
        }

        Ok(sales
            .into_iter()
            .map(|sale| {
                let total_refusal = refused.remove(&sale.id);
                DocumentRow {
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
                    amount: if total_refusal.is_some() {
                        None
                    } else {
                        Some(totals.remove(&sale.id).unwrap_or_default())
                    },
                    total_refusal,
                    quantity: None,
                    created_by: sale.created_by,
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

    async fn receipt_allocations(
        &self,
        receipt_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, Decimal>> {
        if receipt_ids.is_empty() {
            // Nothing was asked: no query, no rows, an empty map.
            return Ok(std::collections::BTreeMap::new());
        }
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT receipt_id, amount FROM sale_payments WHERE receipt_id IN (");
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

        let mut out: std::collections::BTreeMap<i64, Decimal> = std::collections::BTreeMap::new();
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
    use std::time::{Duration, Instant};

    // -- migration 44: the (account_id, method_id) pair on a payment row is
    // guarded by the schema itself, not by caller discipline. Each refusal is
    // proved with the raw statement and the trigger's own text, in the style of
    // the role guards: the message asserted is the one the schema wrote.

    /// The message SQLite put on the refused statement: the raw trigger
    /// refusal, mapped by nothing, so the proof cannot drift from the schema.
    fn refusal_message(err: sqlx::Error) -> String {
        match err {
            sqlx::Error::Database(db) => db.message().to_string(),
            other => panic!("expected a refused statement, got {other:?}"),
        }
    }

    /// One account and one method that account owns, created fresh: the seeded
    /// methods are unassigned on a new database, so a consistent pair has to be
    /// built, not picked.
    async fn owned_pair(pool: &SqlitePool, name: &str) -> (i64, i64) {
        let actor = test_support::audit_actor_id(pool).await.unwrap();
        let account: i64 = match sqlx::query_scalar("SELECT id FROM accounts WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id",
            )
            .bind(name)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        };
        let method_name = format!("{name} cash");
        let method: i64 = match sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE name = ? AND account_id = ?",
        )
        .bind(&method_name)
        .bind(account)
        .fetch_optional(pool)
        .await
        .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO payment_methods (name, account_id, created_by) VALUES (?, ?, ?) RETURNING id",
            )
            .bind(&method_name)
            .bind(account)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        };
        (account, method)
    }

    /// A method owned by NO account (`payment_methods.account_id IS NULL`):
    /// the case the `COALESCE(…, -1)` sentinel exists for, since a bare `<>`
    /// against NULL is NULL and `WHEN NULL` never aborts.
    async fn orphan_method(pool: &SqlitePool, actor: i64) -> i64 {
        match sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE name = 'guard orphan' AND account_id IS NULL",
        )
        .fetch_optional(pool)
        .await
        .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO payment_methods (name, created_by) VALUES ('guard orphan', ?) RETURNING id",
            )
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn a_sale_payment_pairing_an_account_with_a_foreign_method_is_refused() {
        let pool = documents_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, None, "guard buyer", d(2024, 6, 1), actor).await;
        let (account, _method) = owned_pair(&pool, "wallet one").await;
        let (_, foreign_method) = owned_pair(&pool, "wallet two").await;
        let err = sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)\n             VALUES (?, ?, ?, '5', '2024-06-01', ?)",
        )
        .bind(sale)
        .bind(account)
        .bind(foreign_method)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "the payment method does not belong to the named account"
        );
        let (rows,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?")
                .bind(sale)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 0, "the refused row is not there");
    }

    #[tokio::test]
    async fn a_sale_payment_with_a_consistent_pair_still_inserts() {
        let pool = documents_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, None, "guard buyer", d(2024, 6, 1), actor).await;
        let (account, method) = owned_pair(&pool, "wallet one").await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)\n             VALUES (?, ?, ?, '5', '2024-06-01', ?) RETURNING id",
        )
        .bind(sale)
        .bind(account)
        .bind(method)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (stored_account, stored_method): (i64, i64) =
            sqlx::query_as("SELECT account_id, method_id FROM sale_payments WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (stored_account, stored_method),
            (account, method),
            "the consistent pair was written"
        );
    }

    #[tokio::test]
    async fn a_sale_payment_naming_an_unassigned_method_is_refused() {
        let pool = documents_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, None, "guard buyer", d(2024, 6, 1), actor).await;
        let (account, _) = owned_pair(&pool, "wallet one").await;
        let orphan = orphan_method(&pool, actor).await;
        let err = sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)\n             VALUES (?, ?, ?, '5', '2024-06-01', ?)",
        )
        .bind(sale)
        .bind(account)
        .bind(orphan)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "the payment method does not belong to the named account",
            "the COALESCE sentinel must turn the NULL owner into a refusal, not a silent pass"
        );
    }

    #[tokio::test]
    async fn a_sale_payment_naming_a_method_that_does_not_exist_is_refused() {
        let pool = documents_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, None, "guard buyer", d(2024, 6, 1), actor).await;
        let (account, _) = owned_pair(&pool, "wallet one").await;
        // A BEFORE trigger fires before constraint checking, so asserting the
        // trigger's own text proves the sentinel refused it — stronger than the
        // foreign key, which would have answered with its own message.
        let err = sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)\n             VALUES (?, ?, ?, '5', '2024-06-01', ?)",
        )
        .bind(sale)
        .bind(account)
        .bind(999_999_i64)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "the payment method does not belong to the named account"
        );
    }

    #[tokio::test]
    async fn a_sale_payment_row_can_diverge_from_its_method_after_insert() {
        let pool = documents_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let sale = seed_sale(&pool, None, "guard buyer", d(2024, 6, 1), actor).await;
        let (account, method) = owned_pair(&pool, "wallet one").await;
        let (moved_account, _) = owned_pair(&pool, "wallet two").await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)\n             VALUES (?, ?, ?, '5', '2024-06-01', ?) RETURNING id",
        )
        .bind(sale)
        .bind(account)
        .bind(method)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Insert-only, deliberately asymmetric: the stored account is the
        // historical fact of where the money landed, while the method is
        // mutable configuration. A BEFORE UPDATE twin would block exactly this.
        sqlx::query("UPDATE sale_payments SET account_id = ? WHERE id = ?")
            .bind(moved_account)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let (stored_account,): (i64,) =
            sqlx::query_as("SELECT account_id FROM sale_payments WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored_account, moved_account,
            "divergence after birth is allowed; history does not move"
        );
    }

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
        let (walkin_id,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (account_id,): (i64,) =
            sqlx::query_as("INSERT INTO accounts (name) VALUES ('legacy wallet') RETURNING id")
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
        sqlx::raw_sql(k2.sql.clone()).execute(&pool).await.unwrap();

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
        let (lines,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?")
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
            sqlx::query("PRAGMA foreign_key_check")
                .execute(&pool)
                .await
                .is_ok(),
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

    /// One confirmed sale through raw SQL (the projection tests seed shapes the
    /// repository API cannot build: arbitrary numbers, dates and actors).
    async fn seed_sale(
        pool: &SqlitePool,
        number: Option<&str>,
        customer: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (walkin,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
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
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, ?, ?)",
        )
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
        // Migration 44 guards the (account, method) pair on the payment row,
        // so the fixture needs a method THIS wallet owns (the seeded methods
        // are unassigned on a fresh database).
        let method: i64 = match sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE name = 'doc wallet cash' AND account_id = ?",
        )
        .bind(account)
        .fetch_optional(pool)
        .await
        .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO payment_methods (name, account_id, created_by)\n                 VALUES ('doc wallet cash', ?, ?) RETURNING id",
            )
            .bind(account)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        };
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

        let confirmed = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Pérez",
            d(2024, 5, 2),
            actor,
        )
        .await;
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

        let mine = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Pérez",
            d(2024, 5, 2),
            sistema,
        )
        .await;
        let theirs = seed_sale(
            &pool,
            Some("2024-SALE-000002"),
            "Díaz",
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
    async fn sale_document_rows_date_range_is_inclusive() {
        let pool = documents_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let early = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Early",
            d(2024, 5, 1),
            actor,
        )
        .await;
        let first = seed_sale(
            &pool,
            Some("2024-SALE-000002"),
            "First",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let last = seed_sale(
            &pool,
            Some("2024-SALE-000003"),
            "Last",
            d(2024, 5, 4),
            actor,
        )
        .await;
        let late = seed_sale(
            &pool,
            Some("2024-SALE-000004"),
            "Late",
            d(2024, 5, 5),
            actor,
        )
        .await;

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
        seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "González",
            d(2024, 5, 2),
            actor,
        )
        .await;
        seed_sale(
            &pool,
            Some("2024-SALE-000002"),
            "Pérez",
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

        let confirmed = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Pérez",
            d(2024, 5, 2),
            actor,
        )
        .await;
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
        let sale = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Pérez",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let payment_id = seed_payment(&pool, sale, "10", d(2024, 5, 10), actor, None).await;

        repo.reset_reads();
        let found = repo
            .find_payment(payment_id)
            .await
            .unwrap()
            .expect("payment exists");
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
        let (walkin,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
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
        let sale = seed_sale(
            &pool,
            Some("2024-SALE-000001"),
            "Pérez",
            d(2024, 5, 2),
            actor,
        )
        .await;
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
        let (walkin,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
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
        let other = seed_sale_with_status(&pool, "Draft", "Keep Buyer", d(2024, 5, 3), actor).await;
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
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?",
                draft
            )
            .await,
            0,
            "the draft's lines must be gone with it"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", other).await,
            1,
            "the other document must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?",
                other
            )
            .await,
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

        let confirmed =
            seed_sale_with_status(&pool, "Confirmed", "Confirmed Buyer", d(2024, 5, 2), actor)
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
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?",
                confirmed
            )
            .await,
            1,
            "the confirmed sale's lines must survive too"
        );
    }

    /// Re-pinned protection test: a sale that was CONFIRMED (its number was
    /// assigned at confirm and is immutable) and then cancelled stays
    /// undeletable — the audit trail (refund transactions) references it. The
    /// number is stamped directly, exactly the fact confirm would have written:
    /// `sale_number IS NOT NULL` is the only marker that separates this
    /// fixture from a discarded draft.
    #[tokio::test]
    async fn delete_draft_on_a_confirmed_then_cancelled_sale_returns_false_and_the_row_survives() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let cancelled =
            seed_sale_with_status(&pool, "Cancelled", "Cancelled Buyer", d(2024, 5, 2), actor)
                .await;
        sqlx::query("UPDATE sales SET sale_number = '2024-SALE-000001' WHERE id = ?")
            .bind(cancelled)
            .execute(&pool)
            .await
            .unwrap();

        assert!(!repo.delete_draft(cancelled).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", cancelled).await,
            1,
            "a confirmed-then-cancelled sale must survive a direct repository delete"
        );
    }

    /// A discarded (never-confirmed) cancelled sale posts nothing — it is a
    /// garbage row and MUST delete, taking its lines with it (CASCADE), while
    /// a sibling discarded sale keeps its row.
    #[tokio::test]
    async fn delete_draft_on_a_discarded_cancelled_sale_deletes_it_with_its_lines() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let product = seed_delete_product(&pool, actor).await;

        let discarded =
            seed_sale_with_status(&pool, "Cancelled", "Discard Buyer", d(2024, 5, 2), actor).await;
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '1', '10')",
        )
        .bind(discarded)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();
        let other =
            seed_sale_with_status(&pool, "Cancelled", "Other Buyer", d(2024, 5, 3), actor).await;
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '3', '10')",
        )
        .bind(other)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            repo.delete_draft(discarded).await.unwrap(),
            "a never-confirmed cancelled sale is deletable"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", discarded).await,
            0,
            "the discarded row must be gone"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?",
                discarded
            )
            .await,
            0,
            "its lines must be gone with it"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE id = ?", other).await,
            1,
            "the sibling discarded sale must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?",
                other
            )
            .await,
            1,
            "the sibling's lines must survive"
        );
    }

    #[tokio::test]
    async fn delete_draft_on_an_unknown_id_returns_false() {
        let pool = migrated_pool().await;
        let repo = SqliteSaleRepository::new(pool.clone());
        assert!(!repo.delete_draft(999_999).await.unwrap());
    }

    // -- set_confirmed (the confirm write's own DRAFT predicate) ---------------

    /// THE backstop proof for the confirm write, on the same shape as the
    /// delete proof above: the repository is called DIRECTLY on an already
    /// Confirmed sale — no service guard in the way — and must refuse, because
    /// `AND status = 'Draft'` is what makes a duplicate confirmation impossible
    /// even if the service's read-then-write check were relaxed or raced.
    #[tokio::test]
    async fn set_confirmed_called_directly_on_a_confirmed_sale_is_refused_naming_the_state() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale =
            seed_sale_with_status(&pool, "Draft", "Confirm Buyer", d(2024, 5, 2), actor).await;

        // The first confirmation is the only one that may write.
        let confirmed = repo
            .set_confirmed(sale, actor, "2024-SALE-000001")
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(confirmed.sale_number.as_deref(), Some("2024-SALE-000001"));

        // The duplicate submission is refused, and the refusal names the state
        // the statement actually saw rather than a generic "already exists".
        let err = repo
            .set_confirmed(sale, actor, "2024-SALE-000002")
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
        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(
            after.sale_number.as_deref(),
            Some("2024-SALE-000001"),
            "the refused duplicate must leave the confirmed number untouched"
        );
        assert_eq!(after.confirmed_at, confirmed.confirmed_at);
    }

    /// The same refusal for a Cancelled document: only a Draft may be confirmed,
    /// so a cancelled sale is refused too — and it is named as Cancelled, not
    /// Confirmed, because that is the state the row rests in.
    #[tokio::test]
    async fn set_confirmed_called_directly_on_a_cancelled_sale_is_refused_naming_cancelled() {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale =
            seed_sale_with_status(&pool, "Cancelled", "Annulled Buyer", d(2024, 5, 2), actor).await;

        let err = repo
            .set_confirmed(sale, actor, "2024-SALE-000003")
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
        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(after.sale_number, None, "the cancelled sale kept no number");
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
    // signature WAS hard-coded to `&SqlitePool`, because `set_confirmed` ran on
    // autocommit, exactly as `refuse_line`'s had to be changed when
    // `rewrite_draft_line_taxes` moved. It is a `&mut SqliteConnection` now
    // because `confirm` holds one unit and calls the helper on the connection it
    // was handed. Reverting it is a real regression and not a cosmetic one: a
    // migration that misses it passes every happy-path test in this file and
    // fails only the refusal ones, which is why there is a test below that
    // drives the refusal by NAME rather than incidentally.
    //
    // The doors are now WALKED. `confirm` opens one unit
    // (`src/services/sales.rs:1402`) and both of these writes run inside it, so
    // a failure anywhere in the confirm rolls back the number, the movements,
    // the `Income` and the payment with the confirmation. What is still true
    // here is only that no method on this repository opens a transaction across
    // a service call: each `_in` joins the caller's unit and each public twin
    // still opens one of its own, which the last test pins.

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
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale = seed_sale_with_status(&pool, "Draft", "In-Tx Buyer", d(2024, 5, 2), actor).await;
        // The account and the method are looked up BEFORE the unit opens: they
        // are fixture reads, and the pool is the only thing that can answer them.
        let (account, method) = account_and_method(&pool, actor).await;
        assert!(
            count(
                &pool,
                "SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?",
                sale
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
                sale,
                account,
                method,
                dec("10"),
                d(2024, 5, 2),
                None,
                None,
            )
            .await
            .expect("create_payment_in could not run while it held the caller's connection");
        // The RETURNING projection is the row the INSERT wrote, read inside the
        // same unit, so a payment that answered from a private connection would
        // have had to guess this id.
        assert_eq!(payment.sale_id, sale);
        assert_eq!(payment.account_id, account);
        assert_eq!(payment.method_id, method);
        assert_eq!(payment.amount, dec("10"));
        assert_eq!(payment.transaction_id, None);
        assert_eq!(payment.receipt_id, None);
        tx.rollback().await.unwrap();

        // Every assertion that touches the pool is AFTER the rollback, and it
        // says the row is not there. This is the assertion that fails if the
        // `_in` form committed a unit of its own.
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?", sale).await,
            0,
            "the payment survived a rollback of the transaction that created it, so create_payment_in opened and committed a unit of its own"
        );
        assert!(
            repo.list_payments(sale).await.unwrap().is_empty(),
            "list_payments still sees a payment the caller's rollback removed"
        );
        // And the unit left no residue on the document it named.
        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(after.status, crate::models::SaleStatus::Draft);
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
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale =
            seed_sale_with_status(&pool, "Draft", "Held-Conn Buyer", d(2024, 5, 3), actor).await;
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
                sale,
                account,
                method,
                dec("25"),
                d(2024, 5, 3),
                None,
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
                sale,
                account,
                method,
                dec("5"),
                d(2024, 5, 3),
                None,
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
                "SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?",
                sale
            )
            .await,
            0
        );
        assert!(repo
            .create_payment(
                actor,
                sale,
                account,
                method,
                dec("7"),
                d(2024, 5, 3),
                None,
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
    /// This is also the only test in the commit that would catch a `set_confirmed_in`
    /// which stamped the number through a private committed unit: the number
    /// would be visible on the pool the moment the statement returned, and the
    /// rollback could not take it back.
    #[tokio::test]
    async fn set_confirmed_in_stamps_the_number_in_the_callers_unit_and_a_rollback_leaves_the_draft_untouched(
    ) {
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale =
            seed_sale_with_status(&pool, "Draft", "Rollback Buyer", d(2024, 5, 4), actor).await;
        let before = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(before.status, crate::models::SaleStatus::Draft);
        assert_eq!(before.sale_number, None);
        assert_eq!(before.confirmed_at, None);

        let mut tx = pool.begin().await.unwrap();
        let confirmed = repo
            .set_confirmed_in(&mut tx, sale, actor, "2024-SALE-IN-0001")
            .await
            .expect("set_confirmed_in could not run while it held the caller's connection");
        // The read-back is the row the UPDATE wrote, seen through the SAME unit:
        // the number, the status and the stamp all belong to this transaction.
        assert_eq!(confirmed.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(confirmed.sale_number.as_deref(), Some("2024-SALE-IN-0001"));
        assert!(
            confirmed.confirmed_at.is_some(),
            "the confirmed document carries no confirmed_at stamp"
        );
        // While the unit is open the pool is still blind to all of it, which is
        // the other half: an answer identical to the committed one would mean the
        // write had already escaped the caller's transaction.
        tx.rollback().await.unwrap();

        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            crate::models::SaleStatus::Draft,
            "the document was confirmed by a unit the caller's rollback could not reach"
        );
        assert_eq!(
            after.sale_number, None,
            "the number survived a rollback of the transaction that stamped it"
        );
        assert_eq!(
            after.confirmed_at, None,
            "the confirmed_at stamp survived a rollback of the transaction that wrote it"
        );
        // And the draft is still confirmable, so the rollback restored the state
        // rather than corrupting the row.
        let again = repo
            .set_confirmed(sale, actor, "2024-SALE-IN-0002")
            .await
            .unwrap();
        assert_eq!(again.status, crate::models::SaleStatus::Confirmed);
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
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let sale =
            seed_sale_with_status(&pool, "Draft", "Held-Conn Buyer", d(2024, 5, 5), actor).await;

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
            .set_confirmed_in(&mut tx, sale, actor, "2024-SALE-HELD-1")
            .await;
        let elapsed = started.elapsed();
        let confirmed = confirmed.expect(
            "set_confirmed_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert_eq!(confirmed.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(confirmed.sale_number.as_deref(), Some("2024-SALE-HELD-1"));
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
            .set_confirmed_in(&mut tx, sale, actor, "2024-SALE-HELD-2")
            .await;
        assert!(
            matches!(refused, Err(AppError::Validation(_))),
            "the second confirm inside the same unit must be refused by the same DRAFT predicate, and must not be a driver error: {refused:?}"
        );
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM sales WHERE sale_number = ?", 0).await,
            0
        );
        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(after.status, crate::models::SaleStatus::Draft);
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
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        // A Confirmed row, seeded through raw SQL so the service's guard cannot
        // be what refuses it: the point is that the repository's OWN DRAFT
        // predicate is load-bearing inside the caller's transaction.
        let sale = seed_sale_with_status(
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
            .set_confirmed_in(&mut tx, sale, actor, "2024-SALE-DUP-1")
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
        let after = repo.find_sale(sale).await.unwrap().unwrap();
        assert_eq!(
            after.sale_number, None,
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
        let pool = migrated_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteSaleRepository::new(pool.clone());
        let (account, method) = account_and_method(&pool, actor).await;
        let draft =
            seed_sale_with_status(&pool, "Draft", "Public Buyer", d(2024, 5, 7), actor).await;

        // -- set_confirmed: the success path -------------------------------
        let confirmed = repo
            .set_confirmed(draft, actor, "2024-PUBLIC-1")
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(confirmed.sale_number.as_deref(), Some("2024-PUBLIC-1"));
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
            repo.find_sale(draft).await.unwrap().unwrap().sale_number,
            Some("2024-PUBLIC-1".to_string())
        );

        // -- set_confirmed: a frozen document -------------------------------
        let cancelled =
            seed_sale_with_status(&pool, "Cancelled", "Cancelled Buyer", d(2024, 5, 8), actor)
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
        let paid_draft = seed_sale_with_status(&pool, "Draft", "Payer", d(2024, 5, 9), actor).await;
        let payment = repo
            .create_payment(
                actor,
                paid_draft,
                account,
                method,
                dec("42.50"),
                d(2024, 5, 9),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(payment.sale_id, paid_draft);
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
                None,
            )
            .await
            .unwrap();
        assert_ne!(second.id, payment.id);
        assert_eq!(repo.list_payments(paid_draft).await.unwrap().len(), 2);
    }
}
