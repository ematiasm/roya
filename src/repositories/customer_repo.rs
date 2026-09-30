use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Customer, NewCustomer, UpdateCustomer};

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_customer(row: sqlx::sqlite::SqliteRow) -> Customer {
    let walkin: i64 = row.get("is_walkin");
    let active: i64 = row.get("is_active");
    let credit: Option<String> = row.get("credit_limit");
    Customer {
        id: row.get("id"),
        name: row.get("name"),
        phone: row.get("phone"),
        address: row.get("address"),
        tax_id: row.get("tax_id"),
        notes: row.get("notes"),
        is_walkin: walkin == 1,
        is_active: active == 1,
        credit_limit: credit.as_deref().map(parse_decimal),
        due_days: row.get("due_days"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains("is_walkin") || s.contains("one_walkin") {
            AppError::Conflict("only one walk-in customer is allowed".into())
        } else {
            AppError::Conflict("customer already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for customer".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait CustomerRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewCustomer) -> AppResult<Customer>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Customer>>;

    /// [`Self::find_by_id`] inside a transaction the CALLER owns.
    ///
    /// **The deadlock alone, not correctness — stated that way on purpose,
    /// because a doc comment that borrowed `stock_for_product_in`'s argument
    /// would be borrowing a claim this method cannot make.** `confirm` never
    /// writes `customers`, and that is traced rather than assumed:
    /// `src/services/sales.rs` contains no SQL against that table at all, and
    /// the only production writers of it are this file's own `create` /
    /// `update` / `set_active` / `delete`, the named-customer insert at
    /// `src/repositories/customer_receipt_repo.rs:423`, and whatever HTTP
    /// handler calls `CustomerService::create_customer`. None of them is on the
    /// confirm path. So nothing this read validates is ever written by the
    /// transaction that will hold it, and reading it from the caller's
    /// connection buys no fresher truth than reading it from the pool.
    /// Contrast `stock_for_product_in`, which folds rows the same document is
    /// still writing and genuinely cannot answer correctly from a snapshot.
    ///
    /// What this read does need is a CONNECTION it was handed, and where it is
    /// reached is narrower than a casual grep suggests. `SalesService::confirm`
    /// reaches it only from the `PaymentType::Credit` arm, at
    /// `src/services/sales.rs:1303` → `CustomerService::get_customer`
    /// (`src/services/customers.rs:165`) → this. Three conditions must ALL
    /// hold to get there:
    ///
    /// * the document's `payment_type` is `Credit` — the `Cash` arm
    ///   (`sales.rs:1271`) resolves a payment method instead and never looks at
    ///   a customer, so it can never arrive here;
    /// * `cash_method_id` was `None`, or the `Validation` at `sales.rs:1285`
    ///   ("credit sale must not include a payment method") returns first;
    /// * `sale.due_date.is_some()`, or the `Validation` at `sales.rs:1297`
    ///   ("due_date is required for Credit") returns first.
    ///
    /// It is worth being precise about what is NOT a gate, because it is the
    /// thing a reader is most likely to assume: **`enforce_credit_limit` is not
    /// one.** That flag is read at `sales.rs:1311`, AFTER this lookup, so it
    /// guards the debt projection and never the lookup itself. The door is gated
    /// by the document's SHAPE, not by a constructor flag.
    ///
    /// That shape gate cuts both ways for a test suite. A test that builds a
    /// Cash document never touches this door at all — which is exactly why
    /// `payment_method_repo`'s `find_method_in` tests prove nothing about it and
    /// had to be called directly too: the two reads sit on OPPOSITE arms of the
    /// same `match` in the same function. And a document that never grows a due
    /// date never reaches it either, so a half-migration could leave this door
    /// unwired behind a `Validation` that returns first, with a green suite.
    ///
    /// The production failure is worth stating honestly, because the single
    /// connection these tests use would make it look like a timeout. The pool is
    /// `max_connections(5)` (`src/db.rs`), not 1, so a pool-reaching read there
    /// is not a `PoolTimedOut` at all: it takes a SECOND connection and answers
    /// from a snapshot while the unit holds the first. That is the silent
    /// split-brain, and it is the whole argument. This read is a door, not a
    /// correctness fix.
    ///
    /// The argument is stronger than a plain "nothing writes it", because of the
    /// one field the Credit arm branches on that turns out to be UNWRITABLE.
    /// `sales.rs:1304` refuses a credit sale to a walk-in on the strength of
    /// `is_walkin`, and `is_walkin = 1` cannot be produced by any writer: the
    /// seed already holds the only slot `idx_customers_one_walkin` allows
    /// (migration 36:168-169, a partial UNIQUE index over `WHERE is_walkin = 1`)
    /// and `trg_customers_walkin_no_demote` (migration 36:187-193) refuses to
    /// release it. So the field that refusal reads is pinned by the schema, not
    /// by a transaction. `credit_limit` — the other branch, `sales.rs:1312` — is
    /// mutable, and this door is what would let a caller see its own uncommitted
    /// value of it.
    ///
    /// FIFTEEN other production call sites reach it OUTSIDE `confirm` — counted,
    /// not estimated — and that is why the deadlock argument does not depend on
    /// where Phase B opens its BEGIN. Three of them are on the sales DOCUMENT
    /// path rather than the read surface, which is the fact worth carrying: this
    /// read is not a `confirm`-only door. `SalesService::create_draft`
    /// (`sales.rs:420`) and `SalesService::update_draft` (`sales.rs:466`) both
    /// resolve the customer before writing, `update_draft` specifically to
    /// re-derive a cleared due date from the customer's term. The rest are the
    /// customer read-only GET surface (`customers_api.rs:221`, `:305`, `:366`;
    /// `customers_web.rs:468`, `:534`, `:553`; `documents_web.rs:2123`), the
    /// activate/deactivate/delete trio and the `is_walkin` predicate
    /// (`customers.rs:138`, `:180`, `:190`, `:198`, `:212`), and
    /// `CustomerReceiptService` (`customer_receipts.rs:99`), which resolves the
    /// customer while WRITING a receipt. Those are separate units today and stay
    /// separate; this door does not close them.
    ///
    /// One copy of the SQL, on one executor: the public twin is nothing but
    /// BEGIN/delegate/COMMIT around this method, so there is no `find_by_id_raw`
    /// free function here the way `purchase_repo` has `find_purchase_raw`. There,
    /// `find_purchase` is a method in its own right with its own many callers
    /// and has to keep running on the pool, so the statement genuinely had to
    /// exist on two executors. Here it does not: `SELECT id, name, phone, …`
    /// appears exactly once in this file. (`exists`'s `SELECT COUNT(*) FROM
    /// customers WHERE id = ?` shares the tail of that text but is a different
    /// statement with a different projection and stays where it is.)
    ///
    /// Note that [`Self::update`] still calls the PUBLIC `find_by_id` at
    /// `customer_repo.rs:284`, and `update` is itself reached from
    /// `CustomerService::update_customer`, which calls `get_customer` FIRST
    /// (`customers.rs:138`). So one customer edit runs this read THREE times
    /// across three separate transactions: two in the service, one in the
    /// repository. That is pre-existing, deliberate and unchanged here —
    /// `update` is not on the confirm path, rewiring it is a different commit's
    /// business, and touching it would be a caller rewire dressed up as a
    /// repository move.
    ///
    /// Nothing opens a transaction yet. This is the door; `confirm` does not walk
    /// through it until a later commit of Phase A does.
    async fn find_by_id_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
    ) -> AppResult<Option<Customer>>;
    /// Every customer whose name matches exactly, for the duplicate warning.
    async fn find_by_name(&self, name: &str) -> AppResult<Vec<Customer>>;
    /// The seeded cash default, when it exists.
    async fn find_walkin(&self) -> AppResult<Option<Customer>>;
    /// `only_active = false` returns deactivated customers too.
    async fn list(&self, only_active: bool) -> AppResult<Vec<Customer>>;
    /// Update the editable fields (service guarantees cleaned values).
    async fn update(&self, id: i64, actor: i64, patch: &UpdateCustomer) -> AppResult<Customer>;
    async fn set_active(&self, id: i64, actor: i64, active: bool) -> AppResult<Customer>;
    /// DELETE is RESTRICTed by sales once `sales.customer_id` exists.
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

#[derive(Clone)]
pub struct SqliteCustomerRepository {
    pub pool: SqlitePool,
}

impl SqliteCustomerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CustomerRepository for SqliteCustomerRepository {
    async fn create(&self, actor: i64, input: &NewCustomer) -> AppResult<Customer> {
        let row = sqlx::query(
            r#"INSERT INTO customers
                   (name, phone, address, tax_id, notes, is_walkin, credit_limit, due_days, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, due_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(&input.name)
        .bind(input.phone.clone())
        .bind(input.address.clone())
        .bind(input.tax_id.clone())
        .bind(input.notes.clone())
        .bind(if input.is_walkin { 1i64 } else { 0i64 })
        .bind(input.credit_limit.map(|d| d.to_string()))
        .bind(input.due_days)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_customer(row))
    }

    /// A transaction of its own, for a caller with no larger unit to offer. The
    /// query and the projection are `find_by_id_in`'s to inherit unchanged; all
    /// this adds is the BEGIN/COMMIT that it deliberately leaves to someone
    /// else. A read that opens a transaction is not a write's privilege — the
    /// caller that owns the larger unit is the only one who can see what is in
    /// it.
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Customer>> {
        let mut tx = self.pool.begin().await?;
        let found = self.find_by_id_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(found)
    }

    async fn find_by_id_in(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
    ) -> AppResult<Option<Customer>> {
        // The executor is the caller's connection and nothing here opens a unit
        // of its own, so this read joins the caller's unit instead of ending
        // one. The SQL, the bind and the `Ok(row.map(row_to_customer))` mapping
        // are byte-for-byte what `find_by_id` always ran — including the
        // unfiltered projection, because the Credit arm branches on
        // `is_walkin` and on `credit_limit` being `Some` (sales.rs:1304-1327)
        // and must be handed the row as it stands, not a row this statement
        // pre-judged.
        let row = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, due_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(row_to_customer))
    }

    async fn find_by_name(&self, name: &str) -> AppResult<Vec<Customer>> {
        let rows = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, due_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE name = ? ORDER BY id"#,
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_customer).collect())
    }

    async fn find_walkin(&self) -> AppResult<Option<Customer>> {
        let row = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, due_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE is_walkin = 1 ORDER BY id LIMIT 1"#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_customer))
    }

    async fn list(&self, only_active: bool) -> AppResult<Vec<Customer>> {
        let rows = if only_active {
            sqlx::query(
                r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                          credit_limit, due_days, created_by, updated_by, created_at, updated_at
                   FROM customers WHERE is_active = 1 ORDER BY id"#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                          credit_limit, due_days, created_by, updated_by, created_at, updated_at
                   FROM customers ORDER BY id"#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(row_to_customer).collect())
    }

    async fn update(&self, id: i64, actor: i64, patch: &UpdateCustomer) -> AppResult<Customer> {
        let existing = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("customer {id} not found")))?;

        let name = patch.name.clone().unwrap_or(existing.name);
        let phone = match &patch.phone {
            Some(inner) => inner.clone(),
            None => existing.phone,
        };
        let address = match &patch.address {
            Some(inner) => inner.clone(),
            None => existing.address,
        };
        let tax_id = match &patch.tax_id {
            Some(inner) => inner.clone(),
            None => existing.tax_id,
        };
        let notes = match &patch.notes {
            Some(inner) => inner.clone(),
            None => existing.notes,
        };
        let credit_limit = match &patch.credit_limit {
            Some(inner) => inner.map(|d| d.to_string()),
            None => existing.credit_limit.map(|d| d.to_string()),
        };
        let due_days = match patch.due_days {
            Some(inner) => inner,
            None => existing.due_days,
        };

        let row = sqlx::query(
            r#"UPDATE customers
               SET name = ?, phone = ?, address = ?, tax_id = ?, notes = ?,
                   credit_limit = ?, due_days = ?,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, due_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(name)
        .bind(phone)
        .bind(address)
        .bind(tax_id)
        .bind(notes)
        .bind(credit_limit)
        .bind(due_days)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_customer(row))
    }

    async fn set_active(&self, id: i64, actor: i64, active: bool) -> AppResult<Customer> {
        let row = sqlx::query(
            r#"UPDATE customers
               SET is_active = ?,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, due_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(actor)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(row_to_customer)
            .ok_or_else(|| AppError::NotFound(format!("customer {id} not found")))
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM customers WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(
                        "cannot delete customer with sales; deactivate it instead".into(),
                    )
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM customers WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::time::{Duration, Instant};

    /// A valid acting user for the repo-level fixture calls: the migration's
    /// sentinel account. The audit-attribution tests live in the services.
    async fn audit_actor(repo: &SqliteCustomerRepository) -> i64 {
        test_support::audit_actor_id(&repo.pool).await.unwrap()
    }

    async fn migrated_pool() -> SqlitePool {
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

    // -- Phase A: the transaction-joining forms ------------------------------
    //
    // ONE method of the nine here gets an `_in` twin, and the reason it is this
    // one is a REACHABILITY fact rather than a judgement about importance.
    // `SalesService::confirm` reaches it from the `PaymentType::Credit` arm at
    // `sales.rs:1303` and nowhere else — `CustomerService::get_customer`
    // (`customers.rs:165`) is the only path down to it from a document. The
    // `Cash` arm resolves a payment method instead (`sales.rs:1281`) and never
    // looks at a customer, so commit 9's `find_method_in` and this `find_by_id_in`
    // cover OPPOSITE arms of the same `match`: a test that reaches one door
    // through a Cash document proves nothing about the other, and vice versa.
    //
    // The gate is narrower than a casual grep suggests, and that is the second
    // reason the `_in` form is called DIRECTLY below rather than through a
    // document. `sales.rs:1293-1303` reaches this read only when ALL THREE hold:
    // the document's `payment_type` is `Credit`; `due_date.is_some()`, or the
    // `Validation` at `sales.rs:1297` returns first; and `cash_method_id` was
    // `None`, or the `Validation` at `sales.rs:1285` returns first. Note what is
    // NOT a gate: `enforce_credit_limit` sits at `sales.rs:1311`, AFTER this
    // read, so it guards the debt projection and not the lookup. A door reachable
    // only by a document shape that most of the suite never builds is a door that
    // a half-migration can leave unwired without any test noticing.
    //
    // Why it moves is the DEADLOCK ALONE, and the trait doc says so plainly
    // rather than borrowing the correctness argument `find_by_id_in`'s sibling
    // `stock_for_product_in` legitimately makes. `confirm` never writes
    // `customers` — traced, not assumed: `sales.rs` contains no SQL against that
    // table at all, and the only production writers of it are this file's own
    // `create` / `update` / `set_active` / `delete`, the named-customer insert at
    // `customer_receipt_repo.rs:423`, and two HTTP handlers
    // (`routes/sales_api.rs:449`). None is on the confirm path.
    //
    // Every other method in this file is left alone, and the audit is in the
    // commit message. Nothing here opens a transaction across a service call:
    // Phase A installs the door, and `confirm` does not walk through it until a
    // later commit.

    /// One customer, created through the production writer and committed, so it
    /// exists before any transaction is opened.
    fn new_customer(name: &str) -> NewCustomer {
        NewCustomer {
            name: name.into(),
            phone: None,
            address: None,
            tax_id: None,
            notes: None,
            is_walkin: false,
            credit_limit: None,
            due_days: None,
        }
    }

    /// `find_by_id_in` must answer from the connection it was HANDED, and the
    /// two fields it must see are the two `sales.rs:1304-1327` branches the
    /// Credit arm turns refusals on: `is_walkin` (the walk-in customer may not
    /// owe money) and `credit_limit` (`Some` limits the sale, `None` is
    /// unlimited). A read that answered from a snapshot would hand the Credit arm
    /// a different DECISION than the one the transaction has committed to.
    ///
    /// The fixture is this module's own `migrated_pool()`, so `max_connections(1)`
    /// — the lever the next test needs — is inherited rather than restated.
    #[tokio::test]
    async fn find_by_id_in_reads_the_callers_uncommitted_customer_and_a_rollback_hides_it_again() {
        let pool = migrated_pool().await;
        let repo = SqliteCustomerRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let buyer = repo
            .create(actor, &new_customer("PH-A Buyer"))
            .await
            .unwrap();
        assert!(
            !buyer.is_walkin && buyer.credit_limit.is_none(),
            "the fixture must start as an unlimited non-walk-in, or this test proves nothing"
        );

        let mut tx = pool.begin().await.unwrap();
        // Both halves of "uncommitted": a row the unit CREATES, and the column
        // the unit CHANGES on a committed row. Neither is visible to anything
        // outside this connection.
        let inserted: i64 = sqlx::query_scalar(
            "INSERT INTO customers (name, is_walkin, created_by) \
             VALUES ('Created Inside', 0, ?) RETURNING id",
        )
        .bind(actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        // `credit_limit` is the mutable half of what the Credit arm branches on:
        // `Some` limits this sale, `None` is unlimited (`sales.rs:1312`). It is
        // NOT the only branch — `is_walkin` is the other one (`sales.rs:1304`) —
        // but this column cannot be exercised the way that field can, and the
        // reason is itself load-bearing for why this read is deadlock-only: a
        // second walk-in is impossible. `idx_customers_one_walkin` is a partial
        // UNIQUE index over `WHERE is_walkin = 1` (migration 36:168-169) and
        // `trg_customers_walkin_no_demote` (migration 36:187-193) refuses to
        // demote the seeded one, so `is_walkin` cannot be turned to 1 by ANY
        // writer at all — not this transaction, not a concurrent session. The
        // field the walk-in refusal reads is database-immutable.
        sqlx::query("UPDATE customers SET credit_limit = '2500.00' WHERE id = ?")
            .bind(buyer.id)
            .execute(&mut *tx)
            .await
            .unwrap();

        let seen_inserted = repo
            .find_by_id_in(&mut tx, inserted)
            .await
            .unwrap()
            .expect("the row this transaction created is invisible to it");
        assert_eq!(seen_inserted.name, "Created Inside");
        assert!(!seen_inserted.is_walkin);
        assert_eq!(seen_inserted.credit_limit, None);

        let seen_update = repo
            .find_by_id_in(&mut tx, buyer.id)
            .await
            .unwrap()
            .expect("the row this transaction changed is invisible to it");
        assert_eq!(
            seen_update.credit_limit,
            Some(Decimal::from_str("2500.00").unwrap()),
            "the read did not see the caller's own uncommitted write, so the credit-limit branch at sales.rs:1312 would be decided from a different moment than the one the transaction holds"
        );
        assert!(
            !seen_update.is_walkin,
            "the projection mapped is_walkin differently than the committed row says"
        );
        // And it is still a plain read by id: an unknown id is a VALUE, not an
        // error, which is the branch `get_customer` turns into `NotFound`.
        assert!(repo
            .find_by_id_in(&mut tx, 9_999_999)
            .await
            .unwrap()
            .is_none());
        tx.rollback().await.unwrap();

        // The rollback took both with it, which is the other half: a
        // `find_by_id_in` that could not see the rollback was reading something
        // other than the caller's transaction.
        assert!(
            repo.find_by_id(inserted).await.unwrap().is_none(),
            "the customer survived a rollback of the transaction that created it"
        );
        let restored = repo.find_by_id(buyer.id).await.unwrap().unwrap();
        assert_eq!(
            restored.credit_limit, None,
            "the credit limit survived a rollback of the transaction that set it"
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
    /// back as `PoolTimedOut`. This test therefore cannot pass by being slow,
    /// and the timing bound below is corroboration rather than the proof.
    #[tokio::test]
    async fn find_by_id_in_answers_while_the_callers_transaction_holds_the_only_connection() {
        let pool = migrated_pool().await;
        let repo = SqliteCustomerRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let buyer = repo
            .create(actor, &new_customer("PH-B Buyer"))
            .await
            .unwrap();
        repo.update(
            buyer.id,
            actor,
            &UpdateCustomer {
                credit_limit: Some(Some(Decimal::from_str("900.00").unwrap())),
                ..UpdateCustomer::default()
            },
        )
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        // The premise, asserted rather than assumed: the pool cannot serve a
        // read right now, and that is a fact about the pool, not about this
        // test's patience.
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = Instant::now();
        let found = repo.find_by_id_in(&mut tx, buyer.id).await;
        let elapsed = started.elapsed();
        let found = found.expect(
            "find_by_id_in reached for the pool; with the only connection held by the caller's transaction that is a 30s PoolTimedOut, not an answer",
        ).expect("the committed row is visible to a transaction opened after it");
        // MEASURED, not assumed: the pairing above already decides it. Five
        // seconds sits far above what a query on a held connection costs and far
        // below the 30s acquire timeout it is here to rule out.
        assert!(
            elapsed < Duration::from_secs(5),
            "find_by_id_in took {elapsed:?}; that is a read stalling for a connection, not one on the connection it was handed"
        );
        assert_eq!(found.name, "PH-B Buyer");
        assert_eq!(
            found.credit_limit,
            Some(Decimal::from_str("900.00").unwrap())
        );
        // The caller's transaction is still ALIVE and still holds its lock: a
        // second statement on the same connection answers. A `find_by_id_in` that
        // had ended, committed or rolled back the unit it was given could not
        // leave this true.
        assert!(repo
            .find_by_id_in(&mut tx, buyer.id)
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .find_by_id_in(&mut tx, 9_999_999)
            .await
            .unwrap()
            .is_none());
        tx.rollback().await.unwrap();

        // The pool is answerable again now that the unit is over, so the stall
        // above was the transaction and not the connection.
        assert!(repo.find_by_id(buyer.id).await.unwrap().is_some());
    }

    /// The additive claim, proved rather than asserted: the public `find_by_id`
    /// still answers exactly what it always answered.
    ///
    /// The shape difference from `payment_method_repo`'s third test is the
    /// `Option`: `exists` answers a bool and collapses "unknown id" into `false`,
    /// but this read returns `Option<Customer>`, so the missing customer is a
    /// VALUE that a caller turns into a refusal. `CustomerService::get_customer`
    /// (`customers.rs:165`) maps `None` to `AppError::NotFound` and is the reason
    /// the distinction is load-bearing: "customer missing" and "the read failed"
    /// are different answers to the same question, and a rewrite that collapsed
    /// them would turn a clean 404 into a 500 for a sale against a deleted
    /// customer. Asserted here as the raw value, because calling
    /// `services::customers` from a `repositories/` test module would import
    /// `services::` downward and invert the layering rule.
    #[tokio::test]
    async fn the_public_find_by_id_answers_exactly_as_before_including_the_missing_customer() {
        let pool = migrated_pool().await;
        let repo = SqliteCustomerRepository::new(pool.clone());
        let actor = audit_actor(&repo).await;
        let first = repo
            .create(actor, &new_customer("PH-C First"))
            .await
            .unwrap();
        let second = repo
            .create(actor, &new_customer("PH-C Second"))
            .await
            .unwrap();

        // Every field the projection maps is a value the Credit arm or the
        // statement page can read, and `row_to_customer` is shared with the
        // `_in` form — so the wrapper's projection is what is pinned here.
        let found = repo.find_by_id(first.id).await.unwrap().unwrap();
        assert_eq!(found.id, first.id);
        assert_eq!(found.name, "PH-C First");
        assert_eq!(found.created_by, actor);
        assert!(found.is_active);
        assert!(!found.is_walkin);
        assert_eq!(found.credit_limit, None);
        assert_eq!(found.due_days, None);
        assert_eq!(found.phone, None);

        // Per id, not "is the table non-empty": a wrapper that dropped its bind
        // would answer the second read with the first.
        assert_eq!(
            repo.find_by_id(second.id).await.unwrap().unwrap().name,
            "PH-C Second"
        );

        // A customer WITH a limit and a term is a third shape, and `None` for
        // `credit_limit` is "unlimited" rather than "unset" — the Credit arm
        // branches on exactly this (`sales.rs:1312`).
        repo.update(
            second.id,
            actor,
            &UpdateCustomer {
                credit_limit: Some(Some(Decimal::from_str("1500.50").unwrap())),
                due_days: Some(Some(30)),
                phone: Some(Some("555-0100".into())),
                ..UpdateCustomer::default()
            },
        )
        .await
        .unwrap();
        let limited = repo.find_by_id(second.id).await.unwrap().unwrap();
        assert_eq!(
            limited.credit_limit,
            Some(Decimal::from_str("1500.50").unwrap()),
            "the decimal projection changed: credit_limit is stored as TEXT and mapped by parse_decimal"
        );
        assert_eq!(limited.due_days, Some(30));
        assert_eq!(limited.phone.as_deref(), Some("555-0100"));
        // The wrapper's own unit is invisible, so the read is correct
        // immediately after another method's commit.
        assert_eq!(limited.name, "PH-C Second");

        // The MISSING case is a value, not an error — and it stays one. This is
        // the branch `get_customer` maps to `NotFound`, and the branch a deleted
        // customer takes on a sale confirm.
        assert!(matches!(repo.find_by_id(9_999_999).await, Ok(None)));
        assert!(matches!(repo.find_by_id(0).await, Ok(None)));
        // And the row that WAS found is still found afterwards, so the
        // not-found answer did not consume or end anything.
        assert!(repo.find_by_id(first.id).await.unwrap().is_some());

        // Deactivated is still returned, not filtered out: `find_by_id` does not
        // filter, so `is_active = false` reaches the caller as a VALUE rather
        // than as a missing row. (`list` is the method that filters.)
        repo.set_active(first.id, actor, false).await.unwrap();
        let inactive = repo.find_by_id(first.id).await.unwrap().unwrap();
        assert!(
            !inactive.is_active,
            "the read filtered the deactivated customer out instead of reporting it"
        );
        assert_eq!(inactive.updated_by, Some(actor));
        assert!(repo.find_by_id(second.id).await.unwrap().unwrap().is_active);
    }
}
