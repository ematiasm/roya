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
    ProductTaxRepository, PurchaseRepository, SaleRepository, SqliteProductTaxRepository,
    SqlitePurchaseRepository, SqliteSaleRepository, SqliteTaxRepository,
    SqliteTaxSnapshotRepository, TaxRepository, TaxSnapshotRepository,
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

// ---------------------------------------------------------------------------
// U1 — on the web, tax definitions belong to Settings alone
// ---------------------------------------------------------------------------
//
// OWNERSHIP. A tax DEFINITION (code, name, rate, active) is business
// configuration; a product-tax LINK is inventory state. One owner each, one
// permission each. The Products screen used to carry both: `templates/
// products.html:68-96` rendered the whole catalogue and `POST /web/taxes…`
// administered it behind `inventory.write`, so the same form was reachable
// from two screens under two different permissions. U1 removes the catalogue
// from Products and keeps the association.
//
// "Settings alone" is scoped to the WEB surface throughout this block. The JSON
// API is a different surface: `POST /api/taxes`, `PUT /api/taxes/{id}` and
// `POST /api/taxes/{id}/deactivate` are still `Require<InventoryWrite>` in
// `src/routes/inventory_api.rs`, and `tax_api_preserves_inventory_permission_
// boundaries` above pins that. Pre-existing, not a U1 regression, and left
// alone because narrowing it is an open product decision — see
// `odd/tasks/product-price-ladder.md`.
//
// What U1 therefore proves, in three tests, all of it on the web:
//   1. the Products screen offers no catalogue at all;
//   2. no `inventory.write` WEB route can create, rename, re-rate or
//      activate/deactivate a tax definition — they are GONE, not refused;
//   3. `settings.manage` still can, and the drawer's association still works.

