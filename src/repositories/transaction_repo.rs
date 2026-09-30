use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Transaction, TransactionKind};

#[async_trait]
pub trait TransactionRepository: Send + Sync {
    async fn create(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction>;

    /// [`Self::create`] inside a transaction the CALLER owns, so a whole document
    /// can be one unit: the money row and the `accounts.cached_balance` refresh it
    /// depends on are committed together or not at all. A confirmation that rolls
    /// back therefore leaves neither an orphan Income nor a cache that disagrees
    /// with the rows behind it — windows W3 and W4 in
    /// `odd/tasks/confirm-failure-injection-and-state-predicates.md`, where the
    /// books show a full collection against a Draft that is still editable.
    ///
    /// This is the shape `sync_cached` at the bottom of this file was ALREADY
    /// written for: it has always taken a `&mut SqliteConnection`, so the cached
    /// refresh was never a pool read to begin with, and the only thing standing
    /// between it and a caller's transaction was `create`'s own `pool.begin()`.
    /// `create` above is this method wrapped in a transaction of its own, for the
    /// callers that have no larger unit to offer — which, before this method
    /// existed, was the only form there was.
    async fn create_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction>;

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Transaction>>;
    async fn list(&self, filter: &crate::models::TransactionFilter) -> AppResult<Vec<Transaction>>;
    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<Transaction>>;
    async fn update(&self, tx: &Transaction, actor: i64) -> AppResult<Transaction>;
    async fn delete(&self, id: i64) -> AppResult<bool>;

    /// [`Self::balance_for_account`] inside a transaction the CALLER owns — and it
    /// is a READ that has to move for the same correctness reason the write does.
    ///
    /// `TransactionService` folds the balance, validates the line against it, and
    /// only then writes, so in a document with two lines against the same account
    /// the second fold has to see the first line's row. Off the pool it sees a
    /// pre-transaction snapshot: `10` where the account actually holds `80`, which
    /// waves a 90-expense line through a drawer that cannot cover it. Same
    /// argument, and the same commit, as `stock_for_product_in` in
    /// `stock_repo.rs` — where the same read exists one table over.
    ///
    /// Nothing opens a transaction yet. This is the door; the confirm path does not
    /// walk through it until a later commit of Phase A does.
    async fn balance_for_account_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        account_id: i64,
    ) -> AppResult<Decimal>;

    async fn balance_for_account(&self, account_id: i64) -> AppResult<Decimal>;
    async fn sync_cached_balance(&self, account_id: i64) -> AppResult<()>;
}

#[derive(Clone)]
pub struct SqliteTransactionRepository {
    pub pool: SqlitePool,
}

impl SqliteTransactionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// helpers

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn kind_from_str(s: &str) -> TransactionKind {
    match s {
        "Income" => TransactionKind::Income,
        "Expense" => TransactionKind::Expense,
        _ => TransactionKind::Expense,
    }
}

