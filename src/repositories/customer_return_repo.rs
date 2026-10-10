use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{CustomerReturn, CustomerReturnLine, CustomerReturnStatus};

/// A `%…%` LIKE needle whose literal `%`, `_` and `\` are escaped, so the SQL
/// matches the same partial substring every other list in this layer matches.
/// Callers compare it to `LOWER(column) ... ESCAPE '\'`; case folding is ASCII,
/// like SQLite's `LOWER`, because the engine ships no Unicode collation.
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

/// Server-side filter for the credit-notes list, mirroring
/// [`crate::models::PurchaseListFilter`] field for field and in the same order.
///
/// **It lives here rather than in `models.rs`, and that is a boundary, not a
/// preference.** Every other filter struct in this crate is a model, because
/// every other filter was introduced alongside a route that renders it. This one
/// is being added ahead of its route, and the alternative was to reach into a
/// file this change does not own. The precedent for a type declared in its own
/// repository module is `setup_repo::SetupRecord`. Move it to `models.rs`
/// unchanged when the routes land — it has no dependencies on anything private
/// here, so the move is a cut and paste.
///
/// `Default` narrows NOTHING, which is what lets one method serve both the
/// drawer and a whole-family list, and is why every field is an `Option`.
#[derive(Debug, Clone, Default)]
pub struct CustomerReturnListFilter {
    pub status: Option<CustomerReturnStatus>,
    /// Matching customer ids. `Some(empty)` matches NOTHING — a party filter
    /// that found no customer cannot match a document — rather than degrading
    /// into an `IN ()` or, worse, into no predicate at all.
    pub customer_ids: Option<Vec<i64>>,
    /// Partial, case-insensitive match on `credit_note_number`. A Draft has no
    /// number, so the SQL requires `credit_note_number IS NOT NULL`: a NULL is
    /// not a match for any fragment.
    pub number: Option<String>,
    /// Inclusive lower bound on `return_date` — the credit note's OWN day, not
    /// the parent's.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on `return_date`.
    pub to: Option<NaiveDate>,
}

/// A stored amount is TEXT and SQLite cannot compare decimals safely, so a
/// malformed value is reachable in the column rather than impossible. The mapper
/// degrades it to `ZERO` and lets the READ succeed — the same choice
/// `purchase_repo::row_to_line` makes, and for the same reason: the strict
/// variant is reserved for `markup_pct`, where `0%` is a meaningful value and
/// malformed must degrade to `None` rather than pin a sale price to cost.
fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn status_from_str(s: &str) -> CustomerReturnStatus {
    s.parse().unwrap_or(CustomerReturnStatus::Draft)
}