/// The Products screen is ASSOCIATION only.
///
/// Asserted two ways, because they fail differently. The rendered page proves
/// what an operator can reach; the router proves what a stale tab, a bookmarked
/// HTMX address or a replayed form could still reach. A removal that deleted
/// the markup but left a route behind would pass the first and fail the second.
#[tokio::test]
async fn tax_products_screen_offers_no_tax_catalogue() {
    let state = test_state().await;
    let app = crate::routes::router(state.clone());
    // A live tax makes the assertion meaningful: the page must not fall back to
    // rendering the catalogue's rows somewhere else once the section is gone.
    state
        .tax_service
        .create_tax(sentinel(&state.pool).await, tax_input("IVA21", "21"))
        .await
        .unwrap();

    let (status, page) = web_request(
        app.clone(),
        "GET",
        "/products",
        test_support::TEST_COOKIE,
        None,
        &[],
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");

    for gone in [
        // The section wrapper, its list island, and every address the
        // catalogue answered on.
        "id=\"tax-catalog-section\"",
        "id=\"tax-list\"",
        "hx-get=\"/web/taxes\"",
        "hx-post=\"/web/taxes\"",
        "hx-post=\"/web/taxes/edit\"",
        "hx-post=\"/web/taxes/deactivate\"",
        // And the copy that would offer the actions, so the page cannot keep a
        // control that posts nowhere.
        "Create tax",
        "Save tax",
        "Deactivate",
    ] {
        assert!(
            !page.contains(gone),
            "the Products page still serves the tax catalogue ({gone})"
        );
    }

    // What stays is the association: the page still carries the drawer that
    // links and unlinks a tax, so a tax defined in Settings is still usable
    // here. This is the half of the screen that belongs to inventory.
    assert!(
        page.contains("id=\"product-drawer-body\""),
        "the product drawer must survive the catalogue removal: {page}"
    );
}

/// THE WEB AUTHORIZATION INVARIANT, in both halves.
///
/// THE NAME IS SCOPED ON PURPOSE: `…_web_definition_…`. Everything this test
/// asserts is about the `/web/…` surface only, and the test probes only
/// `/web/taxes…`. It must not be read as a claim that `inventory.write` cannot
/// reach a tax definition ANYWHERE in the app — it can, through the JSON API.
/// See THE RESIDUAL at the bottom.
///
/// This test deliberately REPLACES
/// `tax_inventory_catalogue_keeps_its_own_write_access_for_an_inventory_writer`,
/// which pinned the opposite claim: that a principal holding only
/// `inventory.write` still created, renamed, re-rated and deactivated taxes
/// from the Products catalogue. U1 inverts that expectation on purpose, so the
/// test is rewritten rather than deleted — the same claim, with the sign
/// flipped — and the inversion is recorded in `odd/tasks/
/// product-price-ladder.md`.
///
/// HALF ONE — no `inventory.write` WEB route touches a tax DEFINITION. The
/// routes are not merely refused (403, which would still mean they exist and
/// answer); they are GONE, so 404 for every method and body. No WEB route
/// reaches `TaxService::create_tax`, `update_tax` or `deactivate_tax` through
/// the inventory gate, and the stored tax is byte-for-byte unchanged
/// afterwards. The hard delete was already unreachable there and stays so.
///
/// HALF TWO — `settings.manage` still can, on `/web/settings/taxes…`: create,
/// rename, re-rate, deactivate, activate, and the hard delete that no other
/// web route offers. Without this half the test would pass by breaking tax
/// administration entirely, which is not what this unit does.
///
/// THE RESIDUAL, so nobody upgrades this test's scope by reading its name: the
/// JSON API still administers tax definitions behind `inventory.write` —
/// `POST /api/taxes`, `PUT /api/taxes/{id}` and `POST /api/taxes/{id}/deactivate`
/// are all `Require<InventoryWrite>` in `src/routes/inventory_api.rs`, and
/// `tax_api_preserves_inventory_permission_boundaries` above pins that half of
/// the story from the other direction. That is PRE-EXISTING, not a U1
/// regression, and U1 deliberately left it alone: narrowing the API is an open
/// product decision recorded in `odd/tasks/product-price-ladder.md`, not a side
/// effect of de-duplicating the web catalogue. This test is therefore
/// deliberately NOT a global exclusivity test, and it is not allowed to become
/// one by omission — if someone later narrows the API too, the honest move is a
/// new test that says so, not a silent widening of this one.
#[tokio::test]
async fn tax_web_definition_administration_is_exclusive_to_settings_manage() {
    let state = test_state().await;
    let app = crate::routes::router(state.clone());
    let actor = sentinel(&state.pool).await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("INVCAT", "10"))
        .await
        .unwrap();
    let form = "application/x-www-form-urlencoded";
    let htmx = [("HX-Request", "true")];

    // -- HALF ONE: no inventory.write WEB route administers a definition ------
    // Every URI below is a `/web/…` address on purpose. The JSON API is a
    // separate surface that still admits this permission, and listing it here
    // would be asserting the opposite of what this test is scoped to prove.
    let token = test_support::seed_session_with_permissions(&state.pool, &["inventory.write"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&token);

    for (method, uri, payload) in [
        // create, rename + re-rate, deactivate: the three catalogue mutations.
        (
            "POST".to_string(),
            "/web/taxes".to_string(),
            "code=INVCAT2&name=Inventado&rate=99".to_string(),
        ),
        (
            "POST".to_string(),
            "/web/taxes/edit".to_string(),
            format!("id={}&code=INVCAT2&name=Renamed&rate=12.5", tax.id),
        ),
        (
            "POST".to_string(),
            "/web/taxes/deactivate".to_string(),
            format!("id={}", tax.id),
        ),
        // Activation rides the edit form's active box on that surface, so the
        // edit route is the activate route; a payload that carries it is listed
        // explicitly because a separate activate action never existed there.
        (
            "POST".to_string(),
            "/web/taxes/edit".to_string(),
            format!(
                "id={}&code=INVCAT&name=Renamed&rate=12.5&is_active=1",
                tax.id
            ),
        ),
        // And the destructive half, which was never reachable there either.
        (
            "POST".to_string(),
            "/web/taxes/delete".to_string(),
            format!("id={}&confirm=on", tax.id),
        ),
        (
            "POST".to_string(),
            format!("/web/taxes/{}/delete", tax.id),
            format!("id={}&confirm=on", tax.id),
        ),
    ] {
        let (status, body) = web_request(
            app.clone(),
            &method,
            &uri,
            &cookie,
            Some(form),
            &htmx,
            payload,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no inventory.write WEB route may administer a tax definition ({method} {uri}): {body}"
        );
    }
    // The catalogue list route goes with them: there is nothing to serve and no
    // island left to serve it into.
    for (method, uri) in [("GET", "/web/taxes"), ("POST", "/web/taxes")] {
        let (status, body) = web_request(
            app.clone(),
            method,
            uri,
            &cookie,
            if method == "POST" { Some(form) } else { None },
            &[],
            if method == "POST" {
                "code=NOPE&name=Nope&rate=1".into()
            } else {
                String::new()
            },
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the inventory catalogue list must be gone ({method} {uri}): {body}"
        );
    }

    // Nothing above wrote: the definition is exactly as it was created.
    let untouched = state.tax_service.get_tax(tax.id).await.unwrap();
    assert_eq!(untouched.code, "INVCAT");
    assert_eq!(untouched.name, "Tax INVCAT");
    assert_eq!(untouched.rate, dec("10"));
    assert!(untouched.is_active);
    assert!(
        SqliteTaxRepository::new(state.pool.clone())
            .find_by_code("INVCAT2")
            .await
            .unwrap()
            .is_none(),
        "a refused inventory request may never create a tax"
    );

    // -- HALF TWO: settings.manage still owns every definition mutation ------
    let settings_cookie = test_support::cookie_for(
        &test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
            .await
            .unwrap(),
    );

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes",
        &settings_cookie,
        Some(form),
        &htmx,
        "code=NEWSET&name=Settings+tax&rate=21".into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created: i64 = sqlx::query_scalar("SELECT id FROM taxes WHERE code = 'NEWSET'")
        .fetch_one(&state.pool)
        .await
        .unwrap();

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/edit",
        &settings_cookie,
        Some(form),
        &htmx,
        // `is_active=1` mirrors the row form's active box, which is the edit
        // route's authority — the same contract the Products catalogue row had.
        format!("id={created}&code=NEWSET&name=Renamed+in+settings&rate=12.5&is_active=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let edited = state.tax_service.get_tax(created).await.unwrap();
    assert_eq!(edited.name, "Renamed in settings");
    assert_eq!(edited.rate, dec("12.5"));
    assert!(edited.is_active);

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/deactivate",
        &settings_cookie,
        Some(form),
        &htmx,
        format!("id={created}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!state.tax_service.get_tax(created).await.unwrap().is_active);

    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/activate",
        &settings_cookie,
        Some(form),
        &htmx,
        format!("id={created}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(state.tax_service.get_tax(created).await.unwrap().is_active);

    // The irreversible half, which exists on no other surface: a linked product
    // keeps the tax, so the refusal is the safeguard rather than a dead end.
    let product_id = create_product(&state.pool, "TAX-SETTINGS-DELETE").await;
    state
        .tax_service
        .link_product_tax(actor, product_id, created)
        .await
        .unwrap();
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/delete",
        &settings_cookie,
        Some(form),
        &htmx,
        format!("id={created}&confirm=on"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        state.tax_service.get_tax(created).await.is_ok(),
        "a refused delete must keep the tax"
    );
}

/// The drawer's ASSOCIATION half is untouched by the catalogue removal, and it
/// still formats the tax through the request's locale.
///
/// Before U1 this test also drove the catalogue (`POST /web/taxes` to create a
/// tax from a localized `21,5`, `…/edit` to re-rate it, `…/deactivate` last).
/// Those addresses are gone, so the definition is now created through the
/// surface that owns it — `/web/settings/taxes` — and everything asserted about
/// the drawer afterwards is the same assertion, unchanged: the localized
/// percentage renders, linking shows the tax, and unlinking answers with the
/// refreshed drawer body that no longer offers an unlink for it.
#[tokio::test]
async fn tax_product_drawer_links_and_unlinks_and_keeps_the_localized_rate() {
    let state = test_state().await;
    seed_es_ar(&state.pool).await;
    let app = crate::routes::router(state.clone());
    let cookie = test_support::TEST_COOKIE;
    let form = "application/x-www-form-urlencoded";
    let drawer = [("HX-Request", "true"), ("HX-Target", "product-drawer-body")];

    // The definition, created through Settings with a locale-written rate.
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes",
        cookie,
        Some(form),
        &[("HX-Request", "true")],
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
    assert_eq!(
        stored_rate, "21.5",
        "a localized rate is parsed to the canonical decimal"
    );
    assert!(body.contains("21,5 %"), "{body}");

    // Re-rate it through Settings, the way an operator changes a rate now.
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/edit",
        cookie,
        Some(form),
        &[("HX-Request", "true")],
        // `is_active=1` is what the real row form carries: the edit route reads
        // the active box off the body, so a re-rate that omitted it would
        // deactivate the tax. This is the retained surface's existing contract,
        // unchanged by U1 — the old Products catalogue row behaved the same way.
        format!("id={tax_id}&code=IVAWEB&name=IVA%20Web%20Editado&rate=22%2C25&is_active=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("IVA Web Editado"), "{body}");
    assert!(body.contains("22,25 %"), "{body}");

    // The association: linking answers the refreshed drawer body, which shows
    // the linked tax and its localized rate.
    let product_id = create_product(&state.pool, "TAX-WEB").await;
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/product-taxes",
        cookie,
        Some(form),
        &drawer,
        format!("product_id={product_id}&tax_id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("IVA Web Editado"), "{body}");
    assert!(body.contains("22,25 %"), "{body}");

    // And unlinking answers the same refreshed body, which no longer offers an
    // unlink for the tax that is gone.
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/product-taxes/unlink",
        cookie,
        Some(form),
        &drawer,
        format!("product_id={product_id}&tax_id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(">Unlink<"), "{body}");

    // A deactivated tax still links — deactivation is a definition lifecycle
    // action, and the association picker offers whatever is active. The drawer
    // then says so, which is the last thing the old catalogue used to render on
    // the Products page and the only reason the copy key is still referenced.
    let (status, body) = web_request(
        app.clone(),
        "POST",
        "/web/settings/taxes/deactivate",
        cookie,
        Some(form),
        &[("HX-Request", "true")],
        format!("id={tax_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Inactivo"), "{body}");
}

// ---------------------------------------------------------------------------
// T3 — the hard-delete safeguard
// ---------------------------------------------------------------------------
//
// Deletion is the ONE tax operation that can destroy meaning rather than edit
// it, so its contract is stated in full here: a tax may be removed only when
// nothing references it. Two independent reference families block it, and they
// say different things to the operator:
//
//   * a `product_taxes` link is CURRENT state the operator can undo — the
//     remedy is to unlink the product;
//   * a `sale_line_taxes` / `purchase_line_taxes` snapshot is FROZEN history
//     the application will never rewrite — the remedy is to keep the tax.
//
// When BOTH block the tax, the history conflict is the one reported: no amount
// of unlinking can ever free a tax a document already froze, so sending the
// operator to unlink products first would be a pointless errand. The count that
// matters is the one that cannot be acted on.

/// A Draft sale header for the snapshot fixtures below.
async fn create_sale(pool: &SqlitePool) -> i64 {
    let actor = sentinel(pool).await;
    let customer: i64 = sqlx::query_scalar(
        "INSERT INTO customers (name, created_by) VALUES ('Delete buyer', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)
         VALUES ('Draft', 'Cash', ?, 'Delete buyer', '2024-05-01', ?) RETURNING id",
    )
    .bind(customer)
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A Draft purchase header for the snapshot fixtures below.
async fn create_purchase(pool: &SqlitePool) -> i64 {
    let actor = sentinel(pool).await;
    let supplier: i64 = sqlx::query_scalar(
        "INSERT INTO suppliers (name, created_by) VALUES ('Delete supplier', ?) RETURNING id",
    )
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by)
         VALUES (?, 'Draft', 'Cash', '2024-05-01', ?) RETURNING id",
    )
    .bind(supplier)
    .bind(actor)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

#[tokio::test]
async fn tax_hard_delete_removes_an_unreferenced_tax() {
    let state = test_state().await;
    let tax = state
        .tax_service
        .create_tax(sentinel(&state.pool).await, tax_input("IVA21", "21"))
        .await
        .unwrap();

    state.tax_service.delete_tax(tax.id).await.unwrap();

    assert!(
        state.tax_service.get_tax(tax.id).await.is_err(),
        "an unreferenced tax is removed by the hard delete"
    );
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM taxes WHERE id = ?")
        .bind(tax.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0, "the row is really gone, not just unread");
}

#[tokio::test]
async fn tax_hard_delete_of_an_unknown_tax_is_not_found() {
    let state = test_state().await;
    let error = state.tax_service.delete_tax(999_999).await.unwrap_err();
    assert!(matches!(error, AppError::NotFound(_)), "got {error:?}");
}

/// A deactivated tax is still current catalogue state: as long as a product is
/// linked to it, deactivation is not a licence to delete it.
#[tokio::test]
async fn tax_hard_delete_is_refused_while_a_product_is_linked() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-LINK").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();

    let error = state.tax_service.delete_tax(tax.id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)), "got {error:?}");
    assert!(
        state.tax_service.get_tax(tax.id).await.is_ok(),
        "a refused delete writes nothing"
    );

    let counts = state.tax_service.tax_references(tax.id).await.unwrap();
    assert_eq!(counts.product_links, 1);
    assert_eq!(counts.document_snapshots, 0);

    state
        .tax_service
        .unlink_product_tax(product_id, tax.id)
        .await
        .unwrap();
    state.tax_service.delete_tax(tax.id).await.unwrap();
    assert!(state.tax_service.get_tax(tax.id).await.is_err());
}

/// The HISTORY refusal is the one sentence an operator must be able to read at
/// the route, not only at the service: it is the message that tells them the
/// tax is kept and the remedy is to deactivate it, which is advice the
/// product-link message cannot give.
///
/// The fixture is a real line write through the real repository, so the
/// snapshot the refusal counts is one the application itself produced, and the
/// product link is cleared first so the snapshot is the ONLY blocker — the same
/// "one reason at a time" shape a reviewer has to be able to trust.
#[tokio::test]
async fn tax_delete_route_refusal_for_a_recorded_document_names_the_history_rule() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-HIST-ROUTE").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVAHIST", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();
    let sale = create_sale(&state.pool).await;
    SqliteSaleRepository::new(state.pool.clone())
        .create_line(sale, product_id, dec("1"), dec("100"))
        .await
        .unwrap();
    state
        .tax_service
        .unlink_product_tax(product_id, tax.id)
        .await
        .unwrap();

    let app = crate::routes::router(state.clone());
    let (status, response) = web_request(
        app,
        "POST",
        "/web/settings/taxes/delete",
        test_support::TEST_COOKIE,
        Some("application/x-www-form-urlencoded"),
        &[("HX-Request", "true")],
        format!("id={}&confirm=on", tax.id),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert!(
        response.contains("already recorded this tax"),
        "the refusal must state the history rule, not the product one: {response}"
    );
    assert!(
        response.contains("Deactivate it instead"),
        "the history refusal must carry its only remedy: {response}"
    );
    assert!(
        !response.contains("still linked to products"),
        "with the product link cleared, the product message would be a lie: \
         {response}"
    );
    for leak in ["FOREIGN KEY", "UNIQUE", "SQLITE", "sqlite", "no such"] {
        assert!(
            !response.contains(leak),
            "the refusal must not leak database text ({leak}): {response}"
        );
    }
    assert!(
        state.tax_service.get_tax(tax.id).await.is_ok(),
        "a refused delete writes nothing"
    );
}

#[tokio::test]
async fn tax_hard_delete_is_refused_by_a_sale_line_snapshot() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-SALE").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();
    let sale = create_sale(&state.pool).await;
    SqliteSaleRepository::new(state.pool.clone())
        .create_line(sale, product_id, dec("1"), dec("100"))
        .await
        .unwrap();
    // The product link is cleared first so the ONLY remaining reference is the
    // frozen snapshot: this fixture proves the snapshot alone blocks the delete.
    state
        .tax_service
        .unlink_product_tax(product_id, tax.id)
        .await
        .unwrap();

    let error = state.tax_service.delete_tax(tax.id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)), "got {error:?}");
    assert!(
        state.tax_service.get_tax(tax.id).await.is_ok(),
        "a snapshot reference keeps the tax for history"
    );
    let counts = state.tax_service.tax_references(tax.id).await.unwrap();
    assert_eq!(counts.product_links, 0);
    assert_eq!(counts.document_snapshots, 1);

    // The snapshot's own copy of the facts survives untouched: deleting the tax
    // definition is refused, and history is never rewritten.
    let snapshots = SqliteTaxSnapshotRepository::new(state.pool.clone())
        .list_sale_line_taxes(
            sqlx::query_scalar::<_, i64>("SELECT id FROM sale_lines WHERE sale_id = ? ORDER BY id")
                .bind(sale)
                .fetch_one(&state.pool)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].code, "IVA21");
    assert_eq!(snapshots[0].rate, dec("21"));
}

