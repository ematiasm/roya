use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::str::FromStr;

use crate::error::AppResult;
use crate::models::{
    DocumentKind, DocumentQuery, DocumentRow, MovementReason, MovementType, NewMovement,
    StockMovement,
};

/// A `%…%` LIKE needle whose literal `%`, `_` and `\` are escaped, so the SQL
/// matches the same partial substring the retired in-memory filter did. Callers
/// compare it to `LOWER(column) ... ESCAPE '\'`; case folding is ASCII, like
/// SQLite's `LOWER`, because the engine ships no Unicode collation. Each
/// repository module carries its own copy, like sale_repo and purchase_repo.
fn like_needle(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('%');
    for ch in raw.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

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

    /// The STOCK family of the documents index (documents-index): the stored
    /// movement projected to the feed's facts. The only family with a quantity
    /// instead of money.
    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>>;
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
        reason: reason_str.parse().unwrap_or(MovementReason::Purchase),
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
    /// Test-only read counter: proves the filtered list reads scale with the
    /// result set, not the shop's history. Absent from production builds.
    #[cfg(test)]
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SqliteStockMovementRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            #[cfg(test)]
            reads: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Count one repository read (test builds only).
    #[cfg(test)]
    fn tick(&self) {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reset and read the test-only read counter.
    #[cfg(test)]
    pub(crate) fn reset_reads(&self) {
        self.reads.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
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
        let rows = sqlx::query(r#"SELECT qty, type FROM stock_movements WHERE product_id = ?"#)
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

    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>> {
        // `Some(empty)` matched no actor, so no document can match: answer
        // without querying, like every other `Some(empty)` id filter here.
        if let Some(ids) = &query.actor_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }
        // The read-only products JOIN is deliberate and bounded: the row must
        // name the product it moved, one JOIN per family read beats N per-row
        // name lookups in the caller, and a read never moves write ownership —
        // the movements stay this file's table.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT m.id, m.product_id, m.qty, m.type, m.reason, m.reference, m.date, m.created_by, p.name AS product_name, p.sku AS product_sku FROM stock_movements m JOIN products p ON p.id = m.product_id",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND m.created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND m.date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND m.date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // The operator searches a movement by the product (name or sku)
            // and the reference the flow wrote; every side is COALESCEd so a
            // NULL never drops the row from the OR chain.
            let needle = like_needle(search);
            qb.push(" AND (LOWER(COALESCE(p.name, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(COALESCE(p.sku, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(COALESCE(m.reference, '')) LIKE LOWER(")
                .push_bind(needle)
                .push(") ESCAPE '\\')");
        }
        // Newest first: date descending, then id as the stable tiebreak.
        qb.push(" ORDER BY m.date DESC, m.id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();

        Ok(rows
            .into_iter()
            .map(|row| {
                let id: i64 = row.get("id");
                let qty_str: String = row.get("qty");
                let type_str: String = row.get("type");
                let reason_str: String = row.get("reason");
                let movement_type = type_from_str(&type_str);
                let reason = reason_str.parse().unwrap_or(MovementReason::Purchase);
                // The stored reference is the operator's identifier when one
                // was written (a sale or purchase number); an unnamed
                // adjustment still needs a label.
                let reference: String = row.get("reference");
                DocumentRow {
                    kind: DocumentKind::StockMovement,
                    id,
                    // The drill-down target is the product page, which shows
                    // the movement history; the row's own id stays in `id`.
                    owner_id: row.get("product_id"),
                    reference: if reference.is_empty() {
                        format!("Movimiento #{id}")
                    } else {
                        reference
                    },
                    party: row.get("product_name"),
                    date: row.get("date"),
                    // The only family with a quantity instead of money: the
                    // pill reads the stored tokens, e.g. `In · Purchase`.
                    detail: format!("{movement_type} · {reason}"),
                    amount: None,
                    quantity: Some(parse_decimal(&qty_str)),
                    created_by: row.get("created_by"),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use chrono::NaiveDate;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool so the walk-in backstops fire
            // exactly as they do in production.
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn seed_product(pool: &SqlitePool, actor: i64) -> i64 {
        // The test seeds several movements per database; one product row per
        // database is enough, and the sku is UNIQUE so reuse it.
        match sqlx::query_scalar("SELECT id FROM products WHERE sku = 'DOC-P'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                       VALUES ('DOC-P', 'doc prod', 'Product', 'un', '10', 1, ?)
                       RETURNING id"#,
            )
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    /// One movement through raw SQL (the projection tests seed shapes the
    /// repository API cannot build: arbitrary references, dates and actors).
    async fn seed_movement(
        pool: &SqlitePool,
        product_id: i64,
        qty: &str,
        movement_type: &str,
        reason: &str,
        reference: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO stock_movements (product_id, qty, type, reason, reference, date, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(product_id)
        .bind(qty)
        .bind(movement_type)
        .bind(reason)
        .bind(reference)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The projection: the stored reference (or `Movimiento #id`), the
    /// product, the `In · Purchase` pill, the quantity instead of money, the
    /// drill-down owner is the product.
    #[tokio::test]
    async fn stock_movement_document_rows_project_the_feed_facts() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;

        let referenced = seed_movement(
            &pool,
            product,
            "5",
            "In",
            "Purchase",
            "2024-SALE-000001",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let unnamed =
            seed_movement(&pool, product, "2", "Out", "Sale", "", d(2024, 5, 3), actor).await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        let referenced_row = rows
            .iter()
            .find(|r| r.id == referenced)
            .expect("referenced movement row");
        assert_eq!(referenced_row.kind, DocumentKind::StockMovement);
        assert_eq!(referenced_row.reference, "2024-SALE-000001");
        assert_eq!(referenced_row.party, "doc prod");
        assert_eq!(referenced_row.date, d(2024, 5, 2));
        assert_eq!(referenced_row.detail, "In · Purchase");
        assert_eq!(
            referenced_row.owner_id, product,
            "the drill-down target is the product"
        );
        assert_eq!(referenced_row.amount, None, "stock has no money");
        assert_eq!(referenced_row.quantity, Some(dec("5")));
        assert_eq!(referenced_row.created_by, actor);

        let unnamed_row = rows.iter().find(|r| r.id == unnamed).expect("unnamed row");
        assert_eq!(unnamed_row.reference, format!("Movimiento #{unnamed}"));
        assert_eq!(unnamed_row.detail, "Out · Sale");
        assert_eq!(unnamed_row.quantity, Some(dec("2")));
    }

    /// The audit-actor filter narrows the family read to the given ids, and
    /// `Some(empty)` matches nothing without touching the database.
    #[tokio::test]
    async fn stock_movement_document_rows_filter_by_actor_and_empty_actor_set() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let sistema = test_support::audit_actor_id(&pool).await.unwrap();
        let other = test_support::seed_audit_user(&pool, "stock-actor-2", "Stock Actor 2")
            .await
            .unwrap();
        assert_ne!(sistema, other);

        let product = seed_product(&pool, sistema).await;
        let mine = seed_movement(
            &pool,
            product,
            "5",
            "In",
            "Purchase",
            "ref-mine",
            d(2024, 5, 2),
            sistema,
        )
        .await;
        let theirs = seed_movement(
            &pool,
            product,
            "1",
            "Out",
            "Loss",
            "ref-theirs",
            d(2024, 5, 3),
            other,
        )
        .await;

        let only_sistema = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![sistema]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(
            only_sistema.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![mine]
        );

        let only_other = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![other]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(
            only_other.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![theirs]
        );

        let none = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: Some(vec![]),
                from: None,
                to: None,
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    /// Both date bounds are inclusive and exclude the neighbours.
    #[tokio::test]
    async fn stock_movement_document_rows_date_range_is_inclusive() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        let early = seed_movement(
            &pool,
            product,
            "1",
            "In",
            "Purchase",
            "",
            d(2024, 5, 1),
            actor,
        )
        .await;
        let first = seed_movement(
            &pool,
            product,
            "1",
            "In",
            "Purchase",
            "",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let last =
            seed_movement(&pool, product, "1", "Out", "Sale", "", d(2024, 5, 4), actor).await;
        let late =
            seed_movement(&pool, product, "1", "Out", "Sale", "", d(2024, 5, 5), actor).await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: Some(d(2024, 5, 2)),
                to: Some(d(2024, 5, 4)),
                search: None,
                limit: 10,
            })
            .await
            .unwrap();
        let ids = rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert!(!ids.contains(&early) && !ids.contains(&late));
        assert!(ids.contains(&first) && ids.contains(&last));
        assert_eq!(ids.len(), 2);
    }

    /// The search matches product name, sku and reference partially and
    /// case-insensitively; matching nothing is empty, never an error.
    #[tokio::test]
    async fn stock_movement_document_rows_search_name_sku_and_reference() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        seed_movement(
            &pool,
            product,
            "5",
            "In",
            "Purchase",
            "",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let with_reference = seed_movement(
            &pool,
            product,
            "1",
            "Out",
            "Sale",
            "FACT-77",
            d(2024, 5, 3),
            actor,
        )
        .await;

        // Partial, case-insensitive product name match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("doc prod".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        // Sku match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("DOC-p".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        // Reference match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("fact-77".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, with_reference);

        // Matching nothing is empty, never an error.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("zzz-nothing".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    /// The family read returns at most `limit` newest rows, date then id
    /// descending — the feed's newest-first contract.
    #[tokio::test]
    async fn stock_movement_document_rows_limit_returns_newest_first() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        let a = seed_movement(
            &pool,
            product,
            "1",
            "In",
            "Purchase",
            "",
            d(2024, 5, 1),
            actor,
        )
        .await;
        let b = seed_movement(
            &pool,
            product,
            "1",
            "In",
            "Purchase",
            "",
            d(2024, 5, 2),
            actor,
        )
        .await;
        let c = seed_movement(
            &pool,
            product,
            "1",
            "In",
            "Purchase",
            "",
            d(2024, 5, 2),
            actor,
        )
        .await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 2,
            })
            .await
            .unwrap();
        // Same date falls back to id descending, so c beats b.
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![c, b]);
        assert!(!rows.iter().any(|r| r.id == a));
    }

    /// The bounded-reads contract: 20 movements cost exactly one query (the
    /// joined rows query), never one read per row.
    #[tokio::test]
    async fn stock_movement_document_rows_read_count_stays_bounded() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        for i in 1..=20 {
            seed_movement(
                &pool,
                product,
                "1",
                "In",
                "Purchase",
                &format!("REF-{i}"),
                d(2024, 5, 1),
                actor,
            )
            .await;
        }

        repo.reset_reads();
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: None,
                limit: 200,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 20);
        assert_eq!(repo.read_count(), 1, "the joined read is one query");

        // A filter matching nothing is still exactly one query.
        repo.reset_reads();
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("zzz-nothing".into()),
                limit: 200,
            })
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert_eq!(repo.read_count(), 1, "the joined read is one query");
    }
}
