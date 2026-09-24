use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::ProductSupplierCost;

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_cost(row: sqlx::sqlite::SqliteRow) -> ProductSupplierCost {
    let current: String = row.get("current_cost");
    let previous: Option<String> = row.get("previous_cost");
    let preferred: i64 = row.get("is_preferred");
    let created_at = row.get("created_at");
    let updated_at = row.try_get("updated_at").unwrap_or(created_at);
    ProductSupplierCost {
        id: row.get("id"),
        product_id: row.get("product_id"),
        supplier_id: row.get("supplier_id"),
        current_cost: parse_decimal(&current),
        current_cost_date: row.get("current_cost_date"),
        previous_cost: previous.as_deref().map(parse_decimal),
        previous_cost_date: row.get("previous_cost_date"),
        is_preferred: preferred == 1,
        supplier_sku: row.get("supplier_sku"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at,
        updated_at,
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains("is_preferred") || s.contains("one_preferred") {
            AppError::Conflict("only one preferred supplier per product".into())
        } else {
            AppError::Conflict("cost row already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid product or supplier".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait ProductSupplierCostRepository: Send + Sync {
    /// First cost for a (product, supplier) pair: current only, no previous.
    /// `actor` is the acting user's id the service resolved from its request;
    /// it becomes the row's `created_by` and nothing the request itself can
    /// supply names it.
    async fn create_cost(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost>;
    /// Option A: move the existing current into previous (value + date) and
    /// store the new value with its date. The service only calls this when the
    /// new cost actually differs from the current one.
    async fn shift_cost(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost>;
    /// Same-cost confirmation: refresh only `current_cost_date`, leaving
    /// `previous_cost` and `current_cost` untouched so the last distinct price
    /// survives for the derived alert.
    async fn refresh_cost_date(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost>;
    async fn find(
        &self,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<Option<ProductSupplierCost>>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<ProductSupplierCost>>;
    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductSupplierCost>>;
    async fn list_by_supplier(&self, supplier_id: i64) -> AppResult<Vec<ProductSupplierCost>>;
    /// Mark one pair as preferred, clearing any other preferred row of the same
    /// product atomically (partial unique index allows at most one).
    async fn set_preferred(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<ProductSupplierCost>;
    async fn clear_preferred(&self, actor: i64, product_id: i64) -> AppResult<()>;
    async fn count_by_supplier(&self, supplier_id: i64) -> AppResult<i64>;
}

#[derive(Clone)]
pub struct SqliteProductSupplierCostRepository {
    pub pool: SqlitePool,
}

impl SqliteProductSupplierCostRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProductSupplierCostRepository for SqliteProductSupplierCostRepository {
    async fn create_cost(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        let row = sqlx::query(
            r#"INSERT INTO product_supplier_costs
               (product_id, supplier_id, current_cost, current_cost_date, created_by)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(product_id)
        .bind(supplier_id)
        .bind(cost.to_string())
        .bind(when)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_cost(row))
    }

    async fn shift_cost(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        let row = sqlx::query(
            r#"UPDATE product_supplier_costs
               SET previous_cost = current_cost,
                   previous_cost_date = current_cost_date,
                   current_cost = ?, current_cost_date = ?,
                   updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE product_id = ? AND supplier_id = ?
               RETURNING id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(cost.to_string())
        .bind(when)
        .bind(actor)
        .bind(product_id)
        .bind(supplier_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(row_to_cost).ok_or_else(|| {
            AppError::NotFound(format!(
                "cost row for product {product_id} and supplier {supplier_id} not found"
            ))
        })
    }

    async fn refresh_cost_date(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        let row = sqlx::query(
            r#"UPDATE product_supplier_costs
               SET current_cost_date = ?, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE product_id = ? AND supplier_id = ?
               RETURNING id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(when)
        .bind(actor)
        .bind(product_id)
        .bind(supplier_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(row_to_cost).ok_or_else(|| {
            AppError::NotFound(format!(
                "cost row for product {product_id} and supplier {supplier_id} not found"
            ))
        })
    }

    async fn find(
        &self,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<Option<ProductSupplierCost>> {
        let row = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs
               WHERE product_id = ? AND supplier_id = ?"#,
        )
        .bind(product_id)
        .bind(supplier_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_cost))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<ProductSupplierCost>> {
        let row = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_cost))
    }

    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductSupplierCost>> {
        let rows = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs
               WHERE product_id = ? ORDER BY id"#,
        )
        .bind(product_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_cost).collect())
    }

    async fn list_by_supplier(&self, supplier_id: i64) -> AppResult<Vec<ProductSupplierCost>> {
        let rows = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs
               WHERE supplier_id = ? ORDER BY id"#,
        )
        .bind(supplier_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_cost).collect())
    }

    async fn set_preferred(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<ProductSupplierCost> {
        // Two statements inside one transaction: clear first, then set, so the
        // partial unique index never sees two preferred rows for the product.
        // Both writes stamp `updated_by`: the demoted row and the promoted one
        // were each last changed by this request.
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"UPDATE product_supplier_costs SET is_preferred = 0, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE product_id = ? AND is_preferred = 1"#,
        )
        .bind(actor)
        .bind(product_id)
        .execute(&mut *tx)
        .await?;
        let res = sqlx::query(
            r#"UPDATE product_supplier_costs SET is_preferred = 1, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE product_id = ? AND supplier_id = ?"#,
        )
        .bind(actor)
        .bind(product_id)
        .bind(supplier_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(AppError::NotFound(format!(
                "cost row for product {product_id} and supplier {supplier_id} not found"
            )));
        }
        let row = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs
               WHERE product_id = ? AND supplier_id = ?"#,
        )
        .bind(product_id)
        .bind(supplier_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        tx.commit().await?;
        Ok(row_to_cost(row))
    }

    async fn clear_preferred(&self, actor: i64, product_id: i64) -> AppResult<()> {
        sqlx::query(
            r#"UPDATE product_supplier_costs SET is_preferred = 0, updated_by = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE product_id = ? AND is_preferred = 1"#,
        )
        .bind(actor)
        .bind(product_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_by_supplier(&self, supplier_id: i64) -> AppResult<i64> {
        let row: (i64,) =
            sqlx::query_as(r#"SELECT COUNT(*) FROM product_supplier_costs WHERE supplier_id = ?"#)
                .bind(supplier_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }
}