#[tokio::test]
async fn tax_hard_delete_is_refused_by_a_purchase_line_snapshot() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-PUR").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();
    let purchase = create_purchase(&state.pool).await;
    SqlitePurchaseRepository::new(state.pool.clone())
        .create_line(purchase, product_id, dec("1"), dec("100"))
        .await
        .unwrap();
    state
        .tax_service
        .unlink_product_tax(product_id, tax.id)
        .await
        .unwrap();

    let error = state.tax_service.delete_tax(tax.id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)), "got {error:?}");
    assert!(state.tax_service.get_tax(tax.id).await.is_ok());
    let counts = state.tax_service.tax_references(tax.id).await.unwrap();
    assert_eq!(counts.product_links, 0);
    assert_eq!(counts.document_snapshots, 1);
}

/// Both reference families present: the HISTORY conflict is the one reported,
/// because it is the one no operator action can clear.
#[tokio::test]
async fn tax_hard_delete_reports_the_history_conflict_first_when_both_reference_the_tax() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-BOTH").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();
    let sale = create_sale(&state.pool).await;
    SqliteSaleRepository::new(state.pool.clone())
        .create_line(sale, product_id, dec("1"), dec("100"))
        .await
        .unwrap();

    let counts = state.tax_service.tax_references(tax.id).await.unwrap();
    assert_eq!(counts.product_links, 1);
    assert_eq!(counts.document_snapshots, 1);

    let error = state.tax_service.delete_tax(tax.id).await.unwrap_err();
    let AppError::Conflict(message) = error else {
        panic!("expected the history conflict, got {error:?}");
    };
    assert_eq!(
        message,
        crate::services::taxes::TAX_DELETE_BLOCKED_BY_HISTORY,
        "with both blockers the permanent one is reported first"
    );
    assert!(state.tax_service.get_tax(tax.id).await.is_ok());
}

