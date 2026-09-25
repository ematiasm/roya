//! T1 (`tax-calculation-settings`): the immutable line-tax snapshot
//! persistence and the shared deterministic line-tax calculation contract.
//!
//! Two families of facts are covered here, both before any document flow uses
//! them:
//!
//! 1. The pure calculation contract — additive (never compounding) taxes, each
//!    contribution and the tax-inclusive total pinned to two decimals with
//!    half-up rounding, the same strategy the product-markup pricing slice
//!    established for derived money.
//! 2. The persistence — per-family snapshot tables that keep the tax's code,
//!    name, rate and calculated amount, a `tax_id` reference that backs the
//!    hard-delete safeguard with `ON DELETE RESTRICT`, and the `tax_total` the
//!    line carries. Net subtotal and the tax-inclusive total stay DERIVED
//!    (qty x unit price, and net + tax_total), so no line stores a total that
//!    could drift from its own quantity and price.
use std::str::FromStr;

use rust_decimal::Decimal;
use sqlx::{sqlite::SqliteConnectOptions, sqlite::SqlitePoolOptions, SqlitePool};

use crate::models::{NewLineTax, NewTax, SaleLineTax};
use crate::repositories::{
    ProductTaxRepository, PurchaseRepository, SaleRepository, SqliteProductTaxRepository,
    SqlitePurchaseRepository, SqliteSaleRepository, SqliteTaxRepository,
    SqliteTaxSnapshotRepository, TaxRepository, TaxSnapshotRepository,
};
use crate::services::line_taxes::{calculate_line_taxes, round_to_cents};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn pool() -> SqlitePool {
    // `foreign_keys(true)` is the production pragma (`db::base_connect_options`):
    // the RESTRICT and CASCADE assertions below are the schema's own behaviour
    // and are meaningless without it.
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// A database migrated up to (but NOT including) T1, so the T1 migration can be
/// applied over real pre-existing rows the way a live upgrade receives it.
async fn pre_t1_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for migration in sqlx::migrate!("./migrations").iter() {
        if migration.version >= 20240101000039 {
            continue;
        }
        sqlx::raw_sql(migration.sql.clone())
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

async fn sentinel(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = 'sistema'")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn tax(code: &str, rate: &str, is_active: bool) -> NewTax {
    NewTax {
        code: code.to_string(),
        name: format!("Tax {code}"),
        rate: dec(rate),
        is_active,
    }
}

async fn create_product(pool: &SqlitePool, sku: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, cost_price, track_stock, created_by)
         VALUES (?, ?, 'Product', 'un', '100', '0', 0, ?)
         RETURNING id",
    )
    .bind(sku)
    .bind(format!("Product {sku}"))
    .bind(sentinel(pool).await)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn create_supplier(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO suppliers (name, created_by) VALUES ('Snapshot supplier', ?) RETURNING id",
    )
    .bind(sentinel(pool).await)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A Draft sale header; the line fixtures below hang off it.
async fn create_sale(pool: &SqlitePool) -> i64 {
    let actor = sentinel(pool).await;
    let customer: i64 = sqlx::query_scalar(
        "INSERT INTO customers (name, created_by) VALUES ('Snapshot buyer', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
         VALUES ('Draft', 'Cash', ?, 'Snapshot buyer', '2024-05-01', ?) RETURNING id",
    )
    .bind(customer)
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A Draft purchase header; the line fixtures below hang off it.
async fn create_purchase(pool: &SqlitePool) -> i64 {
    let actor = sentinel(pool).await;
    sqlx::query_scalar(
        "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by)
         VALUES (?, 'Draft', 'Cash', '2024-05-01', ?) RETURNING id",
    )
    .bind(create_supplier(pool).await)
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Create each tax through the real repository, link it to the product, and
/// return the created rows: exactly the state a document line resolves its
/// taxes from.
async fn taxes_linked_to(
    pool: &SqlitePool,
    product: i64,
    specs: &[(&str, &str)],
) -> Vec<crate::models::Tax> {
    let actor = sentinel(pool).await;
    let taxes = SqliteTaxRepository::new(pool.clone());
    let links = SqliteProductTaxRepository::new(pool.clone());
    let mut created = Vec::with_capacity(specs.len());
    for (code, rate) in specs {
        let tax = taxes.create(actor, &tax(code, rate, true)).await.unwrap();
        links.link(actor, product, tax.id).await.unwrap();
        created.push(tax);
    }
    created
}

// ---------------------------------------------------------------------------
// The pure calculation contract
// ---------------------------------------------------------------------------

/// No linked tax means no tax money at all: the tax total is exactly zero and
/// the line total is the untouched net amount (never a re-scaled copy of it).
#[test]
fn tax_snapshot_calc_zero_taxes_keeps_the_net_total() {
    let calc = calculate_line_taxes(dec("123.456"), &[]);
    assert!(calc.taxes.is_empty());
    assert_eq!(calc.net_subtotal, dec("123.456"));
    assert_eq!(calc.tax_total, dec("0"));
    assert_eq!(calc.total, dec("123.46"));
}

/// The single-tax case pins the percentage contract: a 21% rate on a 100 net
/// contributes 21, and the final total is net + that contribution.
#[test]
fn tax_snapshot_calc_single_tax_contribution() {
    let tax = tax_value(1, "IVA", "21");
    let calc = calculate_line_taxes(dec("100"), &[tax]);
    assert_eq!(calc.taxes.len(), 1);
    assert_eq!(calc.taxes[0].amount, dec("21"));
    assert_eq!(calc.tax_total, dec("21"));
    assert_eq!(calc.total, dec("121"));
}

/// Taxes are ADDITIVE. 21% + 10% on a 100 net is 31, not the compounded 132.10
/// a sequential application would produce.
#[test]
fn tax_snapshot_calc_multiple_taxes_are_additive_not_compounded() {
    let calc = calculate_line_taxes(
        dec("100"),
        &[tax_value(1, "IVA", "21"), tax_value(2, "IIBB", "10")],
    );
    assert_eq!(calc.taxes[0].amount, dec("21"));
    assert_eq!(calc.taxes[1].amount, dec("10"));
    assert_eq!(calc.tax_total, dec("31"));
    assert_eq!(calc.total, dec("131"));
}

/// The snapshot facts are the immutable values the line will store: the tax's
/// id, code, name and rate as they were resolved, never a live join.
#[test]
fn tax_snapshot_calc_returns_the_immutable_tax_facts() {
    let calc = calculate_line_taxes(dec("50"), &[tax_value(7, "IVA", "21")]);
    let snapshot = &calc.taxes[0];
    assert_eq!(snapshot.tax_id, 7);
    assert_eq!(snapshot.code, "IVA");
    assert_eq!(snapshot.name, "Tax IVA");
    assert_eq!(snapshot.rate, dec("21"));
    assert_eq!(snapshot.amount, dec("10.5"));
}

/// Half-up at the midpoint: 1 x 0.5% is an exact 0.005, and money rounds it
/// AWAY from zero to 0.01 — the same `MidpointAwayFromZero` strategy the
/// product-markup derived price uses. A banker's rounding would answer 0.00.
#[test]
fn tax_snapshot_calc_rounds_a_contribution_half_up_at_the_midpoint() {
    let calc = calculate_line_taxes(dec("1"), &[tax_value(1, "T", "0.5")]);
    assert_eq!(calc.taxes[0].amount, dec("0.01"));
    assert_eq!(calc.tax_total, dec("0.01"));
    assert_eq!(calc.total, dec("1.01"));
}

/// The same midpoint rule applies to the tax-inclusive total, whose net part
/// can carry more than two decimals (a fractional quantity times a price).
#[test]
fn tax_snapshot_calc_rounds_the_total_half_up_at_the_midpoint() {
    let calc = calculate_line_taxes(dec("10.005"), &[tax_value(1, "IVA", "21")]);
    assert_eq!(calc.taxes[0].amount, dec("2.10"));
    assert_eq!(calc.tax_total, dec("2.10"));
    assert_eq!(calc.total, dec("12.11"));
}

/// The stored breakdown must add up to the stored tax total, or a tax
/// breakdown view would show contributions that do not reconcile.
#[test]
fn tax_snapshot_calc_contributions_reconcile_with_the_tax_total() {
    let calc = calculate_line_taxes(
        dec("33.33"),
        &[
            tax_value(1, "A", "21"),
            tax_value(2, "B", "5.5"),
            tax_value(3, "C", "1.1"),
        ],
    );
    let summed: Decimal = calc.taxes.iter().map(|t| t.amount).sum();
    assert_eq!(summed, calc.tax_total);
    assert_eq!(calc.tax_total, dec("9.20"));
    assert_eq!(calc.total, dec("42.53"));
}

/// A 0% tax is a real, linked, snapshotted tax that contributes nothing; the
/// snapshot row still exists so the document shows the fact it was linked.
#[test]
fn tax_snapshot_calc_zero_rate_tax_contributes_nothing() {
    let calc = calculate_line_taxes(dec("80"), &[tax_value(1, "EXENTO", "0")]);
    assert_eq!(calc.taxes.len(), 1);
    assert_eq!(calc.taxes[0].amount, dec("0"));
    assert_eq!(calc.tax_total, dec("0"));
    assert_eq!(calc.total, dec("80"));
}

/// The rounding helper is the single half-up money rule of this feature, and
/// the calculation contract routes every money it produces through it.
#[test]
fn tax_snapshot_calc_round_to_cents_is_half_up_away_from_zero() {
    assert_eq!(round_to_cents(dec("1.005")), dec("1.01"));
    assert_eq!(round_to_cents(dec("1.004")), dec("1"));
    assert_eq!(round_to_cents(dec("-1.005")), dec("-1.01"));
    assert_eq!(round_to_cents(dec("7")), dec("7"));
}

/// A calculated snapshot converts into the shape the persistence writes, field
/// for field, so what the document stores is exactly what the contract decided.
#[test]
fn tax_snapshot_calc_snapshot_converts_to_the_persistence_shape() {
    let calc = calculate_line_taxes(dec("100"), &[tax_value(7, "IVA", "21")]);
    let write: NewLineTax = NewLineTax::from(&calc.taxes[0]);
    assert_eq!(write.tax_id, 7);
    assert_eq!(write.code, "IVA");
    assert_eq!(write.name, "Tax IVA");
    assert_eq!(write.rate, dec("21"));
    assert_eq!(write.amount, dec("21"));
}

// ---------------------------------------------------------------------------
// Foreign keys around the snapshot tables
// ---------------------------------------------------------------------------

/// The sale-side RESTRICT backstop for the hard-delete safeguard: a tax
/// referenced by a sale snapshot cannot be deleted.
#[tokio::test]
async fn tax_snapshot_sale_tax_delete_is_restricted_by_a_line_snapshot() {
    let pool = pool().await;
    let product = create_product(&pool, "FK-1").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();

    let error = sqlx::query("DELETE FROM taxes WHERE id = ?")
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("FOREIGN KEY constraint failed"),
        "expected the RESTRICT backstop, got {error}"
    );
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

/// The sale-side CASCADE on a DRAFT line: a snapshot belongs to its line, so
/// deleting the line takes its breakdown with it instead of leaving rows that
/// reference a dead line. The tax itself survives: only the reference went away.
#[tokio::test]
async fn tax_snapshot_sale_line_delete_cascades_its_snapshots() {
    let pool = pool().await;
    let product = create_product(&pool, "FK-2").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();

    sales.delete_line(line.id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------
// The legacy (pre-T2) line writes
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The public contract: the legacy line writes ARE the tax-aware ones
// ---------------------------------------------------------------------------

/// The seam is closed: `create_line` — the method production code and every
/// other caller uses — is the tax-aware contract, not a tax-free shortcut
/// beside it. A product with a linked active tax gets a snapshot breakdown and
/// a matching aggregate through the ordinary public call, in both families.
#[tokio::test]
async fn tax_snapshot_legacy_public_create_is_the_tax_aware_contract() {
    let pool = pool().await;
    let product = create_product(&pool, "SEAM-1").await;
    taxes_linked_to(&pool, product, &[("IVA", "21")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;

    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(
        line.tax_total,
        dec("21"),
        "the public create must not persist tax_total 0 without a breakdown"
    );
    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    let read_back = snapshots.list_sale_line_taxes(line.id).await.unwrap();
    assert_eq!(
        read_back.len(),
        1,
        "the public create must snapshot the tax"
    );
    let summed: Decimal = read_back.iter().map(|s: &SaleLineTax| s.amount).sum();
    assert_eq!(summed, line.tax_total);

    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let pline = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(pline.tax_total, dec("21"));
    let psnapshots = snapshots.list_purchase_line_taxes(pline.id).await.unwrap();
    assert_eq!(psnapshots.len(), 1);
    let psummed: Decimal = psnapshots.iter().map(|s| s.amount).sum();
    assert_eq!(psummed, pline.tax_total);
}

/// The same closure on the edit path: the public `update_line` recomputes the
/// aggregate from the new quantity and price AND replaces the breakdown, so a
/// tax that stopped applying leaves the line instead of lingering in its total.
#[tokio::test]
async fn tax_snapshot_legacy_public_update_recomputes_and_replaces_the_breakdown() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "SEAM-2").await;
    let iva = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("21"));

    SqliteTaxRepository::new(pool.clone())
        .deactivate(actor, iva.id)
        .await
        .unwrap();
    let updated = sales
        .update_line(line.id, dec("2"), dec("50"))
        .await
        .unwrap();
    assert_eq!(updated.subtotal(), dec("100"));
    assert_eq!(
        updated.tax_total,
        dec("0"),
        "the stale 21% must not survive the edit"
    );
    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap();
    assert!(
        read_back.is_empty(),
        "the breakdown must be replaced, not kept"
    );

    // Purchase side, with a tax still active so the recompute is visible.
    let purchase_product = create_product(&pool, "SEAM-3").await;
    // `taxes.code` is UNIQUE across the catalog, so this purchase-side tax needs
    // its own code: the helper creates real tax rows, it does not stub them.
    taxes_linked_to(&pool, purchase_product, &[("IVA-P", "10")]).await;
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let pline = purchases
        .create_line(purchase, purchase_product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(pline.tax_total, dec("10"));
    let pupdated = purchases
        .update_line(pline.id, dec("3"), dec("100"))
        .await
        .unwrap();
    assert_eq!(pupdated.subtotal(), dec("300"));
    assert_eq!(pupdated.tax_total, dec("30"), "10% of the new net 300");
    let psnapshots = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_purchase_line_taxes(pline.id)
        .await
        .unwrap();
    let psummed: Decimal = psnapshots.iter().map(|s| s.amount).sum();
    assert_eq!(psummed, pupdated.tax_total);
}

/// A Confirmed line cannot be edited through the public method any more. Before
/// this closure `update_line` happily rewrote a closed document's quantity,
/// price and (tax-less) total; now the refusal is the same Conflict the
/// tax-aware path gives, and nothing about the line changes.
#[tokio::test]
async fn tax_snapshot_legacy_public_update_cannot_mutate_a_confirmed_line() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "SEAM-4").await;
    taxes_linked_to(&pool, product, &[("IVA", "21")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    sales
        .set_confirmed(sale, actor, "2024-SALE-000910")
        .await
        .unwrap();

    let error = sales
        .update_line(line.id, dec("9"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "the public update must refuse a confirmed line, got {error:?}"
    );
    let after = sales.find_line(line.id).await.unwrap().unwrap();
    assert_eq!(after.qty, line.qty);
    assert_eq!(after.unit_price, line.unit_price);
    assert_eq!(after.tax_total, line.tax_total);
    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].amount, dec("21"));

    // And a closed document cannot gain a new line through the public create.
    let error = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );

    // The purchase mirror of the same two refusals.
    let purchase_product = create_product(&pool, "SEAM-5").await;
    taxes_linked_to(&pool, purchase_product, &[("IVA-C", "21")]).await;
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let pline = purchases
        .create_line(purchase, purchase_product, dec("1"), dec("100"))
        .await
        .unwrap();
    purchases
        .set_confirmed(purchase, actor, "2024-PURCH-000910")
        .await
        .unwrap();
    let error = purchases
        .update_line(pline.id, dec("9"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );
    let error = purchases
        .create_line(purchase, purchase_product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );
    let pafter = purchases.find_line(pline.id).await.unwrap().unwrap();
    assert_eq!(pafter.tax_total, pline.tax_total);
    assert_eq!(pafter.qty, pline.qty);
    let psnapshots = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_purchase_line_taxes(pline.id)
        .await
        .unwrap();
    assert_eq!(psnapshots.len(), 1);
    assert_eq!(psnapshots[0].amount, dec("21"));
}

// ---------------------------------------------------------------------------
// The atomic line + breakdown + aggregate boundary
//
// Every case below goes through ONE repository call that resolves the product's
// active taxes, calculates them, and writes the line, its snapshot breakdown and
// its tax total inside a single transaction. There is deliberately no way to
// write the aggregate without its breakdown, or the breakdown without the
// aggregate.
// ---------------------------------------------------------------------------

/// The happy path: the resolved active taxes are calculated and persisted with
/// the line, and the stored aggregate is exactly the sum of the stored
/// contributions. A fractional quantity keeps the net at three decimals, so the
/// half-up rounding is part of what is being proven.
#[tokio::test]
async fn tax_snapshot_atomic_sale_line_persists_breakdown_and_total_together() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-1").await;
    // 21% and 2.5% are additive: 21 + 2.5 = 23.5% of the net.
    taxes_linked_to(&pool, product, &[("IVA", "21"), ("IIBB", "2.5")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;

    let line = sales
        .create_line(sale, product, dec("1.5"), dec("99.99"))
        .await
        .unwrap();
    // net 1.5 x 99.99 = 149.985; 21% is 31.49685 -> 31.50, 2.5% is 3.749625 -> 3.75.
    assert_eq!(line.subtotal(), dec("149.985"));
    assert_eq!(line.tax_total, dec("35.25"));
    assert_eq!(
        round_to_cents(line.subtotal() + line.tax_total),
        dec("185.24")
    );

    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    let read_back = snapshots.list_sale_line_taxes(line.id).await.unwrap();
    assert_eq!(read_back.len(), 2);
    // Resolution orders by code, so the stored breakdown is deterministic:
    // "IIBB" sorts before "IVA", not the order the taxes were created in.
    assert_eq!(read_back[0].code, "IIBB");
    assert_eq!(read_back[1].code, "IVA");
    let summed: Decimal = read_back.iter().map(|s: &SaleLineTax| s.amount).sum();
    assert_eq!(summed, line.tax_total, "the breakdown must reconcile");
    for snapshot in &read_back {
        assert_eq!(round_to_cents(snapshot.amount), snapshot.amount);
    }
}

/// The aggregate is stored as canonical TEXT that needs no re-formatting on
/// read: the aggregate and every contribution carry at most two decimals, and
/// a scale-carrying rate keeps its own precision instead of being re-scaled.
#[tokio::test]
async fn tax_snapshot_atomic_stored_aggregate_is_canonical_text() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-2").await;
    taxes_linked_to(&pool, product, &[("IVA", "21.50")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;

    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("21.50"));

    let stored: (String, String, String) = sqlx::query_as(
        "SELECT tax_total, rate, amount FROM sale_line_taxes s
         JOIN sale_lines l ON l.id = s.sale_line_id WHERE l.id = ?",
    )
    .bind(line.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.0, "21.50", "the aggregate keeps its own scale");
    assert_eq!(stored.1, "21.50", "the rate keeps its own scale");
    assert_eq!(stored.2, "21.50", "the contribution is a currency amount");
    for value in [&stored.0, &stored.2] {
        assert_eq!(
            round_to_cents(Decimal::from_str(value).unwrap()).to_string(),
            *value,
            "a stored money value must already be at two decimals"
        );
    }
}

/// FORCED FAILURE AFTER THE FIRST SNAPSHOT INSERT. A trigger aborts the second
/// insert, so the transaction has already written the line and one breakdown
/// row when it fails. Nothing may survive: a line without its breakdown, or a
/// breakdown without its aggregate, is exactly the corruption this boundary
/// exists to prevent.
#[tokio::test]
async fn tax_snapshot_atomic_sale_line_rolls_back_when_a_snapshot_insert_fails() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-3").await;
    // "AAA" is inserted first (resolution orders by code) and succeeds;
    // "BOOM" is second and aborts inside the transaction.
    taxes_linked_to(&pool, product, &[("AAA", "10"), ("BOOM", "5")]).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_boom_sale BEFORE INSERT ON sale_line_taxes
         WHEN NEW.tax_code = 'BOOM'
         BEGIN SELECT RAISE(ABORT, 'forced snapshot failure'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let error = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("forced snapshot failure"),
        "expected the forced failure to surface, got {error}"
    );

    let lines: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?")
        .bind(sale)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        lines, 0,
        "the line insert must roll back with the breakdown"
    );
    let snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(snapshots, 0, "no partial breakdown may survive");
}

/// FORCED MID-TRANSACTION FAILURE ON A DRAFT REPLACEMENT (sales). The line
/// starts with two snapshotted taxes; a third one is linked afterwards and the
/// trigger aborts on it, so the replacement deletes the old breakdown, re-writes
/// two rows and only then fails. Everything must be exactly as it was: the old
/// quantity, the old price, the old aggregate and the old breakdown. A partial
/// replacement would leave a line whose tax_total no longer matches its own
/// snapshots.
#[tokio::test]
async fn tax_snapshot_atomic_draft_replacement_rolls_back_completely() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ROLLBACK-SALE").await;
    taxes_linked_to(&pool, product, &[("AAA", "10"), ("BBB", "10")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("20"));
    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    let before = snapshots.list_sale_line_taxes(line.id).await.unwrap();
    assert_eq!(before.len(), 2);

    // A third tax sorts last, so it is the insert that fails, after the
    // replacement has already deleted and rewritten the two original rows.
    taxes_linked_to(&pool, product, &[("ZZZ", "5")]).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_zzz_sale BEFORE INSERT ON sale_line_taxes
         WHEN NEW.tax_code = 'ZZZ'
         BEGIN SELECT RAISE(ABORT, 'forced replacement failure'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let error = sales
        .update_line(line.id, dec("7"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("forced replacement failure"),
        "expected the forced failure to surface, got {error}"
    );

    let after = sales.find_line(line.id).await.unwrap().unwrap();
    assert_eq!(after.qty, dec("1"), "the old quantity must be restored");
    assert_eq!(
        after.unit_price,
        dec("100"),
        "the old price must be restored"
    );
    assert_eq!(
        after.tax_total,
        dec("20"),
        "the old aggregate must be restored"
    );
    let read_back = snapshots.list_sale_line_taxes(line.id).await.unwrap();
    assert_eq!(
        read_back.len(),
        2,
        "the old breakdown must be restored, not partially replaced"
    );
    assert_eq!(read_back[0].code, "AAA");
    assert_eq!(read_back[0].amount, dec("10"));
    assert_eq!(read_back[1].code, "BBB");
    assert_eq!(read_back[1].amount, dec("10"));
    assert!(
        !read_back.iter().any(|s| s.code == "ZZZ"),
        "the aborted tax must leave no snapshot behind"
    );
    let summed: Decimal = read_back.iter().map(|s: &SaleLineTax| s.amount).sum();
    assert_eq!(
        summed, after.tax_total,
        "the restored breakdown must reconcile"
    );
    let _ = actor;
}

/// FORCED MID-TRANSACTION FAILURE ON A DRAFT REPLACEMENT (purchases): the same
/// all-or-nothing guarantee, proven on the other family.
#[tokio::test]
async fn tax_snapshot_atomic_purchase_draft_replacement_rolls_back_completely() {
    let pool = pool().await;
    let product = create_product(&pool, "ROLLBACK-PURCH").await;
    taxes_linked_to(&pool, product, &[("AAA", "10"), ("BBB", "10")]).await;
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("20"));
    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    assert_eq!(
        snapshots
            .list_purchase_line_taxes(line.id)
            .await
            .unwrap()
            .len(),
        2
    );

    taxes_linked_to(&pool, product, &[("ZZZ", "5")]).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_zzz_purchase BEFORE INSERT ON purchase_line_taxes
         WHEN NEW.tax_code = 'ZZZ'
         BEGIN SELECT RAISE(ABORT, 'forced replacement failure'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let error = purchases
        .update_line(line.id, dec("7"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("forced replacement failure"),
        "expected the forced failure to surface, got {error}"
    );

    let after = purchases.find_line(line.id).await.unwrap().unwrap();
    assert_eq!(after.qty, dec("1"));
    assert_eq!(after.unit_cost, dec("100"));
    assert_eq!(after.tax_total, dec("20"));
    let read_back = snapshots.list_purchase_line_taxes(line.id).await.unwrap();
    assert_eq!(read_back.len(), 2);
    assert_eq!(read_back[0].code, "AAA");
    assert_eq!(read_back[0].amount, dec("10"));
    assert_eq!(read_back[1].code, "BBB");
    assert_eq!(read_back[1].amount, dec("10"));
    assert!(!read_back.iter().any(|s| s.code == "ZZZ"));
    let summed: Decimal = read_back.iter().map(|s| s.amount).sum();
    assert_eq!(summed, after.tax_total);
}

/// A Confirmed document is history: a new line cannot be attached to it, so the
/// taxes of a closed sale can never change.
#[tokio::test]
async fn tax_snapshot_atomic_sale_line_rejects_a_confirmed_document() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-4").await;
    taxes_linked_to(&pool, product, &[("IVA", "21")]).await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    sales
        .set_confirmed(sale, sentinel(&pool).await, "2024-SALE-000901")
        .await
        .unwrap();

    let error = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "a confirmed sale must refuse a new line, got {error:?}"
    );
    let lines: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?")
        .bind(sale)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lines, 0);
}

/// Editing a DRAFT line is the ordinary case the boundary must support: the
/// breakdown is REPLACED, not appended to, and the aggregate is recomputed from
/// the new quantity, the new price and the taxes that are active NOW. Here a
/// tax is deactivated between the two writes, so its snapshot must disappear
/// from the line instead of surviving as stale history.
#[tokio::test]
async fn tax_snapshot_atomic_draft_line_replace_swaps_breakdown_and_total() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ATOM-5").await;
    let taxes = SqliteTaxRepository::new(pool.clone());
    let iva = taxes.create(actor, &tax("IVA", "21", true)).await.unwrap();
    let ibb = taxes
        .create(actor, &tax("IIBB", "2.5", true))
        .await
        .unwrap();
    let links = SqliteProductTaxRepository::new(pool.clone());
    links.link(actor, product, iva.id).await.unwrap();
    links.link(actor, product, ibb.id).await.unwrap();

    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("23.50"));
    assert_eq!(
        SqliteTaxSnapshotRepository::new(pool.clone())
            .list_sale_line_taxes(line.id)
            .await
            .unwrap()
            .len(),
        2
    );

    // The operator edits the draft and IIBB stops applying in the meantime.
    taxes.deactivate(actor, ibb.id).await.unwrap();
    let replaced = sales
        .update_line(line.id, dec("2"), dec("50"))
        .await
        .unwrap();
    assert_eq!(replaced.subtotal(), dec("100"));
    assert_eq!(replaced.tax_total, dec("21"), "only the active tax remains");
    assert_eq!(
        round_to_cents(replaced.subtotal() + replaced.tax_total),
        dec("121")
    );

    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap();
    assert_eq!(
        read_back.len(),
        1,
        "a replaced breakdown keeps no stale row"
    );
    assert_eq!(read_back[0].code, "IVA");
    assert_eq!(read_back[0].amount, dec("21"));
    let summed: Decimal = read_back.iter().map(|s: &SaleLineTax| s.amount).sum();
    assert_eq!(summed, replaced.tax_total);
}

/// Immutability, proven two ways on a Confirmed sale: the line cannot be
/// edited, and re-rating the tax afterwards cannot move the money the line
/// already recorded.
#[tokio::test]
async fn tax_snapshot_atomic_confirmed_line_replace_is_refused_and_changes_nothing() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ATOM-6").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    let before = line.clone();
    sales
        .set_confirmed(sale, actor, "2024-SALE-000902")
        .await
        .unwrap();

    // The catalog moves on after the document closed: doubled rate, new name.
    SqliteTaxRepository::new(pool.clone())
        .update(
            actor,
            created.id,
            "IVA-NUEVO",
            "Renamed tax",
            dec("50"),
            true,
        )
        .await
        .unwrap();

    let error = sales
        .update_line(line.id, dec("9"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "a confirmed line must refuse the change, got {error:?}"
    );

    let after = sales.find_line(line.id).await.unwrap().unwrap();
    assert_eq!(after.qty, before.qty);
    assert_eq!(after.unit_price, before.unit_price);
    assert_eq!(after.tax_total, before.tax_total);
    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(
        read_back[0].amount,
        dec("21"),
        "the frozen amount is untouched by the later re-rate"
    );
    assert_eq!(read_back[0].rate, dec("21"), "the frozen rate is untouched");
    assert_eq!(read_back[0].code, "IVA", "the frozen code is untouched");
}

/// A line that cannot exist is reported as missing, never as a silent no-op: a
/// caller must never believe a line was written to an unknown document.
#[tokio::test]
async fn tax_snapshot_atomic_create_on_an_unknown_document_is_not_found() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-7").await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let error = sales
        .create_line(999_999, product, dec("1"), dec("10"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::NotFound(_)),
        "got {error:?}"
    );

    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let error = purchases
        .create_line(999_999, product, dec("1"), dec("10"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::NotFound(_)),
        "got {error:?}"
    );
}

