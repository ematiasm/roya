use std::str::FromStr;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use rust_decimal::Decimal;
use sqlx::{sqlite::SqliteConnectOptions, sqlite::SqlitePoolOptions, SqlitePool};
use tower::ServiceExt;

use crate::error::AppError;
use crate::models::NewTax;
use crate::repositories::{
    ProductTaxRepository, SqliteProductTaxRepository, SqliteTaxRepository, TaxRepository,
};
use crate::routes::AppState;
use crate::security::test_support;

async fn test_state() -> AppState {
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
    test_support::seed_session(&pool).await.unwrap();
    AppState::new(pool, false, true)
}

async fn sentinel(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = 'sistema'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn create_product(pool: &SqlitePool, sku: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
         VALUES (?, ?, 'Product', 'unit', '10', 0, ?)
         RETURNING id",
    )
    .bind(sku)
    .bind(format!("Product {sku}"))
    .bind(sentinel(pool).await)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_es_ar(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO business_settings
           (id, business_name, default_locale_code, currency_code, timezone)
         VALUES (1, 'Tienda', 'es-AR', 'ARS', 'America/Argentina/Buenos_Aires')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled)
         VALUES ('es-AR', 'es', 'Español (Argentina)', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn tax_input(code: &str, rate: &str) -> NewTax {
    NewTax {
        code: code.into(),
        name: format!("Tax {code}"),
        rate: Decimal::from_str(rate).unwrap(),
        is_active: true,
    }
}

async fn json_request(
    app: axum::Router,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(
                    body.map(|value| value.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn web_request(
    app: axum::Router,
    method: &str,
    uri: &str,
    cookie: &str,
    content_type: Option<&str>,
    headers: &[(&str, &str)],
    body: String,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn tax_repository_creates_lists_updates_and_deactivates_with_audit() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let repository = SqliteTaxRepository::new(state.pool.clone());

    let first = repository
        .create(actor, &tax_input("IVA21", "21.00"))
        .await
        .unwrap();
    let second = repository
        .create(actor, &tax_input("IVA105", "10.50"))
        .await
        .unwrap();
    let listed = repository.list().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|tax| tax.code.as_str())
            .collect::<Vec<_>>(),
        vec!["IVA105", "IVA21"]
    );

    sqlx::query("UPDATE taxes SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?")
        .bind(first.id)
        .execute(&state.pool)
        .await
        .unwrap();
    let updated = repository
        .update(
            actor,
            first.id,
            "IVA210",
            "IVA 21",
            Decimal::from_str("21.50").unwrap(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(updated.code, "IVA210");
    assert_eq!(updated.rate, Decimal::from_str("21.50").unwrap());
    assert_eq!(updated.updated_by, Some(actor));
    let updated_at: String = sqlx::query_scalar("SELECT updated_at FROM taxes WHERE id = ?")
        .bind(first.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(updated_at.as_str() > "2000-01-01T00:00:00.000Z");

    let inactive = repository.deactivate(actor, first.id).await.unwrap();
    assert!(!inactive.is_active);
    assert_eq!(inactive.updated_by, Some(actor));

    let stored_rate: String = sqlx::query_scalar("SELECT rate FROM taxes WHERE id = ?")
        .bind(second.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stored_rate, "10.50");
}

#[tokio::test]
async fn product_tax_repository_supports_zero_many_duplicate_rejection_and_unlink() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-MULTI").await;
    let tax_repository = SqliteTaxRepository::new(state.pool.clone());
    let first_tax = tax_repository
        .create(actor, &tax_input("IVA21", "21"))
        .await
        .unwrap();
    let second_tax = tax_repository
        .create(actor, &tax_input("IVA105", "10.5"))
        .await
        .unwrap();
    let repository = SqliteProductTaxRepository::new(state.pool.clone());

    assert!(repository
        .list_by_product(product_id)
        .await
        .unwrap()
        .is_empty());
    repository
        .link(actor, product_id, first_tax.id)
        .await
        .unwrap();
    repository
        .link(actor, product_id, second_tax.id)
        .await
        .unwrap();
    assert_eq!(
        repository.list_by_product(product_id).await.unwrap().len(),
        2
    );

    let duplicate = repository
        .link(actor, product_id, first_tax.id)
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate, AppError::Conflict(_)),
        "got {duplicate:?}"
    );

    assert!(repository.unlink(product_id, first_tax.id).await.unwrap());
    let links = repository.list_by_product(product_id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].tax_id, second_tax.id);
    assert!(!repository.unlink(product_id, first_tax.id).await.unwrap());
}

#[tokio::test]
async fn tax_service_validates_catalog_and_association_rules() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-VALID").await;

    let empty_code = state
        .tax_service
        .create_tax(
            actor,
            NewTax {
                code: "  ".into(),
                name: "Invalid".into(),
                rate: Decimal::ONE,
                is_active: true,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(empty_code, AppError::Validation(_)));

    let negative_rate = state
        .tax_service
        .create_tax(
            actor,
            NewTax {
                code: "NEG".into(),
                name: "Negative".into(),
                rate: Decimal::from(-1),
                is_active: true,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(negative_rate, AppError::Validation(_)));

    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    let unknown_product = state
        .tax_service
        .link_product_tax(actor, 999_999, tax.id)
        .await
        .unwrap_err();
    assert!(matches!(unknown_product, AppError::NotFound(_)));

    let unknown_tax = state
        .tax_service
        .link_product_tax(actor, product_id, 999_999)
        .await
        .unwrap_err();
    assert!(matches!(unknown_tax, AppError::NotFound(_)));

    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();
    state
        .tax_service
        .deactivate_tax(actor, tax.id)
        .await
        .unwrap();
    let inactive_link = state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap_err();
    assert!(matches!(inactive_link, AppError::Conflict(_)));
}

#[tokio::test]
async fn tax_api_keeps_canonical_json_and_manages_product_links() {
    let state = test_state().await;
    let app = crate::routes::router(state.clone());
    let cookie = test_support::TEST_COOKIE;

    let (status, created) = json_request(
        app.clone(),
        "POST",
        "/api/taxes",
        cookie,
        Some(serde_json::json!({
            "code": "IVA21",
            "name": "IVA 21",
            "rate": "21.00",
            "is_active": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["rate"], "21.00");
    let first_tax_id = created["id"].as_i64().unwrap();

    let (status, second) = json_request(
        app.clone(),
        "POST",
        "/api/taxes",
        cookie,
        Some(serde_json::json!({
            "code": "IVA105",
            "name": "IVA 10.5",
            "rate": "10.50",
            "is_active": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let second_tax_id = second["id"].as_i64().unwrap();

    let (status, listed) = json_request(app.clone(), "GET", "/api/taxes", cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["taxes"].as_array().unwrap().len(), 2);

    let (status, updated) = json_request(
        app.clone(),
        "PUT",
        &format!("/api/taxes/{first_tax_id}"),
        cookie,
        Some(serde_json::json!({ "rate": "21.50" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["rate"], "21.50");

    let product_id = create_product(&state.pool, "TAX-API").await;
    for tax_id in [first_tax_id, second_tax_id] {
        let (status, link) = json_request(
            app.clone(),
            "POST",
            &format!("/api/products/{product_id}/taxes"),
            cookie,
            Some(serde_json::json!({ "tax_id": tax_id })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{link}");
        assert_eq!(link["product_id"], product_id);
        assert_eq!(link["tax_id"], tax_id);
    }
    let (status, duplicate) = json_request(
        app.clone(),
        "POST",
        &format!("/api/products/{product_id}/taxes"),
        cookie,
        Some(serde_json::json!({ "tax_id": first_tax_id })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{duplicate}");

    let (status, links) = json_request(
        app.clone(),
        "GET",
        &format!("/api/products/{product_id}/taxes"),
        cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(links["taxes"].as_array().unwrap().len(), 2);

    let (status, removed) = json_request(
        app.clone(),
        "DELETE",
        &format!("/api/products/{product_id}/taxes/{first_tax_id}"),
        cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{removed}");

    let (status, inactive) = json_request(
        app.clone(),
        "POST",
        &format!("/api/taxes/{first_tax_id}/deactivate"),
        cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inactive}");
    assert_eq!(inactive["is_active"], false);
}

#[tokio::test]
async fn tax_api_preserves_inventory_permission_boundaries() {
    let state = test_state().await;
    let app = crate::routes::router(state.clone());
    let actor = sentinel(&state.pool).await;
    let tax = SqliteTaxRepository::new(state.pool.clone())
        .create(actor, &tax_input("IVA21", "21"))
        .await
        .unwrap();
    let product_id = create_product(&state.pool, "TAX-AUTH").await;
    SqliteProductTaxRepository::new(state.pool.clone())
        .link(actor, product_id, tax.id)
        .await
        .unwrap();

    let probe = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&probe);
    for (method, uri) in [
        ("GET", "/api/taxes".to_string()),
        ("GET", format!("/api/products/{product_id}/taxes")),
    ] {
        let (status, body) = json_request(app.clone(), method, &uri, &cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for (method, uri, body) in [
        (
            "POST",
            "/api/taxes".to_string(),
            Some(serde_json::json!({
                "code": "DENIED",
                "name": "Denied",
                "rate": "1",
                "is_active": true
            })),
        ),
        (
            "PUT",
            format!("/api/taxes/{}", tax.id),
            Some(serde_json::json!({ "name": "Denied" })),
        ),
        ("POST", format!("/api/taxes/{}/deactivate", tax.id), None),
        (
            "POST",
            format!("/api/products/{product_id}/taxes"),
            Some(serde_json::json!({ "tax_id": tax.id })),
        ),
        (
            "DELETE",
            format!("/api/products/{product_id}/taxes/{}", tax.id),
            None,
        ),
    ] {
        let (status, response) = json_request(app.clone(), method, &uri, &cookie, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{response}");
    }

    let stored = state.tax_service.get_tax(tax.id).await.unwrap();
    assert_eq!(stored.name, "Tax IVA21");
    assert!(stored.is_active);
    assert_eq!(
        SqliteProductTaxRepository::new(state.pool.clone())
            .list_by_product(product_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn tax_web_uses_locale_input_and_display_and_refreshes_product_drawer() {
    let state = test_state().await;
    seed_es_ar(&state.pool).await;
    let app = crate::routes::router(state.clone());
    let cookie = test_support::TEST_COOKIE;
    let form = "application/x-www-form-urlencoded";

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/taxes",
        cookie,
        Some(form),
        &[("HX-Request", "true"), ("HX-Target", "tax-list")],
        "code=IVAWEB&name=IVA%20Web&rate=21%2C5".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tax_id: i64 = sqlx::query_scalar("SELECT id FROM taxes WHERE code = 'IVAWEB'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let stored_rate: String = sqlx::query_scalar("SELECT rate FROM taxes WHERE id = ?")
        .bind(tax_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stored_rate, "21.5");
    assert!(body.contains("21,5 %"), "{body}");

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/taxes/edit",
        cookie,
        Some(form),
        &[("HX-Request", "true"), ("HX-Target", "tax-list")],
        format!("id={tax_id}&code=IVAWEB&name=IVA%20Web%20Editado&rate=22%2C25&is_active=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("IVA Web Editado"), "{body}");
    assert!(body.contains("22,25 %"), "{body}");

    let product_id = create_product(&state.pool, "TAX-WEB").await;
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/product-taxes",
        cookie,
        Some(form),
        &[("HX-Request", "true"), ("HX-Target", "product-drawer-body")],
        format!("product_id={product_id}&tax_id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("IVA Web Editado"), "{body}");
    assert!(body.contains("22,25 %"), "{body}");

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/product-taxes/unlink",
        cookie,
        Some(form),
        &[("HX-Request", "true"), ("HX-Target", "product-drawer-body")],
        format!("product_id={product_id}&tax_id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(">Unlink<"), "{body}");

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/taxes/deactivate",
        cookie,
        Some(form),
        &[("HX-Request", "true"), ("HX-Target", "tax-list")],
        format!("id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Inactivo"), "{body}");
}
