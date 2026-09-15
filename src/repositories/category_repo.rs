use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::Category;

#[async_trait]
pub trait CategoryRepository: Send + Sync {
    async fn create(&self, name: &str, parent_id: Option<i64>) -> AppResult<Category>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Category>>;
    async fn find_by_parent_and_name(
        &self,
        parent_id: Option<i64>,
        name: &str,
    ) -> AppResult<Option<Category>>;
    async fn list(&self) -> AppResult<Vec<Category>>;
    async fn list_children(&self, parent_id: i64) -> AppResult<Vec<Category>>;
    async fn update(
        &self,
        id: i64,
        name: &str,
        parent_id: Option<i64>,
    ) -> AppResult<Category>;
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn count_children(&self, id: i64) -> AppResult<i64>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

fn row_to_category(row: sqlx::sqlite::SqliteRow) -> Category {
    Category {
        id: row.get("id"),
        name: row.get("name"),
        parent_id: row.get("parent_id"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("category already exists under this parent".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid parent category".into())
    } else {
        AppError::Database(e)
    }
}

#[derive(Clone)]
pub struct SqliteCategoryRepository {
    pub pool: SqlitePool,
}

impl SqliteCategoryRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CategoryRepository for SqliteCategoryRepository {
    async fn create(&self, name: &str, parent_id: Option<i64>) -> AppResult<Category> {
        let row = sqlx::query(
            r#"INSERT INTO categories (name, parent_id) VALUES (?, ?)
               RETURNING id, name, parent_id, created_at"#,
        )
        .bind(name)
        .bind(parent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_category(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Category>> {
        let row = sqlx::query(
            r#"SELECT id, name, parent_id, created_at FROM categories WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_category))
    }

    async fn find_by_parent_and_name(
        &self,
        parent_id: Option<i64>,
        name: &str,
    ) -> AppResult<Option<Category>> {
        // SQLite treats NULL as distinct in UNIQUE, so root duplicates need
        // an explicit IS NULL comparison here (service-level guard).
        let row = match parent_id {
            Some(pid) => {
                sqlx::query(
                    r#"SELECT id, name, parent_id, created_at FROM categories
                       WHERE parent_id = ? AND name = ?"#,
                )
                .bind(pid)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    r#"SELECT id, name, parent_id, created_at FROM categories
                       WHERE parent_id IS NULL AND name = ?"#,
                )
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        Ok(row.map(row_to_category))
    }

    async fn list(&self) -> AppResult<Vec<Category>> {
        let rows =
            sqlx::query(r#"SELECT id, name, parent_id, created_at FROM categories ORDER BY id"#)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(row_to_category).collect())
    }

    async fn list_children(&self, parent_id: i64) -> AppResult<Vec<Category>> {
        let rows = sqlx::query(
            r#"SELECT id, name, parent_id, created_at FROM categories WHERE parent_id = ? ORDER BY id"#,
        )
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_category).collect())
    }

    async fn update(
        &self,
        id: i64,
        name: &str,
        parent_id: Option<i64>,
    ) -> AppResult<Category> {
        let row = sqlx::query(
            r#"UPDATE categories SET name = ?, parent_id = ? WHERE id = ?
               RETURNING id, name, parent_id, created_at"#,
        )
        .bind(name)
        .bind(parent_id)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_category(row))
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM categories WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn count_children(&self, id: i64) -> AppResult<i64> {
        let row: (i64,) =
            sqlx::query_as(r#"SELECT COUNT(*) FROM categories WHERE parent_id = ?"#)
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM categories WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}
