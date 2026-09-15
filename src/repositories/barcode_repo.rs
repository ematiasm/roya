use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::ProductBarcode;

#[async_trait]
pub trait BarcodeRepository: Send + Sync {
    async fn create(&self, product_id: i64, code: &str) -> AppResult<ProductBarcode>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<ProductBarcode>>;
    async fn find_by_code(&self, code: &str) -> AppResult<Option<ProductBarcode>>;
    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductBarcode>>;
    async fn delete(&self, id: i64) -> AppResult<bool>;
}

fn row_to_barcode(row: sqlx::sqlite::SqliteRow) -> ProductBarcode {
    ProductBarcode {
        id: row.get("id"),
        product_id: row.get("product_id"),
        code: row.get("code"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("barcode already exists".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::NotFound("product not found".into())
    } else {
        AppError::Database(e)
    }
}

#[derive(Clone)]
pub struct SqliteBarcodeRepository {
    pub pool: SqlitePool,
}

impl SqliteBarcodeRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BarcodeRepository for SqliteBarcodeRepository {
    async fn create(&self, product_id: i64, code: &str) -> AppResult<ProductBarcode> {
        let row = sqlx::query(
            r#"INSERT INTO product_barcodes (product_id, code)
               VALUES (?, ?) RETURNING id, product_id, code, created_at"#,
        )
        .bind(product_id)
        .bind(code)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_barcode(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<ProductBarcode>> {
        let row = sqlx::query(
            r#"SELECT id, product_id, code, created_at FROM product_barcodes WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_barcode))
    }

    async fn find_by_code(&self, code: &str) -> AppResult<Option<ProductBarcode>> {
        let row = sqlx::query(
            r#"SELECT id, product_id, code, created_at FROM product_barcodes WHERE code = ?"#,
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_barcode))
    }

    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductBarcode>> {
        let rows = sqlx::query(
            r#"SELECT id, product_id, code, created_at FROM product_barcodes
               WHERE product_id = ? ORDER BY id"#,
        )
        .bind(product_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_barcode).collect())
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM product_barcodes WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}
