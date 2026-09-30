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

    /// [`Self::find_by_id`] inside a transaction the CALLER owns, and the WEAKEST
    /// reason in the closure — stated that way on purpose, because a doc comment
    /// that borrowed `stock_for_product_in`'s argument would be borrowing a claim
    /// this method cannot make.
    ///
    /// There is no correctness argument here. `confirm` writes `doc_sequences`,
    /// `stock_movements`, `transactions`, `sale_payments` and `sales` (plus
    /// `product_supplier_costs` on the purchase side) — never `products`. Nothing
    /// this read validates is ever written by the transaction that will hold it,
    /// so reading it from the caller's connection buys no fresher truth than
    /// reading it from the pool. Contrast `stock_for_product_in`, which folds rows
    /// the same document is still writing and genuinely cannot answer correctly
    /// from a snapshot.
    ///
    /// What this read does need is a CONNECTION it was handed. `confirm` reaches
    /// `find_by_id` from three places, all of them `InventoryService::get_product`
    /// (`src/services/inventory.rs:523`) or `record_movement`'s own guard
    /// (`src/services/inventory.rs:732`):
    ///
    /// * the validation loop, once per line — `sales.rs:1249`, `purchases.rs:1039`;
    /// * the strict stock pre-check, once per distinct product, through
    ///   `stock_for_decision` — `sales.rs:1365` → `inventory.rs:831`;
    /// * `record_movement`, once per tracked movement, INSIDE the write phase —
    ///   `sales.rs:1384`, `purchases.rs:1141`.
    ///
    /// The first two sit before any write, so whether they fall inside confirm's
    /// transaction depends on where Phase B opens the BEGIN, and that placement is
    /// not this commit's decision. The third does not: a transaction that makes
    /// the write phase atomic has to hold `record_movement`, and a pool-based read
    /// under it would stall for sqlx's 30s acquire timeout and answer
    /// `PoolTimedOut` on a one-connection pool — or, on a many-connection pool,
    /// take a second connection and read outside the unit while holding the first.
    /// That is the whole argument: this read is a door, not a correctness fix.
    ///
    /// Nothing opens a transaction yet. This is the door; the confirm path does
    /// not walk through it until a later commit of Phase A does.
    async fn find_by_id_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
    ) -> AppResult<Option<Product>>;

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
    v.as_deref()
        .map(|s| Decimal::from_str(s).unwrap_or(Decimal::ZERO))
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

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query and the projection are `find_by_id_in`'s to inherit unchanged; all
    /// this adds is the BEGIN/COMMIT that it deliberately leaves to someone else.
    /// A read that opens a transaction is not a write's privilege — the caller
    /// that owns the larger unit is the only one who can see what is in it.
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Product>> {
        let mut tx = self.pool.begin().await?;
        let product = self.find_by_id_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(product)
    }

    async fn find_by_id_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
    ) -> AppResult<Option<Product>> {
        // The executor is the caller's connection. Nothing here opens a unit of
        // its own, so the read joins the caller's unit instead of ending one.
        // The SQL, the bind and `row.map(row_to_product)` are byte-for-byte what
        // `find_by_id` always ran.
        let row = sqlx::query(
            r#"SELECT id, sku, name, kind, category_id, unit, sale_price, cost_price, markup_pct, track_stock, min_stock, max_stock, location, notes, is_active, created_by, updated_by, created_at, updated_at FROM products WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&mut *tx)
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
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM products WHERE category_id = ?"#)
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
                    AppError::Validation("cannot delete product with stock movements".into())
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
    use std::time::{Duration, Instant};

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
        let created = r
            .create(actor(&r).await, &product_input("REPO-U1"))
            .await
            .unwrap();
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
        let err = r
            .update(actor(&r).await, 99999, &product_input("REPO-GHOST"))
            .await
            .unwrap_err();
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
        assert_eq!(
            reloaded.markup_pct,
            Some(Decimal::from_str("21.5").unwrap())
        );
    }

    /// Pricing T1: no markup means NULL, and NULL reads back as None — the
    /// "manual price" state, never invented into a numeric markup.
    #[tokio::test]
    async fn markup_pct_none_round_trips_as_none() {
        let r = repo().await;
        let created = r
            .create(actor(&r).await, &product_input("REPO-MK2"))
            .await
            .unwrap();
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
        let created = r
            .create(actor(&r).await, &product_input("REPO-MK3"))
            .await
            .unwrap();
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
        let created = r
            .create(actor(&r).await, &product_input("REPO-MK4"))
            .await
            .unwrap();
        assert_eq!(created.markup_pct, None);
        let mut input = product_input("REPO-MK4");
        input.markup_pct = Some(Decimal::from_str("30").unwrap());
        let updated = r.update(actor(&r).await, created.id, &input).await.unwrap();
        assert_eq!(updated.markup_pct, Some(Decimal::from_str("30").unwrap()));
        let reloaded = r.find_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.markup_pct, Some(Decimal::from_str("30").unwrap()));
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // The smallest edge in the closure: one read, no writes, no transaction of
    // its own to unwind. Its value is that it proves the pattern holds in a file
    // with nothing special in it, so the awkward files that follow are the only
    // ones that can have an argument against them.
    //
    // Nothing here opens a transaction across a service call. Phase A installs
    // the door; `confirm` does not walk through it until a later commit does,
    // and the last test pins that the public `find_by_id` is untouched in the
    // meantime.

    /// THE CAPABILITY, stated precisely because it is weaker than the other reads
    /// in this closure: `find_by_id_in` DOES read the caller's uncommitted
    /// writes — a row inserted inside the unit and a row updated inside it are
    /// both visible — and both disappear again when the unit rolls back.
    ///
    /// This is a capability, not a need, and the difference is the honest reason
    /// this read moves. `confirm` writes `doc_sequences`, `stock_movements`,
    /// `transactions`, `sale_payments` and `sales` (plus
    /// `product_supplier_costs` on the purchase side) — never `products`. So
    /// nothing this read validates is ever written by the transaction that will
    /// hold it, and reading it from the caller's connection buys no fresher
    /// truth than reading it from the pool. Unlike `stock_for_product_in`, which
    /// folds rows the same document is still writing, there is no correctness
    /// argument here and none is claimed.
    #[tokio::test]
    async fn find_by_id_in_reads_the_callers_uncommitted_product_and_a_rollback_hides_it_again() {
        let r = repo().await;
        let actor = actor(&r).await;
        let committed = r.create(actor, &product_input("REPO-PH1")).await.unwrap();
        assert!(committed.is_active);

        let mut tx = r.pool.begin().await.unwrap();
        // Both halves of "uncommitted": a row the unit CREATES and a row the unit
        // CHANGES. Neither is visible to anything outside this connection.
        let inserted: i64 = sqlx::query_scalar(
            r#"INSERT INTO products (sku, name, kind, unit, sale_price, cost_price, track_stock, created_by)
               VALUES ('REPO-PH2', 'created inside', 'Product', 'un', '10', '5', 1, ?)
               RETURNING id"#,
        )
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE products SET is_active = 0, name = 'renamed inside' WHERE id = ?")
            .bind(committed.id)
            .execute(&mut *tx)
            .await
            .unwrap();

        let seen_inserted = r
            .find_by_id_in(&mut tx, inserted)
            .await
            .unwrap()
            .expect("the row this transaction created is invisible to it");
        assert_eq!(seen_inserted.sku, "REPO-PH2");
        let seen_update = r
            .find_by_id_in(&mut tx, committed.id)
            .await
            .unwrap()
            .expect("the row this transaction changed is invisible to it");
        assert!(
            !seen_update.is_active && seen_update.name == "renamed inside",
            "the read did not see the caller's own uncommitted writes, so it is answering about a different moment"
        );
        // And it is still a plain read by id: an unknown id is a VALUE, not an
        // error, which is the branch `get_product` turns into `NotFound`.
        assert!(r.find_by_id_in(&mut tx, 99999).await.unwrap().is_none());
        tx.rollback().await.unwrap();

        // The rollback took both with it, which is the other half: an
        // `find_by_id_in` that could not see the rollback was reading something
        // other than the caller's transaction.
        assert!(
            r.find_by_id(inserted).await.unwrap().is_none(),
            "the product survived a rollback of the transaction that created it"
        );
        let restored = r.find_by_id(committed.id).await.unwrap().unwrap();
        assert!(
            restored.is_active && restored.name == "prod REPO-PH1",
            "the update survived a rollback of the transaction that made it"
        );
    }

    /// THE test of this commit: `find_by_id_in` must not reach for the pool AT
    /// ALL, and the assertion is the pairing itself rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on this
    /// pool at all, ever: it would sit on sqlx's 30s acquire timeout and come
    /// back as `PoolTimedOut`. This test therefore cannot pass by being slow, and
    /// the timing bound below is corroboration rather than the proof.
    #[tokio::test]
    async fn find_by_id_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let r = repo().await;
        let actor = actor(&r).await;
        let product = r.create(actor, &product_input("REPO-PH3")).await.unwrap();

        let mut tx = r.pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a read
        // right now, and that is a fact about the pool, not about this test's
        // patience.
        assert!(
            r.pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let found = r.find_by_id_in(&mut tx, product.id).await;
        let elapsed = started.elapsed();
        let found = found
            .expect(
                "find_by_id_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
            )
            .expect("the committed row is visible to a transaction opened after it");
        // MEASURED, not assumed: the pairing above already decides it. Five
        // seconds sits far above what a query on a held connection costs and far
        // below the 30s acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "find_by_id_in took {elapsed:?}; that is a read stalling for a connection, not one on the connection it was handed"
        );
        assert_eq!(found.sku, "REPO-PH3");
        // The caller's transaction is still ALIVE and still holds its lock: a
        // second statement on the same connection answers. An `find_by_id_in`
        // that had ended, committed or rolled back the unit it was given could
        // not leave this true.
        assert!(r
            .find_by_id_in(&mut tx, product.id)
            .await
            .unwrap()
            .is_some());
        assert!(r.find_by_id_in(&mut tx, 99999).await.unwrap().is_none());
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert!(r.find_by_id(product.id).await.unwrap().is_some());
    }

    /// The additive claim, proved rather than asserted: the public `find_by_id`
    /// still answers exactly what it always answered, in both directions. The
    /// caller distinguishes "product missing" from "error" —
    /// `InventoryService::get_product` turns the first into `AppError::NotFound`
    /// and propagates the second, so `Ok(None)` is an answer this rewrite must
    /// not turn into anything else.
    #[tokio::test]
    async fn the_public_find_by_id_answers_exactly_as_before_including_the_missing_product() {
        let r = repo().await;
        let actor = actor(&r).await;
        let first = r.create(actor, &product_input("REPO-PH4")).await.unwrap();
        let second = r.create(actor, &product_input("REPO-PH5")).await.unwrap();

        // The wrapper opens a unit of its own now, and that unit is invisible:
        // the row is readable immediately afterwards, by the same repository and
        // by anything else on the pool.
        let read = r
            .find_by_id(first.id)
            .await
            .unwrap()
            .expect("committed row");
        assert_eq!(read.sku, "REPO-PH4");
        assert_eq!(read.name, "prod REPO-PH4");
        assert_eq!(read.kind, ProductKind::Product);
        assert_eq!(read.unit, "un");
        assert_eq!(read.sale_price, Decimal::from_str("10").unwrap());
        assert_eq!(read.cost_price, Decimal::from_str("5").unwrap());
        assert!(read.track_stock);
        assert_eq!(read.min_stock, Some(Decimal::from_str("5").unwrap()));
        assert_eq!(read.max_stock, Some(Decimal::from_str("50").unwrap()));
        assert!(read.is_active);
        assert_eq!(read.created_by, actor);
        // Per id, not "is the table non-empty": a wrapper that dropped its bind
        // would answer the second read with the first.
        assert_eq!(
            r.find_by_id(second.id).await.unwrap().unwrap().sku,
            "REPO-PH5"
        );
        // The not-found branch is a VALUE, not an error.
        assert!(matches!(r.find_by_id(99999).await, Ok(None)));
        assert!(matches!(r.find_by_id(0).await, Ok(None)));
        // And the wrapper leaves no unit behind: it is answerable again
        // immediately, and the row it read is still the row it found.
        assert_eq!(
            r.find_by_id(first.id).await.unwrap().unwrap().sku,
            "REPO-PH4"
        );
    }
}
