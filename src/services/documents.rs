//! The cross-department documents index: composes the four families' reads,
//! merges them newest-first, caps the page and says when the cap cut.

use crate::error::AppResult;
use crate::models::{DocumentFilter, DocumentKind, DocumentRow, DOCUMENTS_PAGE_LIMIT};
use crate::repositories::{
    CustomerReceiptRepository, PurchaseRepository, SaleRepository, StockMovementRepository,
};

/// The index over the four repository families. Holds only read paths, so a
/// page render never touches a write surface.
pub struct DocumentService<SR, PR, RR, STR> {
    sales: SR,
    purchases: PR,
    receipts: RR,
    stock: STR,
}

/// One feed: the rows the page shows and whether the cap cut the history.
/// `truncated` is the honest part — a cap the page does not disclose is a
/// silent lie about the shop's documents.
#[derive(Debug, Clone)]
pub struct DocumentFeed {
    pub rows: Vec<DocumentRow>,
    pub truncated: bool,
    /// The cap the feed applied.
    pub limit: usize,
}

impl<SR, PR, RR, STR> DocumentService<SR, PR, RR, STR>
where
    SR: SaleRepository,
    PR: PurchaseRepository,
    RR: CustomerReceiptRepository,
    STR: StockMovementRepository,
{
    pub fn new(sales: SR, purchases: PR, receipts: RR, stock: STR) -> Self {
        Self {
            sales,
            purchases,
            receipts,
            stock,
        }
    }

    /// The feed for one filter. `filter.kinds` is the page's decision (already
    /// narrowed by the principal's permissions in the route); an empty list
    /// reads nothing and answers an empty feed.
    pub async fn list(&self, filter: &DocumentFilter) -> AppResult<DocumentFeed> {
        // The page's decision: no family selected means nothing to show, and
        // the feed is answered before any repository is touched.
        if filter.kinds.is_empty() {
            return Ok(DocumentFeed {
                rows: Vec::new(),
                truncated: false,
                limit: DOCUMENTS_PAGE_LIMIT,
            });
        }

        // The filter carries a kind list the page built by expanding the
        // selected options; a repeated kind would list every row of that
        // family twice, and the read order would otherwise depend on the
        // option order. Read each family in `DocumentKind::ALL` declaration
        // order, once.
        let mut families: Vec<DocumentKind> = Vec::new();
        for kind in DocumentKind::ALL {
            if filter.kinds.contains(kind) && !families.contains(kind) {
                families.push(*kind);
            }
        }

        // Each family is asked for `DOCUMENTS_PAGE_LIMIT + 1` rows: one extra
        // is how the service learns the history did not end — a family that
        // answers the full ask had at least one more row beyond the cap.
        let query = filter.query(DOCUMENTS_PAGE_LIMIT + 1);
        let mut rows: Vec<DocumentRow> = Vec::new();

        // The families are awaited sequentially: at most four local SQLite
        // reads, and sequential keeps the read bound observable — the same
        // discipline the repositories' test-only read counters exist for.
        for kind in &families {
            match kind {
                DocumentKind::Sale => rows.extend(self.sales.list_document_rows(&query).await?),
                DocumentKind::SalePayment => {
                    rows.extend(self.sales.list_payment_document_rows(&query).await?)
                }
                DocumentKind::Purchase => {
                    rows.extend(self.purchases.list_document_rows(&query).await?)
                }
                DocumentKind::PurchasePayment => {
                    rows.extend(self.purchases.list_payment_document_rows(&query).await?)
                }
                DocumentKind::StockMovement => {
                    rows.extend(self.stock.list_document_rows(&query).await?)
                }
                DocumentKind::Receipt => {
                    rows.extend(self.receipts.list_document_rows(&query).await?)
                }
            }
        }

        // Sort by (date, id, kind) descending: a total order, so the merge is
        // deterministic and two documents saved the same day never swap
        // between renders. The kind is the last tiebreak, and declaration
        // order is stable.
        rows.sort_by(|a, b| (b.date, b.id, b.kind).cmp(&(a.date, a.id, a.kind)));

        // The honest part: only a feed that actually had more rows than the
        // cap admits that the history was cut.
        let truncated = rows.len() > DOCUMENTS_PAGE_LIMIT;
        rows.truncate(DOCUMENTS_PAGE_LIMIT);
        Ok(DocumentFeed {
            rows,
            truncated,
            limit: DOCUMENTS_PAGE_LIMIT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::{
        SqliteCustomerReceiptRepository, SqlitePurchaseRepository, SqliteSaleRepository,
        SqliteStockMovementRepository,
    };
    use crate::security::test_support;
    use chrono::NaiveDate;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;

    type Svc = DocumentService<
        SqliteSaleRepository,
        SqlitePurchaseRepository,
        SqliteCustomerReceiptRepository,
        SqliteStockMovementRepository,
    >;

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

    async fn svc(pool: &SqlitePool) -> Svc {
        DocumentService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqlitePurchaseRepository::new(pool.clone()),
            SqliteCustomerReceiptRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
        )
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn filt(kinds: Vec<DocumentKind>) -> DocumentFilter {
        DocumentFilter {
            kinds,
            actor_ids: None,
            from: None,
            to: None,
            search: None,
        }
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

    async fn seed_supplier(pool: &SqlitePool, name: &str, actor: i64) -> i64 {
        match sqlx::query_scalar("SELECT id FROM suppliers WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO suppliers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
                )
                .bind(name)
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap()
            }
        }
    }

    async fn seed_product(pool: &SqlitePool, actor: i64) -> i64 {
        // The tests seed several lines and movements; one product row per
        // database is enough, and the sku is UNIQUE so reuse it.
        match sqlx::query_scalar("SELECT id FROM products WHERE sku = 'DOC-P'")
            .fetch_optional(pool)
            .await
            .unwrap()
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    r#"INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
                       VALUES ('DOC-P', 'doc prod', 'Product', 'un', '10', 1, ?)
                       RETURNING id"#,
                )
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap()
            }
        }
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
                None => {
                    sqlx::query_scalar(
                        "INSERT INTO accounts (name, created_by) VALUES ('doc wallet', ?) RETURNING id",
                    )
                    .bind(actor)
                    .fetch_one(pool)
                    .await
                    .unwrap()
                }
            };
        let (method,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(pool)
                .await
                .unwrap();
        (account, method)
    }

    /// One confirmed sale with one line, through raw SQL: the composure tests
    /// seed shapes the repository API cannot build (arbitrary dates, actors).
    async fn seed_sale(
        pool: &SqlitePool,
        customer: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let walkin: i64 = sqlx::query_scalar("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(pool)
            .await
            .unwrap();
        let sale_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
               VALUES ('Confirmed', 'Credit', ?, ?, ?, ?)
               RETURNING id"#,
        )
        .bind(walkin)
        .bind(customer)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        let product = seed_product(pool, actor).await;
        sqlx::query("INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, '1', '10')")
            .bind(sale_id)
            .bind(product)
            .execute(pool)
            .await
            .unwrap();
        sale_id
    }

    async fn seed_sale_payment(
        pool: &SqlitePool,
        sale_id: i64,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by)
               VALUES (?, ?, ?, '3', ?, ?)
               RETURNING id"#,
        )
        .bind(sale_id)
        .bind(account)
        .bind(method)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_purchase(
        pool: &SqlitePool,
        supplier: &str,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let supplier_id = seed_supplier(pool, supplier, actor).await;
        let purchase_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO purchases (purchase_number, supplier_id, status, payment_type, purchase_date, created_by)
               VALUES (NULL, ?, 'Confirmed', 'Cash', ?, ?)
               RETURNING id"#,
        )
        .bind(supplier_id)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap();
        let product = seed_product(pool, actor).await;
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, '1', '4')",
        )
        .bind(purchase_id)
        .bind(product)
        .execute(pool)
        .await
        .unwrap();
        purchase_id
    }

    async fn seed_purchase_payment(
        pool: &SqlitePool,
        purchase_id: i64,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, created_by)
               VALUES (?, ?, ?, '2', ?, ?)
               RETURNING id"#,
        )
        .bind(purchase_id)
        .bind(account)
        .bind(method)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_movement(
        pool: &SqlitePool,
        product_id: i64,
        date: NaiveDate,
        actor: i64,
    ) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO stock_movements (product_id, qty, type, reason, reference, date, created_by)
               VALUES (?, '1', 'In', 'Purchase', 'MOV-REF', ?, ?)
               RETURNING id"#,
        )
        .bind(product_id)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_receipt(pool: &SqlitePool, customer_id: i64, date: NaiveDate, actor: i64) -> i64 {
        let (account, method) = account_and_method(pool, actor).await;
        sqlx::query_scalar(
            r#"INSERT INTO customer_receipts (customer_id, account_id, method_id, date, notes, created_by)
               VALUES (?, ?, ?, ?, NULL, ?)
               RETURNING id"#,
        )
        .bind(customer_id)
        .bind(account)
        .bind(method)
        .bind(date)
        .bind(actor)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// One document of every family, each on its own date, all by one actor:
    /// the merge test's stage. Dates: sale 05-01, sale payment 05-02,
    /// purchase 05-03, purchase payment 05-04, movement 05-05, receipt 05-06.
    async fn seed_one_of_each(pool: &SqlitePool, actor: i64) {
        let customer = seed_customer(pool, "Cliente Recibo", actor).await;
        let sale = seed_sale(pool, "Pérez", d(2024, 5, 1), actor).await;
        seed_sale_payment(pool, sale, d(2024, 5, 2), actor).await;
        let purchase = seed_purchase(pool, "Distribuidora Sur", d(2024, 5, 3), actor).await;
        seed_purchase_payment(pool, purchase, d(2024, 5, 4), actor).await;
        let product = seed_product(pool, actor).await;
        seed_movement(pool, product, d(2024, 5, 5), actor).await;
        seed_receipt(pool, customer, d(2024, 5, 6), actor).await;
    }

    /// The merge: one row per family, newest first ACROSS families, every
    /// kind represented exactly once.
    #[tokio::test]
    async fn document_feed_merges_families_newest_first() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        seed_one_of_each(&pool, actor).await;
        let s = svc(&pool).await;

        let feed = s.list(&filt(DocumentKind::ALL.to_vec())).await.unwrap();
        assert!(!feed.truncated, "six rows sit far below the cap");
        assert_eq!(feed.limit, DOCUMENTS_PAGE_LIMIT);
        let kinds: Vec<DocumentKind> = feed.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DocumentKind::Receipt,
                DocumentKind::StockMovement,
                DocumentKind::PurchasePayment,
                DocumentKind::Purchase,
                DocumentKind::SalePayment,
                DocumentKind::Sale,
            ],
            "the feed is newest-first across families"
        );
        // Every kind the seed produced is represented.
        for kind in DocumentKind::ALL {
            assert!(kinds.contains(kind), "{kind:?} missing from the feed");
        }
    }

    /// A filter naming only some families returns only those, and a family
    /// named twice is read once — a duplicated family would list every row
    /// twice.
    #[tokio::test]
    async fn document_feed_narrows_by_kind_and_deduplicates() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        seed_one_of_each(&pool, actor).await;
        let s = svc(&pool).await;

        let narrowed = s
            .list(&filt(vec![
                DocumentKind::Sale,
                DocumentKind::StockMovement,
            ]))
            .await
            .unwrap();
        let kinds: Vec<DocumentKind> = narrowed.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![DocumentKind::StockMovement, DocumentKind::Sale],
            "only the named families, newest first"
        );

        // The same family twice (two overlapping groups): each row once.
        let duplicated = s
            .list(&filt(vec![
                DocumentKind::Sale,
                DocumentKind::SalePayment,
                DocumentKind::Sale,
            ]))
            .await
            .unwrap();
        let kinds: Vec<DocumentKind> = duplicated.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![DocumentKind::SalePayment, DocumentKind::Sale],
            "a family named twice must not duplicate its rows"
        );
    }

    /// An empty kind list reads nothing and answers an empty feed — proven by
    /// the repositories' read counters: no family query ran at all.
    #[tokio::test]
    async fn document_feed_with_empty_kinds_reads_nothing() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        seed_one_of_each(&pool, actor).await;

        // Keep handles so the counters stay observable while the service owns
        // its own clones (the repos share the counter through the Arc).
        let sales = SqliteSaleRepository::new(pool.clone());
        let purchases = SqlitePurchaseRepository::new(pool.clone());
        let receipts = SqliteCustomerReceiptRepository::new(pool.clone());
        let stock = SqliteStockMovementRepository::new(pool.clone());
        let s = DocumentService::new(
            sales.clone(),
            purchases.clone(),
            receipts.clone(),
            stock.clone(),
        );

        let feed = s.list(&filt(vec![])).await.unwrap();
        assert!(feed.rows.is_empty());
        assert!(!feed.truncated);
        assert_eq!(sales.read_count(), 0, "no sale read ran");
        assert_eq!(purchases.read_count(), 0, "no purchase read ran");
        assert_eq!(receipts.read_count(), 0, "no receipt read ran");
        assert_eq!(stock.read_count(), 0, "no stock read ran");
    }

    /// The cap: `DOCUMENTS_PAGE_LIMIT + 5` sales produce a truncated feed of
    /// exactly `DOCUMENTS_PAGE_LIMIT` newest rows; below the cap the feed
    /// says the history did not end, because it did not.
    #[tokio::test]
    async fn document_feed_truncates_at_the_page_cap() {
        let pool = memory_pool().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let walkin: i64 = sqlx::query_scalar("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let s = svc(&pool).await;

        // Below the cap: nothing was cut.
        for i in 1..=3 {
            seed_sale(&pool, &format!("Below #{i}"), d(2024, 5, 1), actor).await;
        }
        let feed = s.list(&filt(vec![DocumentKind::Sale])).await.unwrap();
        assert_eq!(feed.rows.len(), 3);
        assert!(!feed.truncated);

        // DOCUMENTS_PAGE_LIMIT + 5 rows, one transaction, direct SQL.
        let mut tx = pool.begin().await.unwrap();
        for i in 1..=(DOCUMENTS_PAGE_LIMIT + 5) {
            sqlx::query(
                r#"INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, created_by)
                   VALUES (?, 'Confirmed', 'Credit', ?, 'Cap', ?, ?)"#,
            )
            .bind(format!("2024-SALE-{i:06}"))
            .bind(walkin)
            .bind(d(2024, 5, 10))
            .bind(actor)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();

        let feed = s.list(&filt(vec![DocumentKind::Sale])).await.unwrap();
        assert!(feed.truncated, "the cap cut the history and the feed says so");
        assert_eq!(feed.rows.len(), DOCUMENTS_PAGE_LIMIT);
        assert_eq!(feed.limit, DOCUMENTS_PAGE_LIMIT);
        // Same date falls back to id descending: the last inserted row is the
        // newest one present, the first inserted one is beyond the cap.
        let (first_id,): (i64,) =
            sqlx::query_as("SELECT MIN(id) FROM sales WHERE customer_name = 'Cap'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (last_id,): (i64,) =
            sqlx::query_as("SELECT MAX(id) FROM sales WHERE customer_name = 'Cap'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            feed.rows.iter().any(|r| r.id == last_id),
            "the newest capped-row must be present"
        );
        assert!(
            !feed.rows.iter().any(|r| r.id == first_id),
            "the row beyond the cap must be absent"
        );
    }

    /// The filter pass-through: an actor filter and a date range narrow the
    /// merged feed — the service does not drop or re-add rows.
    #[tokio::test]
    async fn document_feed_passes_actor_and_date_filters_through() {
        let pool = memory_pool().await;
        let sistema = test_support::audit_actor_id(&pool).await.unwrap();
        let other = test_support::seed_audit_user(&pool, "feed-actor-2", "Feed Actor 2")
            .await
            .unwrap();

        // One document per family per actor: sistema's inside the range,
        // the other actor's outside it (and filtered by actor anyway).
        let customer_a = seed_customer(&pool, "Cliente A", sistema).await;
        let customer_b = seed_customer(&pool, "Cliente B", other).await;
        let sale_a = seed_sale(&pool, "Pérez", d(2024, 5, 2), sistema).await;
        seed_sale(&pool, "Ajena", d(2024, 5, 6), other).await;
        seed_sale_payment(&pool, sale_a, d(2024, 5, 2), sistema).await;
        let product = seed_product(&pool, sistema).await;
        seed_movement(&pool, product, d(2024, 5, 2), sistema).await;
        let other_product = seed_product(&pool, other).await;
        seed_movement(&pool, other_product, d(2024, 5, 6), other).await;
        let purchase_a = seed_purchase(&pool, "Distribuidora Sur", d(2024, 5, 4), sistema).await;
        seed_purchase(&pool, "Importadora Norte", d(2024, 5, 6), other).await;
        seed_purchase_payment(&pool, purchase_a, d(2024, 5, 4), sistema).await;
        seed_receipt(&pool, customer_a, d(2024, 5, 3), sistema).await;
        seed_receipt(&pool, customer_b, d(2024, 5, 6), other).await;

        let filter = DocumentFilter {
            kinds: DocumentKind::ALL.to_vec(),
            actor_ids: Some(vec![sistema]),
            from: Some(d(2024, 5, 2)),
            to: Some(d(2024, 5, 4)),
            search: None,
        };
        let s = svc(&pool).await;
        let feed = s.list(&filter).await.unwrap();
        assert_eq!(feed.rows.len(), 6, "one document per family survived");
        for row in &feed.rows {
            assert_eq!(row.created_by, sistema, "only the filtered actor's rows");
            assert!(
                row.date >= d(2024, 5, 2) && row.date <= d(2024, 5, 4),
                "only the filtered range's rows"
            );
        }
        // The other actor's documents were dropped, not re-added.
        assert!(!feed.rows.iter().any(|r| r.created_by == other));
        assert_eq!(
            feed.rows.iter().map(|r| r.kind).collect::<std::collections::BTreeSet<_>>(),
            DocumentKind::ALL.iter().copied().collect(),
            "every family kept its in-range document"
        );
    }
}