/// Replacing a line that does not exist is NotFound, and replacing one whose
/// document is not a draft is a Conflict: the two refusals are distinguishable
/// so the caller can report the right thing.
#[tokio::test]
async fn tax_snapshot_atomic_replace_distinguishes_missing_from_immutable() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-8").await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let error = sales
        .update_line(999_999, dec("1"), dec("10"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::NotFound(_)),
        "got {error:?}"
    );

    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("10"))
        .await
        .unwrap();
    sales
        .set_confirmed(sale, sentinel(&pool).await, "2024-SALE-000903")
        .await
        .unwrap();
    let error = sales
        .update_line(line.id, dec("2"), dec("10"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );
}

/// A product with no linked tax is an ordinary line: no breakdown rows, a zero
/// aggregate, and a tax-inclusive total equal to the net amount.
#[tokio::test]
async fn tax_snapshot_atomic_line_without_taxes_stores_no_breakdown() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-9").await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("3"), dec("10"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("0"));
    assert_eq!(round_to_cents(line.subtotal() + line.tax_total), dec("30"));
    assert!(SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap()
        .is_empty());
}

/// The point of a snapshot, through the real write path: editing and
/// deactivating the tax later does not rewrite what the line recorded.
#[tokio::test]
async fn tax_snapshot_atomic_line_snapshot_survives_a_later_tax_edit() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ATOM-10").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();

    let taxes = SqliteTaxRepository::new(pool.clone());
    taxes
        .update(
            actor,
            created.id,
            "IVA-NUEVO",
            "Renamed tax",
            dec("27.5"),
            false,
        )
        .await
        .unwrap();

    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].code, "IVA");
    assert_eq!(read_back[0].name, "Tax IVA");
    assert_eq!(read_back[0].rate, dec("21"));
    assert_eq!(read_back[0].amount, dec("21"));
    // And the aggregate still agrees with the frozen breakdown.
    assert_eq!(
        sales.find_line(line.id).await.unwrap().unwrap().tax_total,
        dec("21")
    );
}

