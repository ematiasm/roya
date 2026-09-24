use std::str::FromStr;

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};

use crate::error::AppError;
use crate::models::{
    NewBusinessLocale, NewBusinessSettings, NewCustomer, NewSupplier, NewTax, NewUser,
    TransactionKind, UpdateCustomer, UpdateSupplier,
};
use crate::repositories::{
    AccountRepository, BusinessLocaleRepository, BusinessSettingsRepository, CategoryRepository,
    CustomerRepository, PaymentMethodRepository, ProductSupplierCostRepository,
    ProductTaxRepository, PurchaseRepository, SaleRepository, SqliteAccountRepository,
    SqliteBusinessLocaleRepository, SqliteBusinessSettingsRepository, SqliteCategoryRepository,
    SqliteCustomerRepository, SqlitePaymentMethodRepository, SqliteProductSupplierCostRepository,
    SqliteProductTaxRepository, SqlitePurchaseRepository, SqliteSaleRepository,
    SqliteSupplierRepository, SqliteTaxRepository, SqliteTransactionRepository,
    SqliteUserRepository, SupplierRepository, TaxRepository, TransactionRepository, UserRepository,
};
use crate::services::customers::CustomerService;
use crate::services::suppliers::SupplierService;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_pool("sqlite::memory:").await.unwrap();
    pool
}

