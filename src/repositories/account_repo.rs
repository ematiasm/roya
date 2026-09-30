use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Account, AccountWithBalance, SetMoney};

// ---------------------------------------------------------------------------
// Trait (portable to Postgres)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait AccountRepository: Send + Sync {
    async fn create(&self, actor: i64, name: &str) -> AppResult<Account>;
    async fn list(&self) -> AppResult<Vec<Account>>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>>;
    /// Every account with its derived balance, TOLERANT: an account whose
    /// transactions cannot be added up keeps its place in the list carrying the
    /// rule, so one unmeasurable account never empties the finance page around it.
    async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>>;
    /// One account's derived balance, tolerant for the same reason — a detail page
    /// is a list of one.
    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>>;

    /// [`Self::exists`] inside a transaction the CALLER owns — and it is a READ
    /// that has to move, because every caller of it uses it to guard a write.
    ///
    /// This check validates a row that the SAME transaction is about to write: it
    /// is asked "does account N exist" and the answer is consumed by a money row
    /// or a payment row that carries `account_id = N`. Read inside that unit, it
    /// is a statement about the row the write will touch. Read off the pool it is
    /// a statement about a different moment, and the gap between the two is
    /// exactly the window a concurrent delete falls into: the check says yes, the
    /// unit writes a row against an account that no longer exists, and the
    /// `FOREIGN KEY` is the only thing left to catch it — by which point the
    /// caller is holding a driver error where it expected a validated write.
    ///
    /// It is also the only read in the closure whose guard is enforced by a
    /// timeout rather than by a fold. `TransactionService::create_with_reference`
    /// calls it at `src/services/transaction.rs:86` and then hands the very same
    /// `account_id` to the transaction write; `sales.rs:1632` and `:1657` and
    /// `purchases.rs:1445` do the same for a payment line. Once those writes are
    /// driven from a caller's transaction, a pool-based `exists` inside it cannot
    /// be acquired at all on a one-connection pool — it stalls for sqlx's 30s
    /// acquire timeout and answers `PoolTimedOut` — and on a many-connection pool
    /// it answers from a snapshot the unit is about to invalidate. Both failures
    /// are this method's to prevent.
    ///
    /// Nothing opens a transaction yet. This is the door; the confirm path does
    /// not walk through it until a later commit of Phase A does.
    async fn exists_in(&self, tx: &mut sqlx::SqliteConnection, id: i64) -> AppResult<bool>;

    async fn exists(&self, id: i64) -> AppResult<bool>;
}

