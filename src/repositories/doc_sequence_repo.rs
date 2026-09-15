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
    async fn next_number(&self, doc_type: &str, year: i32) -> AppResult<i64> {
        // Atomic: single UPSERT statement. First call inserts last_number=1,
        // later calls increment. RETURNING gives the new value.
        let row: (i64,) = sqlx::query_as(
            r#"INSERT INTO doc_sequences (doc_type, year, last_number)
               VALUES (?, ?, 1)
               ON CONFLICT(doc_type, year) DO UPDATE SET last_number = last_number + 1
               RETURNING last_number"#,
        )
        .bind(doc_type)
        .bind(year)
        .fetch_one(&self.pool)
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
