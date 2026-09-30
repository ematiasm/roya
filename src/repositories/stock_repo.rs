use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
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
    /// [`Self::create`] inside a transaction the CALLER owns, so a whole
    /// document can be one unit. The movement is committed when the caller's
    /// transaction commits, and a confirmation that rolls back therefore leaves
    /// no movement behind — the `W2` residue in
    /// `odd/tasks/confirm-failure-injection-and-state-predicates.md`, where a
    /// Draft nobody can reconcile has already had stock deducted from it.
    ///
    /// The SQL, the binds and the projected row are `create`'s to inherit
    /// unchanged; all this adds is somewhere to put them. `create` above is
    /// this method wrapped in a transaction of its own, for the callers that
    /// have no larger unit to offer.
    async fn create_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        input: &NewMovement,
    ) -> AppResult<StockMovement>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<StockMovement>>;
    async fn list_by_product(&self, product_id: i64) -> AppResult<Vec<StockMovement>>;
    async fn count_by_product(&self, product_id: i64) -> AppResult<i64>;
    /// Derived stock = SUM of signed qty in Rust (Decimal precision).
    async fn stock_for_product(&self, product_id: i64) -> AppResult<Decimal>;

    /// [`Self::stock_for_product`] inside a transaction the CALLER owns — and
    /// it is a READ that has to move for the same reason the write does.
    ///
    /// `InventoryService::record_movement` reads the level, decides, and only
    /// then writes. In a document with two lines of the same product, the
    /// second read therefore has to see the first line's movement, or the
    /// pre-check is folding a pre-transaction snapshot: `10` where the shop
    /// holds `4`, and a 6-unit sale is waved through. Joining the caller's
    /// transaction is what makes a sequence of movements in one document see
    /// its own writes, in the same order it wrote them — which is also what the
    /// `ORDER BY id` below is folding.
    ///
    /// The same argument applies to `balance_for_account` in
    /// `transaction_repo.rs`: it is read before each transaction row is written
    /// to validate against, so two lines against the same account in one
    /// document need the second read to see the first write. That read moves in
    /// its own commit, because a second file is a second commit — not because
    /// the argument is weaker there.
    ///
    /// Nothing opens a transaction yet. This is the door; the confirm path does
    /// not walk through it until a later commit of Phase A does.
    async fn stock_for_product_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        product_id: i64,
    ) -> AppResult<Decimal>;

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
    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// SQL, the binds and the projection are `create_in`'s to inherit
    /// unchanged; all this adds is the BEGIN/COMMIT that it deliberately leaves
    /// to someone else.
    async fn create(&self, actor: i64, input: &NewMovement) -> AppResult<StockMovement> {
        let mut tx = self.pool.begin().await?;
        let movement = self.create_in(&mut tx, actor, input).await?;
        tx.commit().await?;
        Ok(movement)
    }

    async fn create_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        input: &NewMovement,
    ) -> AppResult<StockMovement> {
        // The executor is the caller's connection, so the movement is theirs to
        // commit or to roll back, and nothing here opens a transaction of its
        // own.
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
        .fetch_one(&mut *tx)
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

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query and the fold are `stock_for_product_in`'s to inherit unchanged; all
    /// this adds is the BEGIN/COMMIT that it deliberately leaves to someone
    /// else.
    async fn stock_for_product(&self, product_id: i64) -> AppResult<Decimal> {
        let mut tx = self.pool.begin().await?;
        let level = self.stock_for_product_in(&mut tx, product_id).await?;
        tx.commit().await?;
        Ok(level)
    }

    async fn stock_for_product_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        product_id: i64,
    ) -> AppResult<Decimal> {
        // `ORDER BY id` is load-bearing, for the same reason as the account
        // balance fold: the check below is on the running sum, so the row order
        // decides which prefixes it sees, and without an `ORDER BY` the
        // `idx_stock_movements_product_date` index on `(product_id, date)` can
        // return DATE order while `InventoryService::record_movement` folded the
        // same rows in INSERTION order. The two can then walk different prefixes
        // of a range that runs out — `In 7.9e28 (d1)`, `Out 0.05e28 (d2)`,
        // `In 0.05e28 (d3)` passes the write pre-check in id order
        // (`7.9 / 7.85 / 7.9`) while a date-ordered fold walks
        // `0.05 / 7.95 → 7.95e28`, refusing a level the pre-check approved. `id`
        // order IS the write order, and the sort is over the product's own rows on
        // a column the planner already filters on. Run on the caller's
        // connection it also folds the rows that caller has written but not yet
        // committed, which is the point of the `_in` form.
        let rows = sqlx::query(
            r#"SELECT qty, type FROM stock_movements WHERE product_id = ? ORDER BY id"#,
        )
        .bind(product_id)
        .fetch_all(&mut *tx)
        .await?;
        // A stock level is a SET SUM over the product's movements, and every one of
        // them was written from a request's quantity: a bounded movement says
        // nothing about the sum of a set of them, exactly as a bounded line says
        // nothing about a document total. Two incoming movements of `4e28` each
        // carry and are `8e28` together, so the fold is checked and the level
        // refuses rather than panicking on the product drawer, the stock list and
        // every stock check that reads it.
        //
        // THE FOLD IS THE GUARANTEE, not an induction over `record_movement`'s
        // pre-check: that pre-check validates `current + delta` where `current` is
        // this very fold, which is a claim about the whole sum and not about the
        // prefixes of the fold's own iteration. What it buys is that the common
        // case refuses EARLY, with a useful message, rather than waiting for a
        // read to find it — and with the `ORDER BY id` above, the two agree on the
        // prefixes as well as on the total.
        let signed: Vec<Decimal> = rows
            .iter()
            .map(|row| {
                let qty = parse_decimal(&row.get::<String, _>("qty"));
                signed_contribution(type_from_str(&row.get::<String, _>("type")), qty)
            })
            .collect();
        Ok(crate::repositories::checked_aggregate_sum(&signed).map_err(AppError::PriceRefused)?)
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
                    // The only family with no money, so it has no document total
                    // to refuse: `amount` is `None` because there is no amount,
                    // never because one could not be computed.
                    total_refusal: None,
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

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // Every repository write here runs on `&self.pool`, so a document that
    // moves stock five times sends five statements to whatever connection is
    // free and can land on five different ones. Each method therefore gains a
    // paired `_in(&mut SqliteConnection)` twin holding the real SQL, and the
    // public method becomes a wrapper that opens a transaction of its own.
    //
    // These three tests are the deliverable. Phase A opens no transaction
    // anywhere: the wrappers must behave exactly as they did, which the third
    // test pins, and the `_in` twins must be real doors rather than painted
    // ones, which is what the first two measure.

    /// The write must join the caller's transaction, so that a document which
    /// rolls back leaves NO movement behind — the residue window W2 in
    /// `odd/tasks/confirm-failure-injection-and-state-predicates.md` is exactly
    /// one committed `Out` on a Draft nobody can reconcile.
    ///
    /// The shape matters, and `max_connections(1)` is why. The rollback happens
    /// BEFORE the assertion, and the table is read only afterwards: a read
    /// issued while `tx` holds the only connection would stall for sqlx's
    /// acquire timeout instead of answering. A door that committed on its own
    /// cannot pass this test either way, but not the way one would guess — it
    /// cannot acquire a second connection here at all, so it surfaces as
    /// `PoolTimedOut` after 30s rather than as a stray row.
    #[tokio::test]
    async fn create_in_writes_into_the_callers_transaction_and_a_rollback_leaves_nothing() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        let input = NewMovement {
            product_id: product,
            qty: dec("5"),
            movement_type: MovementType::In,
            reason: MovementReason::Purchase,
            reference: "2024-SALE-000001".into(),
            date: d(2024, 5, 2),
        };

        let mut tx = pool.begin().await.unwrap();
        let written = repo.create_in(&mut tx, actor, &input).await.unwrap();
        // Read back INSIDE the transaction, off the value RETURNING projected —
        // no pool acquisition, so nothing here can stall. The INSERT ran on the
        // caller's own connection and its commit is the caller's to make.
        assert_eq!(written.qty, dec("5"));
        assert_eq!(written.product_id, product);
        assert_eq!(written.created_by, actor);
        tx.rollback().await.unwrap();

        assert_eq!(
            repo.count_by_product(product).await.unwrap(),
            0,
            "the movement was committed even though the transaction was rolled back"
        );
        assert!(
            repo.find_by_id(written.id).await.unwrap().is_none(),
            "the RETURNING id came from a row that survived the rollback"
        );
    }

    /// The oversell guard, and the reason this READ moves in the same commit as
    /// the write.
    ///
    /// `InventoryService::record_movement` reads the level, decides, and only
    /// then writes — so in a document with two lines of the same product the
    /// second read has to see the first line's movement. Read from the pool it
    /// sees a pre-transaction snapshot, `10` instead of `4`, and waves a
    /// 6-unit sale through a shop holding four.
    ///
    /// It cannot be written through the public wrappers, and that is the point
    /// of the test: on a `max_connections(1)` pool the public
    /// `stock_for_product` would have to acquire the one connection `tx` is
    /// holding, and would stall until sqlx's acquire timeout expires. The
    /// assertion is only reachable at all because the read has a door onto the
    /// caller's transaction.
    #[tokio::test]
    async fn stock_for_product_in_reads_the_callers_uncommitted_movements() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;
        let receiving = NewMovement {
            product_id: product,
            qty: dec("10"),
            movement_type: MovementType::In,
            reason: MovementReason::Purchase,
            reference: String::new(),
            date: d(2024, 5, 2),
        };
        let shipping = NewMovement {
            product_id: product,
            qty: dec("6"),
            movement_type: MovementType::Out,
            reason: MovementReason::Sale,
            reference: "2024-SALE-000001".into(),
            date: d(2024, 5, 2),
        };

        let mut tx = pool.begin().await.unwrap();
        // The document's two lines, both written into the caller's transaction.
        repo.create_in(&mut tx, actor, &receiving).await.unwrap();
        repo.create_in(&mut tx, actor, &shipping).await.unwrap();

        // What the second line's pre-check folds: 10 in, 6 out, so 4. Off the
        // caller's connection it already reflects the first line; off the pool
        // it would still answer 10.
        let level = repo.stock_for_product_in(&mut tx, product).await.unwrap();
        assert_eq!(
            level,
            dec("4"),
            "the read did not see the caller's own writes"
        );
        // And that is the whole guard: a second 6-unit line is now visibly an
        // oversell, which against a stale 10 it would not have been.
        assert!(
            level - dec("6") < Decimal::ZERO,
            "a second 6-unit Out must be refused, not waved through"
        );
        tx.rollback().await.unwrap();

        // The rollback took both lines with it, which is the write test above
        // seen from the read side.
        assert_eq!(repo.count_by_product(product).await.unwrap(), 0);
    }

    /// The additive claim, proved rather than asserted: the public wrappers
    /// still commit their own work and still read it back, exactly as before.
    /// Phase A changes plumbing and nothing else.
    #[tokio::test]
    async fn the_public_wrappers_commit_and_read_back_exactly_as_before() {
        let pool = memory_pool().await;
        let repo = SqliteStockMovementRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = seed_product(&pool, actor).await;

        let written = repo
            .create(
                actor,
                &NewMovement {
                    product_id: product,
                    qty: dec("5"),
                    movement_type: MovementType::In,
                    reason: MovementReason::Purchase,
                    reference: "2024-SALE-000001".into(),
                    date: d(2024, 5, 2),
                },
            )
            .await
            .unwrap();
        // Committed: the row is there for the next statement, and RETURNING
        // projected the same fields it always did.
        assert_eq!(repo.count_by_product(product).await.unwrap(), 1);
        let stored = repo.find_by_id(written.id).await.unwrap().unwrap();
        assert_eq!(stored.id, written.id);
        assert_eq!(stored.product_id, product);
        assert_eq!(stored.qty, dec("5"));
        assert_eq!(stored.movement_type, MovementType::In);
        assert_eq!(stored.reason, MovementReason::Purchase);
        assert_eq!(stored.reference, "2024-SALE-000001");
        assert_eq!(stored.date, d(2024, 5, 2));
        assert_eq!(stored.created_by, actor);
        assert_eq!(
            stored.updated_by, None,
            "an append-only movement has no editor"
        );

        // The read wrapper folds the committed row like it always did.
        assert_eq!(repo.stock_for_product(product).await.unwrap(), dec("5"));
        repo.create(
            actor,
            &NewMovement {
                product_id: product,
                qty: dec("2"),
                movement_type: MovementType::Out,
                reason: MovementReason::Sale,
                reference: String::new(),
                date: d(2024, 5, 3),
            },
        )
        .await
        .unwrap();
        assert_eq!(repo.stock_for_product(product).await.unwrap(), dec("3"));

        // A product with no movements folds to zero, not an error. `seed_product`
        // reuses one sku, so this one is inserted by hand.
        let untouched: i64 = sqlx::query_scalar(
            r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
               VALUES ('DOC-EMPTY', 'no movements', 'Product', 'un', '10', 1, ?)
               RETURNING id"#,
        )
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.stock_for_product(untouched).await.unwrap(),
            Decimal::ZERO
        );
    }
}
