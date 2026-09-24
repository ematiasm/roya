use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{
    BusinessLocale, BusinessSettings, NewBusinessLocale, NewBusinessSettings,
    UpdateBusinessLocale, UpdateBusinessSettings,
};

fn map_db_err(error: sqlx::Error) -> AppError {
    let message = error.to_string();
    if message.contains("UNIQUE constraint failed") {
        AppError::Conflict("business configuration already exists".into())
    } else {
        AppError::Database(error)
    }
}

fn row_to_settings(row: sqlx::sqlite::SqliteRow) -> BusinessSettings {
    BusinessSettings {
        id: row.get("id"),
        business_name: row.get("business_name"),
        default_locale_code: row.get("default_locale_code"),
        currency_code: row.get("currency_code"),
        timezone: row.get("timezone"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_locale(row: sqlx::sqlite::SqliteRow) -> BusinessLocale {
    let is_enabled: i64 = row.get("is_enabled");
    BusinessLocale {
        id: row.get("id"),
        locale_code: row.get("locale_code"),
        language_code: row.get("language_code"),
        display_name: row.get("display_name"),
        is_enabled: is_enabled == 1,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[async_trait]
pub trait BusinessSettingsRepository: Send + Sync {
    async fn create(&self, input: &NewBusinessSettings) -> AppResult<BusinessSettings>;
    async fn find(&self, id: i64) -> AppResult<Option<BusinessSettings>>;
}

#[async_trait]
pub trait BusinessLocaleRepository: Send + Sync {
    async fn create(&self, input: &NewBusinessLocale) -> AppResult<BusinessLocale>;
    async fn find_by_code(&self, locale_code: &str) -> AppResult<Option<BusinessLocale>>;
    async fn list(&self) -> AppResult<Vec<BusinessLocale>>;
}

#[derive(Clone)]
pub struct SqliteBusinessSettingsRepository {
    pub pool: SqlitePool,
}

impl SqliteBusinessSettingsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BusinessSettingsRepository for SqliteBusinessSettingsRepository {
    async fn create(&self, input: &NewBusinessSettings) -> AppResult<BusinessSettings> {
        let row = sqlx::query(
            r#"INSERT INTO business_settings
                   (id, business_name, default_locale_code, currency_code, timezone)
               VALUES (1, ?, ?, ?, ?)
               RETURNING id, business_name, default_locale_code, currency_code, timezone,
                         created_at, updated_at"#,
        )
        .bind(&input.business_name)
        .bind(&input.default_locale_code)
        .bind(&input.currency_code)
        .bind(&input.timezone)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_settings(row))
    }

    async fn find(&self, id: i64) -> AppResult<Option<BusinessSettings>> {
        let row = sqlx::query(
            r#"SELECT id, business_name, default_locale_code, currency_code, timezone,
                      created_at, updated_at
               FROM business_settings WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_settings))
    }
}

#[derive(Clone)]
pub struct SqliteBusinessLocaleRepository {
    pub pool: SqlitePool,
}

impl SqliteBusinessLocaleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BusinessLocaleRepository for SqliteBusinessLocaleRepository {
    async fn create(&self, input: &NewBusinessLocale) -> AppResult<BusinessLocale> {
        let row = sqlx::query(
            r#"INSERT INTO business_locales
                   (locale_code, language_code, display_name, is_enabled)
               VALUES (?, ?, ?, ?)
               RETURNING id, locale_code, language_code, display_name, is_enabled,
                         created_at, updated_at"#,
        )
        .bind(&input.locale_code)
        .bind(&input.language_code)
        .bind(&input.display_name)
        .bind(i64::from(input.is_enabled))
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_locale(row))
    }

    async fn find_by_code(&self, locale_code: &str) -> AppResult<Option<BusinessLocale>> {
        let row = sqlx::query(
            r#"SELECT id, locale_code, language_code, display_name, is_enabled,
                      created_at, updated_at
               FROM business_locales WHERE locale_code = ?"#,
        )
        .bind(locale_code)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_locale))
    }

    async fn list(&self) -> AppResult<Vec<BusinessLocale>> {
        let rows = sqlx::query(
            r#"SELECT id, locale_code, language_code, display_name, is_enabled,
                      created_at, updated_at
               FROM business_locales ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_locale).collect())
    }
}

/// Atomic post-setup writes across the singleton settings row and every locale
/// profile. Keeping the transaction in the persistence boundary prevents a
/// validated settings update from committing while a locale update fails.
#[async_trait]
pub trait BusinessConfigurationRepository: Send + Sync {
    async fn load(&self) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)>;
    async fn update(
        &self,
        settings: &UpdateBusinessSettings,
        locales: &[UpdateBusinessLocale],
    ) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)>;
}

#[derive(Clone)]
pub struct SqliteBusinessConfigurationRepository {
    pub pool: SqlitePool,
}

impl SqliteBusinessConfigurationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BusinessConfigurationRepository for SqliteBusinessConfigurationRepository {
    async fn load(&self) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)> {
        load_configuration(&self.pool).await
    }

    async fn update(
        &self,
        settings: &UpdateBusinessSettings,
        locales: &[UpdateBusinessLocale],
    ) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)> {
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query(
            "SELECT id, locale_code, language_code, display_name, is_enabled, \
                    created_at, updated_at FROM business_locales ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_locale)
        .collect::<Vec<_>>();
        if existing.len() != locales.len()
            || existing.iter().zip(locales).any(|(current, update)| {
                current.locale_code != update.locale_code
            })
        {
            return Err(AppError::Validation(
                "The submitted locale profiles do not match the configured locales.".into(),
            ));
        }
        if !locales
            .iter()
            .any(|locale| locale.is_enabled && locale.locale_code == settings.default_locale_code)
        {
            return Err(AppError::Validation(
                "The default locale must exist and remain enabled.".into(),
            ));
        }

        let settings_row = sqlx::query(
            r#"UPDATE business_settings
               SET business_name = ?, default_locale_code = ?, currency_code = ?, timezone = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = 1
               RETURNING id, business_name, default_locale_code, currency_code, timezone,
                         created_at, updated_at"#,
        )
        .bind(&settings.business_name)
        .bind(&settings.default_locale_code)
        .bind(&settings.currency_code)
        .bind(&settings.timezone)
        .fetch_one(&mut *tx)
        .await?;

        for locale in locales {
            sqlx::query(
                "UPDATE business_locales \
                 SET display_name = ?, is_enabled = ?, \
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE locale_code = ?",
            )
            .bind(&locale.display_name)
            .bind(i64::from(locale.is_enabled))
            .bind(&locale.locale_code)
            .execute(&mut *tx)
            .await?;
        }

        let locale_rows = sqlx::query(
            "SELECT id, locale_code, language_code, display_name, is_enabled, \
                    created_at, updated_at FROM business_locales ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_locale)
        .collect();
        tx.commit().await?;
        Ok((row_to_settings(settings_row), locale_rows))
    }
}

async fn load_configuration(pool: &SqlitePool) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)> {
    let settings = sqlx::query(
        "SELECT id, business_name, default_locale_code, currency_code, timezone, \
                created_at, updated_at FROM business_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("business settings are not configured".into()))?;
    let locales = sqlx::query(
        "SELECT id, locale_code, language_code, display_name, is_enabled, \
                created_at, updated_at FROM business_locales ORDER BY id",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(row_to_locale)
    .collect();
    Ok((row_to_settings(settings), locales))
}
