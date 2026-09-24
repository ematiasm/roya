use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::str::FromStr;
use tower::ServiceExt;

use crate::localization::{resolve_context, LocalizationContext};
use crate::models::{BusinessLocale, BusinessSettings};
use crate::routes::AppState;

fn settings(locale: &str) -> BusinessSettings {
    BusinessSettings {
        id: 1,
        business_name: "Acme".into(),
        default_locale_code: locale.into(),
        currency_code: "ARS".into(),
        timezone: "America/Argentina/Buenos_Aires".into(),
        created_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
        updated_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
    }
}

fn locale(code: &str, language: &str, enabled: bool) -> BusinessLocale {
    BusinessLocale {
        id: 1,
        locale_code: code.into(),
        language_code: language.into(),
        display_name: code.into(),
        is_enabled: enabled,
        created_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
        updated_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
    }
}

#[test]
fn locale_resolution_prefers_the_configured_locale_then_language_then_enabled_fallback() {
    let exact = resolve_context(
        Some(&settings("es-AR")),
        &[locale("en-US", "en", true), locale("es-AR", "es", true)],
    );
    assert_eq!(exact.locale_code, "es-AR");
    assert_eq!(exact.language_code, "es");
    assert_eq!(exact.currency_code, "ARS");

    let language = resolve_context(
        Some(&settings("es-MX")),
        &[
            locale("es-MX", "es", false),
            locale("en-US", "en", true),
            locale("es-ES", "es", true),
        ],
    );
    assert_eq!(language.locale_code, "es-ES");
    assert_eq!(language.language_code, "es");

    let first_enabled = resolve_context(
        Some(&settings("fr-FR")),
        &[locale("fr-FR", "fr", false), locale("en-US", "en", true)],
    );
    assert_eq!(first_enabled.locale_code, "en-US");
    assert_eq!(first_enabled.language_code, "en");
}

#[test]
fn missing_business_configuration_uses_the_pre_setup_fallback_context() {
    let context = resolve_context(None, &[]);
    assert_eq!(context, LocalizationContext::fallback());
    assert_eq!(context.locale_code, "en-US");
    assert_eq!(context.language_code, "en");
    assert_eq!(context.currency_code, "USD");
    assert_eq!(context.timezone, "UTC");
}

#[test]
fn formatting_preserves_every_stored_decimal_place_and_uses_the_business_timezone() {
    let context = resolve_context(Some(&settings("es-AR")), &[locale("es-AR", "es", true)]);
    let exact = Decimal::from_str("1234.56780").unwrap();

    assert_eq!(context.format_decimal(exact), "1.234,56780");
    assert_eq!(context.format_currency(exact), "1.234,56780 ARS");
    assert_eq!(context.format_quantity(exact), "1.234,56780");
    assert_eq!(
        context.format_percentage(Decimal::from_str("12.50").unwrap()),
        "12,50 %"
    );
    assert_eq!(
        context.format_date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
        "31/12/2026"
    );
    assert_eq!(
        context.format_timestamp(
            NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(3, 4, 5)
                .unwrap()
        ),
        "01/01/2026 00:04:05"
    );
}

#[test]
fn decimal_input_parsing_is_locale_aware_and_rejects_ambiguous_grouping() {
    let es = resolve_context(Some(&settings("es-AR")), &[locale("es-AR", "es", true)]);
    let en = resolve_context(
        Some(&{
            let mut value = settings("en-US");
            value.currency_code = "USD".into();
            value
        }),
        &[locale("en-US", "en", true)],
    );

    assert_eq!(
        es.parse_decimal("1.234,56780").unwrap(),
        Decimal::from_str("1234.56780").unwrap()
    );
    assert_eq!(
        en.parse_decimal("1,234.56780").unwrap(),
        Decimal::from_str("1234.56780").unwrap()
    );
    assert!(es.parse_decimal("1.23").is_err());
    assert!(en.parse_decimal("not a number").is_err());
}

async fn app_without_configuration() -> (Router, SqlitePool) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), false, true);
    state.refresh_setup_requirement().await.unwrap();
    (crate::routes::router(state), pool)
}

async fn localized_app() -> (Router, SqlitePool) {
    localized_app_with_timezone("America/Argentina/Buenos_Aires").await
}