// ---------------------------------------------------------------------------
// Helpers: row mapping (Decimal stored as TEXT for SQLite precision)
// ---------------------------------------------------------------------------

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_account(row: sqlx::sqlite::SqliteRow) -> Account {
    let cached_str: String = row.get("cached_balance");
    Account {
        id: row.get("id"),
        name: row.get("name"),
        cached_balance: parse_decimal(&cached_str),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
    }
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SqliteAccountRepository {
    pub pool: SqlitePool,
}

impl SqliteAccountRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountRepository for SqliteAccountRepository {
    async fn create(&self, actor: i64, name: &str) -> AppResult<Account> {
        let row = sqlx::query(
            r#"INSERT INTO accounts (name, created_by) VALUES (?, ?)
               RETURNING id, name, cached_balance, created_by, updated_by, created_at"#,
        )
        .bind(name.trim())
        .bind(actor)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_account(row))
    }

    async fn list(&self) -> AppResult<Vec<Account>> {
        let rows = sqlx::query(
            r#"SELECT id, name, cached_balance, created_by, updated_by, created_at FROM accounts ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_account).collect())
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Account>> {
        let row = sqlx::query(
            r#"SELECT id, name, cached_balance, created_by, updated_by, created_at FROM accounts WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_account))
    }

    /// Balance is always derived from transactions SUM (cached_balance is kept in sync but not trusted).
    /// For SQLite we compute in Rust to preserve Decimal precision (TEXT -> Decimal sum).
    async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>> {
        let accounts = self.list().await?;
        let mut out = Vec::with_capacity(accounts.len());
        for acc in accounts {
            out.push(with_balance(&self.pool, acc).await?);
        }
        Ok(out)
    }

    async fn find_with_balance(&self, id: i64) -> AppResult<Option<AccountWithBalance>> {
        match self.find_by_id(id).await? {
            None => Ok(None),
            Some(acc) => Ok(Some(with_balance(&self.pool, acc).await?)),
        }
    }

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query and the error mapping are `exists_in`'s to inherit unchanged; all
    /// this adds is the BEGIN/COMMIT that it deliberately leaves to someone else.
    /// A read that opens a transaction is not a write's privilege — the caller
    /// that owns the larger unit is the only one who can see what is in it, and
    /// that is the whole point of the `_in` form beside it.
    async fn exists(&self, id: i64) -> AppResult<bool> {
        let mut tx = self.pool.begin().await?;
        let found = self.exists_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(found)
    }

    /// The check runs on the caller's connection, so it sees the rows that caller
    /// has written but not yet committed and is blind to the ones it has rolled
    /// back. The SQL and the `row.0 > 0` mapping are byte-for-byte what `exists`
    /// always ran; all this changes is the executor.
    async fn exists_in(&self, tx: &mut sqlx::SqliteConnection, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM accounts WHERE id = ?"#)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        Ok(row.0 > 0)
    }
}

/// One account with its derived balance, TOLERANT.
///
/// The fold below is the checked one, and a refusal is not an error here: the
/// account keeps its place in the list and the row states the rule. The CACHED
/// balance goes with it only when the derived figure carried — a cache of a sum
/// that cannot be made is a stale number in the place of the number.
async fn with_balance(pool: &SqlitePool, acc: Account) -> AppResult<AccountWithBalance> {
    let balance = match balance_for_account(pool, acc.id).await {
        Ok(amount) => SetMoney::amount(amount),
        Err(AppError::PriceRefused(refusal)) => SetMoney::refused(refusal),
        Err(other) => return Err(other),
    };
    let cached_balance = balance.amount.map(|_| acc.cached_balance);
    Ok(AccountWithBalance {
        id: acc.id,
        name: acc.name,
        balance,
        cached_balance,
        created_by: acc.created_by,
        updated_by: acc.updated_by,
        created_at: acc.created_at,
    })
}

async fn balance_for_account(pool: &SqlitePool, account_id: i64) -> AppResult<Decimal> {
    let rows = sqlx::query(r#"SELECT kind, amount FROM transactions WHERE account_id = ?"#)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    let signed: Vec<Decimal> = rows
        .iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let amt = parse_decimal(&row.get::<String, _>("amount"));
            if kind == "Income" {
                amt
            } else {
                -amt
            }
        })
        .collect();
    // `AppError` rather than sqlx's error, so a refused sum reaches the operator
    // as the same sentence every other refusal answers with, instead of as a
    // driver error. `AppError: From<sqlx::Error>` still covers the query.
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
            // Same posture as db::create_pool, and the reason the account-existence
            // check below is not the only thing standing between a money row and
            // an account nobody owns.
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn seed_account(pool: &SqlitePool, actor: i64, name: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id")
            .bind(name)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // This is the sharpest edge in the closure, and it is a READ that guards a
    // WRITE. `TransactionService::create_with_reference` checks that the account
    // exists, and only then hands `create_in` the very same `account_id` — so the
    // check is a statement about the row the transaction is about to touch. Off
    // the caller's connection it is a statement about a different moment in time
    // entirely.
    //
    // Nothing here opens a transaction across a service call. Phase A installs
    // the door; `create_with_reference` does not walk through it until a later
    // commit of Phase A does, and the last test pins that the public `exists` is
    // untouched in the meantime.

    /// The check must read the row the SAME transaction is about to write, so a
    /// row created inside that transaction is visible to it and a row rolled back
    /// with it disappears from it.
    ///
    /// This is the existence check in the shape the document flows will use it
    /// in: the caller's unit holds the write, and the guard has to hold the same
    /// unit or it guards a snapshot. Read from the pool the two answers below are
    /// both wrong — the second one reports a live account as missing, which is
    /// the direction that turns a valid document into a `NotFound`.
    #[tokio::test]
    async fn exists_in_reads_the_callers_uncommitted_accounts_and_a_rollback_hides_them_again() {
        let pool = memory_pool().await;
        let repo = SqliteAccountRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        // A committed account first, so the uncommitted read below is read
        // against a known value rather than against an empty table.
        let committed = seed_account(&pool, actor, "committed").await;
        assert!(repo.exists(committed).await.unwrap());

        let mut tx = pool.begin().await.unwrap();
        // The account the caller's own transaction creates, still uncommitted.
        // Nothing outside this unit can see it, and `exists_in` is the only
        // executor that can be asked about it while the unit is open.
        let uncommitted = sqlx::query_scalar(
            "INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id",
        )
        .bind("created-inside")
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();

        assert!(
            repo.exists_in(&mut tx, uncommitted).await.unwrap(),
            "the existence check did not see the account its own transaction created, so it is validating a different moment than the write it guards"
        );
        // And it is a plain existence check, not a general read: a row that is
        // not there is still an ordinary `false`, which is the branch the caller
        // turns into `AppError::NotFound`.
        assert!(
            !repo.exists_in(&mut tx, 9_999_999).await.unwrap(),
            "an unknown id must answer false, not error"
        );
        tx.rollback().await.unwrap();

        // The rollback took the account with it, and the read side agrees — which
        // is the other half: an `exists_in` that could not see the rollback was
        // reading something other than the caller's transaction.
        assert!(
            !repo.exists(uncommitted).await.unwrap(),
            "the account survived a rollback of the transaction that created it"
        );
        // The committed one is untouched, so the rollback above was the unit's
        // and not the pool's.
        assert!(repo.exists(committed).await.unwrap());
    }

    /// THE test of this commit, and the reason it is not mechanical: `exists_in`
    /// must not reach for the pool AT ALL, and the assertion is the pairing
    /// itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on this
    /// pool at all, ever, and would not answer `true`/`false`; it would sit on
    /// sqlx's acquire timeout and come back as `PoolTimedOut`. This test therefore
    /// cannot pass by being slow, and it cannot pass by accident.
    #[tokio::test]
    async fn exists_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = memory_pool().await;
        let repo = SqliteAccountRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "pairing").await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a read
        // right now, and that is a fact about the pool, not about this test's
        // patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let found = repo.exists_in(&mut tx, account).await;
        let elapsed = started.elapsed();
        let found = found.expect(
            "exists_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert!(found);
        // MEASURED, not assumed: the pairing above already decides it, and this
        // bound is the corroboration. Five seconds sits four orders of magnitude
        // above what a query on a held connection costs and six below the 30s
        // acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "exists_in took {elapsed:?}; that is a read stalling for a connection, not one on the connection it was handed"
        );
        // The caller's transaction is still ALIVE and still holds its lock: a
        // second statement on the same connection answers. An `exists_in` that had
        // ended, committed or rolled back the unit it was given could not leave
        // this true.
        assert!(repo.exists_in(&mut tx, account).await.unwrap());
        assert!(!repo.exists_in(&mut tx, 9_999_999).await.unwrap());
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert!(repo.exists(account).await.unwrap());
    }

    /// The additive claim, proved rather than asserted: the public `exists`
    /// still answers exactly what it always answered, in both directions. The
    /// caller distinguishes "account missing" from "error" — `create_with_reference`
    /// turns the first into `AppError::NotFound` and propagates the second — so
    /// `Ok(false)` is an answer this rewrite must not turn into anything else.
    #[tokio::test]
    async fn the_public_exists_answers_exactly_as_before_including_the_missing_account() {
        let pool = memory_pool().await;
        let repo = SqliteAccountRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let account = seed_account(&pool, actor, "public").await;

        assert!(repo.exists(account).await.unwrap());
        // The not-found branch is a VALUE, not an error: the wrapper must not
        // have started mapping it, and must not have started refusing it either.
        assert!(matches!(repo.exists(9_999_999).await, Ok(false)));
        assert!(matches!(repo.exists(0).await, Ok(false)));
        // Two accounts, two `true`: the check is per id, not "is the table
        // non-empty", which is what a wrapper that dropped its bind would answer.
        let second = seed_account(&pool, actor, "public-2").await;
        assert!(repo.exists(second).await.unwrap());
        // And the wrapper leaves no unit of its own behind: it is answerable
        // again immediately, and the row it read is still the row it found.
        assert!(repo.exists(account).await.unwrap());
        assert!(repo.exists(second).await.unwrap());
    }
}
