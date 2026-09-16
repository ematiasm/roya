use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{NewSupplier, Supplier, UpdateSupplier};

fn row_to_supplier(row: sqlx::sqlite::SqliteRow) -> Supplier {
    let active: i64 = row.get("is_active");
    Supplier {
        id: row.get("id"),
        name: row.get("name"),
        phone: row.get("phone"),
        notes: row.get("notes"),
        is_active: active == 1,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("supplier name already exists".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for supplier".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait SupplierRepository: Send + Sync {
    async fn create(&self, input: &NewSupplier) -> AppResult<Supplier>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Supplier>>;
    async fn find_by_name(&self, name: &str) -> AppResult<Option<Supplier>>;
    async fn list(&self) -> AppResult<Vec<Supplier>>;
    /// Update name/phone/notes (service guarantees cleaned values).
    async fn update(&self, id: i64, patch: &UpdateSupplier) -> AppResult<Supplier>;
    async fn set_active(&self, id: i64, active: bool) -> AppResult<Supplier>;
    /// DELETE is RESTRICTed by cost rows (and, later, purchases).
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

#[derive(Clone)]
pub struct SqliteSupplierRepository {
    pub pool: SqlitePool,
}

impl SqliteSupplierRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SupplierRepository for SqliteSupplierRepository {
    async fn create(&self, input: &NewSupplier) -> AppResult<Supplier> {
        let row = sqlx::query(
            r#"INSERT INTO suppliers (name, phone, notes)
               VALUES (?, ?, ?)
               RETURNING id, name, phone, notes, is_active, created_at, updated_at"#,
        )
        .bind(&input.name)
        .bind(input.phone.clone())
        .bind(input.notes.clone())
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_supplier(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Supplier>> {
        let row = sqlx::query(
            r#"SELECT id, name, phone, notes, is_active, created_at, updated_at
               FROM suppliers WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_supplier))
    }

    async fn find_by_name(&self, name: &str) -> AppResult<Option<Supplier>> {
        let row = sqlx::query(
            r#"SELECT id, name, phone, notes, is_active, created_at, updated_at
               FROM suppliers WHERE name = ?"#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_supplier))
    }

    async fn list(&self) -> AppResult<Vec<Supplier>> {
        let rows = sqlx::query(
            r#"SELECT id, name, phone, notes, is_active, created_at, updated_at
               FROM suppliers ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_supplier).collect())
    }

    async fn update(&self, id: i64, patch: &UpdateSupplier) -> AppResult<Supplier> {
        let existing = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("supplier {id} not found")))?;

        let name = patch.name.clone().unwrap_or(existing.name);
        let phone = match &patch.phone {
            Some(inner) => inner.clone(),
            None => existing.phone,
        };
        let notes = match &patch.notes {
            Some(inner) => inner.clone(),
            None => existing.notes,
        };

        let row = sqlx::query(
            r#"UPDATE suppliers
               SET name = ?, phone = ?, notes = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, notes, is_active, created_at, updated_at"#,
        )
        .bind(name)
        .bind(phone)
        .bind(notes)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_supplier(row))
    }

    async fn set_active(&self, id: i64, active: bool) -> AppResult<Supplier> {
        let row = sqlx::query(
            r#"UPDATE suppliers
               SET is_active = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, notes, is_active, created_at, updated_at"#,
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(row_to_supplier)
            .ok_or_else(|| AppError::NotFound(format!("supplier {id} not found")))
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM suppliers WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(
                        "cannot delete supplier with cost rows or purchases".into(),
                    )
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM suppliers WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}