async fn localized_app_with_timezone(timezone: &str) -> (Router, SqlitePool) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), false, true);
    sqlx::query(
        r#"INSERT INTO business_locales
           (locale_code, language_code, display_name, is_enabled)
           VALUES ('es-AR', 'es', 'Español (Argentina)', 1)"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO business_settings
           (id, business_name, default_locale_code, currency_code, timezone)
           VALUES (1, 'Acme', 'es-AR', 'ARS', ?)"#,
    )
    .bind(timezone)
    .execute(&pool)
    .await
    .unwrap();
    crate::security::test_support::seed_session(&pool)
        .await
        .unwrap();
    state.mark_setup_complete();
    (crate::routes::router(state), pool)
}

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    content_type: Option<&str>,
    htmx: bool,
    body: String,
) -> (StatusCode, String) {
    let mut builder =
        crate::security::test_support::with_cookie(Request::builder().method(method).uri(uri));
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let (status, body) = request(
        app,
        Method::POST,
        uri,
        Some("application/json"),
        false,
        body.to_string(),
    )
    .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn seed_localized_product(app: &Router, sku: &str) -> i64 {
    let (status, product) = post_json(
        app,
        "/api/products",
        json!({
            "sku": sku,
            "name": format!("Localized {sku}"),
            "kind": "Service",
            "unit": "un",
            "sale_price": "10.50",
            "cost_price": "2.00",
            "track_stock": false
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    product["id"].as_i64().unwrap()
}

async fn seed_localized_customer(app: &Router) -> i64 {
    let (status, customer) = post_json(
        app,
        "/api/customers",
        json!({
            "name": "Localized statement customer",
            "credit_limit": "1234.50",
            "due_days": 0
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{customer}");
    customer["customer"]["id"].as_i64().unwrap()
}

async fn seed_localized_credit_sale(
    app: &Router,
    customer_id: i64,
    product_id: i64,
    sale_date: NaiveDate,
    due_date: NaiveDate,
) -> i64 {
    let (status, sale) = post_json(
        app,
        "/api/sales",
        json!({
            "customer_id": customer_id,
            "payment_type": "Credit",
            "sale_date": sale_date,
            "due_date": due_date
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sale}");
    let sale_id = sale["sale"]["id"].as_i64().unwrap();

    let (status, _) = post_json(
        app,
        &format!("/api/sales/{sale_id}/lines"),
        json!({ "product_id": product_id, "qty": "2.5", "unit_price": "10.50" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, confirmed) = post_json(
        app,
        &format!("/api/sales/{sale_id}/confirm"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    sale_id
}

#[tokio::test]
async fn locale_presentation_authenticated_full_page_shells_use_the_request_locale() {
    let (app, _pool) = localized_app().await;
    let (status, account) = post_json(&app, "/api/accounts", json!({ "name": "Locale shell" })).await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let account_id = account["id"].as_i64().unwrap();

    for uri in [
        "/".to_string(),
        format!("/accounts/{account_id}"),
        "/customers".to_string(),
        "/sales".to_string(),
        "/purchases".to_string(),
        "/products".to_string(),
        "/suppliers".to_string(),
        "/documents".to_string(),
        "/users".to_string(),
        "/roles".to_string(),
        "/password".to_string(),
    ] {
        let (status, body) = request(&app, Method::GET, &uri, None, false, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body:.500}");
        assert!(
            body.contains("<html lang=\"es-AR\">"),
            "{uri} does not use the request locale: {body:.500}"
        );
    }
}

#[tokio::test]
async fn locale_presentation_customer_statement_and_detail_localize_derived_facts() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-CUSTOMER-DETAIL").await;
    let customer_id = seed_localized_customer(&app).await;
    let today = chrono::Local::now().date_naive();
    let sale_date = today - chrono::Duration::days(70);
    let due_date = today - chrono::Duration::days(65);
    let sale_id = seed_localized_credit_sale(
        &app,
        customer_id,
        product_id,
        sale_date,
        due_date,
    )
    .await;

    let (status, page) = request(
        &app,
        Method::GET,
        &format!("/customers/{customer_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("<html lang=\"es-AR\">"), "{page:.500}");
    assert!(page.contains("limit 1.234,50 ARS"), "credit limit: {page}");
    assert!(
        page.contains(&format!("balance as of {}", resolve_context(
            Some(&settings("es-AR")),
            &[locale("es-AR", "es", true)],
        )
        .format_date(today))),
        "statement date: {page}"
    );
    assert!(page.contains("26,250 ARS"), "ageing/balance: {page}");

    let (status, detail) = request(
        &app,
        Method::GET,
        &format!("/web/customers/detail/{customer_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let context = resolve_context(Some(&settings("es-AR")), &[locale("es-AR", "es", true)]);
    assert!(detail.contains(&context.format_date(sale_date)), "sale date: {detail}");
    assert!(detail.contains(&context.format_date(due_date)), "due date: {detail}");
    assert!(detail.contains("total 26,250 ARS"), "sale total: {detail}");
    assert!(detail.contains("paid 0 ARS"), "sale paid: {detail}");
    assert!(detail.contains("26,250 ARS"), "sale due: {detail}");
    assert!(!detail.contains("26.25"), "raw customer Decimal leaked: {detail}");

    let (status, canonical) = request(
        &app,
        Method::GET,
        &format!("/api/sales/{sale_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{canonical}");
    let canonical: Value = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical["total"], "26.250");
    assert_eq!(canonical["sale"]["sale_date"], sale_date.to_string());
    assert_eq!(canonical["sale"]["due_date"], due_date.to_string());
}

#[tokio::test]
async fn locale_presentation_sale_detail_localizes_money_quantity_and_dates() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-SALE-DETAIL").await;
    let customer_id = seed_localized_customer(&app).await;
    let sale_date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let due_date = NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
    let sale_id = seed_localized_credit_sale(
        &app,
        customer_id,
        product_id,
        sale_date,
        due_date,
    )
    .await;

    for uri in [
        format!("/sales/{sale_id}"),
        format!("/web/sales/{sale_id}"),
    ] {
        let (status, body) = request(&app, Method::GET, &uri, None, false, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        if uri.starts_with("/sales/") {
            assert!(body.contains("<html lang=\"es-AR\">"), "{uri}: {body:.500}");
        }
        assert!(body.contains("26,250 ARS"), "{uri} total: {body}");
        assert!(body.contains("2,5"), "{uri} quantity: {body}");
        assert!(body.contains("10,50 ARS"), "{uri} unit price: {body}");
        assert!(body.contains("31/12/2026"), "{uri} sale date: {body}");
        assert!(body.contains("15/01/2027"), "{uri} due date: {body}");
        assert!(!body.contains("$10.50"), "{uri} kept hard-coded currency: {body}");
    }

    let (status, canonical) = request(
        &app,
        Method::GET,
        &format!("/api/sales/{sale_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{canonical}");
    let canonical: Value = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical["total"], "26.250");
    assert_eq!(canonical["lines"][0]["qty"], "2.5");
    assert_eq!(canonical["lines"][0]["unit_price"], "10.50");
}

#[tokio::test]
async fn locale_presentation_purchase_detail_localizes_money_quantity_and_dates() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-PURCHASE-DETAIL").await;
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized purchase detail supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let purchase_date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();

    let due_date = NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
    let (status, purchase) = post_json(
        &app,
        "/api/purchases",
        json!({
            "supplier_id": supplier_id,
            "payment_type": "Credit",
            "purchase_date": purchase_date,
            "due_date": due_date
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{purchase}");
    let purchase_id = purchase["purchase"]["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/lines"),
        json!({ "product_id": product_id, "qty": "1.25", "unit_cost": "2.00" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, draft) = request(
        &app,
        Method::GET,
        &format!("/purchases/{purchase_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert!(
        draft.contains(&format!("value=\"{purchase_date}\"")),
        "purchase date input lost its canonical ISO value: {draft}"
    );
    assert!(draft.contains("Cash scenario"), "cash scenario semantics: {draft}");
    assert!(draft.contains("Credit scenario"), "credit scenario semantics: {draft}");

    let (status, confirmed) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/confirm"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");

    for uri in [
        format!("/purchases/{purchase_id}"),
        format!("/web/purchases/{purchase_id}"),
    ] {
        let (status, body) = request(&app, Method::GET, &uri, None, false, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        if uri.starts_with("/purchases/") {
            assert!(body.contains("<html lang=\"es-AR\">"), "{uri}: {body:.500}");
        }
        assert!(body.contains("1,25"), "{uri} quantity: {body}");
        assert!(body.contains("2,5000 ARS"), "{uri} subtotal/total: {body}");
        assert!(body.contains("31/12/2026"), "{uri} purchase date: {body}");
        assert!(body.contains("15/01/2027"), "{uri} due date: {body}");
        assert!(!body.contains("$2"), "{uri} kept hard-coded currency: {body}");
        assert!(body.contains("Confirmed"), "{uri} purchase status: {body}");
        assert!(body.contains("Credit"), "{uri} payment semantics: {body}");
    }

    let (status, canonical) = request(
        &app,
        Method::GET,
        &format!("/api/purchases/{purchase_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{canonical}");
    let canonical: Value = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical["total"], "2.5000");
    assert_eq!(canonical["lines"][0]["qty"], "1.25");
    assert_eq!(canonical["purchase"]["purchase_date"], purchase_date.to_string());
}

#[tokio::test]
async fn locale_presentation_supplier_detail_localizes_purchase_date_and_decimal_values() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-SUPPLIER-DETAIL").await;
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized supplier detail supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let purchase_date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let (status, purchase) = post_json(
        &app,
        "/api/purchases",
        json!({
            "supplier_id": supplier_id,
            "payment_type": "Credit",
            "purchase_date": purchase_date,
            "due_date": NaiveDate::from_ymd_opt(2027, 1, 15).unwrap()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{purchase}");
    let purchase_id = purchase["purchase"]["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/lines"),
        json!({ "product_id": product_id, "qty": "1.25", "unit_cost": "2.00" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/confirm"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, detail) = request(
        &app,
        Method::GET,
        &format!("/web/suppliers/{supplier_id}/detail"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail.contains("31/12/2026"), "purchase date: {detail}");
    assert!(detail.contains("2,5000 ARS"), "purchase total/paid/due: {detail}");
    assert!(!detail.contains("2026-12-31"), "raw ISO date: {detail}");
    assert!(!detail.contains("2.5000"), "raw Decimal: {detail}");
}

#[tokio::test]
async fn locale_presentation_business_timezone_controls_affected_web_and_api_date_defaults() {
    let (app, pool) = localized_app_with_timezone("Pacific/Kiritimati").await;
    let mut business_settings = settings("es-AR");
    business_settings.timezone = "Pacific/Kiritimati".into();
    let context = resolve_context(Some(&business_settings), &[locale("es-AR", "es", true)]);
    let expected_today = context.today_iso();
    let host_today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert_ne!(
        expected_today, host_today,
        "this regression test must exercise a date boundary"
    );

    let customer_id = seed_localized_customer(&app).await;
    let product_id = seed_localized_product(&app, "LOC-TIMEZONE-DEFAULT").await;
    let (status, account) = post_json(&app, "/api/accounts", json!({ "name": "Timezone account" })).await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let account_id = account["id"].as_i64().unwrap();
    let (method_id,): (i64,) = sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE payment_methods SET account_id = ? WHERE id = ?")
        .bind(account_id)
        .bind(method_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, sale) = request(
        &app,
        Method::POST,
        "/web/sales",
        Some("application/x-www-form-urlencoded"),
        true,
        format!("customer_id={customer_id}&payment_type=Cash&sale_date=&due_date="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sale}");
    let sale_date: String = sqlx::query_scalar("SELECT sale_date FROM sales ORDER BY id DESC LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sale_date, expected_today, "sales default date");

    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Timezone purchase supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let (status, purchase) = request(
        &app,
        Method::POST,
        "/web/purchases",
        Some("application/x-www-form-urlencoded"),
        true,
        format!("supplier_id={supplier_id}&payment_type=Cash&purchase_date=&due_date="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{purchase}");
    let purchase_date: String =
        sqlx::query_scalar("SELECT purchase_date FROM purchases ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(purchase_date, expected_today, "purchases default date");

    let (status, cost) = request(
        &app,
        Method::POST,
        "/web/supplier-costs",
        Some("application/x-www-form-urlencoded"),
        true,
        format!("product_id={product_id}&supplier_id={supplier_id}&cost=3%2C25&date="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cost}");
    let cost_date: String = sqlx::query_scalar(
        "SELECT current_cost_date FROM product_supplier_costs WHERE product_id = ? AND supplier_id = ?",
    )
    .bind(product_id)
    .bind(supplier_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cost_date, expected_today, "supplier cost default date");

    let credit_customer_id = seed_localized_customer(&app).await;
    let expected_due = NaiveDate::parse_from_str(&expected_today, "%Y-%m-%d").unwrap();
    let sale_date = expected_due - chrono::Duration::days(1);
    seed_localized_credit_sale(
        &app,
        credit_customer_id,
        product_id,
        sale_date,
        expected_due,
    )
    .await;
    let (status, statement) = request(
        &app,
        Method::GET,
        &format!("/api/customers/{credit_customer_id}/statement"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{statement}");
    let statement: Value = serde_json::from_str(&statement).unwrap();
    assert_eq!(statement["statement"]["as_of"], expected_today, "customers API default as_of");

    let (status, receipt) = request(
        &app,
        Method::POST,
        "/web/customer-receipts",
        Some("application/x-www-form-urlencoded"),
        true,
        format!("customer_id={credit_customer_id}&method_id={method_id}&amount=1&date="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let receipt_date: String = sqlx::query_scalar("SELECT date FROM customer_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipt_date, expected_today, "customer receipt default date");
}

#[tokio::test]
async fn locale_presentation_sales_list_and_debt_fragments_localize_dates_and_money() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-SALE-LIST").await;
    let customer_id = seed_localized_customer(&app).await;
    let sale_date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let due_date = NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
    seed_localized_credit_sale(&app, customer_id, product_id, sale_date, due_date).await;

    for uri in ["/web/sales", "/web/sales/debt"] {
        let (status, body) = request(&app, Method::GET, uri, None, true, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("31/12/2026"), "{uri} sale date: {body}");
        assert!(
            body.contains("15/01/2027"),
            "{uri} due date: {body}"
        );
        assert!(body.contains("26,250 ARS"), "{uri} total/owed: {body}");
        if uri == "/web/sales" {
            assert!(body.contains("paid 0 ARS"), "{uri} paid: {body}");
            assert!(body.contains("due 26,250 ARS"), "{uri} due: {body}");
        } else {
            assert!(body.contains("owed 26,250 ARS"), "{uri} owed: {body}");
        }
        assert!(!body.contains("2026-12-31"), "{uri} raw ISO date: {body}");
        assert!(!body.contains("26.250"), "{uri} raw Decimal: {body}");
    }
}

#[tokio::test]
async fn locale_presentation_purchase_list_fragment_localizes_dates_and_money() {
    let (app, _pool) = localized_app().await;
    let product_id = seed_localized_product(&app, "LOC-PURCHASE-LIST").await;
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized purchase list supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let purchase_date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let due_date = NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
    let (status, purchase) = post_json(
        &app,
        "/api/purchases",
        json!({
            "supplier_id": supplier_id,
            "payment_type": "Credit",
            "purchase_date": purchase_date,
            "due_date": due_date
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{purchase}");
    let purchase_id = purchase["purchase"]["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/lines"),
        json!({ "product_id": product_id, "qty": "1.25", "unit_cost": "2.00" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_json(
        &app,
        &format!("/api/purchases/{purchase_id}/confirm"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(
        &app,
        Method::GET,
        "/web/purchases",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("31/12/2026"), "purchase date: {body}");
    assert!(body.contains("15/01/2027"), "due date: {body}");
    assert!(body.contains("2,5000 ARS"), "total/due: {body}");
    assert!(!body.contains("2026-12-31"), "raw ISO date: {body}");
    assert!(!body.contains("2.5000"), "raw Decimal: {body}");
}

#[tokio::test]
async fn locale_presentation_reorder_suggestion_fragment_localizes_quantities_and_money() {
    let (app, _pool) = localized_app().await;
    let (status, product) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-SUGGESTION",
            "name": "Localized reorder suggestion",
            "kind": "Product",
            "unit": "un",
            "sale_price": "10",
            "cost_price": "3.25",
            "track_stock": true,
            "min_stock": "2.50",
            "max_stock": "10"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    let product_id = product["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        "/api/stock-movements",
        json!({
            "product_id": product_id,
            "qty": "1.25",
            "type": "In",
            "reason": "Initial",
            "date": "2026-12-31"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized suggestion supplier" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        "/api/product-supplier-costs",
        json!({
            "product_id": product_id,
            "supplier_id": supplier_id,
            "cost": "3.25",
            "date": "2026-12-31"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app,
        Method::GET,
        "/web/purchases/suggestions",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("stock 1,25"), "stock quantity: {body}");
    assert!(body.contains("suggested 8,75"), "suggested quantity: {body}");
    assert!(body.contains("× 3,25 ARS"), "unit cost: {body}");
    assert!(body.contains("28,4375 ARS"), "subtotal: {body}");
    assert!(!body.contains("× $"), "hard-coded currency: {body}");
    assert!(!body.contains("28.4375"), "raw Decimal: {body}");
}

#[tokio::test]
async fn locale_presentation_account_detail_localizes_dates_and_transaction_amounts() {
    let (app, _pool) = localized_app().await;
    let (status, account) =
        post_json(&app, "/api/accounts", json!({ "name": "Localized account detail" })).await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let account_id = account["id"].as_i64().unwrap();
    let (status, body) = request(
        &app,
        Method::POST,
        "/web/transactions",
        Some("application/x-www-form-urlencoded"),
        true,
        format!(
            "account_id={account_id}&type=Income&amount=1.234%2C56780&description=Localized+detail&date=2026-12-31"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, detail) = request(
        &app,
        Method::GET,
        &format!("/accounts/{account_id}"),
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail.contains("31/12/2026"), "transaction date: {detail}");
    assert!(detail.contains("1.234,56780 ARS"), "balance/amount: {detail}");
    assert!(!detail.contains("$1234.56780"), "hard-coded currency: {detail}");
    assert!(!detail.contains(">2026-12-31<"), "raw ISO date: {detail}");
    assert!(!detail.contains(">1234.56780<"), "raw Decimal: {detail}");
}

#[tokio::test]
async fn locale_presentation_document_list_fragment_localizes_dates_and_decimals() {
    let (app, _pool) = localized_app().await;
    let (status, product) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-DOCUMENT-LIST",
            "name": "Localized document list",
            "kind": "Product",
            "unit": "un",
            "sale_price": "10.50",
            "cost_price": "2.00",
            "track_stock": true,
            "min_stock": "0",
            "max_stock": "10"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    let product_id = product["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        "/api/stock-movements",
        json!({
            "product_id": product_id,
            "qty": "1.25",
            "type": "In",
            "reason": "Initial",
            "date": "2026-12-31"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app,
        Method::GET,
        "/web/documents",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("31/12/2026"), "document date: {body}");
    assert!(body.contains("1,25"), "document quantity: {body}");
    assert!(!body.contains("2026-12-31"), "raw ISO date: {body}");
    assert!(!body.contains(">1.25<"), "raw Decimal quantity: {body}");
}

#[tokio::test]
async fn setup_renders_before_business_configuration_exists() {
    let (app, _pool) = app_without_configuration().await;
    let (status, body) = request(&app, Method::GET, "/setup", None, false, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Configuración inicial"));
}

#[tokio::test]
async fn html_htmx_and_static_picker_share_the_request_localization_contract() {
    let (app, pool) = localized_app().await;

    let (status, account) =
        post_json(&app, "/api/accounts", json!({ "name": "Locale Account" })).await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let account_id = account["id"].as_i64().unwrap();

    let (status, accounts_api) = request(
        &app,
        Method::GET,
        "/api/accounts",
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accounts_api}");
    let accounts_api: Value = serde_json::from_str(&accounts_api).unwrap();
    assert_eq!(accounts_api["accounts"][0]["balance"], "0");

    let (status, body) = request(
        &app,
        Method::POST,
        "/web/transactions",
        Some("application/x-www-form-urlencoded"),
        true,
        format!(
            "account_id={account_id}&type=Income&amount=1.234%2C56780&description=Locale+input&date=2026-12-31"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored: String = sqlx::query_scalar("SELECT amount FROM transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "1234.56780");

    let (status, dashboard) = request(&app, Method::GET, "/", None, false, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        dashboard.contains("<html lang=\"es-AR\">"),
        "{dashboard:.500}"
    );
    assert!(dashboard.contains("1.234,56780 ARS"), "{dashboard:.800}");

    let (status, accounts) = request(
        &app,
        Method::GET,
        "/web/accounts",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(accounts.contains("1.234,56780 ARS"), "{accounts}");

    let (status, transactions) = request(
        &app,
        Method::GET,
        &format!("/web/transactions?account_id={account_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(transactions.contains("1.234,56780 ARS"), "{transactions}");
    assert!(transactions.contains("31/12/2026"), "{transactions}");

    let (status, created) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-1",
            "name": "Locale Product",
            "kind": "Product",
            "category_id": null,
            "unit": "un",
            "sale_price": "1234.56780",
            "cost_price": "2.5",
            "track_stock": true,
            "min_stock": "0",
            "max_stock": "100",
            "location": null,
            "notes": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["sale_price"], "1234.56780");

    let (status, search) = request(
        &app,
        Method::GET,
        "/web/product-search.json?q=Locale",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let search: Value = serde_json::from_str(&search).unwrap();
    assert_eq!(search["products"][0]["sale_price"], "1.234,56780 ARS");
    assert_eq!(search["products"][0]["cost_price"], "2,5 ARS");

    let (status, picker_js) = request(
        &app,
        Method::GET,
        "/static/picker.js",
        None,
        false,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(picker_js.contains("'cost ' + product.cost_price"));
    assert!(picker_js.contains("product.sale_price"));
    assert!(!picker_js.contains("'$' + product.sale_price"));
}

#[tokio::test]
async fn localized_product_form_submission_stores_canonical_decimals() {
    let (app, pool) = localized_app().await;

    let (status, body) = request(
        &app,
        Method::POST,
        "/web/products",
        Some("application/x-www-form-urlencoded"),
        true,
        "sku=LOC-FORM&name=Localized+form+product&kind=Product&unit=un&sale_price=1.234%2C56780&cost_price=2%2C5".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored: (String, String) =
        sqlx::query_as("SELECT sale_price, cost_price FROM products WHERE sku = 'LOC-FORM'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, ("1234.56780".into(), "2.5".into()));
}

#[tokio::test]
async fn localized_product_html_and_htmx_fragments_format_money_quantity_and_date() {
    let (app, _pool) = localized_app().await;
    let (status, product) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-HTML",
            "name": "Localized HTML product",
            "kind": "Product",
            "category_id": null,
            "unit": "un",
            "sale_price": "1234.56780",
            "cost_price": "2.5",
            "track_stock": true,
            "min_stock": "0",
            "max_stock": "100",
            "location": null,
            "notes": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    let product_id = product["id"].as_i64().unwrap();

    let (status, _) = post_json(
        &app,
        "/api/stock-movements",
        json!({
            "product_id": product_id,
            "qty": "2.5",
            "type": "In",
            "reason": "Initial",
            "reference": null,
            "date": "2026-12-31"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized supplier", "phone": null, "notes": null }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();
    let (status, _) = post_json(
        &app,
        "/api/product-supplier-costs",
        json!({
            "product_id": product_id,
            "supplier_id": supplier_id,
            "cost": "3.25",
            "date": "2026-12-31"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, products_page) =
        request(&app, Method::GET, "/products", None, false, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        products_page.contains("<html lang=\"es-AR\">"),
        "{products_page:.500}"
    );

    let (status, list) = request(
        &app,
        Method::GET,
        "/web/products",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(
        list.contains("1.234,56780 ARS"),
        "product list money: {list}"
    );
    assert!(list.contains("2,5"), "product list quantity: {list}");
    assert!(
        !list.contains("$1234.56780"),
        "product list keeps no hard-coded currency: {list}"
    );

    let (status, detail) = request(
        &app,
        Method::GET,
        &format!("/web/products/detail/{product_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(
        detail.contains("1.234,56780 ARS"),
        "product detail money: {detail}"
    );
    assert!(
        detail.contains("31/12/2026"),
        "product detail date: {detail}"
    );
    assert!(
        detail.contains("3,25 ARS"),
        "product detail supplier cost: {detail}"
    );
    assert!(
        !detail.contains("$1234.56780"),
        "product detail keeps no hard-coded currency: {detail}"
    );
}

fn assert_decimal_text_controls(template_name: &str, template: &str, fields: &[&str]) {
    for field in fields {
        let marker = format!("name=\"{field}\"");
        let controls = template
            .lines()
            .filter(|line| line.contains(&marker))
            .collect::<Vec<_>>();
        assert!(
            !controls.is_empty(),
            "{template_name} has no control named {field}"
        );
        for control in controls {
            assert!(
                !control.contains("type=\"number\""),
                "{template_name} keeps {field} as a canonical number input: {control}"
            );
            assert!(
                control.contains("type=\"text\"") && control.contains("inputmode=\"decimal\""),
                "{template_name} does not expose {field} as locale-formatted text: {control}"
            );
        }
    }
}

#[test]
fn locale_aware_decimal_html_controls_are_reachable_text_inputs() {
    assert_decimal_text_controls(
        "products.html",
        include_str!("../templates/products.html"),
        &[
            "sale_price",
            "cost_price",
            "markup_pct",
            "min_stock",
            "max_stock",
        ],
    );
    assert_decimal_text_controls(
        "partials/product_detail.html",
        include_str!("../templates/partials/product_detail.html"),
        &[
            "sale_price",
            "cost_price",
            "markup_pct",
            "min_stock",
            "max_stock",
            "cost",
            "qty",
        ],
    );
    assert_decimal_text_controls(
        "partials/product_search_results.html",
        include_str!("../templates/partials/product_search_results.html"),
        &["qty"],
    );
    assert_decimal_text_controls(
        "partials/product_search_results.html dynamic price",
        include_str!("../templates/partials/product_search_results.html"),
        &["{{ price_field_name }}"],
    );
    assert_decimal_text_controls(
        "partials/sale_detail.html",
        include_str!("../templates/partials/sale_detail.html"),
        &["amount"],
    );
    assert_decimal_text_controls(
        "partials/purchase_detail.html",
        include_str!("../templates/partials/purchase_detail.html"),
        &["qty", "unit_cost", "amount"],
    );
    assert_decimal_text_controls(
        "customers.html",
        include_str!("../templates/customers.html"),
        &["credit_limit"],
    );
    assert_decimal_text_controls(
        "partials/customer_edit_form.html",
        include_str!("../templates/partials/customer_edit_form.html"),
        &["credit_limit"],
    );
    assert_decimal_text_controls(
        "partials/customer_detail.html",
        include_str!("../templates/partials/customer_detail.html"),
        &["amount"],
    );
    assert_decimal_text_controls(
        "partials/supplier_detail.html",
        include_str!("../templates/partials/supplier_detail.html"),
        &["amount", "cost"],
    );
}

#[tokio::test]
async fn localized_stock_input_round_trips_and_stock_fragments_format_quantities() {
    let (app, pool) = localized_app().await;

    for (sku, min_stock, max_stock) in [("LOC-LOW", "2.5", "10"), ("LOC-NEG", "1", "20")] {
        let (status, product) = post_json(
            &app,
            "/api/products",
            json!({
                "sku": sku,
                "name": sku,
                "kind": "Product",
                "category_id": null,
                "unit": "un",
                "sale_price": "10",
                "cost_price": "5",
                "track_stock": true,
                "min_stock": min_stock,
                "max_stock": max_stock,
                "location": null,
                "notes": null
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{product}");
    }

    let low_id: i64 = sqlx::query_scalar("SELECT id FROM products WHERE sku = 'LOC-LOW'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let negative_id: i64 = sqlx::query_scalar("SELECT id FROM products WHERE sku = 'LOC-NEG'")
        .fetch_one(&pool)
        .await
        .unwrap();

    for (product_id, kind, qty) in [(low_id, "In", "1%2C25"), (negative_id, "Out", "2%2C5")] {
        let (status, body) = request(
            &app,
            Method::POST,
            "/web/stock-movements",
            Some("application/x-www-form-urlencoded"),
            true,
            format!("product_id={product_id}&type={kind}&qty={qty}&date=2026-12-31"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let quantities: Vec<String> = sqlx::query_scalar(
        "SELECT qty FROM stock_movements WHERE product_id IN (?, ?) ORDER BY product_id",
    )
    .bind(low_id)
    .bind(negative_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(quantities, ["1.25", "2.5"]);

    let (status, low) = request(
        &app,
        Method::GET,
        "/web/low-stock",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{low}");
    assert!(low.contains("stock 1,25"), "low stock quantity: {low}");
    assert!(low.contains("min 2,5"), "low stock minimum: {low}");
    assert!(low.contains("max 10"), "low stock maximum: {low}");
    assert!(low.contains("+8,75"), "low stock suggestion: {low}");
    assert!(
        !low.contains('$'),
        "low stock has no hard-coded currency: {low}"
    );

    let (status, negative) = request(
        &app,
        Method::GET,
        "/web/negative-stock",
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{negative}");
    assert!(
        negative.contains("stock -2,5"),
        "negative stock quantity: {negative}"
    );
    assert!(
        negative.contains("min 1"),
        "negative stock minimum: {negative}"
    );
    assert!(
        negative.contains("max 20"),
        "negative stock maximum: {negative}"
    );
    assert!(
        !negative.contains('$'),
        "negative stock has no hard-coded currency: {negative}"
    );
}

#[tokio::test]
async fn localized_customer_credit_limit_form_stores_canonical_decimal() {
    let (app, pool) = localized_app().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/web/customers",
        Some("application/x-www-form-urlencoded"),
        true,
        "name=Localized+credit&credit_limit=1.234%2C50&due_days=0".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored: String =
        sqlx::query_scalar("SELECT credit_limit FROM customers WHERE name = 'Localized credit'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "1234.50");

    let customer_id: i64 =
        sqlx::query_scalar("SELECT id FROM customers WHERE name = 'Localized credit'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let (status, edit_form) = request(
        &app,
        Method::GET,
        &format!("/web/customers/edit-form/{customer_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edit_form}");
    assert!(
        edit_form.contains("value=\"1.234,50\""),
        "localized credit limit: {edit_form}"
    );

    let (status, body) = request(
        &app,
        Method::POST,
        "/web/customers/edit",
        Some("application/x-www-form-urlencoded"),
        true,
        format!(
            "customer_id={customer_id}&name=Localized+credit&credit_limit=2.000%2C25&due_days=0"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let updated: String = sqlx::query_scalar("SELECT credit_limit FROM customers WHERE id = ?")
        .bind(customer_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(updated, "2000.25");
}

#[tokio::test]
async fn localized_purchase_line_values_render_and_round_trip_through_text_controls() {
    let (app, pool) = localized_app().await;
    let (status, product) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-PURCHASE",
            "name": "Localized purchase product",
            "kind": "Product",
            "category_id": null,
            "unit": "un",
            "sale_price": "10",
            "cost_price": "5",
            "track_stock": false,
            "min_stock": null,
            "max_stock": null,
            "location": null,
            "notes": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    let product_id = product["id"].as_i64().unwrap();
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Localized purchase supplier", "phone": null, "notes": null }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();

    let (status, body) = request(
        &app,
        Method::POST,
        "/web/purchases",
        Some("application/x-www-form-urlencoded"),
        true,
        format!("supplier_id={supplier_id}&payment_type=Cash&purchase_date=2026-12-31"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let purchase_id: i64 = sqlx::query_scalar("SELECT id FROM purchases WHERE supplier_id = ?")
        .bind(supplier_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (status, body) = request(
        &app,
        Method::POST,
        &format!("/web/purchases/{purchase_id}/lines"),
        Some("application/x-www-form-urlencoded"),
        true,
        format!("product_id={product_id}&product=LOC-PURCHASE&qty=1%2C25&unit_cost=2%2C50"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let line_id: i64 = sqlx::query_scalar("SELECT id FROM purchase_lines WHERE purchase_id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (status, detail) = request(
        &app,
        Method::GET,
        &format!("/web/purchases/{purchase_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(
        detail.contains("value=\"1,25\""),
        "localized purchase quantity: {detail}"
    );
    assert!(
        detail.contains("value=\"2,50\""),
        "localized purchase unit cost: {detail}"
    );

    let (status, body) = request(
        &app,
        Method::PUT,
        &format!("/web/purchases/{purchase_id}/lines/{line_id}"),
        Some("application/x-www-form-urlencoded"),
        true,
        "qty=1%2C50&unit_cost=2%2C75".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored: (String, String) =
        sqlx::query_as("SELECT qty, unit_cost FROM purchase_lines WHERE id = ?")
            .bind(line_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, ("1.50".into(), "2.75".into()));
}

#[tokio::test]
async fn localized_supplier_cost_form_stores_canonical_decimal() {
    let (app, pool) = localized_app().await;
    let (status, product) = post_json(
        &app,
        "/api/products",
        json!({
            "sku": "LOC-COST",
            "name": "Localized cost product",
            "kind": "Product",
            "category_id": null,
            "unit": "un",
            "sale_price": "10",
            "cost_price": "2",
            "track_stock": false,
            "min_stock": null,
            "max_stock": null,
            "location": null,
            "notes": null
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{product}");
    let product_id = product["id"].as_i64().unwrap();
    let (status, supplier) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Canonical cost supplier", "phone": null, "notes": null }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{supplier}");
    let supplier_id = supplier["id"].as_i64().unwrap();

    let (status, body) = request(
        &app,
        Method::POST,
        "/web/product-costs",
        Some("application/x-www-form-urlencoded"),
        true,
        format!(
            "product_id={product_id}&supplier_id={supplier_id}&cost=1.234%2C50&date=2026-12-31"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored: String = sqlx::query_scalar(
        "SELECT current_cost FROM product_supplier_costs WHERE product_id = ? AND supplier_id = ?",
    )
    .bind(product_id)
    .bind(supplier_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, "1234.50");
}
