// M4 customers (Slice L): customer receipts. One receipt groups the payments a
// single handover of money produced, so this repository owns the receipt document
// and nothing else. It never runs SQL against a sales or finance table:
// `list_allocations` delegates the `sale_payments` read to the sales repository
// that owns the table, and the customer/account/method references are validated by
// the service before `create` is called. FK failures surface as `Validation`
// (a RESTRICTed reference), UNIQUE failures as `Conflict`.
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{
    CustomerReceipt, DocumentKind, DocumentQuery, DocumentRow, NewReceipt, SalePayment,
};
use crate::repositories::sale_repo::{SaleRepository, SqliteSaleRepository};

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

fn row_to_receipt(row: sqlx::sqlite::SqliteRow) -> CustomerReceipt {
    CustomerReceipt {
        id: row.get("id"),
        customer_id: row.get("customer_id"),
        account_id: row.get("account_id"),
        method_id: row.get("method_id"),
        date: row.get("date"),
        notes: row.get("notes"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        AppError::Conflict("receipt already exists".into())
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for receipt".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait CustomerReceiptRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewReceipt) -> AppResult<CustomerReceipt>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<CustomerReceipt>>;
    /// Receipts of one customer, oldest first (`date`, then id).
    async fn list_by_customer(&self, customer_id: i64) -> AppResult<Vec<CustomerReceipt>>;
    /// The payments this receipt groups (its allocations), by id. The SQL for
    /// `sale_payments` stays in the sales repository; this read only exposes it to
    /// the receipt aggregate that owns the grouping.
    async fn list_allocations(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>>;
    /// DELETE is RESTRICTed by `sale_payments.receipt_id` once a payment references
    /// the receipt; that failure is surfaced as `Validation`, not a database error.
    async fn delete(&self, id: i64) -> AppResult<bool>;

    /// The RECEIPTS family of the documents index (documents-index): the stored
    /// receipt projected to the feed's facts, with its total summed in Rust over
    /// ONE batched allocation read delegated to the nested sales repository — this
    /// file never queries a sales table, not even for the total.
    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>>;
}

#[derive(Clone)]
pub struct SqliteCustomerReceiptRepository {
    pub pool: SqlitePool,
    /// Test-only read counter: proves the filtered list reads scale with the
    /// result set, not the shop's history. Absent from production builds.
    #[cfg(test)]
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The `sale_payments` read is delegated here so this file never queries a
    /// sales table.
    sales: SqliteSaleRepository,
}

impl SqliteCustomerReceiptRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            sales: SqliteSaleRepository::new(pool.clone()),
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
impl CustomerReceiptRepository for SqliteCustomerReceiptRepository {
    async fn create(&self, actor: i64, input: &NewReceipt) -> AppResult<CustomerReceipt> {
        let row = sqlx::query(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, notes, created_by)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at"#,
        )
        .bind(input.customer_id)
        .bind(input.account_id)
        .bind(input.method_id)
        .bind(input.date)
        .bind(input.notes.clone())
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_receipt(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<CustomerReceipt>> {
        let row = sqlx::query(
            r#"SELECT id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at
               FROM customer_receipts WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_receipt))
    }

    async fn list_by_customer(&self, customer_id: i64) -> AppResult<Vec<CustomerReceipt>> {
        let rows = sqlx::query(
            r#"SELECT id, customer_id, account_id, method_id, date, notes, created_by, updated_by, created_at
               FROM customer_receipts WHERE customer_id = ? ORDER BY date, id"#,
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_receipt).collect())
    }

    async fn list_allocations(&self, receipt_id: i64) -> AppResult<Vec<SalePayment>> {
        self.sales.list_payments_by_receipt(receipt_id).await
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM customer_receipts WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(format!(
                        "cannot delete receipt {id}: payments still reference it; the receipt documents what they paid"
                    ))
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }

    async fn list_document_rows(&self, query: &DocumentQuery) -> AppResult<Vec<DocumentRow>> {
        // `Some(empty)` matched no actor, so no document can match: answer
        // without querying, like every other `Some(empty)` id filter here.
        if let Some(ids) = &query.actor_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }
        // The read-only customers JOIN is deliberate and bounded: the row must
        // name the collected customer, one JOIN per family read beats N per-row
        // name lookups in the caller, and a read never moves write ownership —
        // this file still runs no SQL against a sales table, and the total
        // below comes from the nested sales repository.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT r.id, r.customer_id, r.date, r.notes, r.created_by, c.name AS customer_name FROM customer_receipts r JOIN customers c ON c.id = r.customer_id",
        );
        qb.push(" WHERE 1 = 1");
        if let Some(ids) = &query.actor_ids {
            qb.push(" AND r.created_by IN (");
            {
                let mut separated = qb.separated(", ");
                for id in ids {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
            }
        }
        if let Some(from) = query.from {
            qb.push(" AND r.date >= ").push_bind(from);
        }
        if let Some(to) = query.to {
            qb.push(" AND r.date <= ").push_bind(to);
        }
        if let Some(search) = &query.search {
            // The operator searches a receipt by the customer it collected
            // from and the note the handover carried; every nullable side is
            // COALESCEd so a NULL never drops the row from the OR chain.
            let needle = like_needle(search);
            qb.push(" AND (LOWER(COALESCE(c.name, '')) LIKE LOWER(")
                .push_bind(needle.clone())
                .push(") ESCAPE '\\' OR LOWER(COALESCE(r.notes, '')) LIKE LOWER(")
                .push_bind(needle)
                .push(") ESCAPE '\\')");
        }
        // Newest first: date descending, then id as the stable tiebreak.
        qb.push(" ORDER BY r.date DESC, r.id DESC LIMIT ")
            .push_bind(query.limit as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        #[cfg(test)]
        self.tick();
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // ONE batched allocation read for the whole page, delegated to the
        // sales repository that owns `sale_payments`: the query count stays
        // constant no matter how many receipts matched, and the total is
        // folded in Rust — never SQL SUM over TEXT money.
        let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
        let totals = self.sales.receipt_allocations(&ids).await?;
        #[cfg(test)]
        self.tick();

        Ok(rows
            .into_iter()
            .map(|row| {
                let id: i64 = row.get("id");
                let customer_id: i64 = row.get("customer_id");
                DocumentRow {
                    kind: DocumentKind::Receipt,
                    id,
                    // The drill-down target is the customer page, which lists
                    // the receipts; the row's own id stays in `id`.
                    owner_id: customer_id,
                    reference: format!("Recibo #{id}"),
                    party: row.get("customer_name"),
                    date: row.get("date"),
                    // The project's word for a collection.
                    detail: "Cobro".to_string(),
                    amount: Some(totals.get(&id).copied().unwrap_or(Decimal::ZERO)),
                    quantity: None,
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
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn receipt(customer_id: i64, account_id: i64, method_id: i64) -> NewReceipt {
        NewReceipt {
            customer_id,
            account_id,
            method_id,
            date: NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
            notes: None,
        }
    }

    /// FK failures surface as `Validation` and a referenced receipt cannot be
    /// deleted, so the collection service reports both without translating a raw
    /// database error itself.
    #[tokio::test]
    async fn fk_failures_are_validation_and_a_referenced_receipt_cannot_be_deleted() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let (customer_id,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (account_id,): (i64,) = sqlx::query_as(
            "INSERT INTO accounts (name, created_by) VALUES ('Caja', ?) RETURNING id",
        )
        .bind(test_support::audit_actor_id(&pool).await.unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        let (method_id,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(&pool)
                .await
                .unwrap();

        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let err = repo
            .create(actor, &receipt(999_999, account_id, method_id))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let stored = repo
            .create(actor, &receipt(customer_id, account_id, method_id))
            .await
            .unwrap();
        assert_eq!(repo.list_by_customer(customer_id).await.unwrap().len(), 1);

        let (sale_id,): (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_id, sale_date, created_by)\n             VALUES ('Confirmed', 'Credit', ?, '2024-06-01', ?) RETURNING id",
        )
        .bind(customer_id)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by)\n             VALUES (?, ?, ?, '10', '2024-06-01', ?, ?)",
        )
        .bind(sale_id)
        .bind(account_id)
        .bind(method_id)
        .bind(stored.id)
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();
        let allocations = repo.list_allocations(stored.id).await.unwrap();
        assert_eq!(allocations.len(), 1);
        // The receipt's amount is derived from these payments, not stored.
        let applied: Decimal = allocations.iter().map(|payment| payment.amount).sum();
        assert_eq!(applied, Decimal::from_str("10").unwrap());

        let err = repo.delete(stored.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("payments still reference it"),
            "got {err}"
        );
        assert!(repo.find_by_id(stored.id).await.unwrap().is_some());
    }

    // -- documents-index fixtures -------------------------------------------

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn account_and_method(pool: &SqlitePool, actor: i64) -> (i64, i64) {
        // One wallet per test database: the name is UNIQUE, so reuse it when a
        // second payment in the same test needs the pair.
        let account: i64 =
            match sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'doc wallet'")
                .fetch_optional(pool)
                .await
                .unwrap()
            {
                Some(id) => id,
                None => sqlx::query_scalar(
                    "INSERT INTO accounts (name, created_by) VALUES ('doc wallet', ?) RETURNING id",
                )
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap(),
            };
        let (method,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(pool)
                .await
                .unwrap();
        (account, method)
    }

    async fn seed_customer(pool: &SqlitePool, name: &str, actor: i64) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO customers (name, is_walkin, created_by) VALUES (?, 0, ?) RETURNING id",
        )
        .bind(name)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// One receipt through raw SQL (the projection tests seed shapes the
    /// repository API cannot build: arbitrary dates and actors).
    async fn seed_receipt(
        pool: &SqlitePool,
        customer_id: i64,
        date: NaiveDate,
        notes: Option<&str>,
        actor: i64,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, notes, created_by)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(customer_id)
        .bind(account)
        .bind(method)
        .bind(date)
        .bind(notes)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_sale(pool: &SqlitePool, customer_id: i64, date: NaiveDate, actor: i64) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
               VALUES ('Confirmed', 'Credit', ?, (SELECT name FROM customers WHERE id = ?), ?, ?)
               RETURNING id"#,
        )
        .bind(customer_id)
        .bind(customer_id)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_payment(
        pool: &SqlitePool,
        sale_id: i64,
        amount: &str,
        date: NaiveDate,
        actor: i64,
        receipt_id: Option<i64>,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(sale_id)
        .bind(account)
        .bind(method)
        .bind(amount)
        .bind(date)
        .bind(receipt_id)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The projection: `Recibo #id`, the collected customer, the `Cobro` pill,
    /// the exact Rust-summed allocation total, no quantity, the actor — and
    /// the drill-down owner is the customer, not the receipt.
    #[tokio::test]
    async fn receipt_document_rows_project_the_feed_facts() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();

        let perez = seed_customer(&pool, "Pérez", actor).await;
        let diaz = seed_customer(&pool, "Díaz", actor).await;
        let with_notes = seed_receipt(&pool, perez, d(2024, 6, 2), Some("seña"), actor).await;
        let sale = seed_sale(&pool, perez, d(2024, 6, 1), actor).await;
        seed_payment(&pool, sale, "10", d(2024, 6, 2), actor, Some(with_notes)).await;
        seed_payment(&pool, sale, "2.5", d(2024, 6, 2), actor, Some(with_notes)).await; // Σ = 12.5
        let plain = seed_receipt(&pool, diaz, d(2024, 6, 3), None, actor).await;
        let other_sale = seed_sale(&pool, diaz, d(2024, 6, 1), actor).await;
        seed_payment(&pool, other_sale, "7", d(2024, 6, 3), actor, Some(plain)).await;

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

        let plain_row = rows
            .iter()
            .find(|r| r.id == plain)
            .expect("plain receipt row");
        assert_eq!(plain_row.kind, DocumentKind::Receipt);
        assert_eq!(plain_row.reference, format!("Recibo #{plain}"));
        assert_eq!(plain_row.party, "Díaz");
        assert_eq!(plain_row.date, d(2024, 6, 3));
        assert_eq!(plain_row.detail, "Cobro");
        assert_eq!(
            plain_row.owner_id, diaz,
            "the drill-down target is the customer"
        );
        assert_eq!(plain_row.amount, Some(dec("7")));
        assert_eq!(plain_row.quantity, None);
        assert_eq!(plain_row.created_by, actor);

        let noted_row = rows
            .iter()
            .find(|r| r.id == with_notes)
            .expect("receipt with notes row");
        assert_eq!(noted_row.party, "Pérez");
        assert_eq!(
            noted_row.amount,
            Some(dec("12.5")),
            "the total is the summed allocations"
        );
    }

    /// The audit-actor filter narrows the family read to the given ids, and
    /// `Some(empty)` matches nothing without touching the database.
    #[tokio::test]
    async fn receipt_document_rows_filter_by_actor_and_empty_actor_set() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let sistema = test_support::audit_actor_id(&pool).await.unwrap();
        let other = test_support::seed_audit_user(&pool, "receipt-actor-2", "Receipt Actor 2")
            .await
            .unwrap();
        assert_ne!(sistema, other);

        let perez = seed_customer(&pool, "Pérez", sistema).await;
        let diaz = seed_customer(&pool, "Díaz", other).await;
        let mine = seed_receipt(&pool, perez, d(2024, 6, 2), None, sistema).await;
        let theirs = seed_receipt(&pool, diaz, d(2024, 6, 3), None, other).await;

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
    async fn receipt_document_rows_date_range_is_inclusive() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let customer = seed_customer(&pool, "Pérez", actor).await;
        let early = seed_receipt(&pool, customer, d(2024, 6, 1), None, actor).await;
        let first = seed_receipt(&pool, customer, d(2024, 6, 2), None, actor).await;
        let last = seed_receipt(&pool, customer, d(2024, 6, 4), None, actor).await;
        let late = seed_receipt(&pool, customer, d(2024, 6, 5), None, actor).await;

        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: Some(d(2024, 6, 2)),
                to: Some(d(2024, 6, 4)),
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

    /// The search matches the collected customer and the handover notes
    /// partially and case-insensitively; matching nothing is empty, never an
    /// error.
    #[tokio::test]
    async fn receipt_document_rows_search_customer_and_notes() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let gonzalez = seed_customer(&pool, "González", actor).await;
        let perez = seed_customer(&pool, "Pérez", actor).await;
        let for_gonzalez = seed_receipt(&pool, gonzalez, d(2024, 6, 2), None, actor).await;
        let with_notes = seed_receipt(&pool, perez, d(2024, 6, 3), Some("Seña 50"), actor).await;

        // Partial, case-insensitive customer match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("gonzález".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, for_gonzalez);

        // Notes match.
        let rows = repo
            .list_document_rows(&DocumentQuery {
                actor_ids: None,
                from: None,
                to: None,
                search: Some("seña".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, with_notes);

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
    async fn receipt_document_rows_limit_returns_newest_first() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let customer = seed_customer(&pool, "Pérez", actor).await;
        let a = seed_receipt(&pool, customer, d(2024, 6, 1), None, actor).await;
        let b = seed_receipt(&pool, customer, d(2024, 6, 2), None, actor).await;
        let c = seed_receipt(&pool, customer, d(2024, 6, 2), None, actor).await;

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

    /// The bounded-reads contract: 20 receipts cost the receipt repository
    /// exactly two queries (the rows query plus ONE batched allocation read)
    /// and the nested sales repository exactly one (the batched allocation
    /// read) — never one read per receipt.
    #[tokio::test]
    async fn receipt_document_rows_read_count_stays_bounded() {
        let pool = memory_pool().await;
        let repo = SqliteCustomerReceiptRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let customer = seed_customer(&pool, "Bulk", actor).await;
        let sale = seed_sale(&pool, customer, d(2024, 6, 1), actor).await;
        for _ in 1..=20 {
            let receipt = seed_receipt(&pool, customer, d(2024, 6, 1), None, actor).await;
            seed_payment(&pool, sale, "1", d(2024, 6, 1), actor, Some(receipt)).await;
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
        assert_eq!(
            repo.read_count(),
            2,
            "rows query + one batched allocation read"
        );
        assert_eq!(
            repo.sales.read_count(),
            1,
            "the allocation sum is ONE batched read, not one per receipt"
        );

        // A filter matching nothing stops after the rows query: no allocation
        // batch, and the nested sales repository is not touched at all.
        repo.reset_reads();
        repo.sales.reset_reads();
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
        assert_eq!(
            repo.read_count(),
            1,
            "no allocation batch for an empty result"
        );
        assert_eq!(repo.sales.read_count(), 0);
    }
}