/// DELETION BOUNDARY (sales). A DRAFT line and its breakdown both go; the tax
/// survives because only the reference went away. This is the ordinary case the
/// boundary must keep working.
#[tokio::test]
async fn tax_snapshot_public_delete_removes_a_draft_line_and_its_breakdown() {
    let pool = pool().await;
    let product = create_product(&pool, "DEL-1").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21"), ("IIBB", "2.5")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("2"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("47"));
    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    assert_eq!(
        snapshots.list_sale_line_taxes(line.id).await.unwrap().len(),
        2
    );

    sales.delete_line(line.id).await.unwrap();
    assert!(sales.find_line(line.id).await.unwrap().is_none());
    assert!(sales.list_lines(sale).await.unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "the breakdown must CASCADE with the line");
    assert!(SqliteTaxSnapshotRepository::new(pool.clone())
        .list_sale_line_taxes(line.id)
        .await
        .unwrap()
        .is_empty());
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

/// DELETION BOUNDARY (purchases): the same DRAFT behaviour, same CASCADE.
#[tokio::test]
async fn tax_snapshot_public_delete_removes_a_draft_purchase_line() {
    let pool = pool().await;
    let product = create_product(&pool, "DEL-2").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(line.tax_total, dec("21"));

    purchases.delete_line(line.id).await.unwrap();
    assert!(purchases.find_line(line.id).await.unwrap().is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

/// A Confirmed line cannot be deleted through the public method, and the
/// refusal changes NOTHING: the line keeps its quantity, price and aggregate,
/// and every snapshot row is byte-for-byte what it was — same ids, same frozen
/// code, name, rate and amount. Deleting it would cascade away the immutable
/// record of what the document actually charged.
#[tokio::test]
async fn tax_snapshot_public_delete_refuses_a_confirmed_sale_line() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "DEL-3").await;
    let iva = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let sales = SqliteSaleRepository::new(pool.clone());
    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("3"), dec("19.99"))
        .await
        .unwrap();
    // The catalog moves on: re-rated and renamed, so a re-read would differ.
    SqliteTaxRepository::new(pool.clone())
        .update(actor, iva.id, "IVA-NUEVO", "Renamed tax", dec("30"), false)
        .await
        .unwrap();
    sales
        .set_confirmed(sale, actor, "2024-SALE-000920")
        .await
        .unwrap();

    // The exact stored facts, read as raw TEXT before the attempt.
    let before_line: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_price, tax_total FROM sale_lines WHERE id = ?")
            .bind(line.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let before_rows: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, tax_id, tax_code, tax_name, rate, amount
         FROM sale_line_taxes WHERE sale_line_id = ? ORDER BY id",
    )
    .bind(line.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(before_rows.len(), 1);
    assert_eq!(
        before_line.2, "12.59",
        "21% of the net 59.97 pinned to cents"
    );

    let error = sales.delete_line(line.id).await.unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "a confirmed line must refuse deletion, got {error:?}"
    );

    let after_line: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_price, tax_total FROM sale_lines WHERE id = ?")
            .bind(line.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after_line, before_line, "the line must be untouched");
    let after_rows: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, tax_id, tax_code, tax_name, rate, amount
         FROM sale_line_taxes WHERE sale_line_id = ? ORDER BY id",
    )
    .bind(line.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        after_rows, before_rows,
        "every snapshot row must be byte-for-byte unchanged"
    );
}

