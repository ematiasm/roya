use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::AppResult;
use crate::models::{MovementReason, MovementType, NewMovement, StockMovement};

/// The audit actor is an explicit argument on every mutation (M5 Phase B,
/// slice S10): `actor` is the acting user's id from the request's `Principal`.
/// For a movement produced inside a sale/purchase confirm, the flow passes ITS
/// request's actor down — the movement never records a fresh actor (AC18).
#[async_trait]
pub trait StockMovementRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewMovement) -> AppResult<StockMovement>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<StockMovement>>;
    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<StockMovement>>;
    async fn count_by_product(&self, product_id: i64) -> AppResult<i64>;
    /// Derived stock = SUM of signed qty in Rust (Decimal precision).
    async fn stock_for_product(&self, product_id: i64) -> AppResult<Decimal>;
}

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn type_from_str(s: &str) -> MovementType {
    match s {
        "Out" => MovementType::Out,
        "Adjust" => MovementType::Adjust,
        _ => MovementType::In,
    }
}

fn row_to_movement(row: sqlx::sqlite::SqliteRow) -> StockMovement {
    let qty_str: String = row.get("qty");
    let type_str: String = row.get("type");
    let reason_str: String = row.get("reason");
    StockMovement {
        id: row.get("id"),
        product_id: row.get("product_id"),
        qty: parse_decimal(&qty_str),
        movement_type: type_from_str(&type_str),
        reason: reason_str
            .parse()
            .unwrap_or(MovementReason::Purchase),
        reference: row.get("reference"),
        date: row.get("date"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
    }
}

fn signed_contribution(movement_type: MovementType, qty: Decimal) -> Decimal {
    match movement_type {
        MovementType::In => qty,
        MovementType::Out => -qty,
        MovementType::Adjust => qty,
    }
}

#[derive(Clone)]
pub struct SqliteStockMovementRepository {
    pub pool: SqlitePool,
}

impl SqliteStockMovementRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl StockMovementRepository for SqliteStockMovementRepository {
    async fn create(&self, actor: i64, input: &NewMovement) -> AppResult<StockMovement> {
        let row = sqlx::query(
            r#"INSERT INTO stock_movements (product_id, qty, type, reason, reference, date, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id, product_id, qty, type, reason, reference, date, created_by, updated_by, created_at"#,
        )
        .bind(input.product_id)
        .bind(input.qty.to_string())
        .bind(input.movement_type.to_string())
        .bind(input.reason.to_string())
        .bind(&input.reference)
        .bind(input.date)
        .bind(actor)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_movement(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<StockMovement>> {
        let row = sqlx::query(
            r#"SELECT id, product_id, qty, type, reason, reference, date, created_by, updated_by, created_at
               FROM stock_movements WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_movement))
    }

    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<StockMovement>> {
        let rows = sqlx::query(
            r#"SELECT id, product_id, qty, type, reason, reference, date, created_by, updated_by, created_at
               FROM stock_movements WHERE product_id = ? ORDER BY date, id"#,
        )
        .bind(product_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_movement).collect())
    }

    async fn count_by_product(&self, product_id: i64) -> AppResult<i64> {
        let row: (i64,) =
            sqlx::query_as(r#"SELECT COUNT(*) FROM stock_movements WHERE product_id = ?"#)
                .bind(product_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    async fn stock_for_product(&self, product_id: i64) -> AppResult<Decimal> {
        let rows = sqlx::query(
            r#"SELECT qty, type FROM stock_movements WHERE product_id = ?"#,
        )
        .bind(product_id)
        .fetch_all(&self.pool)
        .await?;
        let mut total = Decimal::ZERO;
        for row in rows {
            let qty_str: String = row.get("qty");
            let type_str: String = row.get("type");
            let qty = parse_decimal(&qty_str);
            total += signed_contribution(type_from_str(&type_str), qty);
        }
        Ok(total)
    }
}
