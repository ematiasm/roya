// Identity kernel: users repository (Slice S1a). Reads follow the model rule
// that the ordinary `User` carries no password hash; the `UserWithHash` reads
// exist only for the credential-verification path. Username equality runs
// through the COLLATE NOCASE unique index, so lookups are case-insensitive and
// duplicates are impossible in any casing. Users are deactivated, never
// deleted here; the CASCADE delete lives on the sessions FK for erasure only.
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
            AppError::Conflict("El nombre de usuario ya existe.".into())
        } else {
            AppError::Conflict("El usuario ya existe.".into())
        }
    } else if s.contains("cannot deactivate the last active user holding a protected role") {
        // The guard trigger (AC14): the database refuses to leave the shop
        // without an active administrator. The interface explains the rule,
        // never the trigger string.
        AppError::Conflict(
            "No se puede desactivar: es el último usuario activo que sostiene un rol protegido. Primero asigná el rol a otro usuario.".into(),
        )
    } else if s.contains("FOREIGN KEY constraint failed") {
        // user_roles.granted_by is ON DELETE RESTRICT (and a role holder's own
        // grant records the user too): a role grant records who granted it,
        // and that reference holds the user row the statement tried to delete.
        AppError::Conflict("this user granted a role: the grant records who granted it, and the deletion is held".into())
    } else if s.contains("CHECK constraint failed") {
        if s.contains("users_username_shape") {
            AppError::Validation(
                "El nombre de usuario debe tener entre 3 y 64 caracteres: letras minúsculas, números y . _ - (sin espacios ni mayúsculas).".into(),
            )
        } else if s.contains("users_display_name_shape") {
            AppError::Validation(
                "El nombre para mostrar debe tener entre 1 y 128 caracteres.".into(),
            )
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
    /// Every user, creation order (the S3 users list read). The ordinary
    /// read: no hash material.
    async fn list(&self) -> AppResult<Vec<User>>;
}

// S1a's `count_active_admins` lived here as a username shortcut, with a note
// that it would switch to a roles join in S2. S2 unified the predicate on the
// one the triggers protect — active holders of ANY `is_system` role — and
// that read already existed as `role_repo::count_active_protected_holders`, so
// the method was removed rather than duplicated: one query, one meaning, and
// the bootstrap now consults exactly what the guards enforce.

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
        .await
        // The deactivation guard trigger (AC14) refuses through this write,
        // so the refusal must be mapped here, not left as a raw 500.
        .map_err(map_db_err)?;
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

    async fn list(&self) -> AppResult<Vec<User>> {
        let rows = sqlx::query(
            r#"SELECT id, username, display_name, is_active, must_change_password,
                      last_login_at, created_at, updated_at
               FROM users ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_user).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        // The NIT's mapping test writes a `user_roles` grant fixture, so this
        // pool also takes the shared options (foreign keys + recursive
        // triggers), like every pool that can reach those tables.
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
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
        assert!(
            msg.contains("nombre de usuario"),
            "message must name the rule (Spanish operator copy): {msg}"
        );
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
        assert!(
            msg.contains("nombre para mostrar"),
            "message must name the field (Spanish operator copy): {msg}"
        );
    }

    /// A role grant records who granted it (`user_roles.granted_by ... ON
    /// DELETE RESTRICT`), so deleting that user is held by a foreign key, not
    /// by a guard trigger. The raw SQLite text is a bare SQL string; the
    /// mapping here is what the interface will explain instead.
    #[tokio::test]
    async fn a_grantor_user_delete_maps_the_foreign_key_refusal() {
        let p = pool().await;
        let repo = SqliteUserRepository::new(p.clone());
        let grantor = repo.create(&new_user("hr-grantor", "HR")).await.unwrap();
        let holder = repo.create(&new_user("teller", "Teller")).await.unwrap();
        sqlx::query(
            r#"INSERT INTO user_roles (user_id, role_id, granted_by)
               VALUES (?, (SELECT id FROM roles WHERE code = 'admin'), ?)"#,
        )
        .bind(holder.id)
        .bind(grantor.id)
        .execute(&p)
        .await
        .unwrap();

        let err = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(grantor.id)
            .execute(&p)
            .await
            .unwrap_err();
        let mapped = map_db_err(err);
        match &mapped {
            AppError::Conflict(message) => {
                assert!(
                    message.contains("granted a role"),
                    "the refusal must name the reason: {message}"
                );
                assert!(!message.contains("FOREIGN KEY"), "no raw SQL text: {message}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}