/// The purchase mirror of the same refusal, with the same byte-for-byte proof
/// and a multi-tax breakdown.
#[tokio::test]
async fn tax_snapshot_public_delete_refuses_a_confirmed_purchase_line() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "DEL-4").await;
    taxes_linked_to(&pool, product, &[("IVA", "21"), ("IIBB", "2.5")]).await;
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("2"), dec("50"))
        .await
        .unwrap();
    purchases
        .set_confirmed(purchase, actor, "2024-PURCH-000920")
        .await
        .unwrap();

    let before_line: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_cost, tax_total FROM purchase_lines WHERE id = ?")
            .bind(line.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let before_rows: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, tax_id, tax_code, tax_name, rate, amount
         FROM purchase_line_taxes WHERE purchase_line_id = ? ORDER BY id",
    )
    .bind(line.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(before_rows.len(), 2);
    // The canonical encoding is `Decimal::to_string()`: scale is not part of
    // the value, so 23.50 is stored as "23.5" exactly as every other money
    // column in this project is written.
    assert_eq!(before_line.2, "23.5");

    let error = purchases.delete_line(line.id).await.unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "a confirmed line must refuse deletion, got {error:?}"
    );

    let after_line: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_cost, tax_total FROM purchase_lines WHERE id = ?")
            .bind(line.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after_line, before_line);
    let after_rows: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, tax_id, tax_code, tax_name, rate, amount
         FROM purchase_line_taxes WHERE purchase_line_id = ? ORDER BY id",
    )
    .bind(line.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        after_rows, before_rows,
        "every snapshot row must be byte-for-byte unchanged"
    );
}