fn row_to_return(row: sqlx::sqlite::SqliteRow) -> CustomerReturn {
    let status_str: String = row.get("status");
    CustomerReturn {
        id: row.get("id"),
        credit_note_number: row.get("credit_note_number"),
        customer_id: row.get("customer_id"),
        sale_id: row.get("sale_id"),
        status: status_from_str(&status_str),
        return_date: row.get("return_date"),
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

fn row_to_line(row: sqlx::sqlite::SqliteRow) -> CustomerReturnLine {
    let qty_str: String = row.get("qty");
    let price_str: String = row.get("unit_price");
    CustomerReturnLine {
        id: row.get("id"),
        return_id: row.get("return_id"),
        sale_line_id: row.get("sale_line_id"),
        qty: parse_decimal(&qty_str),
        unit_price: parse_decimal(&price_str),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains(".credit_note_number") {
            AppError::Conflict("credit_note_number already exists".into())
        } else if s.contains("customer_return_lines.") {
            AppError::Conflict("that sale line is already on this customer return".into())
        } else {
            AppError::Conflict("customer return already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::NotFound("referenced sale/return/line/account not found".into())
    } else {
        AppError::Database(e)
    }
}

/// One credit note by id, over whichever connection the caller offers.
///
/// `set_confirmed_in` reads its own row back after the UPDATE, so this statement
/// runs on two executors: the caller's `&mut SqliteConnection` when a unit holds
/// the confirm, and the pool when the public wrapper owns the unit. ONE copy of
/// the SQL, generic over the executor — the same shape and the same reason as
/// `purchase_repo::find_purchase_raw` and as this crate's other free
/// `_raw` readers.
async fn find_customer_return_raw<'e, E>(executor: E, id: i64) -> AppResult<Option<CustomerReturn>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        r#"SELECT id, credit_note_number, customer_id, sale_id, status, return_date, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM customer_returns WHERE id = ?"#,
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(row_to_return))
}

#[async_trait]
pub trait CustomerReturnRepository: Send + Sync {
    /// The connection pool behind this repository, so a caller that owns a
    /// WORKING UNIT can open the transaction the `_in` methods join. It is here
    /// for exactly the reason it is on `PurchaseRepository`: a service is generic
    /// over repository TRAITS and holds no `SqlitePool` of its own, so without
    /// this method the service cannot open the one transaction a credit note's
    /// confirm needs. It is a WIDENING of the trait's surface and nothing else:
    /// the service that receives it is the caller, and every read and write below
    /// still goes through this repository's own methods.
    fn pool(&self) -> &SqlitePool;

    /// Create a DRAFT credit note. `customer_id` and `sale_id` are the facts the
    /// service copied off the parent — this repository stores what it is handed
    /// and decides nothing about which parent is eligible. `actor` is the acting
    /// user's id the service resolved from its request; it becomes the row's
    /// `created_by`, and nothing the request itself can supply names it.
    ///
    /// The row is born with `credit_note_number` NULL and `status = 'Draft'`
    /// written into the INSERT rather than left to a default, because the number
    /// is a counter `confirm` takes and the state is a backstop
    /// `set_confirmed_in` depends on.
    async fn create_return(
        &self,
        actor: i64,
        customer_id: i64,
        sale_id: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<CustomerReturn>;

    async fn find_return(&self, id: i64) -> AppResult<Option<CustomerReturn>>;

    /// Every credit note the filter selects, in id order.
    ///
    /// The collection read, and the reason this service has a `list` at all: with
    /// only `find_return` there is no way to ask "which credit notes exist", so a
    /// records drawer has nothing to query. `Default::default()` narrows nothing,
    /// which is the unfiltered read.
    async fn list_returns(
        &self,
        filter: &CustomerReturnListFilter,
    ) -> AppResult<Vec<CustomerReturn>>;

    /// **What CONFIRMED credit notes have already taken of one parent sale
    /// line.** This is the term the service subtracts from the parent line's own
    /// `qty`, and it is the whole of the repeat-return rule: without it a sale of
    /// five units can be credited five times over and the customer is paid each
    /// time.
    ///
    /// It reads OUTWARD from the parent line rather than inward from a return,
    /// which is why no method on this trait could answer it before: `list_lines`
    /// is keyed by a RETURN id and `find_line` by a LINE id, so every existing
    /// read looks the wrong way. A service that reached for `pool()` to ask
    /// itself would be the one thing the layering rule exists to prevent.
    ///
    /// THREE properties, each of which is a rule rather than an implementation
    /// detail:
    ///
    /// * **Only `Confirmed` counts.** A Draft reserves nothing — a draft credit
    ///   note is a piece of paper, and goods that have not come back cannot come
    ///   back twice — so a draft must not shrink anyone's allowance. A `Cancelled`
    ///   note gave its quantity back: the goods left the shelf again and the
    ///   money came back in, so counting it would refuse a credit of stock the
    ///   shop demonstrably holds.
    /// * **It is scoped to ONE parent line.** A sibling line of the same sale is
    ///   its own line's business — which is worth stating on THIS family in
    ///   particular, because a sale may repeat a product across two lines while a
    ///   credit note may not repeat a sale line.
    /// * **ZERO, never an error, for a line nothing has claimed** — including an
    ///   id that does not exist. The caller subtracts this figure from a parent's
    ///   `qty`, and an unclaimed line has had nothing taken from it.
    ///
    /// The fold is [`crate::repositories::checked_aggregate_sum`], so it refuses
    /// with `PriceRefusal::AggregateTooLarge` rather than panicking on
    /// `Decimal`'s raw `+`. Each `qty` is a single bounded write; the sum of a set
    /// of them is not bounded, and every one of them is operator-supplied text.
    ///
    /// It runs on the POOL and deliberately has no `_in` twin: it is a
    /// validating read that happens ABOVE `confirm`'s unit, and a pre-check buys
    /// EARLY refusal with a message the operator can act on rather than
    /// reachability. Wrapping it would pin the only connection across the whole
    /// pre-check and buy nothing. See the note on `confirm`'s BEGIN.
    async fn confirmed_qty_taken_by_sale_line(&self, sale_line_id: i64) -> AppResult<Decimal>;

    /// Update Draft header fields: the day the return was MADE and what was
    /// written about it. The edit stamps `updated_by` with the acting user, and
    /// `updated_at` is written into the UPDATE rather than left to a trigger —
    /// this table is born with the audit columns and installs none.
    ///
    /// It cannot assign a number, and it deliberately exposes no way to change
    /// `customer_id` or `sale_id`: both are COPIES of the parent, so the credit
    /// note is self-contained on its own page without becoming a second place for
    /// the same fact to disagree with the sale it reverses.
    async fn update_draft(
        &self,
        id: i64,
        actor: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<CustomerReturn>;

    /// Transition Draft -> Confirmed with the assigned credit-note number; the
    /// confirming request is an edit of the document and stamps `updated_by`.
    ///
    /// The Draft precondition is this statement's own WHERE, not the service's
    /// up-front read: a document that is not a Draft is refused with an
    /// [`AppError::Validation`] naming the state it was found in. So a duplicate
    /// submission of an already-Confirmed credit note cannot stamp a second
    /// number, even if the caller's status check were relaxed or raced.
    ///
    /// This public twin opens a unit of its own and delegates, so a caller that
    /// reaches for it directly gets exactly the old behaviour: the single
    /// statement, atomically, with no sibling writes.
    async fn set_confirmed(
        &self,
        id: i64,
        actor: i64,
        credit_note_number: &str,
    ) -> AppResult<CustomerReturn>;

    /// [`Self::set_confirmed`] inside a transaction the CALLER owns.
    ///
    /// This is the write that decides whether the document exists as far as the
    /// shop is concerned, and it is the LAST of `confirm`'s steps — after the
    /// number, the stock movements, the finance rows and the payment rows. A
    /// credit note has more steps than a sale does, and the extra ones are money
    /// leaving, so a failure part-way through must leave the shop having neither
    /// taken the goods nor paid for them.
    ///
    /// Nothing here opens a transaction. That is the door, not a convenience.
    async fn set_confirmed_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        actor: i64,
        credit_note_number: &str,
    ) -> AppResult<CustomerReturn>;

    /// Transition Draft/Confirmed -> Cancelled; the cancelling request stamps
    /// `updated_by`.
    async fn set_cancelled(
        &self,
        id: i64,
        actor: i64,
        reason: Option<&str>,
    ) -> AppResult<CustomerReturn>;
    /// The `_in` twin, for the same reason (T3d).
    async fn set_cancelled_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        actor: i64,
        reason: Option<&str>,
    ) -> AppResult<CustomerReturn>;

    /// Delete a DRAFT credit note — or a DISCARDED one (Cancelled while never
    /// confirmed: `credit_note_number IS NULL`) — and let its lines die by CASCADE.
    /// The predicate in the WHERE is the load-bearing backstop: even if a caller
    /// ever relaxed the service's state guard, a Confirmed row or a Cancelled row
    /// that carries a number cannot be removed by this statement — it answers
    /// `false` instead, so the caller can refuse honestly.
    async fn delete_draft(&self, id: i64) -> AppResult<bool>;

    /// Add one line to a DRAFT credit note, naming the parent sale line it
    /// returns and the quantity of THAT line.
    ///
    /// `unit_price` is FROZEN here, and this method is the only place a credit
    /// note line's price is ever written: a return is always at the parent's
    /// price, so the form has no price field and [`Self::update_line`] takes no
    /// price to change.
    ///
    /// There is deliberately no `unit_cost` on this line and so none in this
    /// signature — what a sale was actually PROFITABLE at needs the cost the
    /// goods carried on the day they sold, `sale_lines` freezes tax but not cost,
    /// and the app computes no margin today. Recorded as open in the feature
    /// document rather than paid for speculatively here.
    ///
    /// The DRAFT predicate is the INSERT's own `WHERE`, so a Confirmed credit
    /// note cannot gain a line even if a caller skipped the service's guard.
    /// `NotFound` for a missing return, `Conflict` for one that is not a Draft.
    async fn create_line(
        &self,
        return_id: i64,
        sale_line_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<CustomerReturnLine>;

    /// One line by id. `None` for an id that does not exist; the service decides
    /// what that means. A line's `return_id` is what resolves the parent a
    /// removal or an edit acts on, which is why the read exists at all.
    async fn find_line(&self, id: i64) -> AppResult<Option<CustomerReturnLine>>;

    async fn list_lines(&self, return_id: i64) -> AppResult<Vec<CustomerReturnLine>>;

    /// Edit a DRAFT credit note line's QUANTITY. There is deliberately no price
    /// argument: the frozen `unit_price` is a copy of a frozen value, so it is
    /// not a thing an operator may move. The DRAFT predicate is in the UPDATE's
    /// own `WHERE`, so a Confirmed credit note's line is provably untouched by a
    /// refused call.
    async fn update_line(&self, id: i64, qty: Decimal) -> AppResult<CustomerReturnLine>;

    /// Remove a DRAFT credit note line. Statement-level DRAFT predicate,
    /// `NotFound` for a missing line, `Conflict` for one whose return is no
    /// longer a Draft.
    async fn delete_line(&self, id: i64) -> AppResult<()>;
}

#[derive(Clone)]
pub struct SqliteCustomerReturnRepository {
    pub pool: SqlitePool,
}

impl SqliteCustomerReturnRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Why a line write matched no row: either the credit note does not exist,
    /// or it exists and is no longer a Draft. The two cases stay distinguishable
    /// — a missing document is a 404, a frozen one is a conflict the caller can
    /// explain.
    async fn refuse_line(conn: &mut SqliteConnection, return_id: i64) -> AppError {
        let status =
            sqlx::query_scalar::<_, String>("SELECT status FROM customer_returns WHERE id = ?")
                .bind(return_id)
                .fetch_optional(&mut *conn)
                .await;
        match status {
            Ok(None) => AppError::NotFound(format!("customer return {return_id} not found")),
            Ok(Some(status)) => AppError::Conflict(format!(
                "customer return {return_id} is {status}: its lines are frozen"
            )),
            Err(error) => AppError::Database(error),
        }
    }

    /// Why a confirm write matched no row: either the credit note does not
    /// exist, or it exists and is no longer a Draft. Read back so the refusal
    /// names the state the document actually rests in.
    ///
    /// The executor is part of what moves here, not an implementation detail.
    /// The refusal is reached ONLY when the DRAFT predicate matched no row, so a
    /// regression to `&SqlitePool` passes every happy-path test in this file and
    /// fails only on refusal — where it would stall for sqlx's 30s acquire
    /// timeout inside the caller's transaction and answer `PoolTimedOut` instead
    /// of a `Validation` the operator is waiting for. The test
    /// `set_confirmed_in_refuses_a_non_draft_credit_note_without_ever_reaching_for_the_pool`
    /// is what catches it.
    async fn refuse_confirm(conn: &mut SqliteConnection, id: i64) -> AppError {
        match sqlx::query_scalar::<_, String>("SELECT status FROM customer_returns WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
        {
            Ok(None) => AppError::NotFound(format!("customer return {id} not found")),
            Ok(Some(status)) => AppError::Validation(format!(
                "customer return {id} is {status}: only a Draft return can be confirmed"
            )),
            Err(error) => AppError::Database(error),
        }
    }
}

#[async_trait]
impl CustomerReturnRepository for SqliteCustomerReturnRepository {
    /// A field read: the struct's `pool` is already `pub` and is already what
    /// every other method on this impl borrows.
    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn create_return(
        &self,
        actor: i64,
        customer_id: i64,
        sale_id: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<CustomerReturn> {
        let row = sqlx::query(
            r#"INSERT INTO customer_returns (customer_id, sale_id, status, return_date, notes, created_by)
               VALUES (?, ?, 'Draft', ?, ?, ?)
               RETURNING id, credit_note_number, customer_id, sale_id, status, return_date, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(customer_id)
        .bind(sale_id)
        .bind(return_date)
        .bind(notes)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_return(row))
    }

    async fn find_return(&self, id: i64) -> AppResult<Option<CustomerReturn>> {
        // The statement lives in `find_customer_return_raw` only because
        // `set_confirmed_in` has to run this SAME query on a caller's connection,
        // and one copy of a statement is the rule.
        find_customer_return_raw(&self.pool, id).await
    }

    /// The projection is written out in full rather than as `SELECT *`: every
    /// other read in this file names its columns, and a row mapper reading a
    /// column nobody projected fails loudly instead of silently defaulting.
    async fn list_returns(
        &self,
        filter: &CustomerReturnListFilter,
    ) -> AppResult<Vec<CustomerReturn>> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, credit_note_number, customer_id, sale_id, status, return_date, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at FROM customer_returns",
        );
        // `WHERE 1 = 1` only when something will be appended, so an empty filter
        // produces the whole table and not a predicate that matches nothing.
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
                // Answering here rather than emitting `IN ()` is deliberate: it
                // is the difference between "nothing matched" and "the predicate
                // was dropped".
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
            // `credit_note_number IS NOT NULL` is load-bearing: in SQL
            // `NULL LIKE '%x%'` is NULL, not false, so the rows would drop out by
            // accident rather than by rule — and a reader would have to know that
            // to trust it. Said out loud, it is a Draft with no number not
            // matching a search for a number.
            qb.push(
                " AND credit_note_number IS NOT NULL AND LOWER(credit_note_number) LIKE LOWER(",
            )
            .push_bind(like_needle(number))
            .push(") ESCAPE '\\'");
        }
        if let Some(from) = filter.from {
            qb.push(" AND return_date >= ").push_bind(from);
        }
        if let Some(to) = filter.to {
            qb.push(" AND return_date <= ").push_bind(to);
        }
        qb.push(" ORDER BY id");
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(row_to_return).collect())
    }

    /// The aggregate, and the only place in this file that folds quantity.
    ///
    /// The shape is a JOIN of the lines to their documents rather than a
    /// correlated subquery, and the join is what makes the two predicates
    /// inseparable: `status` lives on `customer_returns` and `sale_line_id` on
    /// `customer_return_lines`, so filtering one table cannot express the other.
    /// A read that dropped the join would answer over every credit note in the
    /// table.
    ///
    /// `ORDER BY id` IS DELIBERATELY ABSENT, and this is not an oversight —
    /// `AGENTS.md` requires it on the two LEVEL folds
    /// (`transaction_repo.rs`, `stock_repo.rs`) for a reason that does not apply
    /// here. Those check the RUNNING sum, so row order decides which PREFIXES
    /// are seen, and the service's pre-check folds the same rows in insertion
    /// order: the two folds must agree about the answer, not merely about the
    /// total. This fold has no sibling anywhere — it is computed once, in one
    /// place, and its caller subtracts the RESULT. There is nothing for the order
    /// to disagree with, and the one place it could bite (a partial prefix
    /// overflowing before the total settles) is unreachable, because every
    /// quantity here is `> 0` by the service's own rule, so the running sum only
    /// grows and the final add is the largest one. Do not cargo-cult it.
    async fn confirmed_qty_taken_by_sale_line(&self, sale_line_id: i64) -> AppResult<Decimal> {
        let rows = sqlx::query(
            "SELECT qty FROM customer_return_lines l WHERE l.sale_line_id = ? AND EXISTS (SELECT 1 FROM customer_returns r WHERE r.id = l.return_id AND r.status = 'Confirmed')",
        )
        .bind(sale_line_id)
        .fetch_all(&self.pool)
        .await?;
        // `parse_decimal` is the LENIENT boundary mapper, exactly as on every
        // other read in this file: a stored amount that is not a decimal becomes
        // ZERO and the read still succeeds. That is the right choice here for a
        // reason specific to this read — a malformed `qty` must not be able to
        // make a whole DOCUMENT unreturnable, and refusing the read outright
        // would do exactly that. The strict variant belongs to `markup_pct`,
        // where `0%` is a meaningful value and malformed must degrade to `None`
        // rather than pin a sale price to cost.
        let qtys: Vec<Decimal> = rows
            .iter()
            .map(|row| parse_decimal(&row.get::<String, _>("qty")))
            .collect();
        // `checked_aggregate_sum`, NOT `sum` and NOT `checked_money_sum`. The
        // refusal is `AggregateTooLarge` because this is not one document's
        // total: it is a fold inside a repository over a set of separately
        // written rows, which is precisely the case that rule names.
        Ok(crate::repositories::checked_aggregate_sum(qtys.iter())
            .map_err(AppError::PriceRefused)?)
    }

    async fn update_draft(
        &self,
        id: i64,
        actor: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<CustomerReturn> {
        let row = sqlx::query(
            r#"UPDATE customer_returns
               SET return_date = ?, notes = ?,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, credit_note_number, customer_id, sale_id, status, return_date, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(return_date)
        .bind(notes)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_return(row))
    }

    async fn set_confirmed(
        &self,
        id: i64,
        actor: i64,
        credit_note_number: &str,
    ) -> AppResult<CustomerReturn> {
        let mut tx = self.pool.begin().await?;
        let confirmed = self
            .set_confirmed_in(&mut tx, id, actor, credit_note_number)
            .await?;
        tx.commit().await?;
        Ok(confirmed)
    }

    async fn set_confirmed_in(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
        actor: i64,
        credit_note_number: &str,
    ) -> AppResult<CustomerReturn> {
        // The DRAFT predicate is this statement's own WHERE, the same backstop
        // `delete_line` and `delete_draft` already carry. A document that is not
        // a Draft matches nothing, and a zero-row match is a refusal that names
        // the state the statement saw — never a `RowNotFound` for `map_db_err`
        // to guess about.
        //
        // WHAT IT REFUSES: a duplicate submission of a document that has already
        // been confirmed (or cancelled). `confirm` reads the status once, up
        // front, and that read is not a lock — between it and this write another
        // writer can confirm the same document, and without the predicate this
        // statement would stamp a second number over the first and report a
        // success the caller must never be told about.
        //
        // WHAT IT DOES NOT DO: it does not make a failed confirmation retryable
        // on its own. A WHERE clause cannot reach residue; that is what the
        // caller's unit is for. `confirm` opens ONE transaction immediately
        // before `next_number` and commits after this statement, so the number,
        // the stock movement, the `Expense` and the payment row roll back
        // together: a failure anywhere leaves the row `("Draft", NULL)` with
        // nothing behind it, and the retry re-passes this predicate.
        //
        // Inside the unit, the predicate's remaining job is that the statement,
        // the read-back and `refuse_confirm` all have to stay on the connection
        // the caller was handed; it still refuses a duplicate submission without
        // the pool ever being involved.
        let res = sqlx::query(
            r#"UPDATE customer_returns
               SET credit_note_number = ?, status = 'Confirmed',
                   confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
                 AND status = 'Draft'"#,
        )
        .bind(credit_note_number)
        .bind(actor)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_confirm(&mut *tx, id).await);
        }
        // The row the UPDATE just wrote, read back through the SAME statement
        // `find_return` runs and on the SAME connection — not the pool. This is
        // load-bearing rather than tidy: a read-back that reached for the pool
        // here would not be merely slow, it would be unable to answer at all
        // while the caller holds the only connection, and the error it returned
        // (`PoolTimedOut`) would be a driver failure standing in for a document
        // that had in fact just been confirmed correctly. It cannot return
        // `None` in practice — a just-confirmed row is not deletable — but the
        // branch is written out rather than unwrapped so a future caller never
        // sees a panic from a repository method.
        find_customer_return_raw(&mut *tx, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("customer return {id} not found")))
    }

    async fn set_cancelled(
        &self,
        id: i64,
        actor: i64,
        reason: Option<&str>,
    ) -> AppResult<CustomerReturn> {
        let mut tx = self.pool.begin().await?;
        let cancelled = self.set_cancelled_in(&mut tx, id, actor, reason).await?;
        tx.commit().await?;
        Ok(cancelled)
    }

    async fn set_cancelled_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        actor: i64,
        reason: Option<&str>,
    ) -> AppResult<CustomerReturn> {
        let clean = reason.and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        });
        let row = sqlx::query(
            r#"UPDATE customer_returns
               SET status = 'Cancelled', cancel_reason = ?,
                   cancelled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, credit_note_number, customer_id, sale_id, status, return_date, notes, cancel_reason, created_by, updated_by, created_at, updated_at, confirmed_at, cancelled_at"#,
        )
        .bind(clean)
        .bind(actor)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_return(row))
    }

    async fn delete_draft(&self, id: i64) -> AppResult<bool> {
        // The WHERE clause is the backstop that makes deleting an undeletable
        // document impossible even if the service check were relaxed: the
        // statement simply matches nothing and the answer is `false`. Deletable =
        // a Draft, OR a Cancelled row whose `credit_note_number` is NULL. A
        // Cancelled row WITH a number was confirmed first and is permanent audit
        // trail — the stock movements and refund transactions of a credit note
        // reference it.
        //
        // WHAT THE PREDICATE DOES NOT ESTABLISH, and must not be read as
        // establishing, is that the row is CLEAN. It is a backstop on STATUS: it
        // answers "may this row be removed", and it says nothing about what else
        // is pointing at that row. On the sale and purchase families that gap was
        // real — `confirm` used to write the number, the movements, the ledger
        // entry and the payment row on separate autocommit connections, so a Draft
        // could carry a committed payment and an orphan `Expense` while still
        // reading `("Draft", NULL)`, and deleting it removed the document and
        // kept the money.
        //
        // THAT HAZARD DOES NOT REACH A RETURN, for one reason and one reason
        // only: a return NEVER writes the cost satellite. Decision 2 of the
        // design holds that only a confirmed purchase changes a product's cost,
        // so `record_cost` is not one of a return's confirm steps at all, and with
        // it goes the `record_cost`-on-a-Confirmed window that was the one
        // residue with no recovery path. That is a property of the WRITES, not
        // of this WHERE clause — the same clause would be a backstop and nothing
        // more on a family whose confirm did leave residue. So the correct thing
        // for a future reader to take from this comment is: the guard below is
        // never a proof of cleanliness, and the cleanliness of a return's Draft
        // is a property its confirm unit must keep true, to be measured by
        // tests at the service layer rather than inferred from SQL here.
        //
        // The method answers from the predicate alone. Widening or narrowing what
        // it deletes is a behaviour decision with its own test and its own
        // migration story, and it is not a comment's business.
        let res = sqlx::query(
            r#"DELETE FROM customer_returns
               WHERE id = ?
                 AND (status = 'Draft'
                      OR (status = 'Cancelled' AND credit_note_number IS NULL))"#,
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn create_line(
        &self,
        return_id: i64,
        sale_line_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<CustomerReturnLine> {
        let mut tx = self.pool.begin().await?;
        // The DRAFT predicate is the statement's own, so a Confirmed credit note
        // cannot gain a line even if a caller skipped the service's guard. The
        // `UNIQUE (return_id, sale_line_id)` pair is the schema's backstop for
        // the other half of the rule: one line per parent line, because a
        // return's quantity is a quantity OF that line.
        let row = sqlx::query(
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
               SELECT ?, ?, ?, ?
               WHERE EXISTS (SELECT 1 FROM customer_returns WHERE id = ? AND status = 'Draft')
               RETURNING id, return_id, sale_line_id, qty, unit_price, created_at"#,
        )
        .bind(return_id)
        .bind(sale_line_id)
        .bind(qty.to_string())
        .bind(unit_price.to_string())
        .bind(return_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, return_id).await),
        };
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    async fn find_line(&self, id: i64) -> AppResult<Option<CustomerReturnLine>> {
        let row = sqlx::query(
            r#"SELECT id, return_id, sale_line_id, qty, unit_price, created_at
               FROM customer_return_lines WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_line))
    }

    async fn list_lines(&self, return_id: i64) -> AppResult<Vec<CustomerReturnLine>> {
        let rows = sqlx::query(
            r#"SELECT id, return_id, sale_line_id, qty, unit_price, created_at
               FROM customer_return_lines WHERE return_id = ? ORDER BY id"#,
        )
        .bind(return_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_line).collect())
    }

    async fn update_line(&self, id: i64, qty: Decimal) -> AppResult<CustomerReturnLine> {
        let mut tx = self.pool.begin().await?;

        // Read the owning return first, so a refusal can NAME it. A line that
        // does not exist is a 404 here rather than a conflict about a return the
        // caller never named.
        let return_id: Option<i64> =
            sqlx::query_scalar("SELECT return_id FROM customer_return_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let return_id = match return_id {
            Some(return_id) => return_id,
            None => {
                return Err(AppError::NotFound(format!(
                    "customer return line {id} not found"
                )))
            }
        };

        // Only `qty` moves. `unit_price` is a frozen copy of the parent's price
        // and is absent from this statement on purpose: the frozen value has no
        // argument that could rewrite it.
        let row = sqlx::query(
            r#"UPDATE customer_return_lines SET qty = ?
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM customer_returns r WHERE r.id = customer_return_lines.return_id AND r.status = 'Draft')
               RETURNING id, return_id, sale_line_id, qty, unit_price, created_at"#,
        )
        .bind(qty.to_string())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let row = match row {
            Some(row) => row,
            None => return Err(Self::refuse_line(&mut tx, return_id).await),
        };
        tx.commit().await?;
        Ok(row_to_line(row))
    }

    async fn delete_line(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;

        let return_id: Option<i64> =
            sqlx::query_scalar("SELECT return_id FROM customer_return_lines WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_db_err)?;
        let return_id = match return_id {
            Some(return_id) => return_id,
            None => {
                return Err(AppError::NotFound(format!(
                    "customer return line {id} not found"
                )))
            }
        };

        // Statement-level DRAFT predicate: a closed credit note's line cannot be
        // removed. A credit note's lines are the evidence of WHAT came back, so
        // a confirmed document's line is history and not an editable row.
        let res = sqlx::query(
            r#"DELETE FROM customer_return_lines
               WHERE id = ?
                 AND EXISTS (SELECT 1 FROM customer_returns r WHERE r.id = customer_return_lines.return_id AND r.status = 'Draft')"#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            return Err(Self::refuse_line(&mut tx, return_id).await);
        }
        tx.commit().await?;
        Ok(())
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

    // -- migration 44: the (account_id, method_id) pair on a credit-note refund
    // is guarded by the schema. The refusals are proved with the raw statement
    // and the trigger's own text; the full battery lives on sale_payments.

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

    // The migration 44 exemption for a replayed (account, method) pair no
    // longer needs a pin HERE: the pair does not live in a return-payment row
    // any more. A refund IS a `payments` delivery, exempt by (party, direction)
    // in migration 48, and the pin that proves it runs through the real confirm
    // path — `a_refund_replays_the_parent_payments_account_even_after_the_method_is_repointed`
    // in `services::{customer_return,purchase_return}`. Migration 49 dropped this
    // table, so a test here would pin a shape nothing writes.

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
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
        match sqlx::query_scalar("SELECT id FROM products WHERE sku = 'RET-P'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                   VALUES ('RET-P', 'return prod', 'Product', 'un', '10', 1, ?)
                   RETURNING id"#,
            )
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    /// The walk-in customer migration 20 seeds. Every test in this file returns
    /// goods to THAT customer: a credit note names the customer the parent sale
    /// named, and the seeded walk-in is the one customer this database is
    /// guaranteed to have.
    async fn walkin_customer(pool: &SqlitePool) -> i64 {
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(pool)
            .await
            .unwrap();
        id
    }

    /// The whole parent chain a credit note hangs from, in one call: the
    /// customer, a CONFIRMED sale (a return is evidence about a confirmed
    /// document, so that is the only parent a test should be building) and one
    /// of its lines. Returned as `(customer_id, sale_id, sale_line_id)`.
    async fn seed_parent(
        pool: &SqlitePool,
        customer: &str,
        date: NaiveDate,
        actor: i64,
    ) -> (i64, i64, i64) {
        let customer_id = walkin_customer(pool).await;
        let sale: i64 = sqlx::query_scalar(
            r#"INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
               VALUES ('Confirmed', 'Cash', ?, ?, ?, ?) RETURNING id"#,
        )
        .bind(customer_id)
        .bind(customer)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        let product = product_id(pool, actor).await;
        let line: i64 = sqlx::query_scalar(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
               VALUES (?, ?, '3', '4') RETURNING id"#,
        )
        .bind(sale)
        .bind(product)
        .fetch_one(pool)
        .await
        .unwrap();
        (customer_id, sale, line)
    }

    /// One return with an EXPLICIT status, seeded through raw SQL: the predicate
    /// tests must be able to pin a Confirmed or Cancelled row WITHOUT the
    /// service's guard, because the point is proving the SQL backstop
    /// (`WHERE status = 'Draft'`) is load-bearing on its own.
    async fn seed_return_with_status(
        pool: &SqlitePool,
        status: &str,
        customer_id: i64,
        sale_id: i64,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by)
               VALUES (?, ?, ?, ?, ?) RETURNING id"#,
        )
        .bind(customer_id)
        .bind(sale_id)
        .bind(status)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn account_and_method(pool: &SqlitePool, actor: i64) -> (i64, i64) {
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

    /// A finance row to link a payment to. Written through raw SQL because the
    /// repository is not the thing that decides which transactions a refund
    /// produces — the service is.
    async fn transaction_id(
        pool: &SqlitePool,
        account: i64,
        kind: &str,
        amount: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by)
               VALUES (?, ?, ?, 'refund', '2024-SRET-000001', ?, ?) RETURNING id"#,
        )
        .bind(account)
        .bind(kind)
        .bind(amount)
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

    /// The models deliberately derive no `PartialEq`, so a round-trip compares
    /// FIELDS: a projection that dropped a column would still compile, and this
    /// is the assertion that notices.
    fn assert_same_return(actual: &CustomerReturn, expected: &CustomerReturn) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.credit_note_number, expected.credit_note_number);
        assert_eq!(actual.customer_id, expected.customer_id);
        assert_eq!(actual.sale_id, expected.sale_id);
        assert_eq!(actual.status, expected.status);
        assert_eq!(actual.return_date, expected.return_date);
        assert_eq!(actual.notes, expected.notes);
        assert_eq!(actual.cancel_reason, expected.cancel_reason);
        assert_eq!(actual.created_by, expected.created_by);
        assert_eq!(actual.updated_by, expected.updated_by);
        assert_eq!(actual.created_at, expected.created_at);
        assert_eq!(actual.updated_at, expected.updated_at);
        assert_eq!(actual.confirmed_at, expected.confirmed_at);
        assert_eq!(actual.cancelled_at, expected.cancelled_at);
    }

    fn assert_same_line(actual: &CustomerReturnLine, expected: &CustomerReturnLine) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.return_id, expected.return_id);
        assert_eq!(actual.sale_line_id, expected.sale_line_id);
        assert_eq!(actual.qty, expected.qty);
        assert_eq!(actual.unit_price, expected.unit_price);
        assert_eq!(actual.created_at, expected.created_at);
    }

    /// A credit note is born a Draft with NO number, and every column comes back
    /// through `find_return` exactly as it went in. The number is a counter
    /// `confirm` takes, never something the form carries.
    #[tokio::test]
    async fn a_credit_note_created_as_a_draft_round_trips_every_field() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Round Trip Customer", d(2024, 5, 2), actor).await;

        let created = repo
            .create_return(
                actor,
                customer,
                sale,
                d(2024, 6, 1),
                "two of the three came back",
            )
            .await
            .unwrap();
        assert_eq!(created.credit_note_number, None);
        assert_eq!(created.status, CustomerReturnStatus::Draft);
        assert_eq!(created.customer_id, customer);
        assert_eq!(created.sale_id, sale);
        assert_eq!(created.return_date, d(2024, 6, 1));
        assert_eq!(created.notes, "two of the three came back");
        assert_eq!(created.cancel_reason, None);
        assert_eq!(created.created_by, actor);
        assert_eq!(created.updated_by, None);
        assert_eq!(created.confirmed_at, None);
        assert_eq!(created.cancelled_at, None);
        assert_eq!(
            created.created_at, created.updated_at,
            "a row nothing has updated yet carries one timestamp for both"
        );

        let found = repo
            .find_return(created.id)
            .await
            .unwrap()
            .expect("the credit note is readable");
        assert_same_return(&found, &created);
        assert!(
            repo.find_return(999_999).await.unwrap().is_none(),
            "an unknown id is None, not an error"
        );
    }

    /// A Draft edit changes the two fields that belong to the draft — when the
    /// return is MADE and what was written about it — and stamps the acting
    /// user. It cannot assign a number, and the customer and the parent sale are
    /// copies rather than editable fields: a credit note names the customer the
    /// parent sale names, because the money goes back to the parent.
    #[tokio::test]
    async fn update_draft_changes_the_date_and_the_notes_and_stamps_the_acting_user() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Edit Customer", d(2024, 5, 2), actor).await;
        let created = repo
            .create_return(actor, customer, sale, d(2024, 6, 1), "first note")
            .await
            .unwrap();

        let updated = repo
            .update_draft(created.id, actor, d(2024, 6, 3), "credited on account")
            .await
            .unwrap();
        assert_eq!(updated.return_date, d(2024, 6, 3));
        assert_eq!(updated.notes, "credited on account");
        assert_eq!(updated.updated_by, Some(actor));
        assert!(
            updated.updated_at >= created.updated_at,
            "an edit cannot move updated_at backwards"
        );
        assert_eq!(
            updated.credit_note_number, None,
            "an edit never assigns the number confirm takes"
        );
        assert_eq!(updated.status, CustomerReturnStatus::Draft);
        assert_eq!(updated.customer_id, customer, "the customer is a copy");
        assert_eq!(updated.sale_id, sale, "the parent is the subject");

        let read_back = repo.find_return(created.id).await.unwrap().unwrap();
        assert_same_return(&read_back, &updated);
    }

    /// THE backstop proof for the confirm write: the repository is called
    /// DIRECTLY on an already Confirmed credit note — no service guard in the
    /// way — and must refuse, because `AND status = 'Draft'` is what makes a
    /// duplicate confirmation impossible even if the service's read-then-write
    /// check were relaxed or raced.
    #[tokio::test]
    async fn set_confirmed_on_an_already_confirmed_credit_note_is_refused_naming_the_state() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Confirm Customer", d(2024, 5, 2), actor).await;
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;

        // The first confirmation is the only one that may write.
        let confirmed = repo
            .set_confirmed(ret, actor, "2024-SRET-000001")
            .await
            .unwrap();
        assert_eq!(confirmed.status, CustomerReturnStatus::Confirmed);
        assert_eq!(
            confirmed.credit_note_number.as_deref(),
            Some("2024-SRET-000001")
        );
        assert!(confirmed.confirmed_at.is_some());

        // The duplicate submission is refused, and the refusal names the state
        // the statement actually saw rather than a generic "already exists".
        let err = repo
            .set_confirmed(ret, actor, "2024-SRET-000002")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(
                msg.contains("Confirmed"),
                "the refusal must name the state the document was found in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        // Nothing was rewritten: the FIRST number survives, so a refused
        // duplicate did not stamp a second one over the confirmed document.
        let after = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(
            after.credit_note_number.as_deref(),
            Some("2024-SRET-000001"),
            "the refused duplicate must leave the confirmed number untouched"
        );
        assert_eq!(after.confirmed_at, confirmed.confirmed_at);
    }

    /// The same refusal for a Cancelled document: only a Draft may be confirmed,
    /// so a cancelled credit note is refused too — and it is named as Cancelled,
    /// because that is the state the row rests in.
    #[tokio::test]
    async fn set_confirmed_on_a_cancelled_credit_note_is_refused_naming_cancelled() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Cancelled Customer", d(2024, 5, 2), actor).await;
        let ret =
            seed_return_with_status(&pool, "Cancelled", customer, sale, d(2024, 6, 1), actor).await;

        let err = repo
            .set_confirmed(ret, actor, "2024-SRET-000003")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => assert!(
                msg.contains("Cancelled"),
                "the refusal must name the state the document was found in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        let after = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(
            after.credit_note_number, None,
            "a refused confirm stamped a number on a document it did not confirm"
        );
    }

    /// A draft delete removes the draft and its lines (CASCADE) and NOTHING else:
    /// another draft seeded beside it keeps its row and its line.
    #[tokio::test]
    async fn delete_draft_removes_a_draft_credit_note_with_its_lines_and_leaves_others_alive() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Delete Customer", d(2024, 5, 2), actor).await;

        let draft =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        repo.create_line(draft, parent_line, dec("1"), dec("4"))
            .await
            .unwrap();
        let other =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 2), actor).await;
        repo.create_line(other, parent_line, dec("2"), dec("4"))
            .await
            .unwrap();

        assert!(repo.delete_draft(draft).await.unwrap());
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_returns WHERE id = ?",
                draft
            )
            .await,
            0,
            "the draft row must be gone"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_return_lines WHERE return_id = ?",
                draft
            )
            .await,
            0,
            "the draft's lines must be gone with it"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_returns WHERE id = ?",
                other
            )
            .await,
            1,
            "the other document must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_return_lines WHERE return_id = ?",
                other
            )
            .await,
            1,
            "the other document's lines must survive"
        );
        assert!(
            !repo.delete_draft(999_999).await.unwrap(),
            "an unknown id is not deletable"
        );
    }

    /// THE backstop proof for the delete: a Confirmed credit note is called
    /// DIRECTLY — no service guard in the way — and the row survives, because
    /// the WHERE clause is what makes deleting a confirmed document impossible.
    #[tokio::test]
    async fn delete_draft_on_a_confirmed_credit_note_returns_false_and_the_row_survives() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Kept Customer", d(2024, 5, 2), actor).await;
        let confirmed =
            seed_return_with_status(&pool, "Confirmed", customer, sale, d(2024, 6, 1), actor).await;
        sqlx::query(
            "UPDATE customer_returns SET credit_note_number = '2024-SRET-000001' WHERE id = ?",
        )
        .bind(confirmed)
        .execute(&pool)
        .await
        .unwrap();
        let refused = repo
            .create_line(confirmed, parent_line, dec("1"), dec("4"))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, AppError::Conflict(_)),
            "a confirmed credit note takes no line through the API either: {refused:?}"
        );
        let seeded: i64 = sqlx::query_scalar(
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
               VALUES (?, ?, '1', '4') RETURNING id"#,
        )
        .bind(confirmed)
        .bind(parent_line)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(!repo.delete_draft(confirmed).await.unwrap());
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_returns WHERE id = ?",
                confirmed
            )
            .await,
            1,
            "a confirmed credit note must survive a direct repository delete attempt"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_return_lines WHERE return_id = ?",
                confirmed
            )
            .await,
            1,
            "the confirmed credit note's lines must survive too"
        );
        let survivor = repo.find_return(confirmed).await.unwrap().unwrap();
        assert_eq!(
            survivor.credit_note_number.as_deref(),
            Some("2024-SRET-000001")
        );
        assert_eq!(
            repo.find_line(seeded).await.unwrap().unwrap().qty,
            dec("1"),
            "the frozen line kept its quantity"
        );
    }

    /// A line's whole lifecycle on a Draft, and the three refusals around it:
    /// the repeated parent line the UNIQUE pair forbids, the frozen line of a
    /// Confirmed credit note, and the unknown id.
    ///
    /// The frozen line is seeded through raw SQL because the repository API
    /// refuses to create it — which is itself the assertion.
    #[tokio::test]
    async fn credit_note_lines_add_update_and_remove_under_a_draft_predicate_and_refuse_a_repeated_parent_line(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Line Customer", d(2024, 5, 2), actor).await;
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;

        let line = repo
            .create_line(ret, parent_line, dec("2"), dec("4"))
            .await
            .unwrap();
        assert_eq!(line.return_id, ret);
        assert_eq!(line.sale_line_id, parent_line);
        assert_eq!(line.qty, dec("2"));
        assert_eq!(line.unit_price, dec("4"));
        assert_eq!(line.subtotal(), dec("8"));
        let read_back = repo.find_line(line.id).await.unwrap().unwrap();
        assert_same_line(&read_back, &line);
        assert_eq!(repo.list_lines(ret).await.unwrap().len(), 1);

        // UNIQUE (return_id, sale_line_id): a return's quantity is a quantity OF
        // that parent line, so one line per parent line.
        let repeated = repo
            .create_line(ret, parent_line, dec("1"), dec("4"))
            .await
            .unwrap_err();
        assert!(
            matches!(repeated, AppError::Conflict(_)),
            "a second line for one parent line must be refused: {repeated:?}"
        );
        assert_eq!(
            repo.list_lines(ret).await.unwrap().len(),
            1,
            "the refused second line left nothing behind"
        );

        // The update changes the QUANTITY only. A return is always at the
        // parent's price, so `update_line` does not take a price at all — the
        // frozen price cannot move and the method has no argument that could.
        let updated = repo.update_line(line.id, dec("1")).await.unwrap();
        assert_eq!(updated.qty, dec("1"));
        assert_eq!(
            updated.unit_price,
            dec("4"),
            "the frozen price must survive an edit of the quantity"
        );
        assert_eq!(updated.subtotal(), dec("4"));
        assert_same_line(&repo.find_line(line.id).await.unwrap().unwrap(), &updated);

        repo.delete_line(line.id).await.unwrap();
        assert!(
            repo.list_lines(ret).await.unwrap().is_empty(),
            "the removed line is gone from the list"
        );
        let missing = repo.delete_line(line.id).await.unwrap_err();
        assert!(
            matches!(missing, AppError::NotFound(_)),
            "removing a line that does not exist is a 404: {missing:?}"
        );

        // -- the frozen side: a CONFIRMED credit note's lines ------------------
        let frozen_return =
            seed_return_with_status(&pool, "Confirmed", customer, sale, d(2024, 6, 2), actor).await;
        let refused = repo
            .create_line(frozen_return, parent_line, dec("1"), dec("4"))
            .await
            .unwrap_err();
        match refused {
            AppError::Conflict(msg) => assert!(
                msg.contains("Confirmed"),
                "the refusal must name the state the document was found in: {msg}"
            ),
            other => panic!("expected Conflict, got {other:?}"),
        }
        let frozen: i64 = sqlx::query_scalar(
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
               VALUES (?, ?, '1', '4') RETURNING id"#,
        )
        .bind(frozen_return)
        .bind(parent_line)
        .fetch_one(&pool)
        .await
        .unwrap();
        match repo.update_line(frozen, dec("3")).await.unwrap_err() {
            AppError::Conflict(msg) => assert!(msg.contains("Confirmed"), "{msg}"),
            other => panic!("expected Conflict, got {other:?}"),
        }
        match repo.delete_line(frozen).await.unwrap_err() {
            AppError::Conflict(msg) => assert!(msg.contains("Confirmed"), "{msg}"),
            other => panic!("expected Conflict, got {other:?}"),
        }
        let survived = repo.find_line(frozen).await.unwrap().unwrap();
        assert_eq!(
            survived.qty,
            dec("1"),
            "a refused edit cannot change a confirmed credit note's line"
        );
    }

    /// The boundary's leniency, pinned: a stored amount that is not a decimal
    /// reads back as ZERO rather than failing the whole read. SQLite TEXT cannot
    /// compare decimals safely, so the column has no CHECK and the mapper is the
    /// only place the choice can be made — the strict variant belongs to
    /// `markup_pct`, where a malformed 0% would pin a sale price to cost.
    #[tokio::test]
    async fn a_malformed_stored_amount_reads_back_as_zero_instead_of_failing_the_read() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Malformed Customer", d(2024, 5, 2), actor).await;
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        let broken: i64 = sqlx::query_scalar(
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
               VALUES (?, ?, 'not-a-number', '4') RETURNING id"#,
        )
        .bind(ret)
        .bind(parent_line)
        .fetch_one(&pool)
        .await
        .unwrap();

        let line = repo.find_line(broken).await.unwrap().unwrap();
        assert_eq!(line.qty, Decimal::ZERO);
        assert_eq!(
            line.unit_price,
            dec("4"),
            "one malformed column must not take the rest of the row with it"
        );
        assert_eq!(
            repo.list_lines(ret).await.unwrap().len(),
            1,
            "the read returns the row rather than skipping it"
        );
    }

    // -- Phase A: the transaction-joining forms ------------------------------

    /// The confirm write is the one that MATTERS, because it is the statement
    /// that turns a Draft into a numbered document. Inside the caller's unit a
    /// rollback must leave the document exactly as it found it: still a Draft,
    /// still unnumbered, with no `confirmed_at` to mislead a later read.
    ///
    /// This is also the test that would catch a `set_confirmed_in` stamping the
    /// number through a private committed unit: the number would be visible on
    /// the pool the moment the statement returned, and the rollback could not
    /// take it back.
    #[tokio::test]
    async fn set_confirmed_in_stamps_the_number_in_the_callers_unit_and_a_rollback_leaves_the_draft_untouched(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Rollback Customer", d(2024, 5, 4), actor).await;
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        let before = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(before.status, CustomerReturnStatus::Draft);
        assert_eq!(before.credit_note_number, None);
        assert_eq!(before.confirmed_at, None);

        let mut tx = pool.begin().await.unwrap();
        let confirmed = repo
            .set_confirmed_in(&mut tx, ret, actor, "2024-SRET-IN-0001")
            .await
            .expect("set_confirmed_in could not run while it held the caller's connection");
        // The read-back is the row the UPDATE wrote, seen through the SAME unit:
        // the number, the status and the stamp all belong to this transaction.
        assert_eq!(confirmed.status, CustomerReturnStatus::Confirmed);
        assert_eq!(
            confirmed.credit_note_number.as_deref(),
            Some("2024-SRET-IN-0001")
        );
        assert!(
            confirmed.confirmed_at.is_some(),
            "the confirmed document carries no confirmed_at stamp"
        );
        // While the unit is open the pool is still blind to all of it, which is
        // the other half: an answer identical to the committed one would mean the
        // write had already escaped the caller's transaction.
        tx.rollback().await.unwrap();

        let after = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            CustomerReturnStatus::Draft,
            "the document was confirmed by a unit the caller's rollback could not reach"
        );
        assert_eq!(
            after.credit_note_number, None,
            "the number survived a rollback of the transaction that stamped it"
        );
        assert_eq!(
            after.confirmed_at, None,
            "the confirmed_at stamp survived a rollback of the transaction that wrote it"
        );
        // And the draft is still confirmable, so the rollback restored the state
        // rather than corrupting the row.
        let again = repo
            .set_confirmed(ret, actor, "2024-SRET-IN-0002")
            .await
            .unwrap();
        assert_eq!(again.status, CustomerReturnStatus::Confirmed);
    }

    /// `set_confirmed_in` must not reach for the pool on EITHER of the two paths
    /// that run inside the caller's unit, and the assertion is the pairing
    /// itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on
    /// this pool at all, ever: it would sit on sqlx's 30s acquire timeout and
    /// come back as `PoolTimedOut`.
    ///
    /// It is the happy path that makes this interesting rather than the refusal
    /// path, because on the happy path the read-back at the end of
    /// `set_confirmed_in` runs too.
    #[tokio::test]
    async fn set_confirmed_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Held-Conn Customer", d(2024, 5, 5), actor).await;
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;

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
            .set_confirmed_in(&mut tx, ret, actor, "2024-SRET-HELD-1")
            .await;
        let elapsed = started.elapsed();
        let confirmed = confirmed.expect(
            "set_confirmed_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert_eq!(confirmed.status, CustomerReturnStatus::Confirmed);
        assert_eq!(
            confirmed.credit_note_number.as_deref(),
            Some("2024-SRET-HELD-1")
        );
        // MEASURED, not assumed: the pairing above already decides it, and this
        // bound is the corroboration. Five seconds sits four orders of magnitude
        // above what an UPDATE plus a read-back on a held connection costs and
        // six below the 30s acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "set_confirmed_in took {elapsed:?}; that is a write stalling for a connection, not one on the connection it was handed"
        );
        // The caller's transaction is still ALIVE and still holds its lock. An
        // `_in` that had ended, committed or rolled back the unit it was given
        // could not leave a second confirm running on it.
        let refused = repo
            .set_confirmed_in(&mut tx, ret, actor, "2024-SRET-HELD-2")
            .await;
        assert!(
            matches!(refused, Err(AppError::Validation(_))),
            "the second confirm inside the same unit must be refused by the same DRAFT predicate, and must not be a driver error: {refused:?}"
        );
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        let after = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(after.status, CustomerReturnStatus::Draft);
    }

    /// The trap, by name: `set_confirmed_in` on a document that is NOT a Draft
    /// must produce the refusal without the pool ever being involved.
    ///
    /// This is the test that catches a refactor which moves the UPDATE and
    /// forgets `refuse_confirm`. The refusal is only reached when
    /// `rows_affected() == 0`, so every happy-path test in this file would stay
    /// green while the helper still held `&SqlitePool` — and the defect would
    /// only surface later, inside `confirm`'s real unit, as a 30-second stall on
    /// a refusal the operator is waiting for.
    #[tokio::test]
    async fn set_confirmed_in_refuses_a_non_draft_credit_note_without_ever_reaching_for_the_pool() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Already Confirmed", d(2024, 5, 6), actor).await;
        // A Confirmed row, seeded through raw SQL so the service's guard cannot
        // be what refuses it: the point is that the repository's OWN DRAFT
        // predicate is load-bearing inside the caller's transaction.
        let ret =
            seed_return_with_status(&pool, "Confirmed", customer, sale, d(2024, 6, 1), actor).await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let refused = repo
            .set_confirmed_in(&mut tx, ret, actor, "2024-SRET-DUP-1")
            .await;
        let elapsed = started.elapsed();

        // The refusal is a VALUE carrying the state it found, and it is still
        // `Validation` — the same variant the public method produces.
        let refused = refused.expect_err(
            "confirming a Confirmed credit note must be refused, and a refusal that reached for the pool would be a 30s PoolTimedOut instead",
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
        let after = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(
            after.credit_note_number, None,
            "a refused confirm stamped a number on a document it did not confirm"
        );
    }

    /// `set_confirmed` is the public twin, and it still has to answer exactly what
    /// it always answered: the success path, the duplicate submission, a frozen
    /// document and a document that does not exist.
    ///
    /// The error VARIANTS matter as much as the messages: `refuse_confirm` maps
    /// "no such document" to `NotFound` and "exists but frozen" to `Validation`,
    /// and a rewrite that collapsed them would still refuse — in a way the HTTP
    /// layer maps to a different status.
    ///
    /// The payment half of the wrapper surface is gone with the legacy table, so
    /// only the confirm contract is exercised here.
    #[tokio::test]
    async fn set_confirmed_answers_the_success_path_and_every_refusal() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, _line) =
            seed_parent(&pool, "Public Customer", d(2024, 5, 7), actor).await;
        let draft =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;

        // -- the success path --------------------------------------------------
        let confirmed = repo
            .set_confirmed(draft, actor, "2024-PUBLIC-1")
            .await
            .unwrap();
        assert_eq!(confirmed.status, CustomerReturnStatus::Confirmed);
        assert_eq!(
            confirmed.credit_note_number.as_deref(),
            Some("2024-PUBLIC-1")
        );
        assert!(confirmed.confirmed_at.is_some());

        // -- the duplicate submission ------------------------------------------
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
            repo.find_return(draft)
                .await
                .unwrap()
                .unwrap()
                .credit_note_number,
            Some("2024-PUBLIC-1".to_string())
        );

        // -- a frozen document -------------------------------------------------
        let cancelled =
            seed_return_with_status(&pool, "Cancelled", customer, sale, d(2024, 6, 2), actor).await;
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

        // -- a document that does not exist ------------------------------------
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
    }

    // -- the aggregate a cross-return cap subtracts from -----------------------

    /// What CONFIRMED credit notes have already taken of ONE parent sale line.
    ///
    /// Four documents, seeded through raw SQL so the assertion is about this
    /// read and not about whether `confirm` happened to produce those rows. On
    /// the SAME parent line: a Confirmed credit note of 2, a Draft of 3 and a
    /// Cancelled of 1. Only the Confirmed one counts, so the answer is 2.
    ///
    /// Both ways this read can be wrong are visible in one fixture. Dropping
    /// `status = 'Confirmed'` answers 6 and fails. Dropping the parent-line
    /// predicate answers over the whole table, which the sibling line below
    /// exposes — and which a fixture of drafts alone would NOT expose, because
    /// with nothing confirmed every wrong answer is zero too.
    #[tokio::test]
    async fn the_qty_a_confirmed_credit_note_took_of_a_sale_line_counts_only_confirmed_notes_of_that_line(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, line_a) =
            seed_parent(&pool, "Aggregate Customer", d(2024, 5, 2), actor).await;
        // A SECOND line on the same sale, so the parent-line predicate is
        // provably scoped rather than merely present.
        let product = product_id(&pool, actor).await;
        let line_b: i64 = sqlx::query_scalar(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
               VALUES (?, ?, '9', '4') RETURNING id"#,
        )
        .bind(sale)
        .bind(product)
        .fetch_one(&pool)
        .await
        .unwrap();

        let confirmed =
            seed_return_with_status(&pool, "Confirmed", customer, sale, d(2024, 6, 1), actor).await;
        let draft =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 2), actor).await;
        let cancelled =
            seed_return_with_status(&pool, "Cancelled", customer, sale, d(2024, 6, 3), actor).await;
        let other_line =
            seed_return_with_status(&pool, "Confirmed", customer, sale, d(2024, 6, 4), actor).await;

        // A seed closure rather than four copies of one INSERT: this fixture is
        // about WHICH rows the read sees, and a repeated statement four times is
        // four chances to fat-finger one binding.
        let seed = |ret: i64, line: i64, qty: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
                       VALUES (?, ?, ?, '4')"#,
                )
                .bind(ret)
                .bind(line)
                .bind(qty)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        seed(confirmed, line_a, "2").await;
        seed(draft, line_a, "3").await;
        seed(cancelled, line_a, "1").await;
        seed(other_line, line_b, "7").await;

        assert_eq!(
            repo.confirmed_qty_taken_by_sale_line(line_a).await.unwrap(),
            dec("2"),
            "only the CONFIRMED notes of THIS parent line count: a draft reserves \
             nothing, and a cancelled note gave its quantity back"
        );
        assert_eq!(
            repo.confirmed_qty_taken_by_sale_line(line_b).await.unwrap(),
            dec("7"),
            "a sibling line's confirmed credit note is its own line's business"
        );
    }

    /// The aggregate's floor: ZERO for a parent line that does not exist, and
    /// ZERO for one that exists but has only DRAFTS against it. Both are answers
    /// rather than errors, because the caller subtracts this figure from a
    /// parent's `qty` and an unclaimed line has had nothing taken.
    ///
    /// The premise is asserted rather than assumed: the draft claims the WHOLE
    /// line, so a zero here is a statement about drafts and not about an empty
    /// parent.
    #[tokio::test]
    async fn the_qty_taken_by_a_sale_line_is_zero_for_an_unknown_line_and_for_one_with_only_drafts()
    {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, line) =
            seed_parent(&pool, "Empty Aggregate Customer", d(2024, 5, 2), actor).await;

        assert_eq!(
            repo.confirmed_qty_taken_by_sale_line(999_999)
                .await
                .unwrap(),
            Decimal::ZERO,
            "an unknown parent line has had nothing taken from it"
        );

        let draft =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        let claimed = repo
            .create_line(draft, line, dec("3"), dec("4"))
            .await
            .unwrap();
        assert_eq!(
            claimed.qty,
            dec("3"),
            "the draft really does claim the whole line: this test would pass for \
             the wrong reason against a parent line of zero"
        );

        assert_eq!(
            repo.confirmed_qty_taken_by_sale_line(line).await.unwrap(),
            Decimal::ZERO,
            "a DRAFT reserves nothing, so the line's whole allowance is still free"
        );
    }

    /// The `delete_draft` CANCELLED branch, the direction that was unmeasured: a
    /// credit note DISCARDED while it was never confirmed — Cancelled with
    /// `credit_note_number` still NULL — is deletable, and it posted nothing, so
    /// removing it strands nothing.
    ///
    /// The premise is read back before the delete, because "Cancelled with no
    /// number" is the whole claim and a fixture not actually in that state would
    /// make this test pass for the wrong reason.
    #[tokio::test]
    async fn delete_draft_removes_a_credit_note_discarded_while_it_was_never_confirmed() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Discarded Customer", d(2024, 5, 2), actor).await;

        // Draft -> Cancelled, driven through the repository's own writes, and
        // the number never taken.
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        repo.create_line(ret, parent_line, dec("1"), dec("4"))
            .await
            .unwrap();
        repo.set_cancelled(ret, actor, Some("credited the wrong customer"))
            .await
            .unwrap();

        let before = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(before.status, CustomerReturnStatus::Cancelled);
        assert_eq!(
            before.credit_note_number, None,
            "the premise: this credit note was discarded while still a Draft, so it \
             carries no number and is the second deletable state"
        );

        assert!(
            repo.delete_draft(ret).await.unwrap(),
            "a discarded credit note is deletable — that is the second branch of the predicate"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_returns WHERE id = ?",
                ret
            )
            .await,
            0,
            "the row is gone"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_return_lines WHERE return_id = ?",
                ret
            )
            .await,
            0,
            "and its lines went with it by CASCADE"
        );
        assert!(
            !repo.delete_draft(ret).await.unwrap(),
            "and it is not deletable twice"
        );
    }

    /// The refusal side of the SAME branch, which is the half that makes it a
    /// predicate rather than a shortcut: a credit note CONFIRMED and then
    /// cancelled keeps its number, and a number is permanent audit trail — the
    /// stock movements and the refund transactions of a return reference it, so
    /// deleting the document would strand them.
    ///
    /// This is the test that tells the two Cancelled rows apart. A predicate
    /// reading `status = 'Cancelled'` alone would delete this one; only the
    /// `credit_note_number IS NULL` half distinguishes them.
    #[tokio::test]
    async fn delete_draft_refuses_a_cancelled_credit_note_that_was_confirmed_first() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());
        let (customer, sale, parent_line) =
            seed_parent(&pool, "Reversed Customer", d(2024, 5, 2), actor).await;

        // The real state sequence through the repository's own writes.
        let ret =
            seed_return_with_status(&pool, "Draft", customer, sale, d(2024, 6, 1), actor).await;
        repo.create_line(ret, parent_line, dec("1"), dec("4"))
            .await
            .unwrap();
        repo.set_confirmed(ret, actor, "2024-SRET-REVERSED-1")
            .await
            .unwrap();
        repo.set_cancelled(ret, actor, Some("wrong customer"))
            .await
            .unwrap();

        let before = repo.find_return(ret).await.unwrap().unwrap();
        assert_eq!(before.status, CustomerReturnStatus::Cancelled);
        assert_eq!(
            before.credit_note_number.as_deref(),
            Some("2024-SRET-REVERSED-1"),
            "the premise: a confirmed-then-cancelled credit note keeps its number"
        );

        assert!(
            !repo.delete_draft(ret).await.unwrap(),
            "the predicate must tell a DISCARDED note from a REVERSED one: both read \
             Cancelled, and only the number separates them"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_returns WHERE id = ?",
                ret
            )
            .await,
            1,
            "a reversed credit note must survive a direct repository delete attempt"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM customer_return_lines WHERE return_id = ?",
                ret
            )
            .await,
            1,
            "and its lines survive too: they are the evidence of what came back"
        );
    }

    /// The collection read the records drawer has nothing to query without.
    ///
    /// The filter shape is `PurchaseListFilter`'s, field for field and in the
    /// same order, because a new vocabulary for one idea is how two list
    /// surfaces drift: `status`, the party ids the SERVICE resolves, `number`
    /// matched partially and case-insensitively, and inclusive `from`/`to` date
    /// bounds on the document's OWN day.
    ///
    /// The fixture is four documents one dimension apart, so a predicate that
    /// never fires and one that always fires are both visible. `Default` narrows
    /// nothing, which is what lets ONE method serve both the drawer and the
    /// whole-family list — and it is asserted first, because every later
    /// assertion is only meaningful if the unfiltered read returns everything.
    #[tokio::test]
    async fn list_returns_narrows_by_status_number_party_and_date_and_an_empty_filter_narrows_nothing(
    ) {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteCustomerReturnRepository::new(pool.clone());

        let seed_customer = |name: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar(
                    "INSERT INTO customers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
                )
                .bind(name)
                .bind(actor)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let customer_one = seed_customer("List Customer One").await;
        let customer_two = seed_customer("List Customer Two").await;
        let parent = |customer: i64, day: u32| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar(
                    r#"INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
                       VALUES ('Confirmed', 'Cash', ?, 'Listed Customer', ?, ?) RETURNING id"#,
                )
                .bind(customer)
                .bind(d(2024, 5, day))
                .bind(actor)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let sale_one = parent(customer_one, 2).await;
        let sale_two = parent(customer_two, 3).await;

        let seed = |status: &'static str,
                    number: Option<&'static str>,
                    customer: i64,
                    sale: i64,
                    day: u32| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar(
                    r#"INSERT INTO customer_returns (customer_id, sale_id, credit_note_number, status, return_date, notes, created_by)
                       VALUES (?, ?, ?, ?, ?, 'listed', ?) RETURNING id"#,
                )
                .bind(customer)
                .bind(sale)
                .bind(number)
                .bind(status)
                .bind(d(2024, 6, day))
                .bind(actor)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let draft = seed("Draft", None, customer_one, sale_one, 1).await;
        let confirmed_one = seed(
            "Confirmed",
            Some("2024-SRET-000001"),
            customer_one,
            sale_one,
            10,
        )
        .await;
        let confirmed_two = seed(
            "Confirmed",
            Some("2024-SRET-000002"),
            customer_two,
            sale_two,
            20,
        )
        .await;
        let cancelled = seed(
            "Cancelled",
            Some("2024-SRET-000003"),
            customer_one,
            sale_one,
            30,
        )
        .await;

        let ids = |rows: Vec<CustomerReturn>| rows.into_iter().map(|r| r.id).collect::<Vec<i64>>();

        // The empty filter is the whole table, in id order, with every column
        // intact: a projection that dropped one would still compile.
        let all = repo
            .list_returns(&CustomerReturnListFilter::default())
            .await
            .unwrap();
        assert_eq!(
            ids(all.clone()),
            vec![draft, confirmed_one, confirmed_two, cancelled]
        );
        assert!(
            all.iter().all(|r| r.notes == "listed"),
            "the projection carries every column"
        );

        // status, in both directions: one that matches some and one that
        // matches none.
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    status: Some(CustomerReturnStatus::Confirmed),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![confirmed_one, confirmed_two]
        );
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    status: Some(CustomerReturnStatus::Cancelled),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![cancelled],
            "a status matching exactly one document must return exactly that one"
        );

        // number: PARTIAL and case-insensitive.
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    number: Some("00000".into()),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![confirmed_one, confirmed_two, cancelled],
            "a partial number matches every document carrying the fragment"
        );
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    number: Some("2024-sret-000002".into()),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![confirmed_two],
            "the match folds case on both sides, like every other list in this layer"
        );
        assert!(
            !ids(repo
                .list_returns(&CustomerReturnListFilter {
                    number: Some("2024".into()),
                    ..Default::default()
                })
                .await
                .unwrap())
            .contains(&draft),
            "a NULL number is not a match for any fragment: a draft must not be \
             collected by a search for a number it does not have"
        );

        // The party ids the SERVICE resolved.
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    customer_ids: Some(vec![customer_one]),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![draft, confirmed_one, cancelled]
        );
        assert!(
            repo.list_returns(&CustomerReturnListFilter {
                customer_ids: Some(Vec::new()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "`Some(empty)` must match nothing: the party filter found no customer, \
             so no document can match. It must not degrade into `IN ()` or into \
             no predicate at all"
        );

        // Inclusive bounds on the credit note's OWN day, which is not the
        // parent's.
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    from: Some(d(2024, 6, 10)),
                    to: Some(d(2024, 6, 20)),
                    ..Default::default()
                })
                .await
                .unwrap()),
            vec![confirmed_one, confirmed_two],
            "both bounds are inclusive: the first sits ON the lower bound and the \
             second ON the upper one"
        );
        assert!(
            repo.list_returns(&CustomerReturnListFilter {
                from: Some(d(2024, 7, 1)),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a range past the last document matches nothing"
        );

        // The filters COMPOSE. That is the property a list surface depends on,
        // and testing one field at a time never reaches it.
        assert_eq!(
            ids(repo
                .list_returns(&CustomerReturnListFilter {
                    status: Some(CustomerReturnStatus::Confirmed),
                    customer_ids: Some(vec![customer_two]),
                    number: Some("000002".into()),
                    from: Some(d(2024, 6, 1)),
                    to: Some(d(2024, 6, 30)),
                })
                .await
                .unwrap()),
            vec![confirmed_two]
        );
        assert!(
            repo.list_returns(&CustomerReturnListFilter {
                status: Some(CustomerReturnStatus::Draft),
                customer_ids: Some(vec![customer_two]),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "two filters that each match a different document match none together: \
             a broken AND would return one of them"
        );
    }
}