fn row_to_tx(row: sqlx::sqlite::SqliteRow) -> Transaction {
    let kind_str: String = row.get("kind");
    let amt_str: String = row.get("amount");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
    Transaction {
        id: row.get("id"),
        account_id: row.get("account_id"),
        kind: kind_from_str(&kind_str),
        amount: parse_decimal(&amt_str),
        description: row.get("description"),
        reference: row.get("reference"),
        date: row.get("date"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at,
        updated_at,
    }
}

/// The balance fold, over whichever connection the caller offers.
///
/// The query, the `ORDER BY`, the comment and the error mapping below are
/// unchanged from the pool-only form this was: it is now generic over the
/// executor so `balance_for_account_in` can run it on a caller's
/// `&mut SqliteConnection` and the public wrapper on the pool, with ONE copy of
/// the SQL rather than two that can drift on how a row contributes.
async fn balance_for_account_raw<'e, E>(executor: E, account_id: i64) -> AppResult<Decimal>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    // `ORDER BY id` is load-bearing, not decoration. The check below is on the
    // RUNNING sum, so the order it folds in decides which prefixes it sees, and
    // without an `ORDER BY` that order is a planner decision — the
    // `idx_transactions_account_date` index on `(account_id, date)` can hand the
    // rows back in DATE order while the write pre-check in
    // `TransactionService` folds them in INSERTION order, and the two disagree.
    //
    // They can disagree about the ANSWER too, because addition is not
    // associative over a range that runs out: insert `+0.05e28`, `-0.05e28`,
    // `+0.05e28` and a date-ordered fold sees `0.05 / 0 / 0.05` where an
    // id-ordered one sees `0.05 / 0 / 0.05` — but reorder the dates and a
    // date-ordered fold walks `0.05 / 0.05 → 0.1e28` prefixes an id-ordered fold
    // never sees, and can refuse a balance the pre-check approved. `id` order IS
    // the pre-check's order, because every write appends. It is a covering-index
    // friendly sort on a column the planner already filters and sorts on, so it
    // costs one sort of the account's own rows and nothing else.
    let rows =
        sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ? ORDER BY id"#)
            .bind(account_id)
            .fetch_all(executor)
            .await?;
    // Signed, then summed ONCE through the checked fold: an expense contributes
    // `-amount`, so a set of incomes and expenses is one sum and one bound.
    // Folding `+=` and `-=` per row was the same arithmetic with two raw
    // operators and none. `AppError` rather than sqlx's error, so a refused sum
    // reaches the operator as the same sentence every other refusal answers with.
    //
    // THE FOLD IS THE GUARANTEE, not an induction over the write pre-check: the
    // pre-check in `TransactionService` validates `current + delta` where
    // `current` is this very fold, so it is a check on the whole sum, not on the
    // prefixes of the fold's own iteration — what it buys is that the common case
    // refuses EARLY, with a useful message, instead of waiting for a read to
    // discover it. The two agree on the total precisely because both are sums of
    // the same rows, and the row order is now pinned so their prefixes agree too.
    let signed = signed_amounts(&rows);
    Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
}

/// The rows of a balance query as SIGNED amounts: an income is `+amount`, an
/// expense `-amount`. One helper for every copy of this fold in this layer, so
/// they cannot drift on how a row contributes.
fn signed_amounts(rows: &[sqlx::sqlite::SqliteRow]) -> Vec<Decimal> {
    rows.iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let amt = parse_decimal(&row.get::<String, _>("amount"));
            if kind == "Income" {
                amt
            } else {
                -amt
            }
        })
        .collect()
}

#[async_trait]
impl TransactionRepository for SqliteTransactionRepository {
    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// INSERT, the projection and the `sync_cached` refresh are `create_in`'s to
    /// inherit unchanged; all this adds is the BEGIN/COMMIT, which is the one
    /// thing `create_in` deliberately leaves to someone else.
    ///
    /// The error mapping is also unchanged, and it is the part that is easiest to
    /// break here: `sync_cached` reports through `sqlx::Error` because it runs
    /// mid-unit, and the `?` below is what turns that into an `AppError`. Both
    /// failures a caller can provoke — a FOREIGN KEY rejection on the INSERT, and
    /// a sum `sync_cached` cannot make — still answer `AppError::Database`, and
    /// still leave nothing behind, because dropping `tx` is what rolls the unit
    /// back and it is dropped the same way it always was.
    async fn create(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        let mut tx = self.pool.begin().await?;
        let rec = self
            .create_in(
                &mut tx,
                actor,
                account_id,
                kind,
                amount,
                description,
                reference,
                date,
            )
            .await?;
        tx.commit().await?;
        Ok(rec)
    }

