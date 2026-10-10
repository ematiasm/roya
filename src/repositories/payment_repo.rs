use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{
    NewPayment, NewPaymentAllocation, PartyDocumentKind, PartyType, Payment, PaymentAllocation,
};
use crate::repositories::checked_aggregate_sum;
use crate::services::checked_money_sum;
use crate::services::line_taxes::tax_inclusive_total;

/// One document per DELIVERY of money, plus the shares that say which documents
/// it covers (migration 46).
///
/// Three rules the trait carries, because they are the ones a caller can get
/// wrong:
///
/// - **The direction lives on the payment, never in a sign.** `amount` is a
///   magnitude and every allocation is positive; `In`/`Out` says which way the
///   money went. A read that applied a sign of its own would be a second opinion
///   about the same fact.
/// - **The cash movement belongs to the payment.** `transaction_id` is the single
///   `transactions` row (decision 5); an allocation has no account, method or
///   transaction, so applying money to a document later moves no cash.
/// - **The split is capped, and the SERVICE is the gate.** The schema carries an
///   `INSERT`-only trigger as a net against hand-written SQL, but its arithmetic is
///   `REAL` and this project folds money in `Decimal`, so the refusal an operator
///   receives comes from [`Self::allocate_in`], which sees the whole pair.
#[async_trait]
pub trait PaymentRepository: Send + Sync {
    /// Append one payment in a transaction of its own, for a caller with no larger
    /// unit. A payment is a document, not a journal: it has `_in` twins because
    /// every real caller already holds a unit.
    async fn create(&self, payment: &NewPayment) -> AppResult<Payment>;

    /// [`Self::create`] inside a transaction the CALLER owns. On this crate's
    /// `max_connections(1)` fixtures, reaching for the pool form from inside a unit
    /// is a `PoolTimedOut` deadlock rather than wrong data.
    async fn create_in(
        &self,
        tx: &mut SqliteConnection,
        payment: &NewPayment,
    ) -> AppResult<Payment>;

    async fn find_payment(&self, id: i64) -> AppResult<Option<Payment>>;

    /// The payments of one party, newest first — the list a party's page shows.
    async fn list_for_party(&self, party_type: PartyType, party_id: i64)
        -> AppResult<Vec<Payment>>;

    /// **THE CAP, home 1 of 2.** Create one share of `payment_id`, refusing when
    /// the shares would exceed what the payment delivered.
    ///
    /// The fold runs here rather than in the trigger for two measured reasons: it
    /// is `Decimal` (SQLite cannot fold `TEXT`, and the trigger's `CAST` to `REAL`
    /// is not the arithmetic the read uses), and this call sees the payment and its
    /// shares TOGETHER, which a trigger on one table cannot. The refusal is a
    /// `Validation` naming the figures — what an operator can act on.
    ///
    /// `amount` arrives positive; a non-positive share is refused before any write.
    async fn allocate_in(
        &self,
        tx: &mut SqliteConnection,
        allocation: &NewPaymentAllocation,
    ) -> AppResult<PaymentAllocation>;

    /// [`Self::allocate_in`] in a transaction of its own.
    async fn allocate(&self, allocation: &NewPaymentAllocation) -> AppResult<PaymentAllocation>;

    /// The deliveries grouped under one receipt, oldest first.
    ///
    /// This is what makes a receipt able to state how much money it grouped: before it,
    /// the only way was to sum its allocations, which silently drops the UNAPPLIED
    /// remainder — the credit — because no allocation carries it.
    async fn list_for_receipt(&self, receipt_id: i64) -> AppResult<Vec<Payment>>;

