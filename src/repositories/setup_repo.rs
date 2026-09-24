use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::models::{NewBusinessLocale, NewBusinessSettings};

#[derive(Clone)]
pub struct SetupRecord {
    pub settings: NewBusinessSettings,
    pub locales: Vec<NewBusinessLocale>,
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
}

#[async_trait]
pub trait SetupRepository: Send + Sync {
    async fn is_configured(&self) -> AppResult<bool>;

    /// Create every first-run row in one transaction. The implementation must
    /// not commit any part of the record if a later statement fails.
    async fn create_initial(&self, input: &SetupRecord) -> AppResult<()>;
}

#[derive(Clone)]
pub struct SqliteSetupRepository {
    pool: SqlitePool,
}

impl SqliteSetupRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn map_db_err(error: sqlx::Error) -> AppError {
    if error.to_string().contains("UNIQUE constraint failed") {
        AppError::Conflict("La configuración inicial ya existe.".into())
    } else {
        AppError::Database(error)
    }
}

#[async_trait]
impl SetupRepository for SqliteSetupRepository {
    async fn is_configured(&self) -> AppResult<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM business_settings")
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }

    async fn create_initial(&self, input: &SetupRecord) -> AppResult<()> {
        let mut transaction = self.pool.begin().await?;

        for locale in &input.locales {
            sqlx::query(
                r#"INSERT INTO business_locales
                   (locale_code, language_code, display_name, is_enabled)
                   VALUES (?, ?, ?, ?)"#,
            )
            .bind(&locale.locale_code)
            .bind(&locale.language_code)
            .bind(&locale.display_name)
            .bind(locale.is_enabled)
            .execute(&mut *transaction)
            .await
            .map_err(map_db_err)?;
        }

        sqlx::query(
            r#"INSERT INTO business_settings
               (id, business_name, default_locale_code, currency_code, timezone)
               VALUES (1, ?, ?, ?, ?)"#,
        )
        .bind(&input.settings.business_name)
        .bind(&input.settings.default_locale_code)
        .bind(&input.settings.currency_code)
        .bind(&input.settings.timezone)
        .execute(&mut *transaction)
        .await
        .map_err(map_db_err)?;

        let user_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO users
               (username, display_name, password_hash, must_change_password, created_by)
               VALUES (?, ?, ?, 0, NULL)
               RETURNING id"#,
        )
        .bind(&input.username)
        .bind(&input.display_name)
        .bind(&input.password_hash)
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_db_err)?;

        let role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_db_err)?;

        sqlx::query("INSERT INTO user_roles (user_id, role_id, granted_by) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(role_id)
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .map_err(map_db_err)?;

        transaction.commit().await.map_err(map_db_err)
    }
}