    async fn create_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: &str,
        reference: Option<&str>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        let row = sqlx::query(
            r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(account_id)
        .bind(kind.to_string())
        .bind(amount.to_string())
        .bind(description)
        .bind(reference)
        .bind(date)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;

        let rec = row_to_tx(row);
        // Already the transaction-joining form, before this method existed: the
        // cached balance is refreshed on the SAME connection the row was
        // inserted on, so the two are one unit whoever opened it.
        sync_cached(&mut *tx, account_id).await?;
        Ok(rec)
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Transaction>> {
        let row = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_tx))
    }

    async fn list(&self, filter: &crate::models::TransactionFilter) -> AppResult<Vec<Transaction>> {
        // Build dynamic query with binds - using match for type safety
        let rows = match (filter.account_id, filter.from, filter.to) {
            (None, None, None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions ORDER BY date DESC, id DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC",
                )
                .bind(aid)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), None) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date >= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(from)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), None, Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE date >= ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(from).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(aid), Some(from), Some(to)) => {
                sqlx::query(
                    "SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? AND date >= ? AND date <= ? ORDER BY date DESC, id DESC",
                )
                .bind(aid).bind(from).bind(to)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(row_to_tx).collect())
    }

    async fn list_by_account(&self, account_id: i64) -> AppResult<Vec<Transaction>> {
        let rows = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE account_id = ? ORDER BY date DESC, id DESC"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_tx).collect())
    }

    async fn update(&self, tx_rec: &Transaction, actor: i64) -> AppResult<Transaction> {
        let mut conn = self.pool.begin().await?;
        let row = sqlx::query(
            r#"UPDATE transactions
               SET kind = ?, amount = ?, description = ?, date = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(tx_rec.kind.to_string())
        .bind(tx_rec.amount.to_string())
        .bind(&tx_rec.description)
        .bind(tx_rec.date)
        .bind(actor)
        .bind(tx_rec.id)
        .fetch_one(&mut *conn)
        .await?;

        let rec = row_to_tx(row);
        sync_cached(&mut *conn, rec.account_id).await?;
        conn.commit().await?;
        Ok(rec)
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let existing = sqlx::query(
            r#"SELECT id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at, updated_at FROM transactions WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = existing else {
            return Ok(false);
        };
        let rec = row_to_tx(row);

        let mut tx = self.pool.begin().await?;
        sqlx::query(r#"DELETE FROM transactions WHERE id = ?"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                // The FK from payment rows (sale_payments/purchase_payments) is
                // RESTRICT and finance deliberately knows nothing about those
                // modules; the constraint failure is the only signal. A user
                // action that is not allowed must be a 409 with an actionable
                // message, never a 500.
                if e.to_string().contains("FOREIGN KEY constraint failed") {
                    AppError::Conflict(format!(
                        "transaction {id} belongs to a document payment; cancel the document instead of deleting its money entry"
                    ))
                } else {
                    AppError::Database(e)
                }
            })?;
        sync_cached(&mut *tx, rec.account_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query, the fold and the `PriceRefused` mapping are
    /// `balance_for_account_in`'s to inherit unchanged; all this adds is the
    /// BEGIN/COMMIT that it deliberately leaves to someone else. A read that
    /// opens a transaction is not a write's privilege — the caller that owns the
    /// larger unit is the only one who can see what is in it, and that is the
    /// point of the `_in` form beside it.
    async fn balance_for_account(&self, account_id: i64) -> AppResult<Decimal> {
        let mut tx = self.pool.begin().await?;
        let balance = self.balance_for_account_in(&mut tx, account_id).await?;
        tx.commit().await?;
        Ok(balance)
    }

    async fn balance_for_account_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        account_id: i64,
    ) -> AppResult<Decimal> {
        // The executor is the caller's connection, so the fold sees the rows that
        // caller has written but not yet committed — which is the pre-check's
        // whole reason for existing in a document with more than one line
        // against the same account. Nothing here opens a transaction of its own.
        balance_for_account_raw(&mut *tx, account_id).await
    }

    async fn sync_cached_balance(&self, account_id: i64) -> AppResult<()> {
        let mut conn = self.pool.begin().await?;
        sync_cached(&mut *conn, account_id).await?;
        conn.commit().await?;
        Ok(())
    }
}