/// The two refusals stay distinguishable, exactly as they do on create and
/// update: a line that does not exist is `NotFound`, a line on a closed
/// document is a `Conflict` the caller can explain.
#[tokio::test]
async fn tax_snapshot_public_delete_distinguishes_missing_from_immutable() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "DEL-5").await;
    let sales = SqliteSaleRepository::new(pool.clone());
    let error = sales.delete_line(999_999).await.unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::NotFound(_)),
        "got {error:?}"
    );

    // No product is needed: a line id that does not exist is refused before
    // anything about the catalog is read.
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let error = purchases.delete_line(999_999).await.unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::NotFound(_)),
        "got {error:?}"
    );

    let sale = create_sale(&pool).await;
    let line = sales
        .create_line(sale, product, dec("1"), dec("10"))
        .await
        .unwrap();
    sales
        .set_confirmed(sale, actor, "2024-SALE-000921")
        .await
        .unwrap();
    let error = sales.delete_line(line.id).await.unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Purchases: the same boundary, and the same foreign keys
// ---------------------------------------------------------------------------

/// Purchases get the same atomic write and the same replacement semantics as
/// sales: one call, one transaction, breakdown and aggregate together.
#[tokio::test]
async fn tax_snapshot_atomic_purchase_line_persists_and_replaces_breakdown() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ATOM-11").await;
    let taxes = SqliteTaxRepository::new(pool.clone());
    let iva = taxes.create(actor, &tax("IVA", "21", true)).await.unwrap();
    let ibb = taxes
        .create(actor, &tax("IIBB", "2.5", true))
        .await
        .unwrap();
    let links = SqliteProductTaxRepository::new(pool.clone());
    links.link(actor, product, iva.id).await.unwrap();
    links.link(actor, product, ibb.id).await.unwrap();

    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("2"), dec("50"))
        .await
        .unwrap();
    assert_eq!(line.subtotal(), dec("100"));
    assert_eq!(line.tax_total, dec("23.50"));
    let snapshots = SqliteTaxSnapshotRepository::new(pool.clone());
    assert_eq!(
        snapshots
            .list_purchase_line_taxes(line.id)
            .await
            .unwrap()
            .len(),
        2
    );

    taxes.deactivate(actor, ibb.id).await.unwrap();
    let replaced = purchases
        .update_line(line.id, dec("4"), dec("25"))
        .await
        .unwrap();
    assert_eq!(replaced.subtotal(), dec("100"));
    assert_eq!(replaced.tax_total, dec("21"));
    let read_back = snapshots.list_purchase_line_taxes(line.id).await.unwrap();
    assert_eq!(
        read_back.len(),
        1,
        "a replaced breakdown keeps no stale row"
    );
    let summed: Decimal = read_back.iter().map(|s| s.amount).sum();
    assert_eq!(summed, replaced.tax_total);
}