    /// Refund deliveries recorded in the party journal for one document. An
    /// unknown document has no matching entries and therefore returns an empty list.
    /// This is STRICT: it refuses if any Refund entry cannot be resolved, because a
    /// decision must not proceed on an under-counted reversal.
    async fn list_refunds_for_document(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<Vec<Payment>>;

    /// Refund deliveries that can be resolved for a display, plus the number of
    /// journal entries omitted because their reference did not resolve to a payment.
    /// Renderers must use this instead of the strict decision read: they can show
    /// the available rows without turning legacy history into a page failure, while
    /// `unresolved_entries` makes partial data observable (callers should log/report
    /// that count rather than silently presenting the rows as complete).
    async fn list_refunds_for_document_tolerant(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<RefundDeliveries>;

    /// Payments allocating to one document, oldest delivery first. The payment
    /// amount in each result is the share allocated to this target, and includes
    /// its account and method. The SQL order is deterministic by date then id so
    /// refund planning consumes the same order as the former party-wide scan.
    async fn list_allocating_to_target(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Vec<Payment>>;

    /// The number of refund deliveries for this document already reversed in the
    /// journal. This count pairs with `list_refunds_for_document`'s delivery count.
    async fn count_reversals_for_document(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<i64>;

    /// The shares of one payment, oldest first.
    async fn list_allocations(&self, payment_id: i64) -> AppResult<Vec<PaymentAllocation>>;

    /// `delivered − allocated` for one payment, as a checked fold.
    ///
    /// This is the number the business reads as the credit balance when it is
    /// positive, so it is stated as its own read rather than left to every caller
    /// to subtract. It refuses with [`crate::models::PriceRefusal::AggregateTooLarge`]
    /// when a set of bounded shares cannot be added up — a bounded share says
    /// nothing about the sum of a set of them.
    async fn unapplied_for_payment(&self, payment_id: i64) -> AppResult<Decimal>;

    /// How much of one document this party has already covered, across every
    /// payment. The per-document residual's other half (P5 reads the document's
    /// own total).
    async fn allocated_to_target(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal>;

    /// `charge + signed returns - allocations` for one Sale or Purchase. A negative
    /// residual is returned as evidence of inconsistency (the allocation cap should
    /// prevent it), not converted into an invented refusal.
    async fn residual_for_document(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal>;

    /// Return every component needed by document folds, including statements.
    /// The parts are retained rather than only their residual because a statement
    /// presents charge, returns, and allocations separately. The pool form wraps
    /// the caller-owned form; `_in` is available to reads participating in a
    /// caller's unit and sees its uncommitted document, return, and allocation rows.
    async fn residuals_for_documents(
        &self,
        target_kind: PartyDocumentKind,
        target_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, DocumentResidualParts>>;

    async fn residuals_for_documents_in(
        &self,
        tx: &mut SqliteConnection,
        target_kind: PartyDocumentKind,
        target_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, DocumentResidualParts>>;

    /// Caller-owned connection form: the allocation cap must see uncommitted rows.
    async fn residual_for_document_in(
        &self,
        tx: &mut SqliteConnection,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal>;

    /// Available credit for a party. Returns `AppResult<Decimal>` because when the
    /// total cannot be stated safely, both the party page's saldo a favor and the
    /// credit-limit projection must fail visibly, not show a partial balance. Each
    /// bounded remainder is safely subtracted and their sum is checked as
    /// `AggregateTooLarge`; bounded figures do not imply a bounded total. Only `In`
    /// payments count: an unallocated `Out` refund would otherwise count its full
    /// amount as false available credit.
    async fn unapplied_for_party(&self, party_type: PartyType, party_id: i64)
        -> AppResult<Decimal>;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolved journal deliveries for a display, with a count of omitted entries.
#[derive(Debug, Clone)]
pub struct RefundDeliveries {
    pub payments: Vec<Payment>,
    pub unresolved_entries: i64,
}

/// The auditable components of one Sale or Purchase's remaining balance.
/// `signed_returns` is already signed as stored in the party journal.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentResidualParts {
    pub charge: Decimal,
    pub signed_returns: Decimal,
    pub allocated: Decimal,
    pub residual: Decimal,
}

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap_or(Decimal::ZERO)
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> AppResult<Payment> {
    let direction: String = row.try_get("direction")?;
    let party_type: String = row.try_get("party_type")?;
    Ok(Payment {
        id: row.try_get("id")?,
        number: row.try_get("number")?,
        direction: direction
            .parse()
            .map_err(|e: String| AppError::Internal(format!("stored direction: {e}")))?,
        party_type: party_type
            .parse()
            .map_err(|e: String| AppError::Internal(format!("stored party type: {e}")))?,
        party_id: row.try_get("party_id")?,
        method_id: row.try_get("method_id")?,
        account_id: row.try_get("account_id")?,
        amount: parse_decimal(&row.try_get::<String, _>("amount")?),
        date: row.try_get("date")?,
        notes: row.try_get("notes")?,
        transaction_id: row.try_get("transaction_id")?,
        receipt_id: row.try_get("receipt_id")?,
        created_by: row.try_get("created_by")?,
        updated_by: row.try_get("updated_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_allocation(row: sqlx::sqlite::SqliteRow) -> AppResult<PaymentAllocation> {
    let target_kind: String = row.try_get("target_kind")?;
    Ok(PaymentAllocation {
        id: row.try_get("id")?,
        payment_id: row.try_get("payment_id")?,
        target_kind: target_kind
            .parse()
            .map_err(|e: String| AppError::Internal(format!("stored target kind: {e}")))?,
        target_id: row.try_get("target_id")?,
        amount: parse_decimal(&row.try_get::<String, _>("amount")?),
        created_by: row.try_get("created_by")?,
        updated_by: row.try_get("updated_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

const PAYMENT_COLUMNS: &str =
    "id, number, direction, party_type, party_id, method_id, account_id, \
     amount, date, notes, transaction_id, receipt_id, created_by, updated_by, created_at, \
     updated_at";

const ALLOCATION_COLUMNS: &str =
    "id, payment_id, target_kind, target_id, amount, created_by, updated_by, created_at, updated_at";

async fn insert_payment<'e, E>(executor: E, payment: &NewPayment) -> AppResult<Payment>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        r#"INSERT INTO payments
           (number, direction, party_type, party_id, method_id, account_id, amount, date, notes, transaction_id, receipt_id, created_by)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id, number, direction, party_type, party_id, method_id, account_id,
                     amount, date, notes, transaction_id, receipt_id, created_by, updated_by, created_at, updated_at"#,
    )
    // The number is NOT generated here: a caller inside a unit takes it from
    // `doc_sequences` with `next_number_in`, so a rollback returns the number
    // instead of burning it. This method writes whatever it is given.
    .bind(&payment.number)
    .bind(&payment.direction.as_str())
    .bind(payment.party_type.to_string())
    .bind(payment.party_id)
    .bind(payment.method_id)
    .bind(payment.account_id)
    .bind(payment.amount.to_string())
    .bind(payment.date)
    .bind(payment.notes.as_deref())
    .bind(payment.transaction_id)
    .bind(payment.receipt_id)
    .bind(payment.created_by)
    .fetch_one(executor)
    .await?;
    row_to_payment(row)
}

async fn list_allocations_raw<'e, E>(
    executor: E,
    payment_id: i64,
) -> AppResult<Vec<PaymentAllocation>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let sql = format!(
        "SELECT {ALLOCATION_COLUMNS} FROM payment_allocations WHERE payment_id = ? ORDER BY id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(payment_id)
        .fetch_all(executor)
        .await?;
    rows.into_iter().map(row_to_allocation).collect()
}

async fn allocated_to_target_raw<'e, E>(
    executor: E,
    target_kind: &str,
    target_id: i64,
) -> AppResult<Decimal>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT amount FROM payment_allocations WHERE target_kind = ? AND target_id = ? ORDER BY id",
    )
    .bind(target_kind)
    .bind(target_id)
    .fetch_all(executor)
    .await?;
    let amounts: Vec<Decimal> = rows.iter().map(|(amount,)| parse_decimal(amount)).collect();
    checked_aggregate_sum(amounts.iter()).map_err(AppError::PriceRefused)
}

/// The cap check, shared by every write path so the rule has one home.
///
/// Reads the payment and its shares on the SAME connection the caller holds, so
/// the pre-check sees the caller's own uncommitted rows — the property that makes
/// a write-then-check sequence sound inside one unit.
async fn assert_within_cap(
    tx: &mut SqliteConnection,
    payment_id: i64,
    target_kind: PartyDocumentKind,
    target_id: i64,
    new_amount: Decimal,
) -> AppResult<()> {
    let payment = sqlx::query("SELECT amount FROM payments WHERE id = ?")
        .bind(payment_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("payment {payment_id} not found")))?;
    let delivered = parse_decimal(&payment.try_get::<String, _>("amount")?);
    let existing = list_allocations_raw(&mut *tx, payment_id).await?;
    let allocated =
        checked_money_sum(existing.iter().map(|a| &a.amount)).map_err(AppError::PriceRefused)?;
    let future =
        checked_money_sum([allocated, new_amount].iter()).map_err(AppError::PriceRefused)?;
    if future > delivered {
        return Err(AppError::Validation(format!(
            "the allocations of payment {payment_id} would add up to {future}, more than the              {delivered} it delivered: apply at most {} more, or record a second payment.",
            delivered - allocated
        )));
    }

    // ---- THE OTHER CAP: the document's own residual -------------------------
    //
    // Decision 4 caps the SPLIT of a payment. This caps the OTHER end, and it is the
    // same argument applied one level down: the per-document residual that P5 reads
    // is `charge + returns - Σ allocations to it`, and without this it can go
    // NEGATIVE.
    //
    // Why that matters, measured rather than argued: a negative residual is not one
    // bad number, it is the document's whole accounting. `paid_and_due` computes
    // `due = total - paid` and does not refuse a negative; `payment_status_for` calls
    // `paid >= total` Paid; the refund cap reads `collected > 0 && total > collected`
    // and therefore stops refusing; and every debt filter (`due > ZERO`) makes the
    // document VANISH from the debtor list. So an over-allocation silently forgives a
    // debt and unlocks refunding more than came in.
    //
    // The consequence for the operator is the feature, not a limitation: paying more
    // than a document owes is allowed, and the excess stays UNAPPLIED on the payment —
    // which is exactly the credit the plan calls `unapplied > 0`.
    // `target_due` IS the headroom: it is the document's residual, which already nets
    // everything applied to it. Adding the shares again would count the same money
    // twice — and get it wrong in the direction that matters, refusing the operator who
    // is paying off the remainder of an invoice.
    let target_due = target_residual_due(&mut *tx, target_kind, target_id).await?;
    if new_amount > target_due {
        return Err(AppError::Validation(format!(
            "document {target_kind} {target_id} still owes {target_due}, so a share of              {new_amount} would leave it negative. Apply at most {target_due} to it, and leave              the rest unapplied on the payment."
        )));
    }
    Ok(())
}

/// The shared document residual machinery used by both reads and the allocation cap.
///
/// Each line is rounded through the tax feature's one tax-inclusive rule, then the
/// document's line totals are checked together. This mirrors the repository precedent
/// in `party_ledger_repo.rs`: the service owns the document amount, and SQL must never
/// sum money stored as TEXT through SQLite REAL arithmetic.
async fn residuals_for_documents_in(
    tx: &mut SqliteConnection,
    target_kind: PartyDocumentKind,
    target_ids: &[i64],
    query_tick: impl Fn(),
) -> AppResult<std::collections::BTreeMap<i64, DocumentResidualParts>> {
    use std::collections::BTreeMap;

    if target_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let (
        document_table,
        line_table,
        parent_column,
        price_column,
        return_table,
        return_column,
        allocation_kind,
    ) = match target_kind {
        PartyDocumentKind::Sale => (
            "sales",
            "sale_lines",
            "sale_id",
            "unit_price",
            "customer_returns",
            "sale_id",
            "Sale",
        ),
        PartyDocumentKind::Purchase => (
            "purchases",
            "purchase_lines",
            "purchase_id",
            "unit_cost",
            "purchase_returns",
            "purchase_id",
            "Purchase",
        ),
        PartyDocumentKind::CustomerReturn
        | PartyDocumentKind::PurchaseReturn
        | PartyDocumentKind::CustomerReceipt => {
            return Err(AppError::Validation(format!(
                "money cannot be applied to a {target_kind} document: returns reduce their parent, they are not collected against"
            )));
        }
    };

    // The parent LEFT JOIN both reads every requested document and distinguishes a
    // valid zero-line document from an unknown id, without spending a fourth query.
    let mut lines_qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
        "SELECT d.id AS document_id, l.qty, l.{price_column} AS unit_price, l.tax_total \
         FROM {document_table} d LEFT JOIN {line_table} l ON l.{parent_column} = d.id \
         WHERE d.id IN ("
    ));
    {
        let mut separated = lines_qb.separated(", ");
        for id in target_ids {
            separated.push_bind(*id);
        }
        separated.push_unseparated(") ORDER BY d.id, l.id");
    }
    let line_rows = lines_qb.build().fetch_all(&mut *tx).await?;
    query_tick();
    let mut totals: BTreeMap<i64, Vec<Decimal>> = BTreeMap::new();
    let mut found = BTreeMap::new();
    for row in line_rows {
        let document_id: i64 = row.try_get("document_id")?;
        found.insert(document_id, ());
        let qty: Option<String> = row.try_get("qty")?;
        if let Some(qty) = qty {
            let price: String = row.try_get("unit_price")?;
            let tax: String = row.try_get("tax_total")?;
            let net = parse_decimal(&qty) * parse_decimal(&price);
            totals
                .entry(document_id)
                .or_default()
                .push(tax_inclusive_total(net, parse_decimal(&tax)));
        }
    }
    for id in target_ids {
        if !found.contains_key(id) {
            let name = if target_kind == PartyDocumentKind::Sale {
                "sale"
            } else {
                "purchase"
            };
            return Err(AppError::NotFound(format!("{name} {id} not found")));
        }
    }
    let mut parts = BTreeMap::new();
    for id in target_ids {
        let charge = checked_money_sum(totals.get(id).into_iter().flatten())
            .map_err(AppError::PriceRefused)?;
        parts.insert(
            *id,
            DocumentResidualParts {
                charge,
                signed_returns: Decimal::ZERO,
                allocated: Decimal::ZERO,
                residual: Decimal::ZERO,
            },
        );
    }

    let mut returns_qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
        "SELECT r.{return_column} AS target_id, e.amount FROM party_ledger_entries e \
         JOIN {return_table} r ON r.id = e.document_id \
         WHERE e.kind = 'Return' AND e.document_kind = "
    ));
    returns_qb
        .push_bind(target_kind_return_name(target_kind))
        .push(format!(" AND r.{return_column} IN ("));
    {
        let mut separated = returns_qb.separated(", ");
        for id in target_ids {
            separated.push_bind(*id);
        }
        separated.push_unseparated(") ORDER BY e.id");
    }
    let return_rows = returns_qb.build().fetch_all(&mut *tx).await?;
    query_tick();
    let mut returns_by_target: BTreeMap<i64, Vec<Decimal>> = BTreeMap::new();
    for row in return_rows {
        let target_id: i64 = row.try_get("target_id")?;
        let amount: String = row.try_get("amount")?;
        returns_by_target
            .entry(target_id)
            .or_default()
            .push(parse_decimal(&amount));
    }

    let mut allocations_qb: QueryBuilder<Sqlite> =
        QueryBuilder::new("SELECT target_id, amount FROM payment_allocations WHERE target_kind = ");
    allocations_qb
        .push_bind(allocation_kind)
        .push(" AND target_id IN (");
    {
        let mut separated = allocations_qb.separated(", ");
        for id in target_ids {
            separated.push_bind(*id);
        }
        separated.push_unseparated(") ORDER BY id");
    }
    let allocation_rows = allocations_qb.build().fetch_all(&mut *tx).await?;
    query_tick();
    let mut allocations_by_target: BTreeMap<i64, Vec<Decimal>> = BTreeMap::new();
    for row in allocation_rows {
        let target_id: i64 = row.try_get("target_id")?;
        let amount: String = row.try_get("amount")?;
        allocations_by_target
            .entry(target_id)
            .or_default()
            .push(parse_decimal(&amount));
    }

    for (id, item) in &mut parts {
        item.signed_returns =
            checked_aggregate_sum(returns_by_target.get(id).into_iter().flatten())
                .map_err(AppError::PriceRefused)?;
        item.allocated = checked_aggregate_sum(allocations_by_target.get(id).into_iter().flatten())
            .map_err(AppError::PriceRefused)?;
        let after_returns = checked_money_sum([item.charge, item.signed_returns].iter())
            .map_err(AppError::PriceRefused)?;
        item.residual = after_returns.checked_sub(item.allocated).ok_or_else(|| {
            AppError::PriceRefused(crate::models::PriceRefusal::DocumentTotalTooLarge)
        })?;
    }
    Ok(parts)
}

fn target_kind_return_name(target_kind: PartyDocumentKind) -> &'static str {
    match target_kind {
        PartyDocumentKind::Sale => "CustomerReturn",
        PartyDocumentKind::Purchase => "PurchaseReturn",
        PartyDocumentKind::CustomerReturn
        | PartyDocumentKind::PurchaseReturn
        | PartyDocumentKind::CustomerReceipt => unreachable!(),
    }
}

async fn residual_for_document_in(
    tx: &mut SqliteConnection,
    target_kind: PartyDocumentKind,
    target_id: i64,
) -> AppResult<Decimal> {
    residuals_for_documents_in(tx, target_kind, &[target_id], || {})
        .await?
        .get(&target_id)
        .map(|parts| parts.residual)
        .ok_or_else(|| AppError::Internal("single-document residual missing from batch".into()))
}

async fn target_residual_due(
    tx: &mut SqliteConnection,
    target_kind: PartyDocumentKind,
    target_id: i64,
) -> AppResult<Decimal> {
    residual_for_document_in(tx, target_kind, target_id).await
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SqlitePaymentRepository {
    pub pool: SqlitePool,
    /// Test-only counter for the batch machinery's actual SQL reads.
    #[cfg(test)]
    batch_queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SqlitePaymentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            #[cfg(test)]
            batch_queries: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    fn count_batch_query(&self) {
        self.batch_queries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn reset_batch_query_count(&self) {
        self.batch_queries
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn batch_query_count(&self) -> usize {
        self.batch_queries.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl PaymentRepository for SqlitePaymentRepository {
    async fn create(&self, payment: &NewPayment) -> AppResult<Payment> {
        let mut tx = self.pool.begin().await?;
        let created = self.create_in(&mut tx, payment).await?;
        tx.commit().await?;
        Ok(created)
    }

    async fn create_in(
        &self,
        tx: &mut SqliteConnection,
        payment: &NewPayment,
    ) -> AppResult<Payment> {
        insert_payment(&mut *tx, payment).await
    }

    async fn find_payment(&self, id: i64) -> AppResult<Option<Payment>> {
        let sql = format!("SELECT {PAYMENT_COLUMNS} FROM payments WHERE id = ?");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_payment).transpose()
    }

    async fn list_for_party(
        &self,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Vec<Payment>> {
        let sql = format!(
            "SELECT {PAYMENT_COLUMNS} FROM payments WHERE party_type = ? AND party_id = ? \
             ORDER BY date DESC, id DESC"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(party_type.to_string())
            .bind(party_id)
            .fetch_all(&self.pool)
            .await?;
        #[cfg(test)]
        self.count_batch_query();
        rows.into_iter().map(row_to_payment).collect()
    }

    async fn allocate(&self, allocation: &NewPaymentAllocation) -> AppResult<PaymentAllocation> {
        let mut tx = self.pool.begin().await?;
        let created = self.allocate_in(&mut tx, allocation).await?;
        tx.commit().await?;
        Ok(created)
    }

    async fn allocate_in(
        &self,
        tx: &mut SqliteConnection,
        allocation: &NewPaymentAllocation,
    ) -> AppResult<PaymentAllocation> {
        // A share is a positive magnitude. Refused here rather than relying on the
        // schema's `CAST(… AS REAL) <= 0` trigger, which cannot tell a malformed
        // figure from a zero one and is the net rather than the rule.
        if allocation.amount <= Decimal::ZERO {
            return Err(AppError::Validation(
                "an allocation amount must be positive".into(),
            ));
        }
        assert_within_cap(
            &mut *tx,
            allocation.payment_id,
            allocation.target_kind,
            allocation.target_id,
            allocation.amount,
        )
        .await?;
        let row = sqlx::query(
            r#"INSERT INTO payment_allocations
               (payment_id, target_kind, target_id, amount, created_by)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, payment_id, target_kind, target_id, amount, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(allocation.payment_id)
        .bind(allocation.target_kind.to_string())
        .bind(allocation.target_id)
        .bind(allocation.amount.to_string())
        .bind(allocation.created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            let s = e.to_string();
            if s.contains("UNIQUE constraint failed") {
                AppError::Conflict(
                    "this payment already has a share of that document".into(),
                )
            } else {
                AppError::Database(e)
            }
        })?;
        row_to_allocation(row)
    }

    async fn list_for_receipt(&self, receipt_id: i64) -> AppResult<Vec<Payment>> {
        let sql =
            format!("SELECT {PAYMENT_COLUMNS} FROM payments WHERE receipt_id = ? ORDER BY id");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(receipt_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_payment).collect()
    }

    async fn list_refunds_for_document(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<Vec<Payment>> {
        let qualified_columns = PAYMENT_COLUMNS
            .split(", ")
            .map(|column| format!("p.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {qualified_columns} FROM party_ledger_entries e \
             JOIN payments p ON p.number = e.reference \
             WHERE e.kind = 'Refund' AND e.document_kind = ? AND e.document_id = ? \
             ORDER BY e.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(document_kind.to_string())
            .bind(document_id)
            .fetch_all(&self.pool)
            .await?;
        let resolved_count = rows.len();
        let entry_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM party_ledger_entries \
             WHERE kind = 'Refund' AND document_kind = ? AND document_id = ?",
        )
        .bind(document_kind.to_string())
        .bind(document_id)
        .fetch_one(&self.pool)
        .await?;
        if entry_count != resolved_count as i64 {
            return Err(AppError::Internal(format!(
                "internal inconsistency for {document_kind} {document_id}: {entry_count} Refund journal entries but {resolved_count} payment deliveries resolved; a Refund entry reference may not be a payment number (as produced by the T1 backfill, which uses the document number). This is not an operator error."
            )));
        }
        rows.into_iter().map(row_to_payment).collect()
    }

    async fn list_refunds_for_document_tolerant(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<RefundDeliveries> {
        let qualified_columns = PAYMENT_COLUMNS
            .split(", ")
            .map(|column| format!("p.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {qualified_columns} FROM party_ledger_entries e \
             JOIN payments p ON p.number = e.reference \
             WHERE e.kind = 'Refund' AND e.document_kind = ? AND e.document_id = ? \
             ORDER BY e.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(document_kind.to_string())
            .bind(document_id)
            .fetch_all(&self.pool)
            .await?;
        let entry_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM party_ledger_entries \
             WHERE kind = 'Refund' AND document_kind = ? AND document_id = ?",
        )
        .bind(document_kind.to_string())
        .bind(document_id)
        .fetch_one(&self.pool)
        .await?;
        let resolved_count = i64::try_from(rows.len())
            .map_err(|_| AppError::Internal("refund result count exceeded i64".into()))?;
        let unresolved_entries = entry_count.checked_sub(resolved_count).ok_or_else(|| {
            AppError::Internal(format!(
                "resolved Refund rows exceed journal entry count for {document_kind} {document_id}"
            ))
        })?;
        Ok(RefundDeliveries {
            payments: rows
                .into_iter()
                .map(row_to_payment)
                .collect::<AppResult<_>>()?,
            unresolved_entries,
        })
    }

    async fn list_allocating_to_target(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Vec<Payment>> {
        let qualified_columns = PAYMENT_COLUMNS
            .split(", ")
            .map(|column| format!("p.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {qualified_columns}, a.amount AS target_amount \
             FROM payment_allocations a \
             JOIN payments p ON p.id = a.payment_id \
             WHERE a.target_kind = ? AND a.target_id = ? \
             ORDER BY p.date, p.id, a.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(target_kind.to_string())
            .bind(target_id)
            .fetch_all(&self.pool)
            .await?;
        #[cfg(test)]
        self.count_batch_query();
        let mut payments = Vec::with_capacity(rows.len());
        for row in rows {
            let amount = parse_decimal(&row.try_get::<String, _>("target_amount")?);
            let mut payment = row_to_payment(row)?;
            payment.amount = amount;
            payments.push(payment);
        }
        Ok(payments)
    }

    async fn count_reversals_for_document(
        &self,
        document_kind: PartyDocumentKind,
        document_id: i64,
    ) -> AppResult<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM party_ledger_entries \
             WHERE kind = 'Payment' AND document_kind = ? AND document_id = ?",
        )
        .bind(document_kind.to_string())
        .bind(document_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn list_allocations(&self, payment_id: i64) -> AppResult<Vec<PaymentAllocation>> {
        let allocations = list_allocations_raw(&self.pool, payment_id).await?;
        #[cfg(test)]
        self.count_batch_query();
        Ok(allocations)
    }

    async fn unapplied_for_payment(&self, payment_id: i64) -> AppResult<Decimal> {
        let payment = self
            .find_payment(payment_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("payment {payment_id} not found")))?;
        let allocated = checked_aggregate_sum(
            self.list_allocations(payment_id)
                .await?
                .iter()
                .map(|a| &a.amount),
        )
        .map_err(AppError::PriceRefused)?;
        // The subtraction itself is checked: `delivered` and `allocated` are both
        // bounded, and a raw `-` on `Decimal` panics on overflow, so the residual
        // is computed with the same care as the fold above.
        payment
            .amount
            .checked_sub(allocated)
            .ok_or_else(|| AppError::PriceRefused(crate::models::PriceRefusal::AggregateTooLarge))
    }

    async fn allocated_to_target(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal> {
        allocated_to_target_raw(&self.pool, &target_kind.to_string(), target_id).await
    }

    async fn residuals_for_documents(
        &self,
        target_kind: PartyDocumentKind,
        target_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, DocumentResidualParts>> {
        let mut tx = self.pool.begin().await?;
        let result = self
            .residuals_for_documents_in(&mut tx, target_kind, target_ids)
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn residuals_for_documents_in(
        &self,
        tx: &mut SqliteConnection,
        target_kind: PartyDocumentKind,
        target_ids: &[i64],
    ) -> AppResult<std::collections::BTreeMap<i64, DocumentResidualParts>> {
        residuals_for_documents_in(tx, target_kind, target_ids, || {
            #[cfg(test)]
            self.count_batch_query();
        })
        .await
    }

    async fn residual_for_document(
        &self,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal> {
        let mut tx = self.pool.begin().await?;
        let residual = residual_for_document_in(&mut tx, target_kind, target_id).await?;
        tx.commit().await?;
        Ok(residual)
    }

    async fn residual_for_document_in(
        &self,
        tx: &mut SqliteConnection,
        target_kind: PartyDocumentKind,
        target_id: i64,
    ) -> AppResult<Decimal> {
        residual_for_document_in(tx, target_kind, target_id).await
    }

    async fn unapplied_for_party(
        &self,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Decimal> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, amount FROM payments \
             WHERE party_type = ? AND party_id = ? AND direction = 'In' ORDER BY id",
        )
        .bind(party_type.to_string())
        .bind(party_id)
        .fetch_all(&self.pool)
        .await?;
        let mut remainders = Vec::with_capacity(rows.len());
        for (payment_id, amount) in rows {
            let delivered = parse_decimal(&amount);
            let allocations = list_allocations_raw(&self.pool, payment_id).await?;
            let allocated = checked_aggregate_sum(allocations.iter().map(|a| &a.amount))
                .map_err(AppError::PriceRefused)?;
            let remainder = delivered.checked_sub(allocated).ok_or_else(|| {
                AppError::PriceRefused(crate::models::PriceRefusal::AggregateTooLarge)
            })?;
            remainders.push(remainder);
        }
        checked_aggregate_sum(remainders.iter()).map_err(AppError::PriceRefused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PaymentDirection;
    use crate::security::test_support;
    use chrono::NaiveDate;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn test_pool() -> SqlitePool {
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

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// A process-wide counter for fixture names that are UNIQUE in the schema
    /// (`products.sku`). Not derived from any table: the point is to be unique even
    /// when nothing has been written yet.
    fn next_fixture_seq() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(1);
        SEQ.fetch_add(1, Ordering::Relaxed)
    }

    /// The fixture's own counter for the UNIQUE `number`. It does not use
    /// `doc_sequences` because this module tests the REPOSITORY: the sequence has
    /// its own tests, and the service is what joins the two in P3.
    async fn next_test_seq(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(MAX(CAST(substr(number, 10) AS INTEGER)), 0) + 1 FROM payments",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A payment owned by the seeded `Caja` through the seeded `Cash`, so the
    /// inherited migration-44 guard is satisfied by construction.
    async fn seed_payment(pool: &SqlitePool, amount: &str) -> i64 {
        let actor = test_support::audit_actor_id(pool).await.unwrap();
        let repo = SqlitePaymentRepository::new(pool.clone());
        let account: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(pool)
            .await
            .unwrap();
        let method: i64 = sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE account_id = ? AND name = 'Cash'",
        )
        .bind(account)
        .fetch_one(pool)
        .await
        .unwrap();
        let payment = repo
            .create(&NewPayment {
                receipt_id: None,
                // The number comes from `doc_sequences` in production; the fixture
                // writes the shape the sequence produces, per payment so the UNIQUE
                // index is exercised rather than bypassed.
                number: format!("2024-PAY-{:06}", next_test_seq(&pool).await),
                direction: PaymentDirection::In,
                party_type: PartyType::Customer,
                party_id: 1,
                method_id: method,
                account_id: account,
                amount: Decimal::from_str(amount).unwrap(),
                date: d(2024, 5, 1),
                notes: None,
                transaction_id: None,
                created_by: actor,
            })
            .await
            .unwrap();
        payment.id
    }

    /// A confirmed sale owing `total`, with one line and stock so nothing else
    /// complains. The document family is real: `target_residual_due` reads its lines.
    async fn seed_confirmed_sale(pool: &SqlitePool, actor: i64, total: &str) -> (i64, i64) {
        // A SKU per call: `products.sku` is UNIQUE and these tests seed several sales.
        // A process-wide counter rather than the payments sequence: that one only moves
        // when a payment is created, so two sales between payments collided.
        let sku = format!("CAP-{}", next_fixture_seq());
        let product: i64 = sqlx::query_scalar(
            "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by) \
             VALUES (?, 'Cap', 'Product', 'unit', '1', 1, ?) RETURNING id",
        )
        .bind(&sku)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO stock_movements (product_id, qty, type, reason, reference, date, created_by) \
             VALUES (?, '100', 'In', 'Initial', '', '2024-05-01', ?)",
        )
        .bind(product)
        .bind(actor)
        .execute(pool)
        .await
        .unwrap();
        let customer: i64 = sqlx::query_scalar(
            "INSERT INTO customers (name, created_by) VALUES ('Cap Buyer', ?) RETURNING id",
        )
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        let sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by) \
             VALUES ('Confirmed', 'Credit', ?, 'Cap Buyer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        // One line whose `qty * unit_price` is exactly `total`, no tax.
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total) \
             VALUES (?, ?, ?, '1', '0')",
        )
        .bind(sale)
        .bind(product)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
        (sale, product)
    }

    fn allocation(payment_id: i64, target_id: i64, amount: &str) -> NewPaymentAllocation {
        NewPaymentAllocation {
            payment_id,
            target_kind: PartyDocumentKind::Sale,
            target_id,
            amount: Decimal::from_str(amount).unwrap(),
            created_by: 1,
        }
    }

    #[tokio::test]
    async fn a_payment_round_trips_with_its_direction_and_party() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let stored = repo.find_payment(id).await.unwrap().unwrap();
        assert_eq!(stored.direction, PaymentDirection::In);
        assert_eq!(stored.party_type, PartyType::Customer);
        assert_eq!(stored.amount, Decimal::from_str("100").unwrap());
        assert!(stored.number.ends_with("000001"), "got {}", stored.number);
        assert!(
            stored.transaction_id.is_none(),
            "finance is a separate step"
        );
    }

    /// The cap, and the reason it lives in the service rather than only in the
    /// trigger: this sees the whole pair and folds in `Decimal`.
    #[tokio::test]
    async fn allocations_may_reach_the_amount_but_never_exceed_it() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        // Three REAL sales, because the cap reads each document's own residual: a
        // fictional target is now a 404, which is the point of the second cap.
        let (s1, _) = seed_confirmed_sale(&pool, actor, "60").await;
        let (s2, _) = seed_confirmed_sale(&pool, actor, "40").await;
        let (s3, _) = seed_confirmed_sale(&pool, actor, "1").await;

        repo.allocate(&allocation(id, s1, "60")).await.unwrap();
        repo.allocate(&allocation(id, s2, "40")).await.unwrap();
        let err = repo.allocate(&allocation(id, s3, "1")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("101"),
            "the refusal states what it would reach: {msg}"
        );
        assert!(msg.contains("100"), "and what was delivered: {msg}");
        assert!(msg.contains("0 more"), "and the headroom left: {msg}");
    }

    /// `unapplied = delivered − allocated`, which is the number the business reads
    /// as the credit balance.
    #[tokio::test]
    async fn unapplied_is_the_checked_residual() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        assert_eq!(
            repo.unapplied_for_payment(id).await.unwrap(),
            Decimal::from_str("100").unwrap(),
            "nothing allocated yet: the whole delivery is unapplied"
        );
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "30").await;
        repo.allocate(&allocation(id, sale, "30")).await.unwrap();
        assert_eq!(
            repo.unapplied_for_payment(id).await.unwrap(),
            Decimal::from_str("70").unwrap()
        );
    }

    /// A share is a positive magnitude. The service refuses it before writing, so
    /// the message is one an operator can act on rather than a driver abort.
    #[tokio::test]
    async fn a_non_positive_share_is_refused_before_any_write() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        for bad in ["0", "-5"] {
            let err = repo.allocate(&allocation(id, sale, bad)).await.unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        assert!(repo.list_allocations(id).await.unwrap().is_empty());
    }

    /// One share per (payment, document): naming the same document twice would be
    /// two shares of one residual, which has no meaning.
    #[tokio::test]
    async fn the_same_document_cannot_be_shared_twice() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        repo.allocate(&allocation(id, sale, "30")).await.unwrap();
        let err = repo.allocate(&allocation(id, sale, "1")).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
    }

    /// The cap sees the caller's OWN uncommitted shares, which is what makes the
    /// pre-check sound inside a unit. If the read went to the pool instead, a unit
    /// writing two shares in sequence would re-read an empty set each time and the
    /// cap would not exist — the same class of bug the `_in` convention removes.
    #[tokio::test]
    async fn the_cap_inside_a_unit_sees_that_units_own_uncommitted_shares() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "50").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (s1, _) = seed_confirmed_sale(&pool, actor, "30").await;
        let (s2, _) = seed_confirmed_sale(&pool, actor, "30").await;

        let mut tx = pool.begin().await.unwrap();
        repo.allocate_in(&mut tx, &allocation(id, s1, "30"))
            .await
            .unwrap();
        let err = repo
            .allocate_in(&mut tx, &allocation(id, s2, "30"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::Validation(_)),
            "the second share must be measured against the first, got {err:?}"
        );
        tx.rollback().await.unwrap();
        assert!(repo.list_allocations(id).await.unwrap().is_empty());
    }

    /// The inherited migration-44 guard: the pair on a payment is refused by the
    /// schema, exactly as on the three tables migration 44 covers.
    #[tokio::test]
    async fn a_payment_naming_another_accounts_method_is_refused() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cash: i64 = sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE account_id = ? AND name = 'Cash'",
        )
        .bind(caja)
        .fetch_one(&pool)
        .await
        .unwrap();
        let other: i64 = sqlx::query_scalar(
            "INSERT INTO accounts (name, created_by) VALUES ('other box', ?) RETURNING id",
        )
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();

        let repo = SqlitePaymentRepository::new(pool.clone());
        let err = repo
            .create(&NewPayment {
                receipt_id: None,
                number: "2024-PAY-900001".into(),
                direction: PaymentDirection::In,
                party_type: PartyType::Customer,
                party_id: 1,
                method_id: cash,
                account_id: other,
                amount: Decimal::from_str("10").unwrap(),
                date: d(2024, 5, 1),
                notes: None,
                transaction_id: None,
                created_by: actor,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not belong to the named account"),
            "the inherited guard must refuse the pair, got {err}"
        );
    }

    /// The immutability that closes the cap's blind spot, and the edge that makes
    /// it sane: a payment with NO shares can still be corrected.
    #[tokio::test]
    async fn a_payments_amount_freezes_once_it_has_allocations() {
        let pool = test_pool().await;
        let free = seed_payment(&pool, "50").await;
        let frozen = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (target, _) = seed_confirmed_sale(&pool, actor, "100").await;
        repo.allocate(&allocation(frozen, target, "70"))
            .await
            .unwrap();

        sqlx::query("UPDATE payments SET amount = '55' WHERE id = ?")
            .bind(free)
            .execute(&pool)
            .await
            .expect("a payment with no shares has no residual to protect");

        let err = sqlx::query("UPDATE payments SET amount = '10' WHERE id = ?")
            .bind(frozen)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot change once it has allocations"),
            "got {err}"
        );
        // And the residual is still sound: the edit could not drive it negative.
        assert_eq!(
            repo.unapplied_for_payment(frozen).await.unwrap(),
            Decimal::from_str("30").unwrap()
        );
    }

    /// How much of one document is already covered, across every payment — the
    /// read P5 builds the per-document residual on.
    /// **A payment LARGER than the document it is applied to.**
    ///
    /// This is the case the customer-side refusals used to forbid, and the reason they
    /// could be lifted only with a cap at the OTHER end. The document must not go
    /// negative: the share is capped at what the sale still owes, and the excess stays
    /// UNAPPLIED on the payment — which is the credit the plan calls `unapplied > 0`.
    ///
    /// The failure this pins is not cosmetic. A negative residual makes
    /// `paid_and_due` report a negative `due` (it does not refuse), makes
    /// `payment_status_for` say Paid, makes the refund cap stop refusing
    /// (`collected > 0 && total > collected` is false once collected exceeds the
    /// total) and makes every `due > ZERO` filter drop the document from the debtor
    /// list — one bad share, the whole accounting of that document.
    #[tokio::test]
    async fn a_share_cannot_exceed_what_the_document_still_owes() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        // A sale of 30, confirmed, so it owes 30 and has no payments yet.
        let (sale, _) = seed_confirmed_sale(&pool, actor, "30").await;
        // A delivery of 50: MORE than the sale owes.
        let payment = seed_payment(&pool, "50").await;
        let repo = SqlitePaymentRepository::new(pool.clone());

        // 30 is accepted: it is exactly the residual.
        repo.allocate(&allocation(payment, sale, "30"))
            .await
            .unwrap();

        // Asking for one more cent on the SAME payment and document is refused, and
        // the CAP is what refuses it: the document has nothing left, so the pre-check
        // fires before the `UNIQUE(payment, document)` index ever sees the row. The
        // order is the honest one — a cap question is answered before a uniqueness
        // question, because "there is nothing left to apply" is the useful message.
        let err = repo
            .allocate(&allocation(payment, sale, "0.01"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // The UNIQUE index still owns its own case, proven where the cap cannot
        // answer first: a document with money left.
        let (roomy, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let third = seed_payment(&pool, "100").await;
        repo.allocate(&allocation(third, roomy, "10"))
            .await
            .unwrap();
        let err = repo
            .allocate(&allocation(third, roomy, "5"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::Conflict(_)),
            "one share per (payment, document) is the UNIQUE index's rule: {err:?}"
        );

        // THE CAP, measured where it is the only thing that can refuse: a SECOND
        // payment, with plenty of its own headroom, trying to over-apply a document
        // that is already settled. Its own cap passes (1 <= 50), so if this is refused
        // it is refused by the DOCUMENT's residual and nothing else.
        let second = seed_payment(&pool, "50").await;
        let err = repo
            .allocate(&allocation(second, sale, "1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("owes 0"), "names what it still owes: {msg}");
        assert!(
            msg.contains("unapplied"),
            "and tells the operator the excess stays on the payment: {msg}"
        );

        // The document did NOT go negative, and the second payment is ALL credit.
        assert_eq!(
            repo.allocated_to_target(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("30").unwrap()
        );
        assert_eq!(
            repo.unapplied_for_payment(second).await.unwrap(),
            Decimal::from_str("50").unwrap(),
            "a payment with nothing left to apply is entirely credit"
        );
        // And the FIRST payment, which delivered 50 and applied 30, holds 20 as credit:
        // the number the receipt now reports and no allocation carries.
        assert_eq!(
            repo.unapplied_for_payment(payment).await.unwrap(),
            Decimal::from_str("20").unwrap(),
            "the excess of the first delivery is its credit"
        );
        assert_eq!(
            repo.unapplied_for_payment(payment).await.unwrap(),
            Decimal::from_str("20").unwrap(),
            "the 20 the sale did not need is the credit, held by the PAYMENT"
        );
    }

    /// Allocating to a document that has no lines and does not exist is a caller's
    /// mistake, not a cap question: a 404, not a `Validation` about money.
    #[tokio::test]
    async fn allocating_to_a_document_that_does_not_exist_is_not_found() {
        let pool = test_pool().await;
        let payment = seed_payment(&pool, "50").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let err = repo
            .allocate(&allocation(payment, 999_999, "10"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    /// A return is not something money is applied to: a refund replays a parent
    /// payment and a credit note reduces a sale, so allocating to one is refused
    /// rather than guessed at.
    #[tokio::test]
    async fn money_cannot_be_applied_to_a_return_document() {
        let pool = test_pool().await;
        let payment = seed_payment(&pool, "50").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let err = repo
            .allocate(&NewPaymentAllocation {
                payment_id: payment,
                target_kind: PartyDocumentKind::CustomerReturn,
                target_id: 1,
                amount: Decimal::from_str("10").unwrap(),
                created_by: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("returns reduce their parent"),
            "the refusal must say why: {err}"
        );
    }

    #[tokio::test]
    async fn allocated_to_target_sums_across_payments() {
        let pool = test_pool().await;
        let a = seed_payment(&pool, "100").await;
        let b = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        repo.allocate(&allocation(a, sale, "30")).await.unwrap();
        repo.allocate(&allocation(b, sale, "25")).await.unwrap();
        assert_eq!(
            repo.allocated_to_target(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("55").unwrap()
        );
        assert_eq!(
            repo.allocated_to_target(PartyDocumentKind::Sale, 99)
                .await
                .unwrap(),
            Decimal::ZERO,
            "a document nobody allocated to is not an error"
        );
    }

    /// The two shapes share ONE copy of the statement, and the pool form is the
    /// unit form wrapped in BEGIN/COMMIT — the property that stops the two from
    /// drifting on how a row is written.
    #[tokio::test]
    async fn residual_for_document_starts_at_the_sale_total_and_falls_by_allocations() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        assert_eq!(
            repo.residual_for_document(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("100").unwrap()
        );

        let payment = seed_payment(&pool, "40").await;
        repo.allocate(&allocation(payment, sale, "37"))
            .await
            .unwrap();
        assert_eq!(
            repo.residual_for_document(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("63").unwrap()
        );

        // The one-connection fixture proves this read stays on the caller's unit:
        // see a share written in this still-open transaction without asking the pool.
        let another_payment = seed_payment(&pool, "10").await;
        let mut tx = pool.begin().await.unwrap();
        repo.allocate_in(&mut tx, &allocation(another_payment, sale, "10"))
            .await
            .unwrap();
        assert_eq!(
            repo.residual_for_document_in(&mut tx, PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("53").unwrap()
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn a_confirmed_credit_note_return_lowers_its_parent_sale_residual() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let customer: i64 = sqlx::query_scalar("SELECT customer_id FROM sales WHERE id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        let returned: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by) \
             VALUES (?, ?, 'Confirmed', '2024-05-02', ?) RETURNING id",
        )
        .bind(customer)
        .bind(sale)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Mirrors customer_return.rs:697: one signed Return ledger entry names the return document.
        sqlx::query(
            "INSERT INTO party_ledger_entries \
             (party_type, party_id, kind, amount, document_kind, document_id, entry_date, created_by) \
             VALUES ('Customer', ?, 'Return', '-25', 'CustomerReturn', ?, '2024-05-02', ?)",
        )
        .bind(customer)
        .bind(returned)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();

        let repo = SqlitePaymentRepository::new(pool.clone());
        assert_eq!(
            repo.residual_for_document(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("75").unwrap()
        );
    }

    #[tokio::test]
    async fn an_out_refund_does_not_count_as_available_credit_for_the_party() {
        let pool = test_pool().await;
        let payment = seed_payment(&pool, "45").await;
        sqlx::query("UPDATE payments SET direction = 'Out' WHERE id = ?")
            .bind(payment)
            .execute(&pool)
            .await
            .unwrap();
        let repo = SqlitePaymentRepository::new(pool.clone());
        assert_eq!(
            repo.unapplied_for_party(PartyType::Customer, 1)
                .await
                .unwrap(),
            Decimal::ZERO,
            "an unallocated refund is not credit held for the party"
        );
    }

    #[tokio::test]
    async fn unapplied_for_party_sums_unallocated_in_payments() {
        let pool = test_pool().await;
        seed_payment(&pool, "12.5").await;
        seed_payment(&pool, "7.25").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        assert_eq!(
            repo.unapplied_for_party(PartyType::Customer, 1)
                .await
                .unwrap(),
            Decimal::from_str("19.75").unwrap()
        );
    }

    #[tokio::test]
    async fn allocation_cap_uses_credit_note_reduced_residual_without_widening_it() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let customer: i64 = sqlx::query_scalar("SELECT customer_id FROM sales WHERE id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        let returned: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by) \
             VALUES (?, ?, 'Confirmed', '2024-05-02', ?) RETURNING id",
        )
        .bind(customer)
        .bind(sale)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Mirrors customer_return.rs:697: the signed ledger entry is linked to the return.
        sqlx::query(
            "INSERT INTO party_ledger_entries \
             (party_type, party_id, kind, amount, document_kind, document_id, entry_date, created_by) \
             VALUES ('Customer', ?, 'Return', '-25', 'CustomerReturn', ?, '2024-05-02', ?)",
        )
        .bind(customer)
        .bind(returned)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();

        let repo = SqlitePaymentRepository::new(pool.clone());
        let payment = seed_payment(&pool, "100").await;
        repo.allocate(&allocation(payment, sale, "75"))
            .await
            .unwrap();
        let err = repo
            .allocate(&allocation(seed_payment(&pool, "100").await, sale, "1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("owes 0"), "got {err}");
    }

    #[tokio::test]
    async fn a_document_total_rounds_each_line_like_the_service_does() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, product) = seed_confirmed_sale(&pool, actor, "2").await;
        // SQLite stores qty as TEXT and accepts this fractional quantity. Each
        // 0.333 * 3 line is 0.999, which rounds to 1.00; summing raw products
        // first would instead produce 1.998 and disagree with the service.
        sqlx::query("UPDATE sale_lines SET qty = '0.333', unit_price = '3' WHERE sale_id = ?")
            .bind(sale)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total) \
             VALUES (?, ?, '0.333', '3', '0')",
        )
        .bind(sale)
        .bind(product)
        .execute(&pool)
        .await
        .unwrap();

        let repo = SqlitePaymentRepository::new(pool);
        assert_eq!(
            repo.residual_for_document(PartyDocumentKind::Sale, sale)
                .await
                .unwrap(),
            Decimal::from_str("2.00").unwrap()
        );
    }

    #[tokio::test]
    async fn batched_document_parts_equal_each_single_document_residual() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (first, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let (second, _) = seed_confirmed_sale(&pool, actor, "80").await;
        let (third, _) = seed_confirmed_sale(&pool, actor, "50").await;
        let first_payment = seed_payment(&pool, "10").await;
        let second_payment = seed_payment(&pool, "20").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        repo.allocate(&allocation(first_payment, first, "10"))
            .await
            .unwrap();
        repo.allocate(&allocation(second_payment, second, "20"))
            .await
            .unwrap();

        let customer: i64 = sqlx::query_scalar("SELECT customer_id FROM sales WHERE id = ?")
            .bind(second)
            .fetch_one(&pool)
            .await
            .unwrap();
        let returned: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by) \
             VALUES (?, ?, 'Confirmed', '2024-05-02', ?) RETURNING id",
        )
        .bind(customer)
        .bind(second)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO party_ledger_entries \
             (party_type, party_id, kind, amount, document_kind, document_id, entry_date, created_by) \
             VALUES ('Customer', ?, 'Return', '-7', 'CustomerReturn', ?, '2024-05-02', ?)",
        )
        .bind(customer)
        .bind(returned)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();

        let ids = [first, second, third];
        let parts = repo
            .residuals_for_documents(PartyDocumentKind::Sale, &ids)
            .await
            .unwrap();
        assert_eq!(parts.len(), ids.len());
        for id in ids {
            assert_eq!(
                parts[&id].residual,
                repo.residual_for_document(PartyDocumentKind::Sale, id)
                    .await
                    .unwrap(),
                "batch and single reads must share the same definition for {id}"
            );
        }
        assert_eq!(parts[&first].charge, Decimal::from_str("100").unwrap());
        assert_eq!(parts[&first].signed_returns, Decimal::ZERO);
        assert_eq!(parts[&first].allocated, Decimal::from_str("10").unwrap());
        assert_eq!(parts[&first].residual, Decimal::from_str("90").unwrap());
        assert_eq!(parts[&second].charge, Decimal::from_str("80").unwrap());
        assert_eq!(
            parts[&second].signed_returns,
            Decimal::from_str("-7").unwrap()
        );
        assert_eq!(parts[&second].allocated, Decimal::from_str("20").unwrap());
        assert_eq!(parts[&second].residual, Decimal::from_str("53").unwrap());
    }

    #[tokio::test]
    async fn a_batch_reads_three_documents_with_three_queries_not_one_query_per_document() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (first, _) = seed_confirmed_sale(&pool, actor, "10").await;
        let (second, _) = seed_confirmed_sale(&pool, actor, "20").await;
        let (third, _) = seed_confirmed_sale(&pool, actor, "30").await;
        let repo = SqlitePaymentRepository::new(pool);
        repo.reset_batch_query_count();

        repo.residuals_for_documents(PartyDocumentKind::Sale, &[first, second, third])
            .await
            .unwrap();
        assert_eq!(repo.batch_query_count(), 3);
    }

    #[tokio::test]
    async fn an_empty_document_batch_returns_empty_without_querying() {
        let pool = test_pool().await;
        let repo = SqlitePaymentRepository::new(pool);
        repo.reset_batch_query_count();

        assert!(repo
            .residuals_for_documents(PartyDocumentKind::Sale, &[])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(repo.batch_query_count(), 0);
    }

    #[tokio::test]
    async fn batched_target_allocations_match_the_per_payment_read_in_oldest_first_order() {
        let pool = test_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (sale, _) = seed_confirmed_sale(&pool, actor, "100").await;
        let customer: i64 = sqlx::query_scalar("SELECT customer_id FROM sales WHERE id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        let first = seed_payment(&pool, "30").await;
        let second = seed_payment(&pool, "40").await;
        sqlx::query("UPDATE payments SET party_id = ? WHERE id IN (?, ?)")
            .bind(customer)
            .bind(first)
            .bind(second)
            .execute(&pool)
            .await
            .unwrap();
        let repo = SqlitePaymentRepository::new(pool);
        repo.allocate(&allocation(first, sale, "30")).await.unwrap();
        repo.allocate(&allocation(second, sale, "40"))
            .await
            .unwrap();

        // The old implementation loaded every party payment and queried each
        // one's allocations, then retained this target's positive share.
        repo.reset_batch_query_count();
        let mut per_payment = Vec::new();
        for mut payment in repo
            .list_for_party(PartyType::Customer, customer)
            .await
            .unwrap()
        {
            if payment.direction != PaymentDirection::In {
                continue;
            }
            let allocations = repo.list_allocations(payment.id).await.unwrap();
            let amount = checked_money_sum(
                allocations
                    .iter()
                    .filter(|share| {
                        share.target_kind == PartyDocumentKind::Sale && share.target_id == sale
                    })
                    .map(|share| &share.amount),
            )
            .unwrap();
            if amount > Decimal::ZERO {
                payment.amount = amount;
                per_payment.push(payment);
            }
        }
        per_payment.sort_by_key(|payment| (payment.date, payment.id));
        assert_eq!(
            repo.batch_query_count(),
            3,
            "party list plus one allocation read per payment"
        );

        repo.reset_batch_query_count();
        let batched = repo
            .list_allocating_to_target(PartyDocumentKind::Sale, sale)
            .await
            .unwrap();
        assert_eq!(repo.batch_query_count(), 1);
        assert_eq!(
            batched
                .iter()
                .map(|payment| (payment.id, payment.amount))
                .collect::<Vec<_>>(),
            per_payment
                .iter()
                .map(|payment| (payment.id, payment.amount))
                .collect::<Vec<_>>()
        );
        assert_eq!(batched.len(), 2);
    }

    #[tokio::test]
    async fn the_public_allocate_writes_exactly_what_the_in_form_writes() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let (s1, _) = seed_confirmed_sale(&pool, actor, "30").await;
        let (s2, _) = seed_confirmed_sale(&pool, actor, "40").await;
        let via_public = repo.allocate(&allocation(id, s1, "30")).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        let via_in = repo
            .allocate_in(&mut tx, &allocation(id, s2, "40"))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = repo.list_allocations(id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].amount, via_public.amount);
        assert_eq!(rows[1].amount, via_in.amount);
        assert_eq!(rows[0].target_id, s1);
        assert_eq!(rows[1].target_id, s2);
    }
}