async fn sentinel(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = 'sistema'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn updated_at(pool: &SqlitePool, table: &str, id: i64) -> String {
    match table {
        "categories" => sqlx::query_scalar("SELECT updated_at FROM categories WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        "payment_methods" => {
            sqlx::query_scalar("SELECT updated_at FROM payment_methods WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        "transactions" => sqlx::query_scalar("SELECT updated_at FROM transactions WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        "product_supplier_costs" => {
            sqlx::query_scalar("SELECT updated_at FROM product_supplier_costs WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        "sale_payments" => sqlx::query_scalar("SELECT updated_at FROM sale_payments WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        "purchase_payments" => {
            sqlx::query_scalar("SELECT updated_at FROM purchase_payments WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        other => panic!("unsupported table {other}"),
    }
}

async fn set_updated_at(pool: &SqlitePool, table: &str, id: i64) {
    match table {
        "categories" => {
            sqlx::query(
                "UPDATE categories SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        "payment_methods" => {
            sqlx::query(
                "UPDATE payment_methods SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        "transactions" => {
            sqlx::query(
                "UPDATE transactions SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        "product_supplier_costs" => {
            sqlx::query(
                "UPDATE product_supplier_costs
                 SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        "sale_payments" => {
            sqlx::query(
                "UPDATE sale_payments SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        "purchase_payments" => {
            sqlx::query(
                "UPDATE purchase_payments
                 SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        }
        other => panic!("unsupported table {other}"),
    }
}

async fn table_count(pool: &SqlitePool, table: &str) -> i64 {
    match table {
        "business_settings" => sqlx::query_scalar("SELECT COUNT(*) FROM business_settings")
            .fetch_one(pool)
            .await
            .unwrap(),
        "business_locales" => sqlx::query_scalar("SELECT COUNT(*) FROM business_locales")
            .fetch_one(pool)
            .await
            .unwrap(),
        "taxes" => sqlx::query_scalar("SELECT COUNT(*) FROM taxes")
            .fetch_one(pool)
            .await
            .unwrap(),
        "product_taxes" => sqlx::query_scalar("SELECT COUNT(*) FROM product_taxes")
            .fetch_one(pool)
            .await
            .unwrap(),
        other => panic!("unsupported table {other}"),
    }
}

#[tokio::test]
async fn fresh_schema_has_unseeded_business_configuration_and_tax_schema() {
    let pool = pool().await;

    for table in [
        "business_settings",
        "business_locales",
        "taxes",
        "product_taxes",
    ] {
        let count = table_count(&pool, table).await;
        assert_eq!(count, 0, "{table} must not be seeded by migrations");
    }

    let active_admins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM users u
         JOIN user_roles ur ON ur.user_id = u.id
         JOIN roles r ON r.id = ur.role_id
         WHERE r.code = 'admin' AND u.is_active = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active_admins, 0,
        "setup must create the first administrator"
    );

    let sentinel_is_inactive: i64 =
        sqlx::query_scalar("SELECT is_active FROM users WHERE username = 'sistema'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel_is_inactive, 0);
}

#[tokio::test]
async fn configuration_and_tax_repositories_round_trip_canonical_values() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;

    let settings = SqliteBusinessSettingsRepository::new(pool.clone())
        .create(&NewBusinessSettings {
            business_name: "Acme Market".into(),
            default_locale_code: "es-AR".into(),
            currency_code: "ARS".into(),
            timezone: "America/Argentina/Buenos_Aires".into(),
        })
        .await
        .unwrap();
    let found_settings = SqliteBusinessSettingsRepository::new(pool.clone())
        .find(settings.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_settings.business_name, "Acme Market");
    assert_eq!(found_settings.default_locale_code, "es-AR");
    assert_eq!(found_settings.currency_code, "ARS");
    assert_eq!(found_settings.timezone, "America/Argentina/Buenos_Aires");

    let locale = SqliteBusinessLocaleRepository::new(pool.clone())
        .create(&NewBusinessLocale {
            locale_code: "es-AR".into(),
            language_code: "es".into(),
            display_name: "Español (Argentina)".into(),
            is_enabled: true,
        })
        .await
        .unwrap();
    let found_locale = SqliteBusinessLocaleRepository::new(pool.clone())
        .find_by_code("es-AR")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_locale.id, locale.id);
    assert_eq!(found_locale.language_code, "es");
    assert_eq!(found_locale.display_name, "Español (Argentina)");
    assert!(found_locale.is_enabled);

    let tax = SqliteTaxRepository::new(pool.clone())
        .create(
            actor,
            &NewTax {
                code: "IVA".into(),
                name: "IVA 21%".into(),
                rate: Decimal::from_str("21.00").unwrap(),
                is_active: true,
            },
        )
        .await
        .unwrap();
    let found_tax = SqliteTaxRepository::new(pool.clone())
        .find_by_code("IVA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_tax.id, tax.id);
    assert_eq!(found_tax.rate, Decimal::from_str("21.00").unwrap());
    let stored_rate: String = sqlx::query_scalar("SELECT rate FROM taxes WHERE id = ?")
        .bind(tax.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored_rate, "21.00", "tax rate must remain canonical TEXT");
}

#[tokio::test]
async fn duplicate_product_tax_links_are_rejected() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product_id: i64 = sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
         VALUES ('TAX-1', 'Taxable', 'Product', 'unit', '10', 0, ?)
         RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let tax = SqliteTaxRepository::new(pool.clone())
        .create(
            actor,
            &NewTax {
                code: "IVA".into(),
                name: "IVA".into(),
                rate: Decimal::from_str("21").unwrap(),
                is_active: true,
            },
        )
        .await
        .unwrap();

    let repository = SqliteProductTaxRepository::new(pool.clone());
    repository.link(actor, product_id, tax.id).await.unwrap();
    let error = repository
        .link(actor, product_id, tax.id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)), "got {error:?}");

    let links = repository.list_by_product(product_id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].tax_id, tax.id);
    assert_eq!(links[0].created_by, actor);
}

#[tokio::test]
async fn customer_and_supplier_due_days_distinguish_null_zero_and_positive_terms() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let customers = CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
    let suppliers = SupplierService::new(
        SqliteSupplierRepository::new(pool.clone()),
        SqliteProductSupplierCostRepository::new(pool.clone()),
    );

    let customer_input = |name: &str, due_days| NewCustomer {
        name: name.into(),
        phone: None,
        address: None,
        tax_id: None,
        notes: None,
        is_walkin: false,
        credit_limit: None,
        due_days,
    };
    let supplier_input = |name: &str, due_days| NewSupplier {
        name: name.into(),
        phone: None,
        notes: None,
        due_days,
    };

    let null_customer = customers
        .create_customer(actor, customer_input("No term", None))
        .await
        .unwrap()
        .customer;
    let immediate_customer = customers
        .create_customer(actor, customer_input("Immediate", Some(0)))
        .await
        .unwrap()
        .customer;
    let term_customer = customers
        .create_customer(actor, customer_input("Net 30", Some(30)))
        .await
        .unwrap()
        .customer;
    assert_eq!(null_customer.due_days, None);
    assert_eq!(immediate_customer.due_days, Some(0));
    assert_eq!(term_customer.due_days, Some(30));

    let updated = customers
        .update_customer(
            null_customer.id,
            actor,
            UpdateCustomer {
                due_days: Some(Some(15)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.due_days, Some(15));

    assert!(matches!(
        customers
            .create_customer(actor, customer_input("Invalid", Some(-1)))
            .await
            .unwrap_err(),
        AppError::Validation(_)
    ));

    let null_supplier = suppliers
        .create_supplier(actor, supplier_input("Supplier no term", None))
        .await
        .unwrap();
    let immediate_supplier = suppliers
        .create_supplier(actor, supplier_input("Supplier immediate", Some(0)))
        .await
        .unwrap();
    let term_supplier = suppliers
        .create_supplier(actor, supplier_input("Supplier net 45", Some(45)))
        .await
        .unwrap();
    assert_eq!(null_supplier.due_days, None);
    assert_eq!(immediate_supplier.due_days, Some(0));
    assert_eq!(term_supplier.due_days, Some(45));

    let updated = suppliers
        .update_supplier(
            null_supplier.id,
            actor,
            UpdateSupplier {
                due_days: Some(Some(10)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.due_days, Some(10));
    assert!(matches!(
        suppliers
            .create_supplier(actor, supplier_input("Invalid supplier", Some(-1)))
            .await
            .unwrap_err(),
        AppError::Validation(_)
    ));
}

#[tokio::test]
async fn supplier_costs_keep_business_dates_distinct_from_technical_update_time() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let product_id: i64 = sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
         VALUES ('COST-1', 'Costed', 'Product', 'unit', '20', 0, ?)
         RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let supplier_id: i64 = sqlx::query_scalar(
        "INSERT INTO suppliers (name, created_by) VALUES ('Cost supplier', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let repository = SqliteProductSupplierCostRepository::new(pool.clone());
    let first_date = NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();
    let second_date = NaiveDate::from_ymd_opt(2024, 6, 2).unwrap();

    let first = repository
        .create_cost(
            actor,
            product_id,
            supplier_id,
            Decimal::from_str("9.50").unwrap(),
            first_date,
        )
        .await
        .unwrap();
    assert_eq!(first.current_cost_date, first_date);
    assert_eq!(first.previous_cost_date, None);

    set_updated_at(&pool, "product_supplier_costs", first.id).await;
    let shifted = repository
        .shift_cost(
            actor,
            product_id,
            supplier_id,
            Decimal::from_str("10.25").unwrap(),
            second_date,
        )
        .await
        .unwrap();
    assert_eq!(shifted.current_cost_date, second_date);
    assert_eq!(shifted.previous_cost_date, Some(first_date));
    assert!(
        updated_at(&pool, "product_supplier_costs", shifted.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );
}

#[tokio::test]
async fn login_touch_updates_last_login_without_advancing_updated_at() {
    let pool = pool().await;
    let users = SqliteUserRepository::new(pool.clone());
    let user = users
        .create(
            &NewUser {
                username: "login-timestamp".into(),
                display_name: "Login Timestamp".into(),
                password_hash: "$argon2id$v=19$m=1,t=1,p=1$aaaa$bbbb".into(),
                must_change_password: false,
            },
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE users SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?")
        .bind(user.id)
        .execute(&pool)
        .await
        .unwrap();
    let login_at = NaiveDate::from_ymd_opt(2024, 5, 1)
        .unwrap()
        .and_hms_opt(12, 30, 0)
        .unwrap();

    users.touch_last_login(user.id, login_at).await.unwrap();

    let row = sqlx::query("SELECT last_login_at, updated_at FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let last_login_at: NaiveDateTime = row.get("last_login_at");
    let updated_at: NaiveDateTime = row.get("updated_at");
    assert_eq!(last_login_at, login_at);
    assert_eq!(
        updated_at,
        NaiveDate::from_ymd_opt(2000, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    );
}

#[tokio::test]
async fn repositories_stamp_technical_update_time_on_every_actor_mutation() {
    let pool = pool().await;
    let actor = sentinel(&pool).await;
    let date = NaiveDate::from_ymd_opt(2024, 5, 1).unwrap();

    let account = SqliteAccountRepository::new(pool.clone())
        .create(actor, "T1 Audit")
        .await
        .unwrap();
    let category_repo = SqliteCategoryRepository::new(pool.clone());
    let category = category_repo.create(actor, "Before", None).await.unwrap();
    set_updated_at(&pool, "categories", category.id).await;
    category_repo
        .update(actor, category.id, "After", None)
        .await
        .unwrap();
    assert!(
        updated_at(&pool, "categories", category.id).await.as_str() > "2000-01-01T00:00:00.000Z"
    );

    let method_repo = SqlitePaymentMethodRepository::new(pool.clone());
    let method = method_repo
        .create_in_account(actor, "T1 Method", account.id)
        .await
        .unwrap();
    set_updated_at(&pool, "payment_methods", method.id).await;
    method_repo
        .set_method_account(actor, method.id, None)
        .await
        .unwrap();
    assert!(
        updated_at(&pool, "payment_methods", method.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );

    let transaction_repo = SqliteTransactionRepository::new(pool.clone());
    let mut transaction = transaction_repo
        .create(
            actor,
            account.id,
            TransactionKind::Income,
            Decimal::from(10),
            "Before",
            None,
            date,
        )
        .await
        .unwrap();
    set_updated_at(&pool, "transactions", transaction.id).await;
    transaction.description = "After".into();
    transaction_repo.update(&transaction, actor).await.unwrap();
    assert!(
        updated_at(&pool, "transactions", transaction.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );

    let product_id: i64 = sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
         VALUES ('AUDIT-1', 'Audit', 'Product', 'unit', '10', 0, ?)
         RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let supplier_id: i64 = sqlx::query_scalar(
        "INSERT INTO suppliers (name, created_by) VALUES ('Audit supplier', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cost_repo = SqliteProductSupplierCostRepository::new(pool.clone());
    let cost = cost_repo
        .create_cost(actor, product_id, supplier_id, Decimal::from(9), date)
        .await
        .unwrap();
    set_updated_at(&pool, "product_supplier_costs", cost.id).await;
    cost_repo
        .shift_cost(actor, product_id, supplier_id, Decimal::from(10), date)
        .await
        .unwrap();
    assert!(
        updated_at(&pool, "product_supplier_costs", cost.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );

    let customer_id: i64 = sqlx::query_scalar(
        "INSERT INTO customers (name, created_by) VALUES ('Audit customer', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let sale_id: i64 = sqlx::query_scalar(
        "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
         VALUES ('Confirmed', 'Cash', ?, 'Audit customer', ?, ?)
         RETURNING id",
    )
    .bind(customer_id)
    .bind(date)
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let sale_repo = SqliteSaleRepository::new(pool.clone());
    let sale_payment = sale_repo
        .create_payment(
            actor,
            sale_id,
            account.id,
            method.id,
            Decimal::from(5),
            date,
            None,
            None,
        )
        .await
        .unwrap();
    set_updated_at(&pool, "sale_payments", sale_payment.id).await;
    sale_repo
        .set_payment_refund_transaction(actor, sale_payment.id, transaction.id)
        .await
        .unwrap();
    assert!(
        updated_at(&pool, "sale_payments", sale_payment.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );

    let purchase_id: i64 = sqlx::query_scalar(
        "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by)
         VALUES (?, 'Confirmed', 'Cash', ?, ?)
         RETURNING id",
    )
    .bind(supplier_id)
    .bind(date)
    .bind(actor)
    .fetch_one(&pool)
    .await
    .unwrap();
    let purchase_repo = SqlitePurchaseRepository::new(pool.clone());
    let purchase_payment = purchase_repo
        .create_payment(
            actor,
            purchase_id,
            account.id,
            method.id,
            Decimal::from(5),
            date,
            None,
        )
        .await
        .unwrap();
    set_updated_at(&pool, "purchase_payments", purchase_payment.id).await;
    purchase_repo
        .set_payment_refund_transaction(actor, purchase_payment.id, transaction.id)
        .await
        .unwrap();
    assert!(
        updated_at(&pool, "purchase_payments", purchase_payment.id)
            .await
            .as_str()
            > "2000-01-01T00:00:00.000Z"
    );
}
