//! Immutable document-line tax snapshots (tax calculation and settings, T1).
//!
//! TWO explicit tables, one module. The split is deliberate: the persistence
//! must be able to say "this sale line" and "this purchase line" with a real
//! foreign key each, so the SQL never pretends a snapshot can point at either
//! line table.
//!
//! This module owns only the snapshot tables' SQL. The WRITES are not offered
//! as a public repository: a snapshot is meaningful only next to the line that
//! carries it and the aggregate that summarizes it, so the only way to persist
//! one is the atomic line+tax boundary the document repositories expose
//! (`SaleRepository::create_line_with_taxes` and its siblings), which runs these
//! helpers inside its own transaction. A public "insert a snapshot" or "change
//! a tax total" call is deliberately absent: that is the seam through which a
//! breakdown and its aggregate could drift apart.
//!
//! The read side IS public: a document detail view needs the frozen breakdown
//! long after the write, and no write can go through it.
//!
//! The write helpers are `pub(crate)` and their only callers are the document
//! repositories' transactions, which run them inside the same transaction that
//! already proved the line belongs to a DRAFT document.
use async_trait::async_trait;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewLineTax, PurchaseLineTax, SaleLineTax};
use rust_decimal::Decimal;

fn parse_decimal(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap_or(Decimal::ZERO)
}

pub(crate) fn map_snapshot_error(error: sqlx::Error) -> AppError {
    let message = error.to_string();
    if message.contains("FOREIGN KEY constraint failed") {
        // Either the line or the tax does not exist: a caller's mistake about
        // an id, and a validation failure rather than a database fault. The
        // RESTRICT on `tax_id` never surfaces here — that edge fires on
        // deleting a tax, not on inserting a snapshot.
        AppError::Validation("line or tax not found".into())
    } else if message.contains("UNIQUE constraint failed") {
        // The same tax cannot be snapshotted twice on one line; the contract
        // resolves a set, so this only fires on a caller bug.
        AppError::Conflict("tax is already snapshotted on this line".into())
    } else {
        AppError::Database(error)
    }
}

fn row_to_sale_line_tax(row: sqlx::sqlite::SqliteRow) -> SaleLineTax {
    let rate: String = row.get("rate");
    let amount: String = row.get("amount");
    SaleLineTax {
        id: row.get("id"),
        sale_line_id: row.get("sale_line_id"),
        tax_id: row.get("tax_id"),
        code: row.get("tax_code"),
        name: row.get("tax_name"),
        rate: parse_decimal(&rate),
        amount: parse_decimal(&amount),
        created_at: row.get("created_at"),
    }
}

fn row_to_purchase_line_tax(row: sqlx::sqlite::SqliteRow) -> PurchaseLineTax {
    let rate: String = row.get("rate");
    let amount: String = row.get("amount");
    PurchaseLineTax {
        id: row.get("id"),
        purchase_line_id: row.get("purchase_line_id"),
        tax_id: row.get("tax_id"),
        code: row.get("tax_code"),
        name: row.get("tax_name"),
        rate: parse_decimal(&rate),
        amount: parse_decimal(&amount),
        created_at: row.get("created_at"),
    }
}

// ---------------------------------------------------------------------------
// Write helpers: the snapshot half of the document repositories' transactions
// ---------------------------------------------------------------------------

/// REPLACE a sale line's breakdown inside the caller's transaction.
///
/// The delete and the inserts are one unit on purpose: a draft edit swaps its
/// whole breakdown, so a line is never left holding a tax that no longer
/// applies next to one that does. `crate`-visible on purpose — the only
/// permitted caller is the sale repository, which reaches this line inside the
/// same transaction that has already proven the line belongs to a DRAFT sale.
pub(crate) async fn replace_sale_line_taxes(
    conn: &mut SqliteConnection,
    sale_line_id: i64,
    taxes: &[NewLineTax],
) -> AppResult<()> {
    sqlx::query("DELETE FROM sale_line_taxes WHERE sale_line_id = ?")
        .bind(sale_line_id)
        .execute(&mut *conn)
        .await
        .map_err(map_snapshot_error)?;
    for tax in taxes {
        sqlx::query(
            r#"INSERT INTO sale_line_taxes
               (sale_line_id, tax_id, tax_code, tax_name, rate, amount)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(sale_line_id)
        .bind(tax.tax_id)
        .bind(&tax.code)
        .bind(&tax.name)
        .bind(tax.rate.to_string())
        .bind(tax.amount.to_string())
        .execute(&mut *conn)
        .await
        .map_err(map_snapshot_error)?;
    }
    Ok(())
}

/// The purchase-line mirror of [`replace_sale_line_taxes`].
pub(crate) async fn replace_purchase_line_taxes(
    conn: &mut SqliteConnection,
    purchase_line_id: i64,
    taxes: &[NewLineTax],
) -> AppResult<()> {
    sqlx::query("DELETE FROM purchase_line_taxes WHERE purchase_line_id = ?")
        .bind(purchase_line_id)
        .execute(&mut *conn)
        .await
        .map_err(map_snapshot_error)?;
    for tax in taxes {
        sqlx::query(
            r#"INSERT INTO purchase_line_taxes
               (purchase_line_id, tax_id, tax_code, tax_name, rate, amount)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(purchase_line_id)
        .bind(tax.tax_id)
        .bind(&tax.code)
        .bind(&tax.name)
        .bind(tax.rate.to_string())
        .bind(tax.amount.to_string())
        .execute(&mut *conn)
        .await
        .map_err(map_snapshot_error)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Read side
// ---------------------------------------------------------------------------

#[async_trait]
pub trait TaxSnapshotRepository: Send + Sync {
    /// The frozen breakdown of one sale line, in insertion order. A read: it
    /// never re-reads `taxes`, so an edited or deactivated tax cannot change
    /// what it answers.
    async fn list_sale_line_taxes(&self, sale_line_id: i64) -> AppResult<Vec<SaleLineTax>>;
    /// The frozen breakdown of one purchase line, in insertion order.
    async fn list_purchase_line_taxes(
        &self,
        purchase_line_id: i64,
    ) -> AppResult<Vec<PurchaseLineTax>>;
}

#[derive(Clone)]
pub struct SqliteTaxSnapshotRepository {
    pub pool: SqlitePool,
}

impl SqliteTaxSnapshotRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TaxSnapshotRepository for SqliteTaxSnapshotRepository {
    async fn list_sale_line_taxes(&self, sale_line_id: i64) -> AppResult<Vec<SaleLineTax>> {
        let rows = sqlx::query(
            r#"SELECT id, sale_line_id, tax_id, tax_code, tax_name, rate, amount, created_at
               FROM sale_line_taxes WHERE sale_line_id = ? ORDER BY id"#,
        )
        .bind(sale_line_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_sale_line_tax).collect())
    }

    async fn list_purchase_line_taxes(
        &self,
        purchase_line_id: i64,
    ) -> AppResult<Vec<PurchaseLineTax>> {
        let rows = sqlx::query(
            r#"SELECT id, purchase_line_id, tax_id, tax_code, tax_name, rate, amount, created_at
               FROM purchase_line_taxes WHERE purchase_line_id = ? ORDER BY id"#,
        )
        .bind(purchase_line_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_purchase_line_tax).collect())
    }
}