/// The database backstop stays a backstop: if a reference appears between the
/// application's count and its DELETE, the `ON DELETE RESTRICT` refusal is
/// mapped to the same actionable conflict and never surfaces as a raw 500.
#[tokio::test]
async fn tax_hard_delete_maps_the_foreign_key_backstop_to_a_conflict() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-RACE").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();

    // Straight to the repository, with no application-level count in front of
    // it: this is exactly the shape a lost race has.
    let error = SqliteTaxRepository::new(state.pool.clone())
        .hard_delete(tax.id)
        .await
        .unwrap_err();
    assert!(
        matches!(error, AppError::Conflict(_)),
        "the RESTRICT refusal is a conflict, not a database fault: {error:?}"
    );
    let message = error.to_string();
    for leak in ["FOREIGN KEY", "SQLITE", "sqlite", "no such column"] {
        assert!(
            !message.contains(leak),
            "the mapped conflict must not carry database text ({leak}): {message}"
        );
    }
    assert!(state.tax_service.get_tax(tax.id).await.is_ok());
}

/// The application-level refusal never leaks SQL either, and it says which
/// reference family blocks the tax.
#[tokio::test]
async fn tax_hard_delete_conflicts_never_carry_raw_sqlite_text() {
    let state = test_state().await;
    let actor = sentinel(&state.pool).await;
    let product_id = create_product(&state.pool, "TAX-DEL-LEAK").await;
    let tax = state
        .tax_service
        .create_tax(actor, tax_input("IVA21", "21"))
        .await
        .unwrap();
    state
        .tax_service
        .link_product_tax(actor, product_id, tax.id)
        .await
        .unwrap();

    let message = state
        .tax_service
        .delete_tax(tax.id)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        format!(
            "conflict: {}",
            crate::services::taxes::TAX_DELETE_BLOCKED_BY_PRODUCTS
        )
    );
    for leak in ["FOREIGN KEY", "SQLITE", "sqlite", "product_taxes", "SELECT"] {
        assert!(!message.contains(leak), "{leak} leaked: {message}");
    }
}
