use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{
    NewPayment, NewPaymentAllocation, PartyDocumentKind, PartyType, Payment, PaymentAllocation,
};
use crate::repositories::checked_aggregate_sum;
use crate::services::checked_money_sum;

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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
     amount, date, notes, transaction_id, created_by, updated_by, created_at, updated_at";

const ALLOCATION_COLUMNS: &str =
    "id, payment_id, target_kind, target_id, amount, created_by, updated_by, created_at, updated_at";

async fn insert_payment<'e, E>(executor: E, payment: &NewPayment) -> AppResult<Payment>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        r#"INSERT INTO payments
           (number, direction, party_type, party_id, method_id, account_id, amount, date, notes, transaction_id, created_by)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id, number, direction, party_type, party_id, method_id, account_id,
                     amount, date, notes, transaction_id, created_by, updated_by, created_at, updated_at"#,
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

/// The cap check, shared by every write path so the rule has one home.
///
/// Reads the payment and its shares on the SAME connection the caller holds, so
/// the pre-check sees the caller's own uncommitted rows — the property that makes
/// a write-then-check sequence sound inside one unit.
async fn assert_within_cap(
    tx: &mut SqliteConnection,
    payment_id: i64,
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
    Ok(())
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SqlitePaymentRepository {
    pub pool: SqlitePool,
}

impl SqlitePaymentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
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
        assert_within_cap(&mut *tx, allocation.payment_id, allocation.amount).await?;
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

    async fn list_allocations(&self, payment_id: i64) -> AppResult<Vec<PaymentAllocation>> {
        list_allocations_raw(&self.pool, payment_id).await
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
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT amount FROM payment_allocations WHERE target_kind = ? AND target_id = ? \
             ORDER BY id",
        )
        .bind(target_kind.to_string())
        .bind(target_id)
        .fetch_all(&self.pool)
        .await?;
        let sums: Vec<Decimal> = rows.iter().map(|r| parse_decimal(&r.0)).collect();
        checked_aggregate_sum(sums.iter()).map_err(AppError::PriceRefused)
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

        repo.allocate(&allocation(id, 10, "60")).await.unwrap();
        repo.allocate(&allocation(id, 11, "40")).await.unwrap();
        let err = repo.allocate(&allocation(id, 12, "1")).await.unwrap_err();
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
        repo.allocate(&allocation(id, 10, "30")).await.unwrap();
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
        for bad in ["0", "-5"] {
            let err = repo.allocate(&allocation(id, 10, bad)).await.unwrap_err();
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
        repo.allocate(&allocation(id, 10, "30")).await.unwrap();
        let err = repo.allocate(&allocation(id, 10, "1")).await.unwrap_err();
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

        let mut tx = pool.begin().await.unwrap();
        repo.allocate_in(&mut tx, &allocation(id, 10, "30"))
            .await
            .unwrap();
        let err = repo
            .allocate_in(&mut tx, &allocation(id, 11, "30"))
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
        repo.allocate(&allocation(frozen, 10, "70")).await.unwrap();

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
    #[tokio::test]
    async fn allocated_to_target_sums_across_payments() {
        let pool = test_pool().await;
        let a = seed_payment(&pool, "100").await;
        let b = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        repo.allocate(&allocation(a, 42, "30")).await.unwrap();
        repo.allocate(&allocation(b, 42, "25")).await.unwrap();
        assert_eq!(
            repo.allocated_to_target(PartyDocumentKind::Sale, 42)
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
    async fn the_public_allocate_writes_exactly_what_the_in_form_writes() {
        let pool = test_pool().await;
        let id = seed_payment(&pool, "100").await;
        let repo = SqlitePaymentRepository::new(pool.clone());
        let via_public = repo.allocate(&allocation(id, 10, "30")).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        let via_in = repo
            .allocate_in(&mut tx, &allocation(id, 11, "40"))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = repo.list_allocations(id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].amount, via_public.amount);
        assert_eq!(rows[1].amount, via_in.amount);
        assert_eq!(rows[0].target_id, 10);
        assert_eq!(rows[1].target_id, 11);
    }
}
