use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewTax, ProductTax, Tax};
use rust_decimal::Decimal;

fn parse_decimal(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap_or(Decimal::ZERO)
}

fn map_tax_error(error: sqlx::Error) -> AppError {
    if error.to_string().contains("UNIQUE constraint failed") {
        AppError::Conflict("tax code already exists".into())
    } else {
        AppError::Database(error)
    }
}

fn map_product_tax_error(error: sqlx::Error) -> AppError {
    let message = error.to_string();
    if message.contains("product_taxes.product_id, product_taxes.tax_id") {
        AppError::Conflict("tax is already linked to this product".into())
    } else if message.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid product or tax".into())
    } else {
        AppError::Database(error)
    }
}

fn row_to_tax(row: sqlx::sqlite::SqliteRow) -> Tax {
    let rate: String = row.get("rate");
    let is_active: i64 = row.get("is_active");
    Tax {
        id: row.get("id"),
        code: row.get("code"),
        name: row.get("name"),
        rate: parse_decimal(&rate),
        is_active: is_active == 1,
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_product_tax(row: sqlx::sqlite::SqliteRow) -> ProductTax {
    ProductTax {
        id: row.get("id"),
        product_id: row.get("product_id"),
        tax_id: row.get("tax_id"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
    }
}

#[async_trait]
pub trait TaxRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewTax) -> AppResult<Tax>;
    async fn list(&self) -> AppResult<Vec<Tax>>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Tax>>;
    async fn find_by_code(&self, code: &str) -> AppResult<Option<Tax>>;
    async fn update(
        &self,
        actor: i64,
        id: i64,
        code: &str,
        name: &str,
        rate: Decimal,
        is_active: bool,
    ) -> AppResult<Tax>;
    async fn deactivate(&self, actor: i64, id: i64) -> AppResult<Tax>;
}

#[async_trait]
pub trait ProductTaxRepository: Send + Sync {
    async fn link(&self, actor: i64, product_id: i64, tax_id: i64) -> AppResult<ProductTax>;
    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductTax>>;
    /// The tax definitions a document line will charge: every tax currently
    /// LINKED to this product and still ACTIVE, ordered by code then id so the
    /// calculation and the stored breakdown are deterministic.
    ///
    /// This is the resolution boundary where INACTIVE taxes are excluded: the
    /// pure calculation contract applies exactly the definitions it receives,
    /// so a deactivated tax must never survive this read. It is a pure read —
    /// it creates no link and changes no row.
    // T2 is the first caller outside tests (the product tax-inclusive preview
    // and, through the shared statement below, the document line writes), so
    // the `allow` is dropped then. It is here, not on the module, because this
    // trait carries plenty of other methods that are used today.
    #[allow(dead_code)]
    async fn list_active_for_product(&self, product_id: i64) -> AppResult<Vec<Tax>>;
    async fn unlink(&self, product_id: i64, tax_id: i64) -> AppResult<bool>;
}

#[derive(Clone)]
pub struct SqliteTaxRepository {
    pub pool: SqlitePool,
}

impl SqliteTaxRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TaxRepository for SqliteTaxRepository {
    async fn create(&self, actor: i64, input: &NewTax) -> AppResult<Tax> {
        let row = sqlx::query(
            r#"INSERT INTO taxes (code, name, rate, is_active, created_by)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, code, name, rate, is_active, created_by, updated_by,
                         created_at, updated_at"#,
        )
        .bind(&input.code)
        .bind(&input.name)
        .bind(input.rate.to_string())
        .bind(i64::from(input.is_active))
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_tax_error)?;
        Ok(row_to_tax(row))
    }

    async fn list(&self) -> AppResult<Vec<Tax>> {
        let rows = sqlx::query(
            r#"SELECT id, code, name, rate, is_active, created_by, updated_by,
                      created_at, updated_at
               FROM taxes ORDER BY code, id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_tax).collect())
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Tax>> {
        let row = sqlx::query(
            r#"SELECT id, code, name, rate, is_active, created_by, updated_by,
                      created_at, updated_at
               FROM taxes WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_tax))
    }

    async fn find_by_code(&self, code: &str) -> AppResult<Option<Tax>> {
        let row = sqlx::query(
            r#"SELECT id, code, name, rate, is_active, created_by, updated_by,
                      created_at, updated_at
               FROM taxes WHERE code = ?"#,
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_tax))
    }

    async fn update(
        &self,
        actor: i64,
        id: i64,
        code: &str,
        name: &str,
        rate: Decimal,
        is_active: bool,
    ) -> AppResult<Tax> {
        let row = sqlx::query(
            r#"UPDATE taxes
               SET code = ?, name = ?, rate = ?, is_active = ?, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, code, name, rate, is_active, created_by, updated_by,
                         created_at, updated_at"#,
        )
        .bind(code)
        .bind(name)
        .bind(rate.to_string())
        .bind(i64::from(is_active))
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_tax_error)?;
        Ok(row_to_tax(row))
    }

    async fn deactivate(&self, actor: i64, id: i64) -> AppResult<Tax> {
        let row = sqlx::query(
            r#"UPDATE taxes
               SET is_active = 0, updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, code, name, rate, is_active, created_by, updated_by,
                         created_at, updated_at"#,
        )
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_tax(row))
    }
}

