use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewPartyLedgerEntry, PartyLedgerEntry, PartyType};
use crate::repositories::checked_aggregate_sum;

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
}

// ---------------------------------------------------------------------------
// The ledger starts EMPTY
// ---------------------------------------------------------------------------

/// There is deliberately no backfill here.
///
/// Until 2026-10-10 this module filled an empty `party_ledger_entries` from the
/// confirmed history already in the database, because a database could predate
/// the ledger. That was the only reason it existed: every WRITE path
/// (`services::payment_writer` and the four `confirm` paths) stamps its entry
/// inside the same unit that owns the event, so a database whose papers were all
/// created by this code needs nothing reconstructed.
///
/// The dev database is wiped, so the population it was written for no longer
/// exists, and its four SELECTs over `sale_payments`, `purchase_payments`,
/// `customer_return_payments` and `purchase_return_payments` were the last thing
/// holding those tables. A backfill in a codebase with no history to backfill is
/// not a safety net; it is a second writer of the same fact, which is the disease
/// the payments family exists to cure — and it disagreed with the real writer on
/// one visible detail, stamping `reference = <document number>` where
/// `payment_writer` stamps `reference = PAY-…`.
///
/// The entries the backfill used to produce are the ones the four `confirm` paths
/// already write; `a_confirmed_sale_writes_its_charge_and_its_payment_entries`
/// and its three siblings pin them.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PartyDocumentKind, PartyEntryKind, PriceRefusal};
    use chrono::NaiveDate;

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

    /// How many entries the table holds. A COUNT through raw SQL rather than a
    /// repository method: the repository no longer needs one (its only caller was
    /// the deleted backfill's emptiness guard), and a trait method that exists only
    /// for tests is dead code in the binary.
    async fn count_entries(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM party_ledger_entries")
            .fetch_one(pool)
            .await
            .unwrap()
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
            count_entries(&pool).await,
            0,
            "the row lived inside the caller's unit, so the rollback took it"
        );
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 6).await.unwrap(),
            Decimal::ZERO
        );
    }

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
        assert_eq!(count_entries(&pool).await, 1);
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
        assert_eq!(count_entries(&pool).await, 1);
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
        assert_eq!(count_entries(&pool).await, 2);
        assert_eq!(
            r.balance_for_party(PartyType::Customer, 6).await.unwrap(),
            dec("60")
        );
    }
}