/// FORCED FAILURE on the purchase side: same all-or-nothing guarantee, same
/// rollback, and the purchase line itself must not survive.
#[tokio::test]
async fn tax_snapshot_atomic_purchase_line_rolls_back_when_a_snapshot_insert_fails() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-12").await;
    taxes_linked_to(&pool, product, &[("AAA", "10"), ("BOOM", "5")]).await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_boom_purchase BEFORE INSERT ON purchase_line_taxes
         WHEN NEW.tax_code = 'BOOM'
         BEGIN SELECT RAISE(ABORT, 'forced snapshot failure'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let error = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("forced snapshot failure"),
        "expected the forced failure to surface, got {error}"
    );
    let lines: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM purchase_lines WHERE purchase_id = ?")
            .bind(purchase)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(lines, 0);
    let snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(snapshots, 0);
}

/// A Confirmed purchase is immutable in the same way a confirmed sale is.
#[tokio::test]
async fn tax_snapshot_atomic_confirmed_purchase_line_is_immutable() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "ATOM-13").await;
    taxes_linked_to(&pool, product, &[("IVA", "21")]).await;
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();
    let before = line.clone();
    purchases
        .set_confirmed(purchase, actor, "2024-PURCH-000901")
        .await
        .unwrap();

    let error = purchases
        .update_line(line.id, dec("9"), dec("999"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );
    let error = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::AppError::Conflict(_)),
        "got {error:?}"
    );

    let after = purchases.find_line(line.id).await.unwrap().unwrap();
    assert_eq!(after.qty, before.qty);
    assert_eq!(after.unit_cost, before.unit_cost);
    assert_eq!(after.tax_total, before.tax_total);
    let read_back = SqliteTaxSnapshotRepository::new(pool.clone())
        .list_purchase_line_taxes(line.id)
        .await
        .unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].amount, dec("21"));
}

