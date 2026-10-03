use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    CustomerReturnLine, NewPartyLedgerEntry, PartyDocumentKind, PartyEntryKind, PartyLedgerEntry,
    PartyType, PriceRefusal, PurchaseLine, PurchaseReturnLine, SaleLine,
};
use crate::repositories::checked_aggregate_sum;
use crate::services::checked_money_sum;
use crate::services::line_taxes::tax_inclusive_total;

/// One party's signed journal: append-only rows, read as ONE checked fold.
///
/// This is the table every party balance becomes (decision 7): outstanding
/// debt, saldo a favor, ageing, credit limit, payables and the statement all
/// read `balance_for_party` instead of their own derived fold over a document
/// family.
///
/// Two rules the trait itself carries, because they are the ones a caller can
/// get wrong:
///
/// - **The sign is in the row.** `amount` is stored signed by
///   [`PartyEntryKind::signed_amount`]; a read never applies a direction of its
///   own. A negative balance is a legal result, not an error.
/// - **Writes join the caller's unit.** Every entry belongs to the
///   transaction that owns the event (decision 8), which is what the `_in`
///   twins are for: "Nothing opens a transaction yet. This is the door."
#[async_trait]
pub trait PartyLedgerRepository: Send + Sync {
    /// Append one entry in a transaction of its own, for a caller with no
    /// larger unit. The journal never updates or deletes a row (decision 3):
    /// a correction is a new entry, so `insert` is the only write shape here.
    async fn insert(&self, entry: &NewPartyLedgerEntry) -> AppResult<PartyLedgerEntry>;

    /// [`Self::insert`] inside a transaction the CALLER owns. The confirm,
    /// collection and cancel paths each open ONE unit and every write inside it
    /// is an `_in` form — reaching for `insert` from inside a unit would open a
    /// second connection mid-transaction, and on this crate's
    /// `max_connections(1)` fixtures that is a `PoolTimedOut` deadlock rather
    /// than wrong data. ONE copy of the INSERT lives below; `insert` is this
    /// method wrapped in BEGIN/COMMIT.
    async fn insert_in(
        &self,
        tx: &mut SqliteConnection,
        entry: &NewPartyLedgerEntry,
    ) -> AppResult<PartyLedgerEntry>;

    /// The party's balance: one checked fold over the stored SIGNED amounts of
    /// that party's entries, oldest first.
    ///
    /// `Ok(negative)` is a saldo a favor and is as normal an answer as a
    /// positive one. The only error the fold itself produces is
    /// [`PriceRefusal::AggregateTooLarge`]: a bounded entry says nothing about
    /// the sum of a set of them, so the set is what is checked.
    async fn balance_for_party(&self, party_type: PartyType, party_id: i64) -> AppResult<Decimal>;

    /// [`Self::balance_for_party`] inside a transaction the CALLER owns — a
    /// read that must move, for the same reason the write does: a path that
    /// folds the balance and then writes against it has to see the entries the
    /// current unit has already appended, which a pool read cannot see.
    async fn balance_for_party_in(
        &self,
        tx: &mut SqliteConnection,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Decimal>;

    /// The statement read: every entry of the party in write order (`id`
    /// ascending), which is the order `idx_party_ledger_party` serves without a
    /// sort pass.
    async fn list_for_party(
        &self,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Vec<PartyLedgerEntry>>;

    /// How many entries exist at all — the backfill's emptiness guard, and the
    /// cheapest possible one: one indexed count over a table that is either
    /// empty (fresh database) or already filled (a backfill that has run).
    ///
    /// Deliberately pool-only, and deliberately called BEFORE a unit is opened:
    /// it is a pre-check on committed rows, not a read of anything this
    /// transaction writes, so it needs no `_in` twin. Taking the pool's only
    /// connection while a `tx` held it would deadlock the very run it guards.
    async fn count(&self) -> AppResult<i64>;
}

#[derive(Clone)]
pub struct SqlitePartyLedgerRepository {
    pub pool: SqlitePool,
}

impl SqlitePartyLedgerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// helpers

/// The lenient stored-decimal read every repository in this layer owns: a
/// malformed stored value is ZERO, never a panic. (`markup_pct` is the one
/// column that uses the strict variant, because there `0%` is a meaningful
/// value; an amount here is never a percent.)
fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_entry(row: sqlx::sqlite::SqliteRow) -> AppResult<PartyLedgerEntry> {
    let party_type: String = row.get("party_type");
    let kind: String = row.get("kind");
    let document_kind: String = row.get("document_kind");
    // A value this module did not write cannot be interpreted, so it is an
    // internal error rather than a defaulted variant: silently reading a
    // malformed party type as the wrong party would move money in a fold.
    Ok(PartyLedgerEntry {
        id: row.get("id"),
        party_type: party_type.parse().map_err(AppError::Internal)?,
        party_id: row.get("party_id"),
        kind: kind.parse().map_err(AppError::Internal)?,
        amount: parse_decimal(&row.get::<String, _>("amount")),
        document_kind: document_kind.parse().map_err(AppError::Internal)?,
        document_id: row.get("document_id"),
        entry_date: row.get("entry_date"),
        reference: row.get("reference"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
    })
}

/// The INSERT, over whichever connection the caller offers. ONE copy of the
/// SQL: `insert` opens a unit and delegates to `insert_in`, which runs this,
/// so a column added here cannot be added to one form and not the other. The
/// `RETURNING` projection is the same shape `transaction_repo::create_in`
/// uses, because `last_insert_rowid()` read back through a *pool* would be a
/// different connection's id. The column list is spelled out rather than
/// built with `format!`: sqlx 0.9 refuses a non-`'static` SQL string on
/// purpose (`SqlSafeStr`), and a literal is auditable at a glance.
async fn insert_entry<'e, E>(
    executor: E,
    entry: &NewPartyLedgerEntry,
) -> AppResult<PartyLedgerEntry>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        r#"INSERT INTO party_ledger_entries (party_type, party_id, kind, amount, document_kind, document_id, entry_date, reference, created_by)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id, party_type, party_id, kind, amount, document_kind, document_id, entry_date, reference, created_by, created_at"#,
    )
    .bind(entry.party_type.to_string())
    .bind(entry.party_id)
    .bind(entry.kind.to_string())
    .bind(entry.amount.to_string())
    .bind(entry.document_kind.to_string())
    .bind(entry.document_id)
    .bind(entry.entry_date)
    .bind(entry.reference.as_deref())
    .bind(entry.created_by)
    .fetch_one(executor)
    .await?;
    row_to_entry(row)
}

