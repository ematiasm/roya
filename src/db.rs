use chrono::NaiveDateTime;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

/// Canonical TEXT encoding for every timestamp this database stores or binds.
///
/// It produces exactly the shape SQLite itself writes — the same bytes as
/// `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`, i.e. the migration
/// `DEFAULT`s and every DB-side `revoked_at`/`updated_at` stamp:
/// `2024-05-01T12:00:00.000Z` (ISO with `T`, milliseconds, trailing `Z`).
///
/// This is the project-wide convention every `WHERE <db_column> <op> ?` on a
/// timestamp TEXT column must honour. The columns are TEXT, so comparisons
/// are lexical: a Rust-bound value in any other shape (chrono's `Display`,
/// e.g. `2024-05-01 12:00:00`, sorts *before* every ISO string because `' '`
/// < `'T'`) silently mis-orders against DB-written values. Bind through this
/// helper — never a raw `NaiveDateTime` — whenever a bound value can meet a
/// DB-written one in a comparison.
pub fn encode_sqlite_timestamp(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Create a pool from DATABASE_URL.  Falls back to `sqlite://roya.db`.
pub async fn create_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        // Per-connection: SQLite fires BEFORE DELETE triggers during REPLACE
        // conflict resolution (INSERT OR REPLACE, REPLACE INTO, UPDATE OR
        // REPLACE) only when recursive triggers are on. The walk-in backstop
        // triggers only RAISE(ABORT), so enabling this cannot recurse.
        .pragma("recursive_triggers", "1")
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    // Run migrations embedded at compile time (uses `migrations/` dir)
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}
