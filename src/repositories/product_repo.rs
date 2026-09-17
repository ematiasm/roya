use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewProduct, Product, ProductKind};

#[async_trait]
pub trait ProductRepository: Send + Sync {
    async fn create(&self, input: &NewProduct) -> AppResult<Product>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Product>>;
    async fn find_by_sku(&self, sku: &str) -> AppResult<Option<Product>>;
    /// Exact SKU regardless of case, used by the scanner/SKU resolution path.
    async fn find_by_sku_ci(&self, sku: &str) -> AppResult<Option<Product>>;
    /// Bounded picker read over name, SKU and barcode aliases in one query.
    async fn search(&self, query: &str, limit: i64) -> AppResult<Vec<Product>>;
    async fn list(&self) -> AppResult<Vec<Product>>;
    async fn list_by_category(&self, category_id: i64) -> AppResult<Vec<Product>>;
    async fn count_by_category(&self, category_id: i64) -> AppResult<i64>;
    async fn set_active(&self, id: i64, active: bool) -> AppResult<Product>;
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

fn parse_decimal_opt(v: Option<String>) -> Option<Decimal> {
    v.as_deref().map(|s| Decimal::from_str(s).unwrap_or(Decimal::ZERO))
}

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn kind_from_str(s: &str) -> ProductKind {
    match s {
        "Service" => ProductKind::Service,
        _ => ProductKind::Product,
    }
}

/// Escape the LIKE wildcards in a user-typed value so `%` and `_` stay literal.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn row_to_product(row: sqlx::sqlite::SqliteRow) -> Product {
    let sale_str: String = row.get("sale_price");
    let cost_str: String = row.get("cost_price");
    let min_str: Option<String> = row.get("min_stock");
    let max_str: Option<String> = row.get("max_stock");
    let track: i64 = row.get("track_stock");
    let active: i64 = row.get("is_active");
    Product {
        id: row.get("id"),
        sku: row.get("sku"),
        name: row.get("name"),
        kind: kind_from_str(row.get("kind")),
        category_id: row.get("category_id"),
        unit: row.get("unit"),
        sale_price: parse_decimal(&sale_str),
        cost_price: parse_decimal(&cost_str),
        track_stock: track == 1,
        min_stock: parse_decimal_opt(min_str),
        max_stock: parse_decimal_opt(max_str),
        location: row.get("location"),
        notes: row.get("notes"),
        is_active: active == 1,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains(".sku") || s.contains("products.sku") {
            AppError::Conflict("sku already exists".into())
        } else {
            AppError::Conflict("product already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid category".into())
    } else {
        AppError::Database(e)
    }
}

#[derive(Clone)]
pub struct SqliteProductRepository {
    pub pool: SqlitePool,
}

impl SqliteProductRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProductRepository for SqliteProductRepository {
    async fn create(&self, input: &NewProduct) -> AppResult<Product> {
        let row = sqlx::query(
            r#"INSERT INTO products
               (sku, name, kind, category_id, unit, sale_price, cost_price,
                track_stock, min_stock, max_stock, location, notes)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at"#,
        )
        .bind(&input.sku)
        .bind(&input.name)
        .bind(input.kind.to_string())
        .bind(input.category_id)
        .bind(&input.unit)
        .bind(input.sale_price.to_string())
        .bind(input.cost_price.to_string())
        .bind(if input.track_stock { 1i64 } else { 0i64 })
        .bind(input.min_stock.map(|d| d.to_string()))
        .bind(input.max_stock.map(|d| d.to_string()))
        .bind(input.location.clone())
        .bind(input.notes.clone())
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_product(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at FROM products WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn find_by_sku(&self, sku: &str) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at FROM products WHERE sku = ?"#,
        )
        .bind(sku)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn find_by_sku_ci(&self, sku: &str) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at FROM products WHERE sku = ? COLLATE NOCASE ORDER BY id LIMIT 1"#,
        )
        .bind(sku)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn search(&self, query: &str, limit: i64) -> AppResult<Vec<Product>> {
        // One query path: name, SKU and barcode aliases. LIKE is case-insensitive
        // for ASCII in SQLite, and `%`/`_` typed by the user stay literal text.
        let pattern = format!("%{}%", escape_like(query));
        let rows = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at
               FROM products p
               WHERE p.name LIKE ? ESCAPE '\'
                  OR p.sku LIKE ? ESCAPE '\'
                  OR EXISTS (
                       SELECT 1 FROM product_barcodes b
                       WHERE b.product_id = p.id AND b.code LIKE ? ESCAPE '\'
                     )
               ORDER BY p.name, p.id
               LIMIT ?"#,
        )
        .bind(&pattern)
        .bind(&pattern)
        .bind(&pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_product).collect())
    }

    async fn list(&self) -> AppResult<Vec<Product>> {
        let rows = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at FROM products ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_product).collect())
    }

    async fn list_by_category(&self, category_id: i64) -> AppResult<Vec<Product>> {
        let rows = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at FROM products WHERE category_id = ? ORDER BY id"#,
        )
        .bind(category_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_product).collect())
    }

    async fn count_by_category(&self, category_id: i64) -> AppResult<i64> {
        let row: (i64,) =
            sqlx::query_as(r#"SELECT COUNT(*) FROM products WHERE category_id = ?"#)
                .bind(category_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    async fn set_active(&self, id: i64, active: bool) -> AppResult<Product> {
        let row = sqlx::query(
            r#"UPDATE products SET is_active = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sku, name, kind, category_id, unit, sale_price, cost_price, track_stock, min_stock, max_stock, location, notes, is_active, created_at, updated_at"#,
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_product(row))
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM products WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(
                        "cannot delete product with stock movements".into(),
                    )
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM products WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}
