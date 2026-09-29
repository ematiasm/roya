use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::DocSequence;

/// Sequential document numbering (M2 sales first consumer: `SALE`).
/// Number is assigned on confirm via an atomic row UPDATE (UPSERT + RETURNING),
/// so abandoned Drafts never consume numbers.
#[async_trait]
pub trait DocSequenceRepository: Send + Sync {
    /// Atomically increment and return the new `last_number` for `(doc_type, year)`.
    /// Creates the row lazily on first call (returns 1).
    async fn next_number(&self, doc_type: &str, year: i32) -> AppResult<i64>;
    /// [`Self::next_number`] inside a transaction the CALLER owns, so a whole
    /// document can be one unit: the number is consumed at COMMIT rather than at
    /// statement execution, and a confirmation that rolls back therefore gets
    /// its number back instead of burning it.
    ///
    /// This is the shape Odoo's `no_gap` sequences have, and `doc_sequences` was
    /// already built for it: the counter is a row keyed `(doc_type, year)`, not
    /// a `MAX()` over documents, so the increment has somewhere to join a larger
    /// transaction. `next_number` above is this method wrapped in a transaction
    /// of its own, for the callers that have no larger unit to offer.
    async fn next_number_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        doc_type: &str,
        year: i32,
    ) -> AppResult<i64>;
    async fn current(&self, doc_type: &str, year: i32) -> AppResult<Option<DocSequence>>;
}

#[derive(Clone)]
pub struct SqliteDocSequenceRepository {
    pub pool: SqlitePool,
}

impl SqliteDocSequenceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DocSequenceRepository for SqliteDocSequenceRepository {
    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// SQL and the error mapping are `next_number_in`'s to inherit unchanged;
    /// all this adds is the BEGIN/COMMIT that it deliberately leaves to someone
    /// else.
    async fn next_number(&self, doc_type: &str, year: i32) -> AppResult<i64> {
        let mut tx = self.pool.begin().await?;
        let number = self.next_number_in(&mut tx, doc_type, year).await?;
        tx.commit().await?;
        Ok(number)
    }

    async fn next_number_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        doc_type: &str,
        year: i32,
    ) -> AppResult<i64> {
        // Atomic: single UPSERT statement. First call inserts last_number=1,
        // later calls increment. RETURNING gives the new value. The executor is
        // the caller's connection, so the increment is theirs to commit or to
        // roll back, and nothing here opens a transaction of its own.
        let row: (i64,) = sqlx::query_as(
            r#"INSERT INTO doc_sequences (doc_type, year, last_number)
               VALUES (?, ?, 1)
               ON CONFLICT(doc_type, year) DO UPDATE SET last_number = last_number + 1
               RETURNING last_number"#,
        )
        .bind(doc_type)
        .bind(year)
        .fetch_one(&mut *tx)
        .await?;
        Ok(row.0)
    }

    async fn current(&self, doc_type: &str, year: i32) -> AppResult<Option<DocSequence>> {
        let row = sqlx::query_as::<_, (String, i32, i64)>(
            r#"SELECT doc_type, year, last_number FROM doc_sequences
               WHERE doc_type = ? AND year = ?"#,
        )
        .bind(doc_type)
        .bind(year)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(doc_type, year, last_number)| DocSequence {
            doc_type,
            year,
            last_number,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn repo() -> SqliteDocSequenceRepository {
        SqliteDocSequenceRepository::new(test_pool().await)
    }

    /// The counter's last value read straight from the table, the way the
    /// confirm failure-window tests read theirs: what `last_number` actually is,
    /// which is the only thing a spent number is visible in.
    async fn last_number(
        r: &SqliteDocSequenceRepository,
        doc_type: &str,
        year: i32,
    ) -> Option<i64> {
        sqlx::query_as::<_, (i64,)>(
            "SELECT last_number FROM doc_sequences WHERE doc_type = ? AND year = ?",
        )
        .bind(doc_type)
        .bind(year)
        .fetch_optional(&r.pool)
        .await
        .unwrap()
        .map(|row| row.0)
    }

    #[tokio::test]
    async fn next_number_counts_up_and_keeps_every_consumer_separate() {
        let r = repo().await;
        assert_eq!(r.next_number("SALE", 2024).await.unwrap(), 1);
        assert_eq!(r.next_number("SALE", 2024).await.unwrap(), 2);
        assert_eq!(r.next_number("SALE", 2024).await.unwrap(), 3);
        // A different doc_type, and a different year, are different counters:
        // the primary key is (doc_type, year), so neither shares with SALE 2024.
        assert_eq!(r.next_number("PURCH", 2024).await.unwrap(), 1);
        assert_eq!(r.next_number("SALE", 2025).await.unwrap(), 1);
        assert_eq!(last_number(&r, "SALE", 2024).await, Some(3));
    }

    /// The test that decides whether `next_number_in` is a real door or a
    /// painted one: it must spend its number into the caller's transaction, so
    /// that a rolled-back confirm GIVES THE NUMBER BACK instead of burning it.
    ///
    /// The shape matters, and `max_connections(1)` is why. The rollback happens
    /// BEFORE the assertion, and the counter is read only afterwards: a read
    /// issued while `tx` holds the only connection would stall for sqlx's
    /// acquire timeout instead of answering. A door that committed on its own
    /// fails this test either way — measured, not assumed — but not the way one
    /// would guess: it cannot acquire a second connection here at all, so it
    /// surfaces as `PoolTimedOut` after 30s, not as a spent number.
    #[tokio::test]
    async fn next_number_in_spends_into_the_callers_transaction_and_a_rollback_returns_it() {
        let r = repo().await;
        // Seed one committed number first, so "the counter did not advance" is
        // read against a known value rather than against an empty table.
        assert_eq!(r.next_number("SALE", 2024).await.unwrap(), 1);

        let mut tx = r.pool.begin().await.unwrap();
        let spent = r.next_number_in(&mut tx, "SALE", 2024).await.unwrap();
        // Read back INSIDE the transaction: the UPSERT ran, and the new number
        // came off the caller's own connection.
        assert_eq!(spent, 2);
        tx.rollback().await.unwrap();

        assert_eq!(
            last_number(&r, "SALE", 2024).await,
            Some(1),
            "the number was spent even though the transaction was rolled back"
        );
        // And the counter is still usable: the next confirm gets the number the
        // rolled-back attempt had reserved, not the one after it.
        assert_eq!(r.next_number("SALE", 2024).await.unwrap(), 2);
    }
}
