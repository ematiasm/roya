// Identity kernel: users repository (Slice S1a). Reads follow the model rule
// that the ordinary `User` carries no password hash; the `UserWithHash` reads
// exist only for the credential-verification path. Username equality runs
// through the COLLATE NOCASE unique index, so lookups are case-insensitive and
// duplicates are impossible in any casing. Users are deactivated, never
// deleted here; the CASCADE delete lives on the sessions FK for erasure only.
// (dead_code allowed: the identity service is wired into the router in S1b.)
#![allow(dead_code)]
use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::{Row, SqlitePool};

use crate::db::encode_sqlite_timestamp;
use crate::error::{AppError, AppResult};
use crate::models::{NewUser, User, UserWithHash};

fn row_to_user(row: &sqlx::sqlite::SqliteRow) -> User {
    let active: i64 = row.get("is_active");
    let must_change: i64 = row.get("must_change_password");
    User {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        is_active: active == 1,
        must_change_password: must_change == 1,
        last_login_at: row.get("last_login_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_user_with_hash(row: sqlx::sqlite::SqliteRow) -> UserWithHash {
    UserWithHash {
        password_hash: row.get("password_hash"),
        user: row_to_user(&row),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains("username") {
            // The NOCASE unique index is the backstop the service already
            // checks for; a race between two creates lands here, not in a 500.
            AppError::Conflict("username already exists".into())
        } else {
            AppError::Conflict("user already exists".into())
        }
    } else if s.contains("CHECK constraint failed") {
        if s.contains("users_username_shape") {
            AppError::Validation(
                "username must be 3-64 lowercase ASCII characters (letters, digits, dot, underscore, hyphen)".into(),
            )
        } else if s.contains("users_display_name_shape") {
            AppError::Validation("display name must be 1-128 characters".into())
        } else {
            AppError::Validation("user fields violate schema rules".into())
        }
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn create(&self, input: &NewUser) -> AppResult<User>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<User>>;
    /// Case-insensitive lookup through the NOCASE unique index.
    async fn find_by_username(&self, username: &str) -> AppResult<Option<User>>;
    /// Credential path: the same lookup, plus the stored PHC string.
    async fn find_with_hash_by_username(&self, username: &str)
        -> AppResult<Option<UserWithHash>>;
    async fn find_with_hash_by_id(&self, id: i64) -> AppResult<Option<UserWithHash>>;
    /// Replace the stored PHC string (password change / admin reset).
    async fn update_password_hash(&self, id: i64, password_hash: &str) -> AppResult<()>;
    async fn set_must_change_password(&self, id: i64, value: bool) -> AppResult<()>;
    async fn set_active(&self, id: i64, active: bool) -> AppResult<()>;
    async fn touch_last_login(&self, id: i64, when: NaiveDateTime) -> AppResult<()>;
    /// Active holders of the protected administrator identity. Until RBAC
    /// arrives (slice S2, `user_roles`), "administrator" is the seeded `admin`
    /// username; this read switches to the roles join then.
    async fn count_active_admins(&self) -> AppResult<i64>;
}

#[derive(Clone)]
pub struct SqliteUserRepository {
    pub pool: SqlitePool,
}

impl SqliteUserRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserRepository for SqliteUserRepository {
    async fn create(&self, input: &NewUser) -> AppResult<User> {
        let row = sqlx::query(
            r#"INSERT INTO users (username, display_name, password_hash, must_change_password)
               VALUES (?, ?, ?, ?)
               RETURNING id, username, display_name, is_active, must_change_password,
                         last_login_at, created_at, updated_at"#,
        )
        .bind(&input.username)
        .bind(&input.display_name)
        .bind(&input.password_hash)
        .bind(if input.must_change_password { 1i64 } else { 0i64 })
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_user(&row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<User>> {
        let row = sqlx::query(
            r#"SELECT id, username, display_name, is_active, must_change_password,
                      last_login_at, created_at, updated_at
               FROM users WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| row_to_user(&r)))
    }

    async fn find_by_username(&self, username: &str) -> AppResult<Option<User>> {
        let row = sqlx::query(
            r#"SELECT id, username, display_name, is_active, must_change_password,
                      last_login_at, created_at, updated_at
               FROM users WHERE username = ? COLLATE NOCASE"#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| row_to_user(&r)))
    }

    async fn find_with_hash_by_username(
        &self,
        username: &str,
    ) -> AppResult<Option<UserWithHash>> {
        let row = sqlx::query(
            r#"SELECT id, username, display_name, is_active, must_change_password,
                      last_login_at, created_at, updated_at, password_hash
               FROM users WHERE username = ? COLLATE NOCASE"#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_user_with_hash))
    }

    async fn find_with_hash_by_id(&self, id: i64) -> AppResult<Option<UserWithHash>> {
        let row = sqlx::query(
            r#"SELECT id, username, display_name, is_active, must_change_password,
                      last_login_at, created_at, updated_at, password_hash
               FROM users WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_user_with_hash))
    }

    async fn update_password_hash(&self, id: i64, password_hash: &str) -> AppResult<()> {
        sqlx::query("UPDATE users SET password_hash = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?")
            .bind(password_hash)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_must_change_password(&self, id: i64, value: bool) -> AppResult<()> {
        sqlx::query(
            "UPDATE users SET must_change_password = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(if value { 1i64 } else { 0i64 })
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_active(&self, id: i64, active: bool) -> AppResult<()> {
        sqlx::query(
            "UPDATE users SET is_active = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn touch_last_login(&self, id: i64, when: NaiveDateTime) -> AppResult<()> {
        sqlx::query(
            "UPDATE users SET last_login_at = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(encode_sqlite_timestamp(when))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_active_admins(&self) -> AppResult<i64> {
        let row: (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM users
               WHERE is_active = 1 AND username = 'admin' COLLATE NOCASE"#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn new_user(username: &str, display_name: &str) -> NewUser {
        NewUser {
            username: username.into(),
            display_name: display_name.into(),
            password_hash: "not-a-real-hash".into(),
            must_change_password: false,
        }
    }

    /// F4: the users CHECK constraints are the database backstop for rules
    /// the service reports as Validation; these rejections must surface as
    /// `Validation` (400), never as `Database` (500).
    #[tokio::test]
    async fn f4_uppercase_username_hits_the_shape_check_and_maps_to_validation() {
        let repo = SqliteUserRepository::new(pool().await);
        let err = repo.create(&new_user("Admin", "Admin")).await.unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(msg.contains("username"), "message must name the rule: {msg}");
    }

    #[tokio::test]
    async fn f4_username_shorter_than_three_chars_maps_to_validation() {
        let repo = SqliteUserRepository::new(pool().await);
        let err = repo.create(&new_user("ab", "Ab")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn f4_username_with_illegal_characters_maps_to_validation() {
        let repo = SqliteUserRepository::new(pool().await);
        let err = repo.create(&new_user("ok!", "Ok")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn f4_display_name_longer_than_128_chars_maps_to_validation() {
        let repo = SqliteUserRepository::new(pool().await);
        let long: String = "x".repeat(200);
        let err = repo.create(&new_user("teller", &long)).await.unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(msg.contains("display name"), "message must name the field: {msg}");
    }
}