/// The purchase-side RESTRICT backstop for the hard-delete safeguard: a tax
/// referenced by a purchase snapshot cannot be deleted.
#[tokio::test]
async fn tax_snapshot_purchase_tax_delete_is_restricted_by_a_line_snapshot() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-14").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();

    let error = sqlx::query("DELETE FROM taxes WHERE id = ?")
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("FOREIGN KEY constraint failed"),
        "expected the RESTRICT backstop, got {error}"
    );
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

/// The purchase-side CASCADE on a DRAFT line: removing a line takes its
/// breakdown with it, and the tax survives because only the reference went away.
#[tokio::test]
async fn tax_snapshot_purchase_line_delete_cascades_its_snapshots() {
    let pool = pool().await;
    let product = create_product(&pool, "ATOM-15").await;
    let created = taxes_linked_to(&pool, product, &[("IVA", "21")])
        .await
        .remove(0);
    let purchases = SqlitePurchaseRepository::new(pool.clone());
    let purchase = create_purchase(&pool).await;
    let line = purchases
        .create_line(purchase, product, dec("1"), dec("100"))
        .await
        .unwrap();
    purchases.delete_line(line.id).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(SqliteTaxRepository::new(pool.clone())
        .find_by_id(created.id)
        .await
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------
// The migration over a database that already has lines
// ---------------------------------------------------------------------------

/// The migration must run on a database that already holds pre-T1 lines, the
/// way a live installation would receive it: existing lines keep their
/// quantity and price and gain a truthful zero tax total (a line created
/// before the feature owed no tax), the snapshot tables appear empty, and no
/// existing row is lost.
#[tokio::test]
async fn tax_snapshot_migration_backfills_zero_tax_total_on_existing_lines() {
    let pool = pre_t1_pool().await;
    let actor = sentinel(&pool).await;
    let product = create_product(&pool, "PRE-1").await;
    let sale = create_sale(&pool).await;
    let purchase = create_purchase(&pool).await;
    let sale_line: i64 = sqlx::query_scalar(
        "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
         VALUES (?, ?, '3', '10.5') RETURNING id",
    )
    .bind(sale)
    .bind(product)
    .fetch_one(&pool)
    .await
    .unwrap();
    let purchase_line: i64 = sqlx::query_scalar(
        "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost)
         VALUES (?, ?, '2', '7.25') RETURNING id",
    )
    .bind(purchase)
    .bind(product)
    .fetch_one(&pool)
    .await
    .unwrap();
    let _ = actor;

    // Apply only the T1 migration, exactly as a live upgrade would.
    let migrator = sqlx::migrate!("./migrations");
    let t1 = migrator
        .iter()
        .find(|m| m.version == 20240101000039)
        .expect("the T1 line-tax migration is missing");
    sqlx::raw_sql(t1.sql.clone()).execute(&pool).await.unwrap();

    let sale_row: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_price, tax_total FROM sale_lines WHERE id = ?")
            .bind(sale_line)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sale_row.0, "3", "quantity must survive the migration");
    assert_eq!(sale_row.1, "10.5", "price must survive the migration");
    assert_eq!(sale_row.2, "0", "a pre-T1 line owed no tax");

    let purchase_row: (String, String, String) =
        sqlx::query_as("SELECT qty, unit_cost, tax_total FROM purchase_lines WHERE id = ?")
            .bind(purchase_line)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(purchase_row.0, "2");
    assert_eq!(purchase_row.1, "7.25");
    assert_eq!(purchase_row.2, "0");

    // The snapshot tables exist and are empty: no history is invented.
    let sale_snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    let purchase_snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_line_taxes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((sale_snapshots, purchase_snapshots), (0, 0));

    // A line created after the migration on the same database behaves normally.
    let sales = SqliteSaleRepository::new(pool.clone());
    let fresh = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(
        fresh.tax_total,
        dec("0"),
        "no linked tax means no tax money"
    );
    let taxes = SqliteTaxRepository::new(pool.clone());
    let created = taxes.create(actor, &tax("IVA", "21", true)).await.unwrap();
    SqliteProductTaxRepository::new(pool.clone())
        .link(actor, product, created.id)
        .await
        .unwrap();
    let taxed = sales
        .create_line(sale, product, dec("1"), dec("100"))
        .await
        .unwrap();
    assert_eq!(taxed.tax_total, dec("21"));
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// A `Tax` value for the pure contract. The contract takes resolved tax
/// definitions, so a test builds one without touching the database.
fn tax_value(id: i64, code: &str, rate: &str) -> crate::models::Tax {
    crate::models::Tax {
        id,
        code: code.to_string(),
        name: format!("Tax {code}"),
        rate: dec(rate),
        is_active: true,
        created_by: 0,
        updated_by: None,
        created_at: chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        updated_at: chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    }
}