/// The cached balance of an account, refreshed inside the caller's transaction.
///
/// The error channel is sqlx's because this runs mid-transaction, where every
/// other statement's channel is sqlx's too. A refused sum therefore travels as
/// `Error::Protocol` carrying the rule's own text — the same bytes
/// `PriceRefusal::as_str` is, so the sentence an operator reads is the sentence
/// the catalog holds and not a driver string.
async fn sync_cached(
    conn: &mut sqlx::SqliteConnection,
    account_id: i64,
) -> Result<(), sqlx::Error> {
    let balance = balance_for_account_raw_pool(conn, account_id)
        .await
        .map_err(|error| match error {
            AppError::PriceRefused(refusal) => sqlx::Error::Protocol(refusal.as_str().to_string()),
            other => sqlx::Error::Protocol(other.to_string()),
        })?;
    sqlx::query(r#"UPDATE accounts SET cached_balance = ? WHERE id = ?"#)
        .bind(balance.to_string())
        .bind(account_id)
        .execute(conn)
        .await?;
    Ok(())
}

async fn balance_for_account_raw_pool(
    conn: &mut sqlx::SqliteConnection,
    account_id: i64,
) -> AppResult<Decimal> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(&mut *conn)
        .await?;
    // Signed, then summed ONCE through the checked fold: an expense contributes
    // `-amount`, so a set of incomes and expenses is one sum and one bound.
    // Folding `+=` and `-=` per row was the same arithmetic with two raw
    // operators and none. `AppError` rather than sqlx's error, so a refused sum
    // reaches the operator as the same sentence every other refusal answers with.
    let signed = signed_amounts(&rows);
    Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use std::time::{Duration, Instant};

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            // Same posture as db::create_pool, and the reason a bad account id is
            // a FOREIGN KEY failure below rather than a row nobody owns.
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

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn seed_account(pool: &SqlitePool, actor: i64, name: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id")
            .bind(name)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The CACHED balance column, read straight from `accounts` the way the
    /// confirm failure-window tests read their residue: `create` refreshes it
    /// inside the same unit as the row, so it is a second thing a rollback has
    /// to take back, and `balance_for_account`'s fold never looks at it.
    async fn cached_balance(pool: &SqlitePool, account_id: i64) -> String {
        sqlx::query_scalar("SELECT cached_balance FROM accounts WHERE id = ?")
            .bind(account_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // This repository is not a copy of the other two, which is why it is its own
    // commit. `create` ALREADY opened a transaction of its own, so moving it is
    // not "add a twin, thin the method" — it is unwinding an existing BEGIN out
    // of the write path, with the free function `sync_cached` (which already
    // takes a `&mut SqliteConnection`) as the shape to inherit. `create_in` is
    // therefore the FIRST version of these statements that has no `pool.begin()`
    // anywhere near it.
    //
    // Nothing here opens a transaction across a service call yet. Phase A adds
    // the doors and changes no behaviour, which the last two tests pin.

    /// The write must join the caller's transaction, so that a document which
    /// rolls back leaves NEITHER the money row nor the refreshed cache behind.
    /// That pair is the finance half of the residue windows W3/W4 in
    /// `odd/tasks/confirm-failure-injection-and-state-predicates.md`: an Income
    /// carrying the burned document number and the full total, on a Draft the
    /// books already show as collected.
    ///
    /// The shape matters, and `max_connections(1)` is why. The rollback happens
    /// BEFORE any pool read, and the only assertions made while `tx` is open are
    /// on values already in hand: a read issued while `tx` holds the only
    /// connection stalls for sqlx's acquire timeout instead of answering. A door
    /// that opened a transaction of its own cannot pass this test at all — it
    /// would have to acquire a second connection here, and on this pool there
    /// isn't one to acquire.
    #[tokio::test]
    async fn create_in_writes_into_the_callers_transaction_and_a_rollback_leaves_nothing() {
        let pool = memory_pool().await;
        let repo = SqliteTransactionRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "create-in").await;

        let mut tx = pool.begin().await.unwrap();
        let written = repo
            .create_in(
                &mut tx,
                actor,
                account,
                TransactionKind::Income,
                dec("10"),
                "2024-SALE-000001",
                Some("2024-SALE-000001"),
                d(2024, 5, 2),
            )
            .await
            .unwrap();
        // Only values the RETURNING clause already produced: no acquisition, so
        // nothing here can stall on the connection `tx` is holding.
        assert_eq!(written.account_id, account);
        assert_eq!(written.kind, TransactionKind::Income);
        assert_eq!(written.amount, dec("10"));
        assert_eq!(written.reference, Some("2024-SALE-000001".to_string()));
        assert_eq!(written.created_by, actor);
        tx.rollback().await.unwrap();

        // Everything below is a pool read, and every one of them is AFTER the
        // rollback, which is what makes them answerable on a one-connection pool.
        assert!(
            repo.find_by_id(written.id).await.unwrap().is_none(),
            "the money row was committed even though the transaction was rolled back"
        );
        assert_eq!(repo.list_by_account(account).await.unwrap().len(), 0);
        assert_eq!(
            repo.balance_for_account(account).await.unwrap(),
            Decimal::ZERO
        );
        // The cached column went with it. `create` refreshes
        // `accounts.cached_balance` INSIDE the same unit, so a rollback that left
        // it at 10 would hand the next page a number with no rows behind it.
        assert_eq!(
            cached_balance(&pool, account).await,
            "0",
            "the cached balance was refreshed outside the caller's rolled-back transaction"
        );
    }

    /// The READ moves in this commit for the same reason the stock level check
    /// moved in the second one, and the arithmetic below is the finance copy of
    /// the same argument.
    ///
    /// `TransactionService` folds the balance, validates the line against it, and
    /// only then writes. In a document with two lines against the same account the
    /// second fold therefore has to see the first line's row: read from the pool it
    /// sees a pre-transaction snapshot, `10` where the account holds `80`, and a
    /// 90-expense line is waved through a drawer that cannot cover it.
    ///
    /// It is only expressible through the `_in` pair, and that is the point: on a
    /// `max_connections(1)` pool the public `balance_for_account` would have to
    /// acquire the one connection `tx` is holding.
    #[tokio::test]
    async fn balance_for_account_in_reads_the_callers_uncommitted_transactions() {
        let pool = memory_pool().await;
        let repo = SqliteTransactionRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "balance-in").await;

        // A committed opening balance first, so "the read saw the caller's writes"
        // is read against a known value rather than against an empty account.
        repo.create(
            actor,
            account,
            TransactionKind::Income,
            dec("10"),
            "opening",
            None,
            d(2024, 5, 1),
        )
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        // The document's two lines, both written into the caller's transaction.
        repo.create_in(
            &mut tx,
            actor,
            account,
            TransactionKind::Income,
            dec("100"),
            "line 1",
            None,
            d(2024, 5, 2),
        )
        .await
        .unwrap();
        repo.create_in(
            &mut tx,
            actor,
            account,
            TransactionKind::Expense,
            dec("30"),
            "line 2",
            None,
            d(2024, 5, 2),
        )
        .await
        .unwrap();

        // 10 + 100 - 30. Off the caller's connection both uncommitted lines are
        // already in the fold; off the pool it would still answer 10.
        let level = repo.balance_for_account_in(&mut tx, account).await.unwrap();
        assert_eq!(
            level,
            dec("80"),
            "the read did not see the caller's own writes"
        );
        // And that is the whole guard: the next line's pre-check folds `80 - 90`,
        // an overdraft, where a stale 10 would have approved it.
        assert!(
            level - dec("90") < Decimal::ZERO,
            "a 90 expense against a stale balance of 10 must be refused, not waved through"
        );
        tx.rollback().await.unwrap();

        // After the rollback the two lines are gone and the balance is back to the
        // committed opening figure — the read side of the write test above.
        assert_eq!(repo.balance_for_account(account).await.unwrap(), dec("10"));
        assert_eq!(repo.list_by_account(account).await.unwrap().len(), 1);
    }

    /// The nesting is GONE, and this is what proves it rather than asserts it.
    ///
    /// `create_in` is called with a transaction ALREADY open on the pool, and it
    /// returns `Ok` in milliseconds. It could only do that by having nowhere to
    /// begin: `self.pool` is in scope and unused, and the only connection the
    /// pool owns is the one the caller's transaction is holding. The timing bound
    /// is the corroboration, not the proof, and its figure is MEASURED in this
    /// module rather than inherited — the one number in it that comes from a timer
    /// is a bound, not an assumption.
    #[tokio::test]
    async fn create_in_joins_an_open_transaction_instead_of_opening_one_of_its_own() {
        let pool = memory_pool().await;
        let repo = SqliteTransactionRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "nesting").await;

        let mut tx = pool.begin().await.unwrap();
        let started = Instant::now();
        let outcome = repo
            .create_in(
                &mut tx,
                actor,
                account,
                TransactionKind::Income,
                dec("10"),
                "line 1",
                None,
                d(2024, 5, 2),
            )
            .await;
        let elapsed = started.elapsed();
        // The caller's transaction is still ALIVE and still holds the write: a
        // second statement on the same connection sees it. A `create_in` that had
        // ended or committed the unit it was handed could not leave this true.
        let level_inside = repo.balance_for_account_in(&mut tx, account).await.unwrap();
        tx.rollback().await.unwrap();

        let written = outcome
            .expect("create_in opened a transaction of its own instead of joining the caller's");
        assert_eq!(written.account_id, account);
        assert_eq!(
            level_inside,
            dec("10"),
            "the caller's transaction lost the write"
        );
        // MEASURED, not assumed: on this `max_connections(1)` pool a `pool.begin()`
        // issued while a transaction holds the only connection returns
        // `pool timed out while waiting for an open connection` after 30.000s. Five
        // seconds sits four orders of magnitude above what a joined write costs and
        // six below the failure it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "create_in took {elapsed:?}; that is a nested BEGIN stalling for a connection, not a joined write"
        );
        // Nothing was committed, which is what "joined" means from the outside.
        assert!(repo.find_by_id(written.id).await.unwrap().is_none());
        assert_eq!(
            repo.balance_for_account(account).await.unwrap(),
            Decimal::ZERO
        );
    }

    /// The additive claim, proved rather than asserted: the public wrappers
    /// still commit their own work, still return the same row, still move the same
    /// two balances, and still fail with the same `AppError` variants. Phase A
    /// changes plumbing and nothing else — so this is the test that would notice
    /// if unwinding `create`'s BEGIN had changed any of that.
    #[tokio::test]
    async fn the_public_wrappers_commit_and_read_back_exactly_as_before() {
        let pool = memory_pool().await;
        let repo = SqliteTransactionRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "wrappers").await;

        let written = repo
            .create(
                actor,
                account,
                TransactionKind::Income,
                dec("10"),
                "2024-SALE-000001",
                Some("2024-SALE-000001"),
                d(2024, 5, 2),
            )
            .await
            .unwrap();
        // Committed: the row is there for the next statement, and RETURNING
        // projected the same fields it always did.
        assert_eq!(repo.list_by_account(account).await.unwrap().len(), 1);
        let stored = repo.find_by_id(written.id).await.unwrap().unwrap();
        assert_eq!(stored.id, written.id);
        assert_eq!(stored.account_id, account);
        assert_eq!(stored.kind, TransactionKind::Income);
        assert_eq!(stored.amount, dec("10"));
        assert_eq!(stored.description, "2024-SALE-000001");
        assert_eq!(stored.reference, Some("2024-SALE-000001".to_string()));
        assert_eq!(stored.date, d(2024, 5, 2));
        assert_eq!(stored.created_by, actor);
        assert_eq!(stored.updated_by, None, "a new money row has no editor");
        assert_eq!(stored.created_at, written.created_at);
        assert_eq!(stored.updated_at, written.updated_at);
        // Both balances move together, which is the half of `create` that a
        // transaction-joining rewrite could plausibly split: the derived fold AND
        // the cached column `create` refreshes inside the same unit.
        assert_eq!(repo.balance_for_account(account).await.unwrap(), dec("10"));
        assert_eq!(cached_balance(&pool, account).await, "10");

        // An expense signs the other way, in both figures.
        repo.create(
            actor,
            account,
            TransactionKind::Expense,
            dec("4"),
            "expense",
            None,
            d(2024, 5, 3),
        )
        .await
        .unwrap();
        assert_eq!(repo.balance_for_account(account).await.unwrap(), dec("6"));
        assert_eq!(cached_balance(&pool, account).await, "6");

        // A NULL reference stays NULL: the column is nullable and the bind is a
        // bare `Option`, so the wrapper must not have started writing "None".
        let unreferenced = repo
            .create(
                actor,
                account,
                TransactionKind::Income,
                dec("1"),
                "manual",
                None,
                d(2024, 5, 4),
            )
            .await
            .unwrap();
        assert_eq!(unreferenced.reference, None);

        // An account with no transactions folds to zero, not an error.
        let untouched = seed_account(&pool, actor, "wrappers-empty").await;
        assert_eq!(
            repo.balance_for_account(untouched).await.unwrap(),
            Decimal::ZERO
        );
        assert_eq!(cached_balance(&pool, untouched).await, "0");
    }

    /// The error mapping, pinned from both sides. This is the one thing a
    /// "begin, delegate, commit" rewrite can quietly change, because the mapping
    /// is not per statement: `sync_cached` reports through `sqlx::Error` so a
    /// refused sum travels mid-unit, and `create`'s `?` is what turns it into an
    /// `AppError`. Both failures below are the ones `create` has always produced.
    #[tokio::test]
    async fn the_public_create_still_fails_with_the_same_app_error_variants() {
        let pool = memory_pool().await;
        let repo = SqliteTransactionRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "errors").await;

        // A `FOREIGN KEY` failure on the INSERT: still `Database`, still sqlx's
        // own message, and still nothing left behind.
        let missing = 9_999_999i64;
        let err = repo
            .create(
                actor,
                missing,
                TransactionKind::Income,
                dec("10"),
                "no such account",
                None,
                d(2024, 5, 2),
            )
            .await
            .expect_err("a transaction against a missing account must fail");
        assert!(
            matches!(err, AppError::Database(ref e) if e.to_string().contains("FOREIGN KEY constraint failed")),
            "the FK failure changed variant or message: {err:?}"
        );
        assert_eq!(repo.list_by_account(missing).await.unwrap().len(), 0);

        // A refused SUM, which is the interesting one: it happens on the SECOND
        // create, inside `sync_cached`, after the row was already inserted. The
        // wrapper must roll that row back, and must report the refusal the way it
        // always has — through `sqlx::Error::Protocol`, carrying the rule's own
        // text, NOT as `PriceRefused`. `PriceRefused` belongs to the READ path
        // (`balance_for_account`); the WRITE path has always answered `Database`.
        repo.create(
            actor,
            account,
            TransactionKind::Income,
            dec("40000000000000000000000000000"),
            "first half",
            None,
            d(2024, 5, 1),
        )
        .await
        .unwrap();
        let err = repo
            .create(
                actor,
                account,
                TransactionKind::Income,
                dec("40000000000000000000000000000"),
                "second half",
                None,
                d(2024, 5, 2),
            )
            .await
            .expect_err("a sum that cannot be made must fail");
        let AppError::Database(ref e) = err else {
            panic!("the refused sum changed variant: {err:?}");
        };
        assert!(
            e.to_string()
                .contains(crate::models::PriceRefusal::AggregateTooLarge.as_str()),
            "the refusal lost the rule's own text: {e}"
        );
        // The half that could be written is still the only half: the failed unit
        // left no second row behind.
        assert_eq!(repo.list_by_account(account).await.unwrap().len(), 1);
        assert_eq!(
            cached_balance(&pool, account).await,
            "40000000000000000000000000000"
        );
    }
}
