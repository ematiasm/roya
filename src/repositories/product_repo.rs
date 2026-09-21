use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewProduct, Product, ProductBarcode, ProductKind};

/// The audit actor is an explicit argument on every mutation (M5 Phase B,
/// slice S10): the acting user's id from the request's `Principal`, threaded
/// route → service → repository, never invented by the repository.
#[async_trait]
pub trait ProductRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewProduct) -> AppResult<Product>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Product>>;
    async fn find_by_sku(&self, sku: &str) -> AppResult<Option<Product>>;
    /// Exact SKU regardless of case, used by the scanner/SKU resolution path.
    async fn find_by_sku_ci(&self, sku: &str) -> AppResult<Option<Product>>;
    /// Every barcode alias. The normalized catalogue search matches the whole
    /// (small) set in Rust, so name/SKU/barcode share one matching definition; this
    /// read owns the `product_barcodes` SQL the retired picker query used to hold.
    async fn list_barcodes(&self) -> AppResult<Vec<ProductBarcode>>;
    async fn list(&self) -> AppResult<Vec<Product>>;
    async fn list_by_category(&self, category_id: i64) -> AppResult<Vec<Product>>;
    async fn count_by_category(&self, category_id: i64) -> AppResult<i64>;
    async fn set_active(&self, actor: i64, id: i64, active: bool) -> AppResult<Product>;
    /// Persist a full merged row for an existing product. The service merges the
    /// patch over the current row and validates it, so the repository stays
    /// patch-agnostic: one UPDATE rewrites every editable column and returns the
    /// re-read row. `actor` is the audit actor of the request performing the
    /// edit: it lands on `updated_by` while `created_by` stays untouched.
    async fn update(&self, actor: i64, id: i64, input: &NewProduct) -> AppResult<Product>;
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

fn parse_decimal_opt(v: Option<String>) -> Option<Decimal> {
    v.as_deref().map(|s| Decimal::from_str(s).unwrap_or(Decimal::ZERO))
}

