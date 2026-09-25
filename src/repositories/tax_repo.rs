use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewTax, ProductTax, Tax, TaxReferenceCounts};
use rust_decimal::Decimal;

fn parse_decimal(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap_or(Decimal::ZERO)
}

/// What the `ON DELETE RESTRICT` backstop maps to when a `DELETE FROM taxes`
/// hits a reference.
///
/// It is an INTERNAL signal, not operator copy: `TaxService::delete_tax`
/// re-reads the reference counts after this refusal and replaces it with the
/// specific reason (a product link, or frozen document history) before any
/// response is built, so no database text and no bare marker ever reaches an
/// operator. Mapping it to `AppError::Conflict` rather than
/// `AppError::Database` is what keeps a lost race a 409 instead of a raw 500.
const DELETE_REFUSED_BY_FOREIGN_KEY: &str = "tax delete refused by a foreign key";

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
    /// Every row that currently references one tax, split into the two
    /// families the delete safeguard distinguishes.
    ///
    /// THREE counts in ONE pass on purpose. A separate count per table would
    /// read the three tables at three different moments, so a link created
    /// between two of them could be counted as absent by the check and still
    /// stop the delete; the `ON DELETE RESTRICT` backstop remains the authority
    /// for that case, but the application-level answer should be internally
    /// consistent on its own.
    async fn reference_counts(&self, id: i64) -> AppResult<TaxReferenceCounts>;
    /// Remove the tax row outright, and report whether a row was removed.
    ///
    /// NO ACTOR PARAMETER, deliberately: the `taxes` table has no delete audit
    /// (`created_by`/`updated_by` die with the row and this project has no
    /// generic audit log), so an actor here could be recorded nowhere. Carrying
    /// one anyway would advertise an audit trail that does not exist.
    ///
    /// It never cascades and never rewrites a snapshot: the only thing this
    /// statement does is remove the tax definition, and the database refuses it
    /// outright while any reference remains.
    async fn hard_delete(&self, id: i64) -> AppResult<bool>;
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

    async fn reference_counts(&self, id: i64) -> AppResult<TaxReferenceCounts> {
        // One statement, three scalar subqueries: the link table and both
        // snapshot tables are read at the same instant, so the counts describe
        // one consistent state of the catalogue and the history.
        let row = sqlx::query(
            r#"SELECT
                 (SELECT COUNT(*) FROM product_taxes WHERE tax_id = ?) AS product_links,
                 (SELECT COUNT(*) FROM sale_line_taxes WHERE tax_id = ?)
                   + (SELECT COUNT(*) FROM purchase_line_taxes WHERE tax_id = ?)
                   AS document_snapshots"#,
        )
        .bind(id)
        .bind(id)
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(TaxReferenceCounts {
            product_links: row.get("product_links"),
            document_snapshots: row.get("document_snapshots"),
        })
    }

    async fn hard_delete(&self, id: i64) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM taxes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                if error.to_string().contains("FOREIGN KEY constraint failed") {
                    AppError::Conflict(DELETE_REFUSED_BY_FOREIGN_KEY.into())
                } else {
                    AppError::Database(error)
                }
            })?;
        Ok(result.rows_affected() == 1)
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