/// The balance fold, over whichever connection the caller offers — the same
/// generic-over-the-executor shape `transaction_repo::balance_for_account_raw`
/// established, so the pool form and the caller's-transaction form share ONE
/// copy of the query instead of two that can drift on how a row contributes.
async fn balance_for_party_raw<'e, E>(
    executor: E,
    party_type: PartyType,
    party_id: i64,
) -> AppResult<Decimal>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    // `ORDER BY id` is load-bearing, not decoration — the argument is spelled
    // out at `transaction_repo.rs:131` and applies verbatim: the check runs on
    // the RUNNING sum, so the order decides which prefixes the check sees, and
    // without an `ORDER BY` that order is a planner decision. Here it is also
    // free: `idx_party_ledger_party` ends in `id`, so the index-ordered scan IS
    // insertion order.
    let rows = sqlx::query(
        "SELECT amount FROM party_ledger_entries WHERE party_type = ? AND party_id = ? ORDER BY id",
    )
    .bind(party_type.to_string())
    .bind(party_id)
    .fetch_all(executor)
    .await?;
    // The sign is ALREADY in the stored amount (decision 1), so this is one
    // sum over stored values and no per-kind sign function exists on the read
    // side to disagree with the write side. `checked_aggregate_sum` and not
    // `checked_money_sum`: this is an account-level accumulation of stored
    // rows, which is the repository layer's own bound and answers
    // `PriceRefusal::AggregateTooLarge`.
    let amounts: Vec<Decimal> = rows
        .iter()
        .map(|row| parse_decimal(&row.get::<String, _>("amount")))
        .collect();
    checked_aggregate_sum(amounts.iter()).map_err(AppError::PriceRefused)
}

async fn list_for_party_raw<'e, E>(
    executor: E,
    party_type: PartyType,
    party_id: i64,
) -> AppResult<Vec<PartyLedgerEntry>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query(
        "SELECT id, party_type, party_id, kind, amount, document_kind, document_id, entry_date, reference, created_by, created_at FROM party_ledger_entries WHERE party_type = ? AND party_id = ? ORDER BY id",
    )
    .bind(party_type.to_string())
    .bind(party_id)
    .fetch_all(executor)
    .await?;
    rows.into_iter().map(row_to_entry).collect()
}

