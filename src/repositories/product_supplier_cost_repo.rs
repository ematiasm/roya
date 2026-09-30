use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};
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

    /// [`Self::create_cost`] inside a transaction the CALLER owns, and the first
    /// of the three satellite writes `PurchasesService::confirm` performs after
    /// `set_confirmed` has already succeeded
    /// (`src/services/purchases.rs:1197-1207`).
    ///
    /// That loop is WINDOW 5, and it is the one failure window in this
    /// repository with no recovery path at all: `record_cost` runs once per line
    /// AFTER the document is Confirmed, numbered and paid, so a failure part way
    /// through the loop leaves a finished purchase carrying only SOME of its
    /// supplier costs. Nothing recovers it — the retry is refused at `confirm`'s
    /// own opening read ("purchase already confirmed"), and nothing on the
    /// document records that a cost is missing, while the satellite is what
    /// feeds cost freshness and reorder suggestions. Measured by
    /// `purchase_confirm_failure_in_record_cost_leaves_a_confirmed_purchase_with_one_cost`
    /// (`src/services/purchases.rs:5880`).
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn create_cost_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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

    /// [`Self::shift_cost`] inside a transaction the CALLER owns.
    ///
    /// This is the DESTRUCTIVE one of the three, and the reason it cannot be
    /// left to commit on a connection of its own: the statement moves
    /// `current_cost`/`current_cost_date` into
    /// `previous_cost`/`previous_cost_date` and overwrites the current pair. A
    /// shift that commits for a purchase that then fails is a cost history
    /// displaced for a document nobody can retry — and that displacement is
    /// precisely what the derived price-change alert and every later reorder
    /// suggestion read.
    ///
    /// `fetch_optional` plus the explicit `NotFound` are unchanged, so this is
    /// the straight case: there is no read-back to move and no refusal helper,
    /// and the statement is all the method owns. The ORDER BY question that
    /// AGENTS.md flags in `transaction_repo`/`stock_repo` does not arise — this
    /// statement touches exactly one row, selected by the
    /// `UNIQUE(product_id, supplier_id)` pair, and folds no money.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn shift_cost_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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

    /// [`Self::refresh_cost_date`] inside a transaction the CALLER owns.
    ///
    /// The cheapest of the three to get wrong and the one a careless migration
    /// is most likely to break silently: it is the same-price confirmation, so
    /// it must move `current_cost_date` and NOTHING else. `previous_cost` is the
    /// last genuinely different price and it is what lets the derived alert say
    /// anything at all — the service takes this branch only when the price did
    /// NOT change (`src/services/suppliers.rs:276-282`), so a shift here would
    /// make the alert read "Unchanged" forever.
    ///
    /// Same shape as `shift_cost_in`: `fetch_optional`, the explicit `NotFound`,
    /// no read-back, one copy of the SQL.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn refresh_cost_date_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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

    /// [`Self::find`] inside a transaction the CALLER owns — and this is a READ,
    /// which is why it is here at all.
    ///
    /// `record_cost` validates `when` against state the SAME unit writes: it
    /// refuses a backdated cost when `when < existing.current_cost_date`
    /// (`src/services/suppliers.rs:271-275`) and only then chooses between
    /// `create_cost`, `refresh_cost_date` and `shift_cost`. A `find` that ran
    /// on the pool while the caller's unit held an uncommitted cost write would
    /// compare the new `when` against the PREVIOUS line's date and refuse a
    /// legitimate confirmation — or answer `None` for a pair the same unit just
    /// created, and take the `create_cost` branch against a row that already
    /// exists. The same reasoning as `stock_for_product_in` and
    /// `balance_for_account_in`.
    ///
    /// The public twin wraps THIS method in a transaction rather than sharing a
    /// second copy of the SELECT, which is why there is no `find_cost_raw` free
    /// function here the way `purchase_repo` has `find_purchase_raw`. There,
    /// `find_purchase` is a method in its own right with its own many callers
    /// and has to keep running on the pool, so the statement genuinely had to
    /// exist on two executors. Here `find` is nothing but the wrapper of this
    /// method, so one copy of the SQL is one executor too.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not
    /// walk through it until a later commit of Phase A does.
    async fn find_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
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
        // No transaction before this commit, so this one is new. It changes no
        // answer: the statement was already atomic on its own, and the unit here
        // holds exactly the same single INSERT.
        let mut tx = self.pool.begin().await?;
        let created = self
            .create_cost_in(&mut tx, actor, product_id, supplier_id, cost, when)
            .await?;
        tx.commit().await?;
        Ok(created)
    }

    async fn create_cost_in(
        &self,
        tx: &mut SqliteConnection,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        // The executor is the caller's connection and nothing here opens a unit
        // of its own, so this write joins the caller's unit instead of ending
        // one. The SQL, the binds, the `RETURNING` projection and `map_db_err`
        // are byte-for-byte what `create_cost` always ran — including the
        // `UNIQUE(product_id, supplier_id)` -> `Conflict` mapping, which
        // `record_cost` relies on to refuse a duplicate pair rather than
        // silently starting a second cost history.
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
        .fetch_one(&mut *tx)
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
        let mut tx = self.pool.begin().await?;
        let shifted = self
            .shift_cost_in(&mut tx, actor, product_id, supplier_id, cost, when)
            .await?;
        tx.commit().await?;
        Ok(shifted)
    }

    async fn shift_cost_in(
        &self,
        tx: &mut SqliteConnection,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        // One row, selected by the pair, displaced in place. Nothing is folded
        // and nothing is ordered: `previous_cost = current_cost` reads the very
        // row the WHERE names, so there is no running-sum prefix for a row order
        // to decide. The statement is unchanged.
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
        .fetch_optional(&mut *tx)
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
        let mut tx = self.pool.begin().await?;
        let refreshed = self
            .refresh_cost_date_in(&mut tx, actor, product_id, supplier_id, when)
            .await?;
        tx.commit().await?;
        Ok(refreshed)
    }

    async fn refresh_cost_date_in(
        &self,
        tx: &mut SqliteConnection,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        // `current_cost_date` and the audit stamps only. `previous_cost` and
        // `current_cost` are not in the SET list and must not be: the service
        // takes this branch only on a same-price confirmation, and the previous
        // slot is the last genuinely distinct price the derived alert needs.
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
        .fetch_optional(&mut *tx)
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
        // A read that opens a transaction is not a write's privilege — the
        // caller that owns the larger unit is the only one who can see what is
        // in it. The BEGIN/COMMIT here is what the rule deliberately leaves to
        // the public twin; it adds nothing to the answer, and it is the same
        // posture `product_repo::find_by_id` already took.
        let mut tx = self.pool.begin().await?;
        let found = self.find_in(&mut tx, product_id, supplier_id).await?;
        tx.commit().await?;
        Ok(found)
    }

    async fn find_in(
        &self,
        tx: &mut SqliteConnection,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<Option<ProductSupplierCost>> {
        // The executor is the caller's connection, so this answers about the
        // unit's uncommitted rows. `record_cost` compares `when` against the
        // `current_cost_date` this projection carries, so a read that missed the
        // unit would refuse a legitimate confirmation. SQL, binds and
        // `row.map(row_to_cost)` are unchanged.
        let row = sqlx::query(
            r#"SELECT id, product_id, supplier_id, current_cost, current_cost_date, previous_cost, previous_cost_date, is_preferred, supplier_sku, created_by, updated_by, created_at, updated_at
               FROM product_supplier_costs
               WHERE product_id = ? AND supplier_id = ?"#,
        )
        .bind(product_id)
        .bind(supplier_id)
        .fetch_optional(&mut *tx)
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::time::{Duration, Instant};

    async fn memory_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::base_connect_options so the walk-in backstops
            // fire exactly as they do in production.
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

    /// One product per test database. `sku` is UNIQUE and every test here only
    /// needs the row to satisfy the FK — the satellite is keyed on the pair, not
    /// on a second product.
    async fn product_id(pool: &SqlitePool, actor: i64) -> i64 {
        match sqlx::query_scalar("SELECT id FROM products WHERE sku = 'COST-P'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                   VALUES ('COST-P', 'cost prod', 'Product', 'un', '10', 1, ?)
                   RETURNING id"#,
            )
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    async fn seed_supplier(pool: &SqlitePool, name: &str, actor: i64) -> i64 {
        // The supplier name is UNIQUE, so a repeated seed in one database reuses
        // the existing row.
        match sqlx::query_scalar("SELECT id FROM suppliers WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO suppliers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
            )
            .bind(name)
            .bind(actor)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    async fn cost_count(pool: &SqlitePool, product: i64, supplier: i64) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM product_supplier_costs WHERE product_id = ? AND supplier_id = ?",
        )
        .bind(product)
        .bind(supplier)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // THREE writes and ONE read, and they are not interchangeable.
    //
    // `create_cost` and `refresh_cost_date` are additive: a stray commit adds a
    // row that a later `record_cost` re-adds, or moves a date forward. Only
    // `shift_cost` DISPLACES: it moves `current_cost`/`current_cost_date` into
    // `previous_cost`/`previous_cost_date` and overwrites the current pair, so a
    // shift that survives a rollback leaves a product's price history one step
    // ahead of the truth. That residue is what the satellite feeds the derived
    // price-change alert and every later reorder suggestion, and no document
    // records that it happened. It gets a test of its own below for that reason.
    //
    // `find` is here for the same reason `stock_for_product_in` and
    // `balance_for_account_in` are: `record_cost` compares `when` against the
    // `current_cost_date` this read returns, so a read on the pool under a
    // caller's uncommitted write would refuse a legitimate confirmation.
    //
    // `set_preferred` owns this file's only other `pool.begin()` and is the one
    // thing here `confirm` does NOT reach; it is a separate slice and is
    // deliberately left alone.
    //
    // Nothing here opens a transaction across a service call. Phase A installs
    // the doors; `confirm` does not walk through them until a later commit, and
    // the last test pins that all four public wrappers are untouched in the
    // meantime.

    /// The write must land in the caller's unit, not in one of its own: a cost
    /// created inside a transaction and rolled back with it is GONE, and one
    /// that escaped into a private unit would be visible to the pool the moment
    /// it committed.
    ///
    /// This is the rollback half, and every assertion that touches the pool is
    /// AFTER the rollback. That is the assertion that fails if the `_in` form
    /// committed a unit of its own; a single-connection pool is what makes it
    /// visible instead of merely likely, because there is no spare connection
    /// for a nested `begin()` to take.
    #[tokio::test]
    async fn create_cost_in_writes_into_the_callers_transaction_and_a_rollback_takes_it_away() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "In-Tx Supplier", actor).await;
        assert_eq!(
            cost_count(&pool, product, supplier).await,
            0,
            "the fixture must start with no cost row, or this test proves nothing"
        );

        let mut tx = pool.begin().await.unwrap();
        let created = repo
            .create_cost_in(
                &mut tx,
                actor,
                product,
                supplier,
                dec("7.25"),
                d(2024, 5, 2),
            )
            .await
            .expect("create_cost_in could not run while it held the caller's connection");
        // The RETURNING projection is the row the INSERT wrote, read inside the
        // same unit, so a `create_cost_in` that answered from a private
        // connection would have had to guess this id.
        assert_eq!(created.product_id, product);
        assert_eq!(created.supplier_id, supplier);
        assert_eq!(created.current_cost, dec("7.25"));
        assert_eq!(created.current_cost_date, d(2024, 5, 2));
        assert_eq!(created.previous_cost, None);
        assert_eq!(created.previous_cost_date, None);
        assert!(!created.is_preferred);
        tx.rollback().await.unwrap();

        assert_eq!(
            cost_count(&pool, product, supplier).await,
            0,
            "the cost row survived a rollback of the transaction that created it, so create_cost_in opened and committed a unit of its own"
        );
        assert!(
            repo.find(product, supplier).await.unwrap().is_none(),
            "find still sees a cost row the caller's rollback removed"
        );
        assert!(repo.list_by_product(product).await.unwrap().is_empty());
        assert!(
            repo.list_by_supplier(supplier).await.unwrap().is_empty(),
            "the supplier reports a cost row the caller's rollback removed"
        );
        assert_eq!(repo.count_by_supplier(supplier).await.unwrap(), 0);
    }

    /// THE test of this commit, because `shift_cost` is the only one of the
    /// three writes that DISPLACES: it moves `current_cost`/
    /// `current_cost_date` into `previous_cost`/`previous_cost_date` and
    /// overwrites the current pair.
    ///
    /// A rollback must take the whole displacement back. If it does not, a
    /// purchase whose confirm rolled back leaves the product's price history one
    /// step ahead of the truth — the old current now sitting in `previous` and
    /// the abandoned purchase's price sitting in `current`. Nothing on the
    /// purchase records that, `record_cost` will not run again for that
    /// document, and the satellite serves cost freshness and reorder suggestions
    /// off exactly these two columns.
    ///
    /// The assertion is on the restored row, after the rollback, and it names
    /// all four columns rather than only the current: a half-restored history is
    /// the failure this test exists to catch.
    #[tokio::test]
    async fn shift_cost_in_writes_into_the_callers_transaction_and_a_rollback_takes_the_shift_back()
    {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "Shift Supplier", actor).await;
        let seeded = repo
            .create_cost(actor, product, supplier, dec("5"), d(2024, 5, 1))
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        let shifted = repo
            .shift_cost_in(&mut tx, actor, product, supplier, dec("8"), d(2024, 5, 3))
            .await
            .expect("shift_cost_in could not run while it held the caller's connection");
        // Same row, displaced: this is what the caller's unit has written.
        assert_eq!(shifted.id, seeded.id, "the shift created a second cost row");
        assert_eq!(shifted.current_cost, dec("8"));
        assert_eq!(shifted.current_cost_date, d(2024, 5, 3));
        assert_eq!(shifted.previous_cost, Some(dec("5")));
        assert_eq!(shifted.previous_cost_date, Some(d(2024, 5, 1)));
        tx.rollback().await.unwrap();

        // AFTER the rollback. Every one of the four columns, because a history
        // that came back with only the current pair restored is still displaced.
        let after = repo
            .find(product, supplier)
            .await
            .unwrap()
            .expect("the seeded cost row is gone; the rollback removed a committed row");
        assert_eq!(
            after.current_cost,
            dec("5"),
            "the abandoned purchase's price stayed in current_cost after a rollback, so every later cost freshness read reports a price the supplier never charged"
        );
        assert_eq!(
            after.current_cost_date,
            d(2024, 5, 1),
            "the current cost kept the abandoned purchase's date"
        );
        assert_eq!(
            after.previous_cost, None,
            "the previous slot kept the displaced price: the history is one step ahead of the truth"
        );
        assert_eq!(
            after.previous_cost_date, None,
            "the previous slot kept the displaced date"
        );
        // And the row is still shiftable, so the rollback restored the state
        // rather than corrupting the row into something unusable.
        let again = repo
            .shift_cost(actor, product, supplier, dec("9"), d(2024, 5, 4))
            .await
            .unwrap();
        assert_eq!(again.previous_cost, Some(dec("5")));
        assert_eq!(again.current_cost, dec("9"));
        assert_eq!(cost_count(&pool, product, supplier).await, 1);
    }

    /// `refresh_cost_date_in` is the same-cost branch: it must move the
    /// confirmation date and NOTHING else. The `previous_cost` it must preserve
    /// is the last genuinely different price — the service only takes this branch
    /// when the price did not change (`src/services/suppliers.rs:276-282`), and
    /// a shift here would make the derived price-change alert read "Unchanged"
    /// forever.
    #[tokio::test]
    async fn refresh_cost_date_in_moves_only_the_confirmation_date_and_a_rollback_takes_it_back() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "Refresh Supplier", actor).await;
        repo.create_cost(actor, product, supplier, dec("5"), d(2024, 5, 1))
            .await
            .unwrap();
        // Give the row a real previous price first, so "did not shift" is a
        // statement about a row that HAS something to lose rather than about a
        // row whose previous slot was already empty.
        repo.shift_cost(actor, product, supplier, dec("8"), d(2024, 5, 3))
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        let refreshed = repo
            .refresh_cost_date_in(&mut tx, actor, product, supplier, d(2024, 5, 9))
            .await
            .expect("refresh_cost_date_in could not run while it held the caller's connection");
        assert_eq!(
            refreshed.current_cost,
            dec("8"),
            "a refresh moved the price"
        );
        assert_eq!(refreshed.current_cost_date, d(2024, 5, 9));
        assert_eq!(
            refreshed.previous_cost,
            Some(dec("5")),
            "a refresh displaced the last distinct price, which is what the derived alert reads"
        );
        assert_eq!(refreshed.previous_cost_date, Some(d(2024, 5, 1)));
        tx.rollback().await.unwrap();

        let after = repo.find(product, supplier).await.unwrap().unwrap();
        assert_eq!(
            after.current_cost_date,
            d(2024, 5, 3),
            "the confirmation date survived a rollback of the transaction that wrote it"
        );
        assert_eq!(after.current_cost, dec("8"));
        assert_eq!(after.previous_cost, Some(dec("5")));
        assert_eq!(after.previous_cost_date, Some(d(2024, 5, 1)));
    }

    /// The read, and the reason a read is in this commit at all.
    ///
    /// `record_cost` refuses a backdated cost by comparing `when` against the
    /// `current_cost_date` this read returns (`src/services/suppliers.rs:271`).
    /// A `find` that ran on the pool while the caller's unit held an uncommitted
    /// cost write would compare the new `when` against the PREVIOUS line's date
    /// and refuse a legitimate confirmation — so `find_in` has to see what the
    /// same unit wrote.
    ///
    /// Two writes happen inside ONE unit here, and the read answers for the
    /// SECOND. That is the whole claim: on the pool the pair does not exist at
    /// all, and through `find_in` it carries the exact history the unit built.
    #[tokio::test]
    async fn find_in_sees_the_callers_uncommitted_rows() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "Read-In-Tx Supplier", actor).await;

        let mut tx = pool.begin().await.unwrap();
        repo.create_cost_in(&mut tx, actor, product, supplier, dec("7"), d(2024, 5, 10))
            .await
            .unwrap();
        repo.shift_cost_in(&mut tx, actor, product, supplier, dec("8"), d(2024, 5, 20))
            .await
            .unwrap();

        let seen = repo
            .find_in(&mut tx, product, supplier)
            .await
            .expect("find_in could not run while it held the caller's connection");
        let seen = seen.expect(
            "find_in saw none of the unit's own uncommitted rows, so a backdated confirmation would be judged against a date that does not exist yet",
        );
        assert_eq!(seen.current_cost, dec("8"));
        assert_eq!(
            seen.current_cost_date,
            d(2024, 5, 20),
            "the date find_in reports is the one the caller's own write stored, and it is what the backdating refusal compares against"
        );
        assert_eq!(seen.previous_cost, Some(dec("7")));
        assert_eq!(seen.previous_cost_date, Some(d(2024, 5, 10)));

        // The other half, and it is the premise rather than a flourish: while the
        // unit is open the pool has nothing to hand, so the ONLY connection any
        // other reader could use is the one this unit owns. An answer identical
        // to the one above would mean the writes had already escaped.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );
        tx.rollback().await.unwrap();

        assert!(
            repo.find(product, supplier).await.unwrap().is_none(),
            "the unit's rows were still there after the rollback, so create_cost_in and shift_cost_in escaped it"
        );
        assert_eq!(cost_count(&pool, product, supplier).await, 0);
    }

    /// All four `_in` forms must answer while the caller's transaction holds the
    /// only connection the pool owns, and the assertion is the pairing itself
    /// rather than a stopwatch.
    ///
    /// `max_connections(1)` is the lever. While `tx` is open it holds the only
    /// connection the pool owns, so `try_acquire` answering `None` is not a
    /// timing accident — it is the pool stating, at that instant, that it has
    /// nothing to hand. A door that reached for the pool could not answer on this
    /// pool at all, ever: it would sit on sqlx's 30s acquire timeout and come
    /// back as `PoolTimedOut`. The timing bound below is corroboration; the
    /// premise is the proof.
    #[tokio::test]
    async fn the_in_forms_answer_while_the_callers_transaction_holds_the_only_connection() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "Held-Conn Supplier", actor).await;

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve
        // anything right now, and that is a fact about the pool, not about this
        // test's patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let created = repo
            .create_cost_in(&mut tx, actor, product, supplier, dec("3"), d(2024, 5, 1))
            .await;
        let shifted = repo
            .shift_cost_in(&mut tx, actor, product, supplier, dec("4"), d(2024, 5, 2))
            .await;
        let refreshed = repo
            .refresh_cost_date_in(&mut tx, actor, product, supplier, d(2024, 5, 3))
            .await;
        let seen = repo.find_in(&mut tx, product, supplier).await;
        let elapsed = started.elapsed();

        let created = created.expect(
            "create_cost_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );
        let shifted = shifted.expect(
            "shift_cost_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );
        let refreshed = refreshed.expect(
            "refresh_cost_date_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );
        let seen = seen.expect(
            "find_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        );

        assert_eq!(created.current_cost, dec("3"));
        assert_eq!(shifted.current_cost, dec("4"));
        assert_eq!(shifted.previous_cost, Some(dec("3")));
        // The refresh found the SHIFT the same unit performed, which is the
        // `record_cost` sequence end to end: None -> create, create -> shift,
        // shift -> refresh, every step reading its own unit's writes.
        assert_eq!(refreshed.current_cost, dec("4"));
        assert_eq!(refreshed.current_cost_date, d(2024, 5, 3));
        assert_eq!(refreshed.previous_cost, Some(dec("3")));
        assert_eq!(seen.unwrap().current_cost_date, d(2024, 5, 3));

        // MEASURED, not assumed: the pairing above already decides it. Five
        // seconds sits four orders of magnitude above what four statements on a
        // held connection cost and six below the 30s acquire timeout it is here
        // to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "the four _in forms took {elapsed:?}; that is at least one of them stalling for a connection, not one on the connection it was handed"
        );

        // The caller's transaction is still ALIVE and still holds its lock: a
        // further write on the same connection answers. An `_in` that had ended,
        // committed or rolled back the unit it was given could not leave this
        // true.
        let again = repo
            .shift_cost_in(&mut tx, actor, product, supplier, dec("5"), d(2024, 5, 4))
            .await
            .expect("a second write on the same connection could not run");
        assert_eq!(
            again.id, created.id,
            "the second shift created a second row instead of displacing the first"
        );
        assert_eq!(again.previous_cost, Some(dec("4")));
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the premise
        // above was the transaction and not the connection.
        assert_eq!(cost_count(&pool, product, supplier).await, 0);
        assert_eq!(repo.count_by_supplier(supplier).await.unwrap(), 0);
        assert!(repo
            .create_cost(actor, product, supplier, dec("7"), d(2024, 5, 1))
            .await
            .is_ok());
    }

    /// The additive claim, proved rather than asserted: all four public wrappers
    /// still answer exactly what they always answered, in every direction.
    ///
    /// The REFUSALS are the interesting half, because they are the paths a
    /// careless migration drops. `create_cost` maps the table's
    /// `UNIQUE(product_id, supplier_id)` to `Conflict` through `map_db_err`; the
    /// two UPDATEs answer `fetch_optional` and turn the miss into `NotFound`
    /// themselves; `find` answers `None` for a pair that does not exist. If any
    /// of those had been lost in the move, `record_cost` would either raise a
    /// driver error or silently create a duplicate history row.
    ///
    /// The backdated-cost refusal is deliberately NOT asserted here, and the
    /// reason is worth stating: it is not raised in this repository at all.
    /// `SupplierService::record_cost` compares `when` against
    /// `existing.current_cost_date` and refuses with
    /// `Validation("cost date cannot precede the current cost date")`
    /// (`src/services/suppliers.rs:271-275`). What this repository owes that
    /// check is the VALUE it reports — so the test pins that every wrapper
    /// stores and returns the date it was given, unchanged, which is exactly
    /// what the comparison is made against.
    #[tokio::test]
    async fn the_public_wrappers_answer_exactly_as_before_including_every_refusal() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let repo = SqliteProductSupplierCostRepository::new(pool.clone());
        let product = product_id(&pool, actor).await;
        let supplier = seed_supplier(&pool, "Public Supplier", actor).await;
        let empty = seed_supplier(&pool, "Empty Supplier", actor).await;

        // -- find: a pair that does not exist -------------------------------
        assert!(
            repo.find(product, supplier).await.unwrap().is_none(),
            "find invented a cost row for a pair that was never recorded"
        );

        // -- shift_cost / refresh_cost_date on a row that does not exist -------
        for missing in [
            repo.shift_cost(actor, product, empty, dec("3"), d(2024, 5, 2))
                .await
                .unwrap_err(),
            repo.refresh_cost_date(actor, product, empty, d(2024, 5, 2))
                .await
                .unwrap_err(),
        ] {
            match missing {
                AppError::NotFound(msg) => assert_eq!(
                    msg,
                    format!("cost row for product {product} and supplier {empty} not found"),
                    "the refusal must name the pair it could not find"
                ),
                other => panic!("expected NotFound, got {other:?}"),
            }
        }
        assert_eq!(
            cost_count(&pool, product, empty).await,
            0,
            "a refused write created the row it refused to update"
        );

        // -- create_cost: the success path ------------------------------------
        let created = repo
            .create_cost(actor, product, supplier, dec("5"), d(2024, 6, 1))
            .await
            .unwrap();
        assert_eq!(created.current_cost, dec("5"));
        assert_eq!(
            created.current_cost_date,
            d(2024, 6, 1),
            "the wrapper must store the date it was handed, because that stored value is what record_cost compares a later `when` against"
        );
        assert_eq!(created.previous_cost, None);
        assert_eq!(created.previous_cost_date, None);
        assert_eq!(created.created_by, actor);
        assert!(created.updated_by.is_none());

        // The wrapper leaves no unit of its own behind: the row is readable the
        // moment the call returns.
        let read_back = repo.find(product, supplier).await.unwrap().unwrap();
        assert_eq!(read_back.id, created.id);

        // -- create_cost: the duplicate pair ---------------------------------
        match repo
            .create_cost(actor, product, supplier, dec("6"), d(2024, 6, 2))
            .await
            .unwrap_err()
        {
            AppError::Conflict(msg) => {
                assert_eq!(msg, "cost row already exists")
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(
            cost_count(&pool, product, supplier).await,
            1,
            "a refused duplicate created a second history row for the pair"
        );

        // -- shift_cost: the destructive path, on the wrapper -----------------
        let shifted = repo
            .shift_cost(actor, product, supplier, dec("8"), d(2024, 6, 3))
            .await
            .unwrap();
        assert_eq!(shifted.current_cost, dec("8"));
        assert_eq!(shifted.current_cost_date, d(2024, 6, 3));
        assert_eq!(shifted.previous_cost, Some(dec("5")));
        assert_eq!(shifted.previous_cost_date, Some(d(2024, 6, 1)));
        assert_eq!(shifted.updated_by, Some(actor));

        // -- refresh_cost_date: the same-price path ---------------------------
        let refreshed = repo
            .refresh_cost_date(actor, product, supplier, d(2024, 6, 9))
            .await
            .unwrap();
        assert_eq!(refreshed.current_cost, dec("8"));
        assert_eq!(refreshed.current_cost_date, d(2024, 6, 9));
        assert_eq!(
            refreshed.previous_cost,
            Some(dec("5")),
            "a same-price confirmation destroyed the last distinct price"
        );
        assert_eq!(refreshed.previous_cost_date, Some(d(2024, 6, 1)));

        // -- the reads the cost drawer uses -----------------------------------
        assert_eq!(
            repo.find_by_id(created.id).await.unwrap().unwrap().id,
            created.id
        );
        assert!(repo.find_by_id(999_999).await.unwrap().is_none());
        assert_eq!(repo.list_by_product(product).await.unwrap().len(), 1);
        assert_eq!(repo.list_by_supplier(supplier).await.unwrap().len(), 1);
        assert_eq!(repo.count_by_supplier(supplier).await.unwrap(), 1);
        assert_eq!(repo.count_by_supplier(empty).await.unwrap(), 0);

        // -- set_preferred is untouched by this commit -----------------------
        // It is the one method in this file that still owns its own BEGIN, and
        // it is the one `confirm` never reaches. Asserting it here pins that the
        // slice did not reach past its scope.
        let preferred = repo.set_preferred(actor, product, supplier).await.unwrap();
        assert!(preferred.is_preferred);
        assert!(
            repo.clear_preferred(actor, product).await.is_ok(),
            "clear_preferred stopped working"
        );
        assert!(
            !repo
                .find(product, supplier)
                .await
                .unwrap()
                .unwrap()
                .is_preferred
        );
    }
}