/// Strict sibling of `parse_decimal_opt` for `markup_pct`. `parse_decimal_opt`
/// degrades a malformed stored value to `Decimal::ZERO`, but for markup that
/// ZERO is a *meaningful* value: a 0% markup pins sale_price to cost_price
/// once the T4 derivation lands. So a malformed `markup_pct` degrades to `None`
/// ("no markup, manual price") instead, leaving the stored price alone.
fn parse_decimal_opt_strict(v: Option<String>) -> Option<Decimal> {
    v.as_deref().and_then(|s| Decimal::from_str(s).ok())
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
fn row_to_product(row: sqlx::sqlite::SqliteRow) -> Product {
    let sale_str: String = row.get("sale_price");
    let cost_str: String = row.get("cost_price");
    let markup_str: Option<String> = row.get("markup_pct");
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
        markup_pct: parse_decimal_opt_strict(markup_str),
        track_stock: track == 1,
        min_stock: parse_decimal_opt(min_str),
        max_stock: parse_decimal_opt(max_str),
        location: row.get("location"),
        notes: row.get("notes"),
        is_active: active == 1,
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
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
    async fn create(&self, actor: i64, input: &NewProduct) -> AppResult<Product> {
        let row = sqlx::query(
            r#"INSERT INTO products
               (sku, name, kind, category_id, unit, sale_price, cost_price,
                markup_pct, track_stock, min_stock, max_stock, location, notes, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(&input.sku)
        .bind(&input.name)
        .bind(input.kind.to_string())
        .bind(input.category_id)
        .bind(&input.unit)
        .bind(input.sale_price.to_string())
        .bind(input.cost_price.to_string())
        .bind(input.markup_pct.map(|d| d.to_string()))
        .bind(if input.track_stock { 1i64 } else { 0i64 })
        .bind(input.min_stock.map(|d| d.to_string()))
        .bind(input.max_stock.map(|d| d.to_string()))
        .bind(input.location.clone())
        .bind(input.notes.clone())
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_product(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn find_by_sku(&self, sku: &str) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products WHERE sku = ?"#,
        )
        .bind(sku)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn find_by_sku_ci(&self, sku: &str) -> AppResult<Option<Product>> {
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products WHERE sku = ? COLLATE NOCASE ORDER BY id LIMIT 1"#,
        )
        .bind(sku)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_product))
    }

    async fn list_barcodes(&self) -> AppResult<Vec<ProductBarcode>> {
        let rows = sqlx::query(
            "SELECT id, product_id, code, created_at FROM product_barcodes ORDER BY product_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ProductBarcode {
                id: row.get("id"),
                product_id: row.get("product_id"),
                code: row.get("code"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    async fn list(&self) -> AppResult<Vec<Product>> {
        let rows = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_product).collect())
    }

    async fn list_by_category(&self, category_id: i64) -> AppResult<Vec<Product>> {
        let rows = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products WHERE category_id = ? ORDER BY id"#,
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

    async fn set_active(&self, actor: i64, id: i64, active: bool) -> AppResult<Product> {
        let row = sqlx::query(
            r#"UPDATE products SET is_active = ?, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? RETURNING id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_product(row))
    }

    async fn update(&self, actor: i64, id: i64, input: &NewProduct) -> AppResult<Product> {
        let row = sqlx::query(
            r#"UPDATE products
               SET sku = ?, name = ?, kind = ?, category_id = ?, unit = ?,
                   sale_price = ?, cost_price = ?, markup_pct = ?, track_stock = ?, min_stock = ?,
                   max_stock = ?, location = ?, notes = ?,
                   updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(&input.sku)
        .bind(&input.name)
        .bind(input.kind.to_string())
        .bind(input.category_id)
        .bind(&input.unit)
        .bind(input.sale_price.to_string())
        .bind(input.cost_price.to_string())
        .bind(input.markup_pct.map(|d| d.to_string()))
        .bind(if input.track_stock { 1i64 } else { 0i64 })
        .bind(input.min_stock.map(|d| d.to_string()))
        .bind(input.max_stock.map(|d| d.to_string()))
        .bind(input.location.clone())
        .bind(input.notes.clone())
        .bind(actor)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        // Zero rows affected means the id does not exist: surface the same 404
        // the reads do, so an edit can never silently no-op.
        row.map(row_to_product)
            .ok_or_else(|| AppError::NotFound(format!("product {id} not found")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProductKind;
    use rust_decimal::Decimal;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn test_pool() -> SqlitePool {
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

    async fn repo() -> SqliteProductRepository {
        SqliteProductRepository::new(test_pool().await)
    }

    /// The acting user for repo-level writes: the migration's sentinel, valid
    /// as an actor. Attribution differences are asserted at the service level,
    /// where two dedicated users exist.
    async fn actor(r: &SqliteProductRepository) -> i64 {
        crate::security::test_support::audit_actor_id(&r.pool)
            .await
            .unwrap()
    }

    fn product_input(sku: &str) -> NewProduct {
        NewProduct {
            sku: sku.to_string(),
            name: format!("prod {sku}"),
            kind: ProductKind::Product,
            category_id: None,
            unit: "un".to_string(),
            sale_price: Decimal::from_str("10").unwrap(),
            cost_price: Decimal::from_str("5").unwrap(),
            track_stock: true,
            min_stock: Some(Decimal::from_str("5").unwrap()),
            max_stock: Some(Decimal::from_str("50").unwrap()),
            location: None,
            notes: None,
            markup_pct: None,
        }
    }

    /// T1: `update` persists the full merged row in one UPDATE and returns the
    /// re-read row, so the service merge is the only place patch semantics live.
    #[tokio::test]
    async fn update_persists_the_full_row_and_returns_it() {
        let r = repo().await;
        let created = r.create(actor(&r).await, &product_input("REPO-U1")).await.unwrap();
        let input = NewProduct {
            sku: "REPO-U2".to_string(),
            name: "renamed".to_string(),
            kind: ProductKind::Product,
            category_id: None,
            unit: "kg".to_string(),
            sale_price: Decimal::from_str("20.5").unwrap(),
            cost_price: Decimal::from_str("8").unwrap(),
            track_stock: false,
            min_stock: None,
            max_stock: None,
            location: Some("shelf 9".to_string()),
            notes: Some("repo note".to_string()),
            markup_pct: None,
        };
        let updated = r.update(actor(&r).await, created.id, &input).await.unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.sku, "REPO-U2");
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.unit, "kg");
        assert_eq!(updated.sale_price, Decimal::from_str("20.5").unwrap());
        assert_eq!(updated.cost_price, Decimal::from_str("8").unwrap());
        assert!(!updated.track_stock);
        assert_eq!(updated.min_stock, None);
        assert_eq!(updated.max_stock, None);
        assert_eq!(updated.location.as_deref(), Some("shelf 9"));
        assert_eq!(updated.notes.as_deref(), Some("repo note"));
        // The row really changed in the table, not only in the return value.
        let reread = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reread.sku, "REPO-U2");
        assert_eq!(reread.name, "renamed");
    }

    /// T1: zero rows affected maps to NotFound, matching the service's 404.
    #[tokio::test]
    async fn update_unknown_id_is_not_found() {
        let r = repo().await;
        let err = r.update(actor(&r).await, 99999, &product_input("REPO-GHOST")).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    /// Pricing T1: a stored markup_pct round-trips through create and find_by_id.
    #[tokio::test]
    async fn markup_pct_set_round_trips_through_create_and_find_by_id() {
        let r = repo().await;
        let mut input = product_input("REPO-MK1");
        input.markup_pct = Some(Decimal::from_str("21.5").unwrap());
        let created = r.create(actor(&r).await, &input).await.unwrap();
        assert_eq!(created.markup_pct, Some(Decimal::from_str("21.5").unwrap()));
        let reloaded = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.markup_pct, Some(Decimal::from_str("21.5").unwrap()));
    }

    /// Pricing T1: no markup means NULL, and NULL reads back as None — the
    /// "manual price" state, never invented into a numeric markup.
    #[tokio::test]
    async fn markup_pct_none_round_trips_as_none() {
        let r = repo().await;
        let created = r.create(actor(&r).await, &product_input("REPO-MK2")).await.unwrap();
        assert_eq!(created.markup_pct, None);
        let reloaded = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.markup_pct, None);
    }

    /// Pricing T1 regression guard for the parser choice: a malformed stored
    /// value reads back as None, NOT Decimal::ZERO. ZERO would be a meaningful
    /// 0% markup that pins sale_price to cost_price (what T4 will derive), so
    /// garbage must degrade to "no markup", leaving the stored price alone.
    #[tokio::test]
    async fn malformed_stored_markup_pct_reads_back_as_none_not_zero() {
        let r = repo().await;
        let created = r.create(actor(&r).await, &product_input("REPO-MK3")).await.unwrap();
        sqlx::query("UPDATE products SET markup_pct = 'abc' WHERE id = ?")
            .bind(created.id)
            .execute(&r.pool)
            .await
            .unwrap();
        let reloaded = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.markup_pct, None);
    }

    /// Pricing T1: the full-row update can set markup_pct on an existing row.
    /// NOTE: the repository update takes a full `NewProduct` (not a patch), so
    /// the clear (`Some(None)`) vs leave-unchanged (`None`) distinction of the
    /// double option lives at the service merge (inventory::update_product);
    /// here only the set path is covered.
    #[tokio::test]
    async fn update_sets_markup_pct_on_existing_row() {
        let r = repo().await;
        let created = r.create(actor(&r).await, &product_input("REPO-MK4")).await.unwrap();
        assert_eq!(created.markup_pct, None);
        let mut input = product_input("REPO-MK4");
        input.markup_pct = Some(Decimal::from_str("30").unwrap());
        let updated = r.update(actor(&r).await, created.id, &input).await.unwrap();
        assert_eq!(updated.markup_pct, Some(Decimal::from_str("30").unwrap()));
        let reloaded = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.markup_pct, Some(Decimal::from_str("30").unwrap()));
    }
}