#[async_trait]
impl PartyLedgerRepository for SqlitePartyLedgerRepository {
    async fn insert(&self, entry: &NewPartyLedgerEntry) -> AppResult<PartyLedgerEntry> {
        let mut tx = self.pool.begin().await?;
        let row = self.insert_in(&mut tx, entry).await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn insert_in(
        &self,
        tx: &mut SqliteConnection,
        entry: &NewPartyLedgerEntry,
    ) -> AppResult<PartyLedgerEntry> {
        insert_entry(&mut *tx, entry).await
    }

    async fn balance_for_party(&self, party_type: PartyType, party_id: i64) -> AppResult<Decimal> {
        let mut tx = self.pool.begin().await?;
        let balance = self
            .balance_for_party_in(&mut tx, party_type, party_id)
            .await?;
        tx.commit().await?;
        Ok(balance)
    }

    async fn balance_for_party_in(
        &self,
        tx: &mut SqliteConnection,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Decimal> {
        // The executor is the caller's connection, so the fold sees the entries
        // that caller has appended but not yet committed. Nothing here opens a
        // transaction of its own.
        balance_for_party_raw(&mut *tx, party_type, party_id).await
    }

    async fn list_for_party(
        &self,
        party_type: PartyType,
        party_id: i64,
    ) -> AppResult<Vec<PartyLedgerEntry>> {
        let mut tx = self.pool.begin().await?;
        let entries = list_for_party_raw(&mut *tx, party_type, party_id).await?;
        tx.commit().await?;
        Ok(entries)
    }

    async fn count(&self) -> AppResult<i64> {
        let count: i64 = sqlx::query("SELECT COUNT(*) FROM party_ledger_entries")
            .fetch_one(&self.pool)
            .await?
            .get("COUNT(*)");
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// The backfill (decision 9)
// ---------------------------------------------------------------------------

/// Fill an EMPTY `party_ledger_entries` from the confirmed history already in
/// the database, then never run again.
///
/// Called from `db.rs` immediately after `sqlx::migrate!`, and it has to be
/// Rust rather than the migration itself for a reason the feature document
/// states and migration 42's header repeats: a document total is DERIVED —
/// `round_half_up(qty * unit_price + tax_total, 2)` per line — and SQLite
/// arithmetic over TEXT decimals silently runs in REAL, which is the floating
/// point this project forbids for money.
///
/// What it writes, for every CONFIRMED parent (cash and credit alike,
/// decision 2) and nothing else:
///
/// - `Charge +total` per confirmed sale/purchase,
/// - `Payment −amount` per `sale_payments`/`purchase_payments` row of a
///   confirmed parent,
/// - `Return −total` per confirmed customer return/purchase return,
/// - `Refund +amount` per `customer_return_payments`/
///   `purchase_return_payments` row of a confirmed parent.
///
/// Draft and Cancelled documents are excluded entirely: their net
/// contribution to the old folds was zero, so writing them would move the
/// balance the backfill is supposed to reproduce.
///
/// The guard is an empty table, checked before the unit opens (see
/// [`PartyLedgerRepository::count`]): a fresh database is a no-op either way,
/// and a second run writes nothing. Everything else runs inside ONE
/// transaction — all or nothing, like `confirm`.
///
/// Returns how many entries it appended (0 when it did not run).
pub async fn backfill_party_ledger(pool: &SqlitePool) -> AppResult<u64> {
    let repo = SqlitePartyLedgerRepository::new(pool.clone());
    if repo.count().await? > 0 {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let written = backfill_in(&repo, &mut tx).await?;
    tx.commit().await?;
    Ok(written)
}

async fn backfill_in(
    repo: &SqlitePartyLedgerRepository,
    tx: &mut SqliteConnection,
) -> AppResult<u64> {
    let mut written = 0;
    written += backfill_sales(repo, tx).await?;
    written += backfill_purchases(repo, tx).await?;
    written += backfill_customer_returns(repo, tx).await?;
    written += backfill_purchase_returns(repo, tx).await?;
    Ok(written)
}

/// The document-total rule, applied the way the rest of the tree applies it:
/// each line's tax-inclusive total through `line_taxes::tax_inclusive_total`
/// — the ONE half-up rounding rule, whose own doc calls it "the single
/// half-up money rule of the tax feature" — folded with `checked_money_sum`,
/// the same `checked_add` fold `SalesService::tax_split` runs at
/// `src/services/sales.rs:260-262` and `sale_repo.rs:1376` runs per document
/// row.
///
/// The items are the line's own `(net_subtotal, tax_total)` pair: the caller
/// gets `net_subtotal` from `Line::subtotal()` rather than from arithmetic
/// written here, so no part of the rule is re-derived in this file. The
/// document-level wrappers themselves (`tax_split`, `document_money`,
/// `paid_and_due`) are PRIVATE associated functions of their services, so this
/// is the reachable definition they all delegate to.
///
/// Returns [`PriceRefusal::DocumentTotalTooLarge`] — the same rule, the same
/// variant, because no single line is at fault.
fn tax_inclusive_document_total(
    lines: impl IntoIterator<Item = (Decimal, Decimal)>,
) -> Result<Decimal, PriceRefusal> {
    let per_line: Vec<Decimal> = lines
        .into_iter()
        .map(|(net_subtotal, tax_total)| tax_inclusive_total(net_subtotal, tax_total))
        .collect();
    checked_money_sum(per_line.iter())
}

/// The return families' total, which deliberately has NO rounding step: a
/// return line is `qty * frozen price` and nothing else
/// (`CustomerReturnLine::subtotal` says why), so the rule is exactly
/// `checked_money_sum` over the subtotals — the fold
/// `CustomerReturnService::document_money` (`src/services/customer_return.rs:
/// 141-147`) runs.
fn return_document_total(subtotals: Vec<Decimal>) -> Result<Decimal, PriceRefusal> {
    checked_money_sum(subtotals.iter())
}

fn row_to_sale_line(row: sqlx::sqlite::SqliteRow) -> SaleLine {
    SaleLine {
        id: row.get("id"),
        sale_id: row.get("sale_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&row.get::<String, _>("qty")),
        unit_price: parse_decimal(&row.get::<String, _>("unit_price")),
        tax_total: parse_decimal(&row.get::<String, _>("tax_total")),
        created_at: row.get("created_at"),
    }
}

fn row_to_purchase_line(row: sqlx::sqlite::SqliteRow) -> PurchaseLine {
    PurchaseLine {
        id: row.get("id"),
        purchase_id: row.get("purchase_id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&row.get::<String, _>("qty")),
        unit_cost: parse_decimal(&row.get::<String, _>("unit_cost")),
        tax_total: parse_decimal(&row.get::<String, _>("tax_total")),
        created_at: row.get("created_at"),
    }
}

fn row_to_customer_return_line(row: sqlx::sqlite::SqliteRow) -> CustomerReturnLine {
    CustomerReturnLine {
        id: row.get("id"),
        return_id: row.get("return_id"),
        sale_line_id: row.get("sale_line_id"),
        qty: parse_decimal(&row.get::<String, _>("qty")),
        unit_price: parse_decimal(&row.get::<String, _>("unit_price")),
        created_at: row.get("created_at"),
    }
}

fn row_to_purchase_return_line(row: sqlx::sqlite::SqliteRow) -> PurchaseReturnLine {
    PurchaseReturnLine {
        id: row.get("id"),
        return_id: row.get("return_id"),
        purchase_line_id: row.get("purchase_line_id"),
        qty: parse_decimal(&row.get::<String, _>("qty")),
        unit_cost: parse_decimal(&row.get::<String, _>("unit_cost")),
        created_at: row.get("created_at"),
    }
}

async fn backfill_sales(
    repo: &SqlitePartyLedgerRepository,
    tx: &mut SqliteConnection,
) -> AppResult<u64> {
    let rows = sqlx::query(
        "SELECT id, customer_id, sale_date, sale_number, created_by FROM sales WHERE status = 'Confirmed' ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut written = 0;
    for row in rows {
        let sale_id: i64 = row.get("id");
        let customer_id: i64 = row.get("customer_id");
        let sale_date: NaiveDate = row.get("sale_date");
        let sale_number: Option<String> = row.get("sale_number");
        let created_by: i64 = row.get("created_by");

        let line_rows = sqlx::query(
            "SELECT id, sale_id, product_id, qty, unit_price, tax_total, created_at FROM sale_lines WHERE sale_id = ? ORDER BY id",
        )
        .bind(sale_id)
        .fetch_all(&mut *tx)
        .await?;
        let lines: Vec<SaleLine> = line_rows.into_iter().map(row_to_sale_line).collect();
        let total = tax_inclusive_document_total(lines.iter().map(|l| (l.subtotal(), l.tax_total)))
            .map_err(AppError::PriceRefused)?;

        repo.insert_in(
            &mut *tx,
            &NewPartyLedgerEntry {
                party_type: PartyType::Customer,
                party_id: customer_id,
                kind: PartyEntryKind::Charge,
                amount: PartyEntryKind::Charge.signed_amount(total),
                document_kind: PartyDocumentKind::Sale,
                document_id: sale_id,
                entry_date: sale_date,
                reference: sale_number.clone(),
                created_by,
            },
        )
        .await?;
        written += 1;

        let payment_rows = sqlx::query(
            "SELECT amount, date, created_by FROM sale_payments WHERE sale_id = ? ORDER BY id",
        )
        .bind(sale_id)
        .fetch_all(&mut *tx)
        .await?;
        for payment in payment_rows {
            let magnitude = parse_decimal(&payment.get::<String, _>("amount"));
            let payment_date: NaiveDate = payment.get("date");
            let payment_by: i64 = payment.get("created_by");
            repo.insert_in(
                &mut *tx,
                &NewPartyLedgerEntry {
                    party_type: PartyType::Customer,
                    party_id: customer_id,
                    kind: PartyEntryKind::Payment,
                    amount: PartyEntryKind::Payment.signed_amount(magnitude),
                    document_kind: PartyDocumentKind::Sale,
                    document_id: sale_id,
                    entry_date: payment_date,
                    reference: sale_number.clone(),
                    created_by: payment_by,
                },
            )
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

async fn backfill_purchases(
    repo: &SqlitePartyLedgerRepository,
    tx: &mut SqliteConnection,
) -> AppResult<u64> {
    let rows = sqlx::query(
        "SELECT id, supplier_id, purchase_date, purchase_number, created_by FROM purchases WHERE status = 'Confirmed' ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut written = 0;
    for row in rows {
        let purchase_id: i64 = row.get("id");
        let supplier_id: i64 = row.get("supplier_id");
        let purchase_date: NaiveDate = row.get("purchase_date");
        let purchase_number: Option<String> = row.get("purchase_number");
        let created_by: i64 = row.get("created_by");

        let line_rows = sqlx::query(
            "SELECT id, purchase_id, product_id, qty, unit_cost, tax_total, created_at FROM purchase_lines WHERE purchase_id = ? ORDER BY id",
        )
        .bind(purchase_id)
        .fetch_all(&mut *tx)
        .await?;
        let lines: Vec<PurchaseLine> = line_rows.into_iter().map(row_to_purchase_line).collect();
        let total = tax_inclusive_document_total(lines.iter().map(|l| (l.subtotal(), l.tax_total)))
            .map_err(AppError::PriceRefused)?;

        repo.insert_in(
            &mut *tx,
            &NewPartyLedgerEntry {
                party_type: PartyType::Supplier,
                party_id: supplier_id,
                kind: PartyEntryKind::Charge,
                amount: PartyEntryKind::Charge.signed_amount(total),
                document_kind: PartyDocumentKind::Purchase,
                document_id: purchase_id,
                entry_date: purchase_date,
                reference: purchase_number.clone(),
                created_by,
            },
        )
        .await?;
        written += 1;

        let payment_rows = sqlx::query(
            "SELECT amount, date, created_by FROM purchase_payments WHERE purchase_id = ? ORDER BY id",
        )
        .bind(purchase_id)
        .fetch_all(&mut *tx)
        .await?;
        for payment in payment_rows {
            let magnitude = parse_decimal(&payment.get::<String, _>("amount"));
            let payment_date: NaiveDate = payment.get("date");
            let payment_by: i64 = payment.get("created_by");
            repo.insert_in(
                &mut *tx,
                &NewPartyLedgerEntry {
                    party_type: PartyType::Supplier,
                    party_id: supplier_id,
                    kind: PartyEntryKind::Payment,
                    amount: PartyEntryKind::Payment.signed_amount(magnitude),
                    document_kind: PartyDocumentKind::Purchase,
                    document_id: purchase_id,
                    entry_date: payment_date,
                    reference: purchase_number.clone(),
                    created_by: payment_by,
                },
            )
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

async fn backfill_customer_returns(
    repo: &SqlitePartyLedgerRepository,
    tx: &mut SqliteConnection,
) -> AppResult<u64> {
    let rows = sqlx::query(
        "SELECT id, customer_id, return_date, credit_note_number, created_by FROM customer_returns WHERE status = 'Confirmed' ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut written = 0;
    for row in rows {
        let return_id: i64 = row.get("id");
        let customer_id: i64 = row.get("customer_id");
        let return_date: NaiveDate = row.get("return_date");
        let credit_note_number: Option<String> = row.get("credit_note_number");
        let created_by: i64 = row.get("created_by");

        let line_rows = sqlx::query(
            "SELECT id, return_id, sale_line_id, qty, unit_price, created_at FROM customer_return_lines WHERE return_id = ? ORDER BY id",
        )
        .bind(return_id)
        .fetch_all(&mut *tx)
        .await?;
        let subtotals: Vec<Decimal> = line_rows
            .into_iter()
            .map(row_to_customer_return_line)
            .map(|line| line.subtotal())
            .collect();
        let total = return_document_total(subtotals).map_err(AppError::PriceRefused)?;

        repo.insert_in(
            &mut *tx,
            &NewPartyLedgerEntry {
                party_type: PartyType::Customer,
                party_id: customer_id,
                kind: PartyEntryKind::Return,
                amount: PartyEntryKind::Return.signed_amount(total),
                document_kind: PartyDocumentKind::CustomerReturn,
                document_id: return_id,
                entry_date: return_date,
                reference: credit_note_number.clone(),
                created_by,
            },
        )
        .await?;
        written += 1;

        let refund_rows = sqlx::query(
            "SELECT amount, date, created_by FROM customer_return_payments WHERE return_id = ? ORDER BY id",
        )
        .bind(return_id)
        .fetch_all(&mut *tx)
        .await?;
        for refund in refund_rows {
            let magnitude = parse_decimal(&refund.get::<String, _>("amount"));
            let refund_date: NaiveDate = refund.get("date");
            let refund_by: i64 = refund.get("created_by");
            repo.insert_in(
                &mut *tx,
                &NewPartyLedgerEntry {
                    party_type: PartyType::Customer,
                    party_id: customer_id,
                    kind: PartyEntryKind::Refund,
                    amount: PartyEntryKind::Refund.signed_amount(magnitude),
                    document_kind: PartyDocumentKind::CustomerReturn,
                    document_id: return_id,
                    entry_date: refund_date,
                    reference: credit_note_number.clone(),
                    created_by: refund_by,
                },
            )
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

async fn backfill_purchase_returns(
    repo: &SqlitePartyLedgerRepository,
    tx: &mut SqliteConnection,
) -> AppResult<u64> {
    let rows = sqlx::query(
        "SELECT id, supplier_id, return_date, return_number, created_by FROM purchase_returns WHERE status = 'Confirmed' ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut written = 0;
    for row in rows {
        let return_id: i64 = row.get("id");
        let supplier_id: i64 = row.get("supplier_id");
        let return_date: NaiveDate = row.get("return_date");
        let return_number: Option<String> = row.get("return_number");
        let created_by: i64 = row.get("created_by");

        let line_rows = sqlx::query(
            "SELECT id, return_id, purchase_line_id, qty, unit_cost, created_at FROM purchase_return_lines WHERE return_id = ? ORDER BY id",
        )
        .bind(return_id)
        .fetch_all(&mut *tx)
        .await?;
        let subtotals: Vec<Decimal> = line_rows
            .into_iter()
            .map(row_to_purchase_return_line)
            .map(|line| line.subtotal())
            .collect();
        let total = return_document_total(subtotals).map_err(AppError::PriceRefused)?;

        repo.insert_in(
            &mut *tx,
            &NewPartyLedgerEntry {
                party_type: PartyType::Supplier,
                party_id: supplier_id,
                kind: PartyEntryKind::Return,
                amount: PartyEntryKind::Return.signed_amount(total),
                document_kind: PartyDocumentKind::PurchaseReturn,
                document_id: return_id,
                entry_date: return_date,
                reference: return_number.clone(),
                created_by,
            },
        )
        .await?;
        written += 1;

        let refund_rows = sqlx::query(
            "SELECT amount, date, created_by FROM purchase_return_payments WHERE return_id = ? ORDER BY id",
        )
        .bind(return_id)
        .fetch_all(&mut *tx)
        .await?;
        for refund in refund_rows {
            let magnitude = parse_decimal(&refund.get::<String, _>("amount"));
            let refund_date: NaiveDate = refund.get("date");
            let refund_by: i64 = refund.get("created_by");
            repo.insert_in(
                &mut *tx,
                &NewPartyLedgerEntry {
                    party_type: PartyType::Supplier,
                    party_id: supplier_id,
                    kind: PartyEntryKind::Refund,
                    amount: PartyEntryKind::Refund.signed_amount(magnitude),
                    document_kind: PartyDocumentKind::PurchaseReturn,
                    document_id: return_id,
                    entry_date: refund_date,
                    reference: return_number.clone(),
                    created_by: refund_by,
                },
            )
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support::audit_actor_id;

    async fn test_pool() -> SqlitePool {
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn repo(pool: &SqlitePool) -> SqlitePartyLedgerRepository {
        SqlitePartyLedgerRepository::new(pool.clone())
    }

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn d(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    /// One entry stating a MAGNITUDE — the sign comes from the kind, which is
    /// the rule under test.
    fn entry(
        party_id: i64,
        kind: PartyEntryKind,
        magnitude: &str,
        document_id: i64,
    ) -> NewPartyLedgerEntry {
        NewPartyLedgerEntry {
            party_type: PartyType::Customer,
            party_id,
            kind,
            amount: kind.signed_amount(dec(magnitude)),
            document_kind: PartyDocumentKind::Sale,
            document_id,
            entry_date: d(2024, 5, 1),
            reference: Some(format!("T1-DOC-{document_id}")),
            created_by: 1,
        }
    }

    /// The message the database put on the refused statement — the raw text,
    /// mapped by nothing, so the proof cannot drift from the schema (the same
    /// shape `role_repo.rs::refusal_message` established).
    fn refusal_message(err: sqlx::Error) -> String {
        match err {
            sqlx::Error::Database(db) => db.message().to_string(),
            other => panic!("expected a refused statement, got {other:?}"),
        }
    }

    // -- the fold ------------------------------------------------------------

    #[tokio::test]
    async fn charge_and_refund_fold_up_while_payment_return_and_cancel_fold_down() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // One party per kind, so each assertion measures exactly one sign and
        // no earlier entry can mask a later one.
        let cases = [
            (1, PartyEntryKind::Charge, dec("100")),
            (2, PartyEntryKind::Refund, dec("100")),
            (3, PartyEntryKind::Payment, dec("-100")),
            (4, PartyEntryKind::Return, dec("-100")),
            (5, PartyEntryKind::Cancel, dec("-100")),
        ];
        for (party_id, kind, expected) in cases {
            r.insert(&entry(party_id, kind, "100", 1)).await.unwrap();
            assert_eq!(
                r.balance_for_party(PartyType::Customer, party_id)
                    .await
                    .unwrap(),
                expected,
                "{kind} of 100 must contribute {expected}"
            );
        }
    }

    #[tokio::test]
    async fn a_mixed_ledger_folds_to_the_hand_computed_signed_sum() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // The decision-1 worked example plus every remaining kind:
        //   +200 −250 −100 +40 −10 = −120
        for (kind, magnitude) in [
            (PartyEntryKind::Charge, "200"),
            (PartyEntryKind::Payment, "250"),
            (PartyEntryKind::Return, "100"),
            (PartyEntryKind::Refund, "40"),
            (PartyEntryKind::Cancel, "10"),
        ] {
            r.insert(&entry(9, kind, magnitude, 7)).await.unwrap();
        }

        assert_eq!(
            r.balance_for_party(PartyType::Customer, 9).await.unwrap(),
            dec("200") - dec("250") - dec("100") + dec("40") - dec("10"),
            "the fold is one signed sum over the stored amounts"
        );

        // The statement read is the same rows in write order, with the sign
        // already in each amount — no reader applies a direction of its own.
        let entries = r.list_for_party(PartyType::Customer, 9).await.unwrap();
        let kinds: Vec<PartyEntryKind> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PartyEntryKind::Charge,
                PartyEntryKind::Payment,
                PartyEntryKind::Return,
                PartyEntryKind::Refund,
                PartyEntryKind::Cancel,
            ]
        );
        assert_eq!(
            entries.iter().map(|e| e.amount).collect::<Vec<Decimal>>(),
            vec![dec("200"), dec("-250"), dec("-100"), dec("40"), dec("-10"),]
        );
        assert_eq!(entries[0].document_kind, PartyDocumentKind::Sale);
        assert_eq!(entries[0].reference, Some("T1-DOC-7".to_string()));
    }

    #[tokio::test]
    async fn a_negative_balance_is_the_legal_result_of_the_fold_and_not_an_error() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // Owes 200, pays 250: the saldo a favor decision 4 makes legal, and the
        // worked example decision 1 states (`+200 −250 = −50`). The assertion
        // that this is Ok(...) is the point — a fold that refused a credit
        // balance would answer the same as an error.
        r.insert(&entry(3, PartyEntryKind::Charge, "200", 1))
            .await
            .unwrap();
        r.insert(&entry(3, PartyEntryKind::Payment, "250", 2))
            .await
            .unwrap();

        let balance = r
            .balance_for_party(PartyType::Customer, 3)
            .await
            .expect("a saldo a favor is a balance, not an error");
        assert_eq!(
            balance,
            dec("-50"),
            "a negative balance is a saldo a favor, not a refusal"
        );
    }

    #[tokio::test]
    async fn an_aggregate_whose_sum_leaves_the_decimal_range_is_refused_with_a_price_rule() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // Each row alone is representable (5e28 < Decimal::MAX ≈ 7.92e28);
        // their sum is not. The fold must REFUSE with the repository-level rule
        // instead of panicking on raw `+`.
        r.insert(&entry(4, PartyEntryKind::Charge, "5e28", 1))
            .await
            .unwrap();
        r.insert(&entry(4, PartyEntryKind::Charge, "5e28", 2))
            .await
            .unwrap();

        let balance = r.balance_for_party(PartyType::Customer, 4).await;
        match balance {
            Err(AppError::PriceRefused(PriceRefusal::AggregateTooLarge)) => {}
            other => panic!("expected PriceRefused(AggregateTooLarge), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn insert_in_writes_into_the_callers_transaction_so_a_rollback_leaves_no_row() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // `max_connections(1)` on purpose: if `insert_in` reached for the pool
        // instead of the caller's connection, it would contend with the very
        // `tx` held here and fail as PoolTimedOut rather than as wrong data.
        let mut tx = pool.begin().await.unwrap();
        r.insert_in(&mut tx, &entry(6, PartyEntryKind::Charge, "100", 1))
            .await
            .unwrap();

        // The read twin must see the caller's uncommitted row — a pool read
        // would be a pre-transaction snapshot.
        assert_eq!(
            r.balance_for_party_in(&mut tx, PartyType::Customer, 6)
                .await
                .unwrap(),
            dec("100"),
            "the fold must see the caller's own unit"
        );

        tx.rollback().await.unwrap();

        assert_eq!(
            r.count().await.unwrap(),
            0,
            "the row lived inside the caller's unit, so the rollback took it"
        );
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 6).await.unwrap(),
            Decimal::ZERO
        );
    }

    // -- the backfill --------------------------------------------------------

    /// The ids a backfill test needs to assert what was EXCLUDED.
    struct Seed {
        customer: i64,
        supplier: i64,
        draft_sale: i64,
        cancelled_sale: i64,
        draft_return: i64,
        draft_purchase: i64,
    }

    /// A database carrying, for ONE customer and ONE supplier, every shape the
    /// backfill reads and every shape it must skip — seeded through raw SQL
    /// because the documents' own confirm paths are their services' subject and
    /// are tested there.
    ///
    /// Customer side:
    /// - credit sale T1-CS-1, lines `2×30 + (1×40.5 + 9.5 tax)` = 110, paid 40,
    /// - cash sale T1-CS-2, line `1×10` = 10, paid 10 in full (net 0),
    /// - a Draft sale and a Cancelled sale with a line each (both excluded),
    /// - confirmed credit note T1-CN-1 returning `1×30` = 30, refunded 10,
    /// - a Draft credit note (excluded).
    ///
    /// Supplier side:
    /// - purchase T1-CP-1, line `4×45 + 20 tax` = 200, paid 50,
    /// - a Draft purchase (excluded),
    /// - confirmed purchase return T1-PRN-1 returning `2×45` = 90, refunded 10.
    async fn seed_documents(pool: &SqlitePool) -> Seed {
        let who = audit_actor_id(pool).await.unwrap();
        let product: i64 = sqlx::query_scalar(
            "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
             VALUES ('T1-LEDGER', 'ledger product', 'Product', 'un', '10', 0, ?) RETURNING id",
        )
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let account: i64 = sqlx::query_scalar(
            "INSERT INTO accounts (name, created_by) VALUES ('T1 ledger cash', ?) RETURNING id",
        )
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        // Migration 44 guards the (account, method) pair on the payment rows
        // this fixture writes, so the method must be one the fixture's account
        // OWNS (the seeded methods are unassigned on a fresh database).
        let method: i64 = sqlx::query_scalar(
            "INSERT INTO payment_methods (name, account_id, created_by) \
             VALUES ('T1 ledger cash cash', ?, ?) RETURNING id",
        )
        .bind(account)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let customer: i64 = sqlx::query_scalar(
            "INSERT INTO customers (name, is_walkin, is_active, created_by)
             VALUES ('T1 Ledger Customer', 0, 1, ?) RETURNING id",
        )
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let supplier: i64 = sqlx::query_scalar(
            "INSERT INTO suppliers (name, is_active, created_by)
             VALUES ('T1 Ledger Supplier', 1, ?) RETURNING id",
        )
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();

        // -- the customer's confirmed credit sale: 110, paid 40.
        let sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, created_by)
             VALUES ('T1-CS-1', 'Confirmed', 'Credit', ?, 'T1 Ledger Customer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let line_one: i64 = sqlx::query_scalar(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
             VALUES (?, ?, '2', '30', '0') RETURNING id",
        )
        .bind(sale)
        .bind(product)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
             VALUES (?, ?, '1', '40.5', '9.5')",
        )
        .bind(sale)
        .bind(product)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '40', '2024-05-02', ?)",
        )
        .bind(sale)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        // -- a confirmed CASH sale, paid in full: net 0, so it enters the
        //    ledger (decision 2) without moving the balance the old fold had.
        let cash_sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, created_by)
             VALUES ('T1-CS-2', 'Confirmed', 'Cash', ?, 'T1 Ledger Customer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let cash_line: i64 = sqlx::query_scalar(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
             VALUES (?, ?, '1', '10', '0') RETURNING id",
        )
        .bind(cash_sale)
        .bind(product)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '10', '2024-05-01', ?)",
        )
        .bind(cash_sale)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        // -- a Draft sale and a Cancelled sale, each with a line: neither may
        //    produce a single entry, and the cancelled one's payment may not
        //    either.
        let draft_sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
             VALUES ('Draft', 'Credit', ?, 'T1 Ledger Customer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
             VALUES (?, ?, '3', '10', '0')",
        )
        .bind(draft_sale)
        .bind(product)
        .execute(pool)
        .await
        .unwrap();
        let cancelled_sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, created_by)
             VALUES ('T1-CS-4', 'Cancelled', 'Credit', ?, 'T1 Ledger Customer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total)
             VALUES (?, ?, '4', '10', '0')",
        )
        .bind(cancelled_sale)
        .bind(product)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '40', '2024-05-01', ?)",
        )
        .bind(cancelled_sale)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        // -- the confirmed credit note: 30 returned, 10 handed back as cash.
        let credit_note: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (credit_note_number, customer_id, sale_id, status, return_date, created_by)
             VALUES ('T1-CN-1', ?, ?, 'Confirmed', '2024-05-05', ?) RETURNING id",
        )
        .bind(customer)
        .bind(sale)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
             VALUES (?, ?, '1', '30')",
        )
        .bind(credit_note)
        .bind(line_one)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customer_return_payments (return_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '10', '2024-05-05', ?)",
        )
        .bind(credit_note)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        // -- a Draft credit note (excluded), plus a return pointing at the
        //    cash sale's line so no FK is left dangling in the fixture.
        let draft_return: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by)
             VALUES (?, ?, 'Draft', '2024-05-06', ?) RETURNING id",
        )
        .bind(customer)
        .bind(cash_sale)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
             VALUES (?, ?, '1', '10')",
        )
        .bind(draft_return)
        .bind(cash_line)
        .execute(pool)
        .await
        .unwrap();

        // -- the supplier's confirmed purchase: 200, paid 50.
        let purchase: i64 = sqlx::query_scalar(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, purchase_number, created_by)
             VALUES (?, 'Confirmed', 'Credit', '2024-05-02', 'T1-CP-1', ?) RETURNING id",
        )
        .bind(supplier)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        let purchase_line: i64 = sqlx::query_scalar(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost, tax_total)
             VALUES (?, ?, '4', '45', '20') RETURNING id",
        )
        .bind(purchase)
        .bind(product)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '50', '2024-05-03', ?)",
        )
        .bind(purchase)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        // -- a Draft purchase (excluded).
        let draft_purchase: i64 = sqlx::query_scalar(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by)
             VALUES (?, 'Draft', 'Credit', '2024-05-02', ?) RETURNING id",
        )
        .bind(supplier)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost, tax_total)
             VALUES (?, ?, '2', '10', '0')",
        )
        .bind(draft_purchase)
        .bind(product)
        .execute(pool)
        .await
        .unwrap();

        // -- the confirmed purchase return: 90 returned, 10 handed back.
        let purchase_return: i64 = sqlx::query_scalar(
            "INSERT INTO purchase_returns (return_number, supplier_id, purchase_id, status, return_date, created_by)
             VALUES ('T1-PRN-1', ?, ?, 'Confirmed', '2024-05-06', ?) RETURNING id",
        )
        .bind(supplier)
        .bind(purchase)
        .bind(who)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_return_lines (return_id, purchase_line_id, qty, unit_cost)
             VALUES (?, ?, '2', '45')",
        )
        .bind(purchase_return)
        .bind(purchase_line)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_return_payments (return_id, account_id, method_id, amount, date, created_by)
             VALUES (?, ?, ?, '10', '2024-05-06', ?)",
        )
        .bind(purchase_return)
        .bind(account)
        .bind(method)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();

        Seed {
            customer,
            supplier,
            draft_sale,
            cancelled_sale,
            draft_return,
            draft_purchase,
        }
    }

    #[tokio::test]
    async fn the_backfill_reproduces_the_old_derived_fold_plus_the_returns_correction() {
        let pool = test_pool().await;
        let seed = seed_documents(&pool).await;

        let written = backfill_party_ledger(&pool).await.unwrap();
        assert_eq!(
            written, 10,
            "two sales × (charge + payment) + credit note × (return + refund) \
             + purchase × (charge + payment) + purchase return × (return + refund)"
        );

        let r = repo(&pool);

        // CUSTOMER. The OLD fold was `Σ(total − paid)` over confirmed CREDIT
        // sales only — cash sales never entered it:
        //     110 total − 40 paid = 70
        // The ledger's answer is that fold plus the returns correction, where
        // the correction is the confirmed credit note (−30) and the cash handed
        // back on it (+10):
        //     70 − 30 + 10 = 50
        let old_derived_fold = dec("110") - dec("40");
        let returns_correction = -dec("30") + dec("10");
        let customer_balance = r
            .balance_for_party(PartyType::Customer, seed.customer)
            .await
            .unwrap();
        assert_eq!(customer_balance, old_derived_fold + returns_correction);
        assert_eq!(customer_balance, dec("50"));

        // SUPPLIER. The OLD fold was `Σ(total − paid)` over confirmed
        // purchases (cash and credit both):
        //     200 total − 50 paid = 150
        // plus the same correction shape, on the purchase return:
        //     150 − 90 + 10 = 70
        let old_derived_fold = dec("200") - dec("50");
        let returns_correction = -dec("90") + dec("10");
        let supplier_balance = r
            .balance_for_party(PartyType::Supplier, seed.supplier)
            .await
            .unwrap();
        assert_eq!(supplier_balance, old_derived_fold + returns_correction);
        assert_eq!(supplier_balance, dec("70"));

        // Nothing was written for a document that was not Confirmed: not the
        // Draft sale, not the Cancelled sale (nor its payment), not the Draft
        // credit note, not the Draft purchase.
        for (document_kind, document_id) in [
            (PartyDocumentKind::Sale, seed.draft_sale),
            (PartyDocumentKind::Sale, seed.cancelled_sale),
            (PartyDocumentKind::CustomerReturn, seed.draft_return),
            (PartyDocumentKind::Purchase, seed.draft_purchase),
        ] {
            let rows: i64 = sqlx::query(
                "SELECT COUNT(*) FROM party_ledger_entries WHERE document_kind = ? AND document_id = ?",
            )
            .bind(document_kind.to_string())
            .bind(document_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("COUNT(*)");
            assert_eq!(rows, 0, "{document_kind} #{document_id} was not Confirmed");
        }

        // The cash sale DID write its pair (decision 2) — net zero.
        let cash_entries: Vec<String> = sqlx::query(
            "SELECT amount FROM party_ledger_entries WHERE document_kind = 'Sale' AND reference = 'T1-CS-2' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>("amount"))
        .collect();
        assert_eq!(cash_entries, vec!["10".to_string(), "-10".to_string()]);
    }

    #[tokio::test]
    async fn running_the_backfill_against_a_non_empty_ledger_writes_nothing() {
        let pool = test_pool().await;
        let seed = seed_documents(&pool).await;

        let first = backfill_party_ledger(&pool).await.unwrap();
        assert!(first > 0, "the first run must fill the empty table");
        let after_first = repo(&pool).count().await.unwrap();
        let balance_after_first = repo(&pool)
            .balance_for_party(PartyType::Customer, seed.customer)
            .await
            .unwrap();

        let second = backfill_party_ledger(&pool).await.unwrap();
        assert_eq!(
            second, 0,
            "the empty-table guard must stop a run against a filled ledger"
        );
        assert_eq!(repo(&pool).count().await.unwrap(), after_first);
        assert_eq!(
            repo(&pool)
                .balance_for_party(PartyType::Customer, seed.customer)
                .await
                .unwrap(),
            balance_after_first,
            "a second run must not double a balance"
        );
    }

    // -- integrity: the single-instance guard and append-only (decisions 10
    //    and 11). Each refusal is proved with the raw statement any future
    //    writer, screen or script would run, asserting the database's own
    //    message, then asserting the state it leaves behind.

    #[tokio::test]
    async fn a_second_charge_for_the_same_document_is_refused_by_the_unique_index() {
        let pool = test_pool().await;
        let r = repo(&pool);

        r.insert(&entry(1, PartyEntryKind::Charge, "100", 1))
            .await
            .unwrap();
        let err = sqlx::query(
            "INSERT INTO party_ledger_entries (party_type, party_id, kind, amount, document_kind, document_id, entry_date, reference, created_by) VALUES ('Customer', 1, 'Charge', '100', 'Sale', 1, '2024-05-01', 'T1-DOC-1', 1)",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        let msg = refusal_message(err);
        assert_eq!(
            msg,
            "UNIQUE constraint failed: party_ledger_entries.document_kind, \
             party_ledger_entries.document_id, party_ledger_entries.kind",
            "observed verbatim: SQLite names the COLUMNS, not the partial index"
        );
        // Not written: the guard protects the single instance, so the table
        // still holds exactly one row and the balance is the first Charge's.
        assert_eq!(r.count().await.unwrap(), 1);
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 1).await.unwrap(),
            dec("100")
        );
    }

    #[tokio::test]
    async fn a_second_cancel_for_one_document_is_refused_too() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // A cancel of its own document (decision 3: every kind is written
        // against the parent). The first Cancel lands; the second is a
        // duplicated cancellation, not a second legitimate one.
        r.insert(&entry(2, PartyEntryKind::Cancel, "10", 5))
            .await
            .unwrap();
        let err = sqlx::query(
            "INSERT INTO party_ledger_entries (party_type, party_id, kind, amount, document_kind, document_id, entry_date, reference, created_by) VALUES ('Customer', 2, 'Cancel', '10', 'Sale', 5, '2024-05-01', 'T1-DOC-5', 1)",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(
            refusal_message(err).contains("UNIQUE constraint failed"),
            "a duplicated Cancel is the same idempotency break a duplicated Charge is"
        );
        assert_eq!(r.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn two_charges_for_two_different_documents_are_allowed() {
        let pool = test_pool().await;
        let r = repo(&pool);

        r.insert(&entry(3, PartyEntryKind::Charge, "100", 1))
            .await
            .unwrap();
        r.insert(&entry(3, PartyEntryKind::Charge, "40", 2))
            .await
            .unwrap();
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 3).await.unwrap(),
            dec("140"),
            "the guard is per document, not per party"
        );
    }

    #[tokio::test]
    async fn two_payments_for_the_same_document_are_allowed() {
        let pool = test_pool().await;
        let r = repo(&pool);

        // Several payments of one document settle it progressively — the
        // reason Payment and Refund stay OUTSIDE the partial index (decision
        // 11).
        r.insert(&entry(4, PartyEntryKind::Payment, "100", 1))
            .await
            .unwrap();
        r.insert(&entry(4, PartyEntryKind::Payment, "50", 1))
            .await
            .unwrap();
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 4).await.unwrap(),
            dec("-150"),
            "both payments folded; Payment is not single-instance"
        );
    }

    #[tokio::test]
    async fn an_update_of_a_ledger_entry_is_refused_and_the_row_and_fold_are_unchanged() {
        let pool = test_pool().await;
        let r = repo(&pool);

        r.insert(&entry(5, PartyEntryKind::Charge, "100", 1))
            .await
            .unwrap();
        let err =
            sqlx::query("UPDATE party_ledger_entries SET amount = '999' WHERE document_id = 1")
                .execute(&pool)
                .await
                .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "party ledger entries are append-only: an entry cannot be updated",
            "the trigger's own refusal text"
        );

        // The attempted rewrite changed nothing — not the row, not the fold
        // every balance reads.
        let (amount,): (String,) = sqlx::query_as(
            "SELECT amount FROM party_ledger_entries WHERE party_id = 5 AND document_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(amount, "100", "the refused UPDATE stored its old value");
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 5).await.unwrap(),
            dec("100"),
            "the fold still reads the original entry"
        );
    }

    #[tokio::test]
    async fn a_delete_of_ledger_entries_is_refused_and_the_row_count_is_unchanged() {
        let pool = test_pool().await;
        let r = repo(&pool);

        r.insert(&entry(6, PartyEntryKind::Charge, "100", 1))
            .await
            .unwrap();
        r.insert(&entry(6, PartyEntryKind::Payment, "40", 1))
            .await
            .unwrap();
        let err = sqlx::query("DELETE FROM party_ledger_entries")
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            refusal_message(err),
            "party ledger entries are append-only: an entry cannot be deleted",
            "the trigger's own refusal text"
        );

        // No entry left the journal, and the two attempts above (an UPDATE and
        // a mass DELETE) must not have burned or moved even one of them.
        assert_eq!(r.count().await.unwrap(), 2);
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 6).await.unwrap(),
            dec("60")
        );
    }
}