#[derive(Clone)]
pub struct SqliteProductTaxRepository {
    pub pool: SqlitePool,
}

impl SqliteProductTaxRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProductTaxRepository for SqliteProductTaxRepository {
    async fn link(&self, actor: i64, product_id: i64, tax_id: i64) -> AppResult<ProductTax> {
        let row = sqlx::query(
            r#"INSERT INTO product_taxes (product_id, tax_id, created_by)
               VALUES (?, ?, ?)
               RETURNING id, product_id, tax_id, created_by, created_at"#,
        )
        .bind(product_id)
        .bind(tax_id)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_product_tax_error)?;
        Ok(row_to_product_tax(row))
    }

    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<ProductTax>> {
        let rows = sqlx::query(
            "SELECT id, product_id, tax_id, created_by, created_at
             FROM product_taxes WHERE product_id = ? ORDER BY id",
        )
        .bind(product_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_product_tax).collect())
    }

    async fn unlink(&self, product_id: i64, tax_id: i64) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM product_taxes WHERE product_id = ? AND tax_id = ?")
            .bind(product_id)
            .bind(tax_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn list_active_for_product(&self, product_id: i64) -> AppResult<Vec<Tax>> {
        let mut conn = self.pool.acquire().await?;
        active_taxes_for_product(&mut conn, product_id).await
    }
}

/// The active-tax resolution statement, shared by the public read above and by
/// the document repositories' line transactions.
///
/// It takes a CONNECTION, not a pool, so a caller that is already inside a
/// transaction resolves the taxes through that same transaction: the snapshot
/// a line writes and the catalog state it was resolved from are then one
/// consistent view, and a tax deactivated or re-rated while the line was being
/// written cannot slip between the two.
pub(crate) async fn active_taxes_for_product(
    conn: &mut SqliteConnection,
    product_id: i64,
) -> AppResult<Vec<Tax>> {
    // ONE joined read: the link table decides membership, `taxes.is_active`
    // decides whether the tax still applies. The existing `list_by_product` +
    // per-tax `find_by_id` shape would need N+1 reads and would apply the
    // active filter in Rust, where a missing row would be indistinguishable
    // from an inactive one.
    let rows = sqlx::query(
        r#"SELECT t.id, t.code, t.name, t.rate, t.is_active, t.created_by, t.updated_by,
                  t.created_at, t.updated_at
           FROM product_taxes pt
           JOIN taxes t ON t.id = pt.tax_id
           WHERE pt.product_id = ? AND t.is_active = 1
           ORDER BY t.code, t.id"#,
    )
    .bind(product_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(row_to_tax).collect())
}
