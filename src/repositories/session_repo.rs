// Identity kernel: sessions repository (Slice S1a). One row per login; only
// the sha256 digest of the cookie token is stored or compared (`token_hash`),
// so no read path ever touches a raw token. Validity is decided in SQL
// (`revoked_at IS NULL AND expires_at > :now` joined to an active user) so an
// expired, revoked or deactivated-owner session is refused by the query, not
// only by service code. Renewal is a single UPDATE of `last_seen_at` +
// `expires_at`; revocation is idempotent and permanent (schema trigger).
use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::{Row, SqlitePool};

use crate::db::encode_sqlite_timestamp;
use crate::error::{AppError, AppResult};
use crate::models::{NewSession, Session, User};

fn row_to_session(row: &sqlx::sqlite::SqliteRow) -> Session {
    Session {
        id: row.get("id"),
        token_hash: row.get("token_hash"),
        user_id: row.get("user_id"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        last_seen_at: row.get("last_seen_at"),
        revoked_at: row.get("revoked_at"),
        user_agent: row.get("user_agent"),
    }
}

fn row_to_user(row: &sqlx::sqlite::SqliteRow) -> User {
    let active: i64 = row.get("u_is_active");
    let must_change: i64 = row.get("u_must_change_password");
    User {
        id: row.get("u_id"),
        username: row.get("u_username"),
        display_name: row.get("u_display_name"),
        is_active: active == 1,
        must_change_password: must_change == 1,
        last_login_at: row.get("u_last_login_at"),
        created_by: row.get("u_created_by"),
        updated_by: row.get("u_updated_by"),
        created_at: row.get("u_created_at"),
        updated_at: row.get("u_updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        // token_hash UNIQUE: a collision (or a replayed insert) must never
        // overwrite an existing session row.
        AppError::Conflict("session already exists".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for session".into())
    } else if s.contains("revoked_at cannot be cleared") {
        // The schema trigger refuses to un-revoke; surface it as the
        // programming/validation error it is, not a 500.
        AppError::Validation("revoked_at cannot be cleared; revocation is permanent".into())
    } else if s.contains("CHECK constraint failed") {
        if s.contains("sessions_user_agent_shape") {
            AppError::Validation("user agent must be at most 256 characters".into())
        } else {
            AppError::Validation("session fields violate schema rules".into())
        }
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn insert(&self, input: &NewSession) -> AppResult<Session>;
    /// Validity decided in SQL: not revoked, not expired, owner active.
    async fn resolve_valid(
        &self,
        token_hash: &str,
        now: NaiveDateTime,
    ) -> AppResult<Option<(Session, User)>>;
    /// Sliding renewal: one UPDATE of `last_seen_at` and `expires_at` together,
    /// guarded on the row still being unrevoked.
    async fn renew(
        &self,
        token_hash: &str,
        last_seen_at: NaiveDateTime,
        expires_at: NaiveDateTime,
    ) -> AppResult<()>;
    /// Idempotent: revoking twice affects one row, then zero, and never errors.
    async fn revoke(&self, token_hash: &str) -> AppResult<bool>;
    /// Revoke every live session of a user (password change / deactivate).
    async fn revoke_all_for_user(&self, user_id: i64) -> AppResult<u64>;
    /// Revoke every live session of a user EXCEPT the one whose stored digest
    /// is `keep_token_hash`: the password-change form (a successful change
    /// kills the user's other sessions and keeps the acting one). One
    /// statement — no delete, no re-insert, no window in which the acting
    /// cookie names no row, and the kept row keeps its id and its expiry.
    /// Idempotent like `revoke`: the second call matches zero rows.
    async fn revoke_all_for_user_except(
        &self,
        user_id: i64,
        keep_token_hash: &str,
    ) -> AppResult<u64>;
    /// Removes rows that are expired or revoked, immediately: there is no
    /// retention window, so a revoked-but-unexpired row goes away on the very
    /// next prune. Called by the service, never a background task.
    async fn prune(&self, now: NaiveDateTime) -> AppResult<u64>;
}

#[derive(Clone)]
pub struct SqliteSessionRepository {
    pub pool: SqlitePool,
}

impl SqliteSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn insert(&self, input: &NewSession) -> AppResult<Session> {
        let row = sqlx::query(
            r#"INSERT INTO sessions (token_hash, user_id, expires_at, last_seen_at, user_agent)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, token_hash, user_id, created_at, expires_at,
                         last_seen_at, revoked_at, user_agent"#,
        )
        .bind(&input.token_hash)
        .bind(input.user_id)
        .bind(encode_sqlite_timestamp(input.expires_at))
        .bind(encode_sqlite_timestamp(input.last_seen_at))
        .bind(input.user_agent.clone())
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_session(&row))
    }

    async fn resolve_valid(
        &self,
        token_hash: &str,
        now: NaiveDateTime,
    ) -> AppResult<Option<(Session, User)>> {
        let row = sqlx::query(
            r#"SELECT s.id, s.token_hash, s.user_id, s.created_at, s.expires_at,
                      s.last_seen_at, s.revoked_at, s.user_agent,
                      u.id AS u_id, u.username AS u_username,
                      u.display_name AS u_display_name, u.is_active AS u_is_active,
                      u.must_change_password AS u_must_change_password,
                      u.last_login_at AS u_last_login_at,
                      u.created_by AS u_created_by, u.updated_by AS u_updated_by,
                      u.created_at AS u_created_at, u.updated_at AS u_updated_at
               FROM sessions s
               JOIN users u ON u.id = s.user_id
               WHERE s.token_hash = ?
                 AND s.revoked_at IS NULL
                 AND s.expires_at > ?
                 AND u.is_active = 1"#,
        )
        .bind(token_hash)
        .bind(encode_sqlite_timestamp(now))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (row_to_session(&r), row_to_user(&r))))
    }

    async fn renew(
        &self,
        token_hash: &str,
        last_seen_at: NaiveDateTime,
        expires_at: NaiveDateTime,
    ) -> AppResult<()> {
        // One UPDATE: the horizon check lives with the caller (SessionPolicy),
        // the atomic last_seen/expires pair lives here.
        sqlx::query("UPDATE sessions SET last_seen_at = ?, expires_at = ? WHERE token_hash = ? AND revoked_at IS NULL")
            .bind(encode_sqlite_timestamp(last_seen_at))
            .bind(encode_sqlite_timestamp(expires_at))
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke(&self, token_hash: &str) -> AppResult<bool> {
        // Idempotent by construction: the second call matches zero rows.
        let res = sqlx::query(
            "UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn revoke_all_for_user(&self, user_id: i64) -> AppResult<u64> {
        let res = sqlx::query(
            "UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn revoke_all_for_user_except(
        &self,
        user_id: i64,
        keep_token_hash: &str,
    ) -> AppResult<u64> {
        // One UPDATE, same shape as `revoke_all_for_user` minus the kept row.
        // The database stamps `revoked_at` with its own strftime 'now' — the
        // same form every other revocation in this repository writes — and
        // the kept row is excluded by its stored digest, so no Rust-bound
        // timestamp crosses the boundary at all.
        let res = sqlx::query(
            "UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE user_id = ? AND token_hash != ? AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(keep_token_hash)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn prune(&self, now: NaiveDateTime) -> AppResult<u64> {
        let res = sqlx::query(
            "DELETE FROM sessions WHERE expires_at <= ? OR (revoked_at IS NOT NULL AND revoked_at <= ?)",
        )
        .bind(encode_sqlite_timestamp(now))
        .bind(encode_sqlite_timestamp(now))
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn pool() -> SqlitePool {
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

    fn base_time() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 5, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    async fn seed_user_id(db: &SqlitePool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO users (username, display_name, password_hash) VALUES ('teller', 'Teller', 'not-a-real-hash') RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        row.0
    }

    async fn insert_session(
        repo: &SqliteSessionRepository,
        token_hash: &str,
        user_id: i64,
        now: NaiveDateTime,
    ) -> Session {
        repo.insert(&NewSession {
            token_hash: token_hash.into(),
            user_id,
            expires_at: now + Duration::hours(12),
            last_seen_at: now,
            user_agent: None,
        })
        .await
        .unwrap()
    }

    /// F3 regression: a timestamp written by the DATABASE (its strftime
    /// canonical form) must compare correctly against a Rust-bound `now` of
    /// the same instant, in the exact direction `prune` uses. Under the old
    /// chrono-`Display` encoding the bound `now` sorted before every DB
    /// string (`'T'` > `' '`), so a row revoked the same instant was never
    /// matched. `expires_at` stays in the future under both encodings, so
    /// only the revoked branch can decide the outcome.
    #[tokio::test]
    async fn f3_prune_matches_a_db_written_revoked_at_against_the_same_bound_instant() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let now = base_time();

        let revoked_row = insert_session(&repo, "hash-revoked", user_id, now).await;
        let _live = insert_session(&repo, "hash-live", user_id, now).await;

        // Revocation stamps `revoked_at` in SQLite's own canonical form —
        // written by the database (strftime), not bound from Rust.
        sqlx::query("UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', ?) WHERE token_hash = 'hash-revoked'")
            .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
            .execute(&repo.pool)
            .await
            .unwrap();

        // Sanity: the database form and the helper form are byte-identical.
        let stored: String = sqlx::query("SELECT revoked_at FROM sessions WHERE id = ?")
            .bind(revoked_row.id)
            .fetch_one(&repo.pool)
            .await
            .unwrap()
            .get("revoked_at");
        assert_eq!(stored, encode_sqlite_timestamp(now));

        // A bound `now` one second past the revocation: same instant, so the
        // lexical comparison must decide on TIME, not on encoding.
        let pruned = repo.prune(now + Duration::seconds(1)).await.unwrap();
        assert_eq!(pruned, 1, "the revoked-but-unexpired row must be pruned");

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, 1, "the live row must survive");
    }

    // -- N2: every map_db_err branch is pinned ----------------------------------

    // -- the password-change revocation (AC16, slice S3 part 1) ------------------

    /// The `_except` form is the rule the password change lives on: of three
    /// live sessions for one user it revokes exactly two, leaves the named
    /// row fully intact (same id, same expiry, `revoked_at` still NULL), is
    /// idempotent on a second call, and never touches another user's rows.
    #[tokio::test]
    async fn revoke_all_for_user_except_revokes_the_others_and_keeps_the_named_row_alive() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let now = base_time();

        let kept = insert_session(&repo, "hash-kept", user_id, now).await;
        let _a = insert_session(&repo, "hash-a", user_id, now).await;
        let _b = insert_session(&repo, "hash-b", user_id, now).await;

        // A second user with a live session of their own: the statement is
        // scoped by user_id and must leave this row alone.
        let other_user: (i64,) = sqlx::query_as(
            "INSERT INTO users (username, display_name, password_hash) VALUES ('cashier', 'Cashier', 'not-a-real-hash') RETURNING id",
        )
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        let other = insert_session(&repo, "hash-other", other_user.0, now).await;

        let revoked = repo
            .revoke_all_for_user_except(user_id, "hash-kept")
            .await
            .unwrap();
        assert_eq!(
            revoked, 2,
            "exactly the two other live sessions of the user"
        );

        // The named row survives with its identity untouched: same id, same
        // expiry, still unrevoked — resolvable like nothing happened.
        let kept_after = repo
            .resolve_valid("hash-kept", now + Duration::seconds(1))
            .await
            .unwrap()
            .expect("the kept session must still resolve");
        assert_eq!(kept_after.0.id, kept.id, "the session row keeps its id");
        assert_eq!(
            kept_after.0.expires_at, kept.expires_at,
            "the expiry is untouched"
        );
        assert!(kept_after.0.revoked_at.is_none());
        let live_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(user_id)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(live_count.0, 1, "exactly one live row remains for the user");

        // The other two sessions of the same user are dead.
        assert!(repo
            .resolve_valid("hash-a", now + Duration::seconds(1))
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .resolve_valid("hash-b", now + Duration::seconds(1))
            .await
            .unwrap()
            .is_none());

        // Another user's rows are never touched.
        let other_after = repo
            .resolve_valid("hash-other", now + Duration::seconds(1))
            .await
            .unwrap()
            .expect("another user's session must be untouched");
        assert_eq!(other_after.0.id, other.id);
        assert!(other_after.0.revoked_at.is_none());

        // Idempotent: the second call matches zero rows.
        let again = repo
            .revoke_all_for_user_except(user_id, "hash-kept")
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    /// The kept digest names a row that does not exist (or belongs to someone
    /// else): the statement must still be a scoped success, revoking the
    /// user's rows and never another user's.
    #[tokio::test]
    async fn revoke_all_for_user_except_with_an_unknown_kept_digest_scopes_by_user() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let other_user: (i64,) = sqlx::query_as(
            "INSERT INTO users (username, display_name, password_hash) VALUES ('cajero', 'Cajero', 'not-a-real-hash') RETURNING id",
        )
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        let now = base_time();
        insert_session(&repo, "hash-mine", user_id, now).await;
        let theirs = insert_session(&repo, "hash-theirs", other_user.0, now).await;

        // Keeping an unknown digest revokes everything of the user's...
        let revoked = repo
            .revoke_all_for_user_except(user_id, "hash-no-such-row")
            .await
            .unwrap();
        assert_eq!(revoked, 1);
        assert!(repo
            .resolve_valid("hash-mine", now + Duration::seconds(1))
            .await
            .unwrap()
            .is_none());
        // ... and still never touches the other user's row.
        let theirs_after = repo
            .resolve_valid("hash-theirs", now + Duration::seconds(1))
            .await
            .unwrap()
            .expect("another user's session must be untouched");
        assert_eq!(theirs_after.0.id, theirs.id);
        assert!(theirs_after.0.revoked_at.is_none());
    }

    /// N2: a duplicate `token_hash` insert (the UNIQUE backstop the service
    /// trusts) must surface as `Conflict`, never as `Database` (500).
    #[tokio::test]
    async fn n2_duplicate_token_hash_maps_to_conflict() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let now = base_time();
        insert_session(&repo, "hash-dup", user_id, now).await;

        let err = repo
            .insert(&NewSession {
                token_hash: "hash-dup".into(),
                user_id,
                expires_at: now + Duration::hours(12),
                last_seen_at: now,
                user_agent: None,
            })
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Conflict(m) => m.clone(),
            other => panic!("expected Conflict, got {other:?}"),
        };
        assert!(
            msg.contains("already exists"),
            "message must name the collision: {msg}"
        );
    }

    /// N2: a session for a nonexistent user must surface the FK violation as
    /// `Validation`, never as `Database` (500).
    #[tokio::test]
    async fn n2_missing_user_fk_maps_to_validation() {
        let repo = SqliteSessionRepository::new(pool().await);
        let now = base_time();
        let err = repo
            .insert(&NewSession {
                token_hash: "hash-orphan".into(),
                user_id: 999_999,
                expires_at: now + Duration::hours(12),
                last_seen_at: now,
                user_agent: None,
            })
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("reference"),
            "message must name the reference rule: {msg}"
        );
    }

    /// N2: the sessions user-agent CHECK backstop must surface as
    /// `Validation`, never as `Database` (500).
    #[tokio::test]
    async fn n2_overlong_user_agent_maps_to_validation() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let now = base_time();
        let agent: String = "x".repeat(300);
        let err = repo
            .insert(&NewSession {
                token_hash: "hash-long-agent".into(),
                user_id,
                expires_at: now + Duration::hours(12),
                last_seen_at: now,
                user_agent: Some(agent),
            })
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("user agent"),
            "message must name the rule: {msg}"
        );
    }

    /// N2: the revocation trigger branch. No repository write path clears
    /// `revoked_at` (revocation is permanent by design), so the raw error is
    /// produced by the exact UPDATE the schema trigger refuses and then run
    /// through the same `map_db_err` every mapped repository write uses: it
    /// must surface as `Validation`, never as `Database` (500).
    #[tokio::test]
    async fn n2_clearing_revoked_at_maps_to_validation_not_database() {
        let repo = SqliteSessionRepository::new(pool().await);
        let user_id = seed_user_id(&repo.pool).await;
        let session = insert_session(&repo, "hash-gone", user_id, base_time()).await;
        repo.revoke("hash-gone").await.unwrap();

        let raw = sqlx::query("UPDATE sessions SET revoked_at = NULL WHERE id = ?")
            .bind(session.id)
            .execute(&repo.pool)
            .await
            .unwrap_err();
        let mapped = map_db_err(raw);
        let msg = match &mapped {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("revoked_at"),
            "message must name the permanence rule: {msg}"
        );
    }
}
