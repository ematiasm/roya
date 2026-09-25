//! End-to-end smoke tests for the whole app, driven over the HTTP surface.
//!
//! These tests build the app exactly like `main` does (in-memory SQLite with the
//! real migrations plus `AppState::new`) and send every request through the full
//! Axum router, including form extraction and Askama rendering. They exist
//! because two real bugs shipped with a green unit suite:
//!
//! 1. HTMX posts the literal `hx-post` value and never reads the form `action`,
//!    so typed-id forms pointed at `/web/sales/0/...` and 404'd from the browser.
//! 2. Accounts created through the web form had an empty payment-method
//!    allowlist, so every payment was rejected with 400.
//!
//! [`assert_htmx_targets_are_wired`] catches class 1 on every rendered page, and
//! the flow tests catch class 2 (plus the rest of the money behaviour) by
//! exercising the same routes the templates render.
//!
//! Note on the harness: this crate's `axum-test` dev-dependency resolves to an
//! axum 0.7-only release while the app runs axum 0.8, so `TestServer::new` does
//! not accept this `Router`. Requests therefore go through
//! `tower::ServiceExt::oneshot`, like every other route test in the repo. That
//! still drives the complete router, extractors and templates; it only skips the
//! TCP socket.

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::str::FromStr;
use tower::ServiceExt;

use crate::routes::AppState;
use crate::security::test_support;

/// Substring the router-level 404 fallback must render. Handler-level 404s carry
/// their own message, so this marker is what makes a routing miss observable.
const ROUTE_FALLBACK_MARKER: &str = "route not found";

/// Path that must never be registered; the oracle self-check probes it.
const MISSING_ROUTE_PROBE: &str = "/__smoke_missing_route__";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Build the same in-memory app the existing route tests use.
async fn test_app() -> (Router, SqlitePool) {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        // Same posture as db::create_pool: the customer triggers fire under
        // REPLACE conflict resolution too.
        .pragma("recursive_triggers", "1");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    // S1b part 1: seed the fixed test session every request will authenticate with.
    test_support::seed_session(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), false, true);
    (crate::routes::router(state), pool)
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    htmx: bool,
    body: String,
) -> (StatusCode, String) {
    let mut builder = test_support::with_cookie(Request::builder().method(method).uri(uri));
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    send(app, "GET", uri, None, false, String::new()).await
}

/// The opening tag that carries `needle`, for attribute assertions such as
/// the creation action's `onclick` (the same helper the route tests use).
fn element_tag_containing<'a>(html: &'a str, needle: &str) -> &'a str {
    let pos = html
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
    let start = html[..pos]
        .rfind('<')
        .expect("the attribute must sit inside a tag");
    let end = pos + html[pos..].find('>').expect("unterminated tag");
    &html[start..=end]
}

/// FIX-4: a browser must be able to load the vendored assets before a session
/// exists, so the property is asserted over HTTP and without any cookie. The
/// route is on the public allowlist; `is_public` alone would not catch a
/// regression in the middleware that still refused the request.
#[tokio::test]
async fn static_assets_load_without_a_session() {
    let (app, _pool) = test_app().await;
    for uri in ["/static/htmx.min.js", "/static/tailwind.css"] {
        let (status, body) = send_anonymous(&app, "GET", uri, None, false, String::new()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{uri} must load without a session; body: {body:.200}"
        );
    }
}

/// Same as [`send`] but without the shared session cookie: only the targets
/// that must be reachable anonymously (the static assets) use it.
async fn send_anonymous(
    app: &Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    htmx: bool,
    body: String,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// POST the way HTMX does: `hx-post` target plus the `HX-Request` header.
async fn post_form(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
    send(
        app,
        "POST",
        uri,
        Some("application/x-www-form-urlencoded"),
        true,
        body.to_string(),
    )
    .await
}

/// POST a plain full-page form (no `HX-Request`): the redirect branch. The
/// response's `Location` header is the assertion, not the body.
async fn post_form_plain(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
    let builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    let builder = test_support::with_cookie(builder);
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (status, location)
}

/// POST the way the drawer's inline edit form does: `HX-Request` plus
/// `HX-Target: product-drawer-body`, so the handler takes the drawer branch and
/// answers the fresh detail fragment (see `web_edit_product`'s `from_drawer`).
/// [`post_form`] covers the non-drawer HTMX caller; this is the drawer's shape.
async fn post_drawer_form(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("HX-Target", "product-drawer-body"),
    );
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// POST a plain browser form (no HTMX header), e.g. the account detail form.
async fn post_browser_form(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
    send(
        app,
        "POST",
        uri,
        Some("application/x-www-form-urlencoded"),
        false,
        body.to_string(),
    )
    .await
}

async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, String) {
    send(
        app,
        "POST",
        uri,
        Some("application/json"),
        false,
        body.to_string(),
    )
    .await
}

/// PUT/POST JSON as ANOTHER principal: the cookie value comes from the caller
/// (see `seed_session_with_permissions`), so a test can drive a second user
/// next to the shared fixture session.
async fn send_json_with_cookie(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
    cookie: &str,
) -> (StatusCode, String) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header("content-type", "application/json");
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// A JSON mutation as a given principal.
async fn post_json_with_cookie(
    app: &Router,
    uri: &str,
    body: Value,
    cookie: &str,
) -> (StatusCode, String) {
    send_json_with_cookie(app, "PUT", uri, body, cookie).await
}

/// GET as ANOTHER principal: the cookie value comes from the caller (see
/// `seed_session_with_permissions`), so a test can drive a second user's page
/// loads next to the shared fixture session. `htmx` adds the `HX-Request`
/// header, the way the browser's filter form fetches a fragment.
async fn get_as(app: &Router, uri: &str, cookie: &str, htmx: bool) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", cookie);
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// GET a full page as ANOTHER principal.
async fn get_with_cookie(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
    get_as(app, uri, cookie, false).await
}

/// GET a fragment as ANOTHER principal, with the `HX-Request` header the
/// browser's filter form sends.
async fn get_fragment_with_cookie(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
    get_as(app, uri, cookie, true).await
}

/// POST a form as ANOTHER principal: the cookie value comes from the caller
/// (see `seed_session_with_permissions`), so a test can drive a second user
/// through the HTMX form endpoints next to the shared fixture session.
async fn post_form_with_cookie(
    app: &Router,
    uri: &str,
    body: &str,
    cookie: &str,
) -> (StatusCode, String) {
    let builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("HX-Request", "true");
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn json_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("expected JSON, got {body:.400}: {e}"))
}

fn dec(value: &Value) -> Decimal {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("expected a decimal string, got {value}"));
    Decimal::from_str(raw).unwrap_or_else(|e| panic!("invalid decimal {raw}: {e}"))
}

// ---------------------------------------------------------------------------
// Seeding helpers: every business entity is created through the routes the
// browser uses; ids are derived from the created data, never hardcoded.
// ---------------------------------------------------------------------------

async fn method_id(pool: &SqlitePool, name: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM payment_methods WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The method row one account owns (same names repeat across accounts).
async fn account_method_id(pool: &SqlitePool, account_id: i64, name: &str) -> i64 {
    let row: (i64,) =
        sqlx::query_as("SELECT id FROM payment_methods WHERE account_id = ? AND name = ?")
            .bind(account_id)
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
    row.0
}

async fn account_id_by_name(pool: &SqlitePool, name: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn product_id_by_sku(pool: &SqlitePool, sku: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM products WHERE sku = ?")
        .bind(sku)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn supplier_id_by_name(pool: &SqlitePool, name: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM suppliers WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn customer_id_by_name(pool: &SqlitePool, name: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM customers WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn create_account_via_web(
    app: &Router,
    pool: &SqlitePool,
    name: &str,
    method_ids: &[i64],
) -> i64 {
    let mut body = format!("name={name}");
    for id in method_ids {
        body.push_str(&format!("&method_ids={id}"));
    }
    let (status, resp) = post_form(app, "/web/accounts", &body).await;
    assert_eq!(status, StatusCode::OK, "create account {name}: {resp}");
    account_id_by_name(pool, name).await
}

async fn create_product_via_web(
    app: &Router,
    pool: &SqlitePool,
    sku: &str,
    min: &str,
    max: &str,
) -> i64 {
    let body = format!(
        "sku={sku}&name=product+{sku}&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock={min}&max_stock={max}"
    );
    let (status, resp) = post_form(app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "create product {sku}: {resp}");
    product_id_by_sku(pool, sku).await
}

async fn record_stock_via_web(app: &Router, product_id: i64, qty: &str) {
    let body = format!("product_id={product_id}&type=In&qty={qty}&reason=Initial&date=2024-05-01");
    let (status, resp) = post_form(app, "/web/stock-movements", &body).await;
    assert_eq!(status, StatusCode::OK, "record stock: {resp}");
}

async fn create_supplier_via_web(app: &Router, pool: &SqlitePool, name: &str) -> i64 {
    let body = format!("name={name}&phone=555&notes=smoke");
    let (status, resp) = post_form(app, "/web/suppliers", &body).await;
    assert_eq!(status, StatusCode::OK, "create supplier {name}: {resp}");
    supplier_id_by_name(pool, name).await
}

async fn record_supplier_cost_via_web(app: &Router, product_id: i64, supplier_id: i64, cost: &str) {
    let body =
        format!("product_id={product_id}&supplier_id={supplier_id}&cost={cost}&date=2024-05-01");
    let (status, resp) = post_form(app, "/web/supplier-costs", &body).await;
    assert_eq!(status, StatusCode::OK, "record supplier cost: {resp}");
}

async fn seed_customer(
    pool: &SqlitePool,
    name: &str,
    credit_limit: Option<&str>,
    due_days: Option<i64>,
) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO customers (name, credit_limit, due_days, created_by) \
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(credit_limit)
    .bind(due_days)
    .bind(test_support::audit_actor_id(pool).await.unwrap())
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

/// Post to the sale form with an explicit customer id and return the created
/// draft's id (the last sale for that customer in the list).
async fn create_sale_draft_for_customer(
    app: &Router,
    customer_id: i64,
    payment_type: &str,
    due_date: &str,
) -> i64 {
    let body = format!(
        "customer_id={customer_id}&payment_type={payment_type}&sale_date=2024-05-02&due_date={due_date}"
    );
    let (status, resp) = post_form(app, "/web/sales", &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create sale for customer {customer_id}: {resp}"
    );
    let (status, body) = get(app, "/api/sales").await;
    assert_eq!(status, StatusCode::OK, "list sales: {body}");
    let v = json_body(&body);
    v["sales"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["sale"]["customer_id"] == json!(customer_id))
        .last()
        .and_then(|d| d["sale"]["id"].as_i64())
        .unwrap_or_else(|| panic!("sale for customer {customer_id} not found: {v}"))
}

async fn create_sale_draft_via_web(
    app: &Router,
    pool: &SqlitePool,
    customer: &str,
    payment_type: &str,
    due_date: &str,
) -> i64 {
    let customer_id = seed_customer(pool, customer, None, None).await;
    create_sale_draft_for_customer(app, customer_id, payment_type, due_date).await
}

async fn add_sale_line_via_web(app: &Router, sale_id: i64, product_id: i64, qty: &str) {
    let body = format!("sale_id={sale_id}&product_id={product_id}&qty={qty}");
    let (status, resp) = post_form(app, "/web/sales/lines", &body).await;
    assert_eq!(status, StatusCode::OK, "add sale line: {resp}");
}

async fn confirm_sale_via_web(app: &Router, sale_id: i64, method_id: Option<i64>) {
    let mut body = format!("sale_id={sale_id}");
    if let Some(method) = method_id {
        body.push_str(&format!("&method_id={method}"));
    }
    let (status, resp) = post_form(app, "/web/sales/confirm", &body).await;
    assert_eq!(status, StatusCode::OK, "confirm sale {sale_id}: {resp}");
}

async fn pay_sale_via_web(
    app: &Router,
    sale_id: i64,
    method_id: i64,
    amount: &str,
) -> (StatusCode, String) {
    let body = format!("sale_id={sale_id}&method_id={method_id}&amount={amount}&date=2024-05-10");
    post_form(app, "/web/sales/payments", &body).await
}

async fn find_only_purchase_id(app: &Router) -> i64 {
    let (status, body) = get(app, "/api/purchases").await;
    assert_eq!(status, StatusCode::OK, "list purchases: {body}");
    let v = json_body(&body);
    v["purchases"]
        .as_array()
        .and_then(|purchases| purchases.first())
        .and_then(|detail| detail["purchase"]["id"].as_i64())
        .unwrap_or_else(|| panic!("no purchase found: {v}"))
}

async fn sale_detail(app: &Router, sale_id: i64) -> Value {
    let (status, body) = get(app, &format!("/api/sales/{sale_id}")).await;
    assert_eq!(status, StatusCode::OK, "sale {sale_id} detail: {body}");
    json_body(&body)
}

async fn purchase_detail(app: &Router, purchase_id: i64) -> Value {
    let (status, body) = get(app, &format!("/api/purchases/{purchase_id}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "purchase {purchase_id} detail: {body}"
    );
    json_body(&body)
}

async fn stock_of(app: &Router, product_id: i64) -> Decimal {
    let (status, body) = get(app, &format!("/api/products/{product_id}/stock")).await;
    assert_eq!(status, StatusCode::OK, "stock for {product_id}: {body}");
    dec(&json_body(&body)["stock"])
}

async fn transactions_for(app: &Router, account_id: i64) -> Vec<Value> {
    let (status, body) = get(app, &format!("/api/transactions?account_id={account_id}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "transactions for {account_id}: {body}"
    );
    json_body(&body)["transactions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn payment_methods_catalog(app: &Router, account_id: i64) -> Value {
    let (status, body) = get(app, &format!("/api/accounts/{account_id}/payment-methods")).await;
    assert_eq!(status, StatusCode::OK, "catalog for {account_id}: {body}");
    json_body(&body)
}

// ---------------------------------------------------------------------------
// Deliverable 2: generic HTMX form-wiring guard
// ---------------------------------------------------------------------------

/// HTMX request attributes and the HTTP verb each one actually sends. The verb
/// matters: htmx issues exactly this method, so a target whose path is
/// registered for another verb is as dead as an unregistered path.
const HTMX_ATTRIBUTES: [(&str, &str); 5] = [
    ("hx-get", "GET"),
    ("hx-post", "POST"),
    ("hx-put", "PUT"),
    ("hx-patch", "PATCH"),
    ("hx-delete", "DELETE"),
];

/// One request attribute rendered by a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedTarget {
    attr: String,
    method: String,
    target: String,
    /// The attribute sits on a `<form>` (shell wiring) instead of a data-bound
    /// row/link. Where `concrete_ids_are_defects` is set, a form target must
    /// never hardcode an id.
    form_bound: bool,
}

/// A native `<form ...>` opening tag: `action`/`onsubmit` wiring that htmx
/// ignores but a browser follows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedForm {
    action: Option<String>,
    method: String,
    onsubmit: Option<String>,
}

fn path_of(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or(target)
}

fn path_segments(target: &str) -> Vec<&str> {
    path_of(target)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Every `<form ...>...</form>` region on the page (templates never nest forms).
fn form_regions(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<form") {
        let after = &rest[start..];
        let end = after
            .find("</form>")
            .map(|e| e + "</form>".len())
            .unwrap_or(after.len());
        out.push(&after[..end]);
        rest = &after[end..];
    }
    out
}

/// Value of a double-quoted attribute inside an HTML tag. The attribute name must
/// start at the tag or after whitespace, so `data-action` is not read as
/// `action` and `hx-method` is not read as `method`.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let mut offset = 0;
    while let Some(found) = tag[offset..].find(&needle) {
        let start = offset + found;
        let at_boundary = tag[..start]
            .chars()
            .next_back()
            .map(|c| c.is_ascii_whitespace())
            .unwrap_or(true);
        if at_boundary {
            let after = &tag[start + needle.len()..];
            let end = after.find('"')?;
            return Some(&after[..end]);
        }
        offset = start + needle.len();
    }
    None
}

fn htmx_targets_in(html: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (attr, method) in HTMX_ATTRIBUTES {
        let needle = format!("{attr}=\"");
        let mut rest = html;
        while let Some(start) = rest.find(&needle) {
            let after = &rest[start + needle.len()..];
            let end = after
                .find('"')
                .unwrap_or_else(|| panic!("unterminated {attr} attribute"));
            out.push((
                attr.to_string(),
                method.to_string(),
                after[..end].to_string(),
            ));
            rest = &after[end..];
        }
    }
    out
}

/// Every `hx-*` request attribute on a rendered page with its verb and whether
/// it belongs to a `<form>`, deduplicated by (attribute, target).
fn extract_htmx_targets(html: &str) -> Vec<RenderedTarget> {
    let mut out: Vec<RenderedTarget> = Vec::new();
    let mut push = |attr: String, method: String, target: String, form_bound: bool| {
        if let Some(existing) = out
            .iter_mut()
            .find(|t| t.attr == attr && t.target == target)
        {
            existing.form_bound |= form_bound;
        } else {
            out.push(RenderedTarget {
                attr,
                method,
                target,
                form_bound,
            });
        }
    };
    for (attr, method, target) in htmx_targets_in(html) {
        push(attr, method, target, false);
    }
    for region in form_regions(html) {
        for (attr, method, target) in htmx_targets_in(region) {
            push(attr, method, target, true);
        }
    }
    out
}

fn extract_rendered_forms(html: &str) -> Vec<RenderedForm> {
    form_regions(html)
        .into_iter()
        .filter_map(|region| {
            let open_end = region.find('>')?;
            let tag = &region[..open_end];
            Some(RenderedForm {
                action: attr_value(tag, "action").map(str::to_string),
                method: attr_value(tag, "method")
                    .map(|m| m.to_ascii_uppercase())
                    .unwrap_or_else(|| "GET".to_string()),
                onsubmit: attr_value(tag, "onsubmit").map(str::to_string),
            })
        })
        .collect()
}

/// Decode the HTML entities templates can use to hide JS quotes from the raw
/// attribute scan (`&quot;GET&quot;`). `&amp;` is replaced last so `&amp;quot;`
/// stays literal instead of becoming a quote.
fn decode_html_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Values of `hx-on` attributes (all event variants), which carry JS bodies.
/// Single- and double-quoted attribute values are both supported, and entities
/// are decoded before scanning.
fn hx_on_values(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("hx-on") {
        let after = &rest[start..];
        let dq = after.find("=\"");
        let sq = after.find("='");
        let (quote, eq) = match (dq, sq) {
            (Some(d), Some(s)) if d <= s => ('"', d),
            (Some(d), None) => ('"', d),
            (_, Some(s)) => ('\'', s),
            (None, None) => {
                rest = &after["hx-on".len()..];
                continue;
            }
        };
        let value = &after[eq + 2..];
        if let Some(end) = value.find(quote) {
            out.push(decode_html_entities(&value[..end]));
            rest = &value[end..];
        } else {
            rest = &after["hx-on".len()..];
        }
    }
    out
}

/// Bodies of inline `<script>...</script>` blocks, entity-decoded. An external
/// `<script src="...">` has an empty body, so its attribute never becomes a
/// candidate.
fn inline_script_bodies(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let body = &after[open_end + 1..];
        let end = body.find("</script>").unwrap_or(body.len());
        out.push(decode_html_entities(&body[..end]));
        rest = &body[end..];
    }
    out
}

/// A string literal in a JS-ish blob: quoted or backtick, with the byte range
/// needed to inspect its surroundings and whether a backtick interpolates.
struct JsLiteral {
    start: usize,
    end: usize,
    text: String,
    interpolated: bool,
}

/// Every `'...'`, `"..."` and `` `...` `` literal in a blob. Backticks are kept
/// so plain templates become verifiable URLs and interpolated ones can be
/// rejected by the convention check.
fn js_literals(blob: &str) -> Vec<JsLiteral> {
    let mut out = Vec::new();
    let mut chars = blob.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '\'' && c != '"' && c != '`' {
            continue;
        }
        let mut text = String::new();
        let mut interpolated = false;
        let mut end = blob.len();
        while let Some((index, next)) = chars.next() {
            if next == '\\' {
                if let Some((_, escaped)) = chars.next() {
                    text.push(escaped);
                }
                continue;
            }
            if next == '$' && c == '`' && matches!(chars.peek(), Some(&(_, '{'))) {
                interpolated = true;
            }
            if next == c {
                end = index + next.len_utf8();
                break;
            }
            text.push(next);
        }
        out.push(JsLiteral {
            start,
            end,
            text,
            interpolated,
        });
    }
    out
}

/// Application path literal: a single leading `/`, no whitespace, and not the
/// start of a `//` protocol-relative URL or a `/*` comment string.
fn is_app_path(literal: &str) -> bool {
    literal.starts_with('/')
        && !literal.starts_with("//")
        && !literal.starts_with("/*")
        && !literal.chars().any(char::is_whitespace)
}

/// An interpolated template that builds an application path, e.g.
/// `` `/web/sales/${id}/confirm` ``.
fn templated_path(template: &str) -> bool {
    let bytes = template.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'/' {
            continue;
        }
        let boundary = index == 0
            || matches!(
                bytes[index - 1],
                b' ' | b'\t'
                    | b'\n'
                    | b'\''
                    | b'"'
                    | b'`'
                    | b'('
                    | b','
                    | b'='
                    | b'{'
                    | b'}'
                    | b':'
                    | b'?'
                    | b';'
            );
        let path_char = bytes
            .get(index + 1)
            .map(|next| next.is_ascii_alphanumeric() || matches!(next, b'_' | b'-' | b'/' | b'.'))
            .unwrap_or(false);
        if boundary && path_char {
            return true;
        }
    }
    false
}

/// A path literal directly connected to a `+` instead of standing alone.
fn concatenated(blob: &str, literal: &JsLiteral) -> bool {
    blob[..literal.start].trim_end().ends_with('+')
        || blob[literal.end..].trim_start().starts_with('+')
}

fn dynamic_url_error(snippet: &str, why: &str) -> String {
    format!(
        "dynamic URL construction ({why}: {snippet:?}); write the URL as a plain single or double quoted literal so the guard can verify it"
    )
}

/// Method of the nearest preceding `htmx.ajax(...)` call whose first argument
/// is a verb literal; `None` when the call shape differs (GET by default).
fn htmx_ajax_method(blob: &str, url_pos: usize) -> Option<String> {
    let prefix = &blob[..url_pos];
    let call = prefix.rfind("htmx.ajax(")?;
    let args = &prefix[call + "htmx.ajax(".len()..];
    let first = js_literals(args).into_iter().next()?.text;
    let upper = first.to_ascii_uppercase();
    matches!(upper.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE").then_some(upper)
}

/// Every application URL written in a JS blob, with the verb of its call.
///
/// A URL built dynamically cannot be verified statically, so concatenations and
/// interpolated templates touching an application path are a hard failure with
/// an actionable message instead of a silent miss.
fn js_url_targets(blob: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for literal in js_literals(blob) {
        if literal.interpolated {
            if templated_path(&literal.text) {
                return Err(dynamic_url_error(&literal.text, "template interpolation"));
            }
            continue;
        }
        if !is_app_path(&literal.text) {
            continue;
        }
        if concatenated(blob, &literal) {
            return Err(dynamic_url_error(&literal.text, "concatenation"));
        }
        let method = htmx_ajax_method(blob, literal.start).unwrap_or_else(|| "GET".to_string());
        out.push((method, literal.text));
    }
    Ok(out)
}

/// Request targets written in JS: `hx-on` bodies and inline scripts. Selectors
/// (`#...`) and event names are not paths and are ignored by construction.
fn extract_script_targets(html: &str) -> Result<Vec<RenderedTarget>, String> {
    let mut out: Vec<RenderedTarget> = Vec::new();
    let mut push = |attr: &str, method: String, target: String| {
        if !out.iter().any(|t| t.method == method && t.target == target) {
            out.push(RenderedTarget {
                attr: attr.to_string(),
                method,
                target,
                form_bound: false,
            });
        }
    };
    for blob in hx_on_values(html) {
        for (method, target) in js_url_targets(&blob).map_err(|e| format!("hx-on url: {e}"))? {
            push("hx-on url", method, target);
        }
    }
    for blob in inline_script_bodies(html) {
        for (method, target) in js_url_targets(&blob).map_err(|e| format!("script url: {e}"))? {
            push("script url", method, target);
        }
    }
    Ok(out)
}

/// Fail if `target` still contains something a template should have
/// interpolated: an id 0 segment (`/0/` was the HTMX bug), raw `{`/`}` or the
/// URL-encoded `%7B`. A `:` is a placeholder marker only in the path, so
/// date/time query values (`?from=2024-05-01T00:00`) stay legitimate.
///
/// When `concrete_ids_are_defects` is set, a concrete numeric segment means
/// the template hardcoded an id: that page's URL carries no id and its forms
/// have no data-bound ids. A final numeric segment on a non-form target is a
/// data-bound record link and stays valid; anything else, or any form-bound
/// target, is rejected.
fn check_target_shape(
    page: &str,
    attr: &str,
    target: &str,
    concrete_ids_are_defects: bool,
    form_bound: bool,
) -> Result<(), String> {
    if target.split('/').any(|segment| segment == "0") {
        return Err(format!(
            "{page}: {attr}=\"{target}\" still contains an id 0 placeholder"
        ));
    }
    if target.contains('{') || target.contains('}') {
        return Err(format!(
            "{page}: {attr}=\"{target}\" still contains an unrendered template placeholder"
        ));
    }
    if target.to_ascii_uppercase().contains("%7B") {
        return Err(format!(
            "{page}: {attr}=\"{target}\" still contains a URL-encoded placeholder"
        ));
    }
    if path_of(target).contains(':') {
        return Err(format!(
            "{page}: {attr}=\"{target}\" still contains a template placeholder marker"
        ));
    }
    if concrete_ids_are_defects {
        let segments = path_segments(target);
        let last = segments.len().saturating_sub(1);
        for (index, segment) in segments.iter().enumerate() {
            if segment.parse::<i64>().is_ok() && (form_bound || index != last) {
                return Err(format!(
                    "{page}: {attr}=\"{target}\" hardcodes a concrete id segment on a page whose URL carries no id; the id must come from the URL or a data-bound link"
                ));
            }
        }
    }
    Ok(())
}

/// Native form wiring: a `this.action` rewrite is dead under htmx, and native
/// actions are shape-checked like any other target.
fn check_native_form(
    page: &str,
    form: &RenderedForm,
    concrete_ids_are_defects: bool,
) -> Result<(), String> {
    if let Some(onsubmit) = &form.onsubmit {
        if onsubmit.replace(' ', "").contains("this.action=") {
            return Err(format!(
                "{page}: form onsubmit rewrites this.action, which htmx ignores; use hx-post/hx-delete instead"
            ));
        }
    }
    if let Some(action) = &form.action {
        if !action.is_empty() && action != "#" {
            check_target_shape(page, "form action", action, concrete_ids_are_defects, true)?;
        }
    }
    Ok(())
}

/// Shape-only guard over a rendered page: pure, so mutation tests can assert
/// the exact rejection without building an app. `concrete_ids_are_defects`
/// applies the id-free rule to pages whose URL carries no id and whose forms
/// have no data-bound ids.
fn check_rendered_wiring_shape(
    page: &str,
    html: &str,
    concrete_ids_are_defects: bool,
) -> Result<(), String> {
    for target in extract_htmx_targets(html) {
        check_target_shape(
            page,
            &target.attr,
            &target.target,
            concrete_ids_are_defects,
            target.form_bound,
        )?;
    }
    for target in extract_script_targets(html).map_err(|e| format!("{page}: {e}"))? {
        check_target_shape(
            page,
            &target.attr,
            &target.target,
            concrete_ids_are_defects,
            false,
        )?;
    }
    for form in extract_rendered_forms(html) {
        check_native_form(page, &form, concrete_ids_are_defects)?;
    }
    Ok(())
}

/// Every `hx-target` / `hx-include` attribute value on the page, with the
/// attribute it came from. Templates write these selectors literally, so the raw
/// attribute scan is exact.
fn hx_selector_attrs(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for attr in ["hx-target", "hx-include"] {
        let needle = format!("{attr}=\"");
        let mut rest = html;
        while let Some(start) = rest.find(&needle) {
            let after = &rest[start + needle.len()..];
            let end = after
                .find('"')
                .unwrap_or_else(|| panic!("unterminated {attr} attribute"));
            out.push((attr.to_string(), after[..end].to_string()));
            rest = &after[end..];
        }
    }
    out
}

/// A selector a page declares external, bound to the guarded page that must
/// render it. The guard reads that host page in the same run, so a declaration
/// cannot name a selector no page renders.
#[derive(Clone)]
struct ExternalSelector {
    selector: &'static str,
    host: &'static str,
}

/// The HTML the guarded run rendered, keyed by page label, so a declaration can
/// be checked against the page that actually hosts it.
type RenderedPages = std::collections::BTreeMap<&'static str, String>;

/// `hx-target` / `hx-include` must point at an element a page actually renders,
/// not just at a route that resolves. A page's selectors are read the same way
/// for every seeded page, and each page names the few selectors it may reach on
/// a host document instead:
///
/// - a detail fragment renders into the record-page wrapper (`#sale-record`,
///   `#purchase-record`), which the fragment itself does not emit; and
/// - an out-of-band-swapped element is inserted into the host page the same way,
///   so its selectors belong to the host too (the picker results fragment reuses
///   the host picker's `#line-picker` and money region).
///
/// Every declaration is a bound check against the named host page in the same
/// run, so a declaration that no page renders fails. Anything not declared
/// external must match an `id` in this page's HTML. The codebase targets elements
/// with absolute `#id` selectors; any other form fails loudly instead of being
/// skipped.
fn check_same_page_selectors(
    page: &str,
    html: &str,
    external_selectors: &[ExternalSelector],
    rendered_pages: &RenderedPages,
) -> Result<(), String> {
    // Declarations first: an unused bogus declaration must not hide.
    for declared in external_selectors {
        let host_html = rendered_pages.get(declared.host).ok_or_else(|| {
            format!(
                "{page}: external selector {:?} names host page {:?}, which is not in the guarded page list",
                declared.selector, declared.host
            )
        })?;
        let Some(id) = declared
            .selector
            .strip_prefix('#')
            .filter(|id| !id.is_empty())
        else {
            return Err(format!(
                "{page}: external selector {:?} must be an absolute #id so its host page can be checked",
                declared.selector
            ));
        };
        if !host_html.contains(&format!("id=\"{id}\"")) {
            return Err(format!(
                "{page}: external selector {:?} is declared external, but host page {:?} does not render it",
                declared.selector, declared.host
            ));
        }
    }

    for (attr, value) in hx_selector_attrs(html) {
        for token in value.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if external_selectors.iter().any(|e| e.selector == token) {
                continue;
            }
            if !token.starts_with('#') || token.len() == 1 {
                return Err(format!(
                    "{page}: {attr}=\"{value}\" uses selector {token:?}; this guard resolves absolute #id selectors, so extend it rather than skipping a new form"
                ));
            }
            let id = &token[1..];
            if !html.contains(&format!("id=\"{id}\"")) {
                return Err(format!(
                    "{page}: {attr}=\"{value}\" points at {token:?}, but no element with id={id:?} is rendered on this page"
                ));
            }
        }
    }
    Ok(())
}

/// Probe one target with the verb htmx (or the browser form) will actually
/// send. Both a routing fallback and a 405 mean the wiring is dead: 405 means
/// the path is registered for a different verb, which is the exact shape of a
/// path-param route masking an action target.
async fn probe_or_fail(
    probe_app: &Router,
    page: &str,
    attr: &str,
    method: &str,
    target: &str,
) -> Result<(), String> {
    // POST /logout revokes the session its cookie carries, so it cannot be
    // probed on the shared probe app: the first probe would kill the session
    // every later probe authenticates with. It also cannot be probed
    // anonymously: the gate refuses an anonymous request with the same 303 a
    // registered logout answers, so a rename would leave the guard green. A
    // dedicated app with its own pool gives the probe a live session that only
    // this one probe revokes, which restores the route-existence oracle.
    let (status, body) = if method == "POST" && target == "/logout" {
        let (logout_app, _logout_pool) = test_app().await;
        send(&logout_app, method, target, None, false, String::new()).await
    } else {
        send(probe_app, method, target, None, false, String::new()).await
    };
    if body.contains(ROUTE_FALLBACK_MARKER) {
        return Err(format!(
            "{page}: {attr}=\"{target}\" ({method}) hit the routing fallback: no route matches"
        ));
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        return Err(format!(
            "{page}: {attr}=\"{target}\" is registered but not routed for {method} (405); htmx sends {method}"
        ));
    }
    Ok(())
}

/// The guard's non-vacuity decision, extracted so mutation tests can pin its
/// boundary directly. A page is worth guarding when it renders at least one
/// `hx-*` request target, or a native form whose action is a real URL:
/// non-empty and not the placeholder `#` (an `action="#"` form posts nowhere,
/// so it checks nothing). Extracted unchanged from the inline check the
/// S4-widened guard carried; behaviour-preserving.
fn wiring_is_vacuous(targets: &[RenderedTarget], forms: &[RenderedForm]) -> bool {
    let renders_wired_native_form = forms.iter().any(|f| {
        f.action
            .as_deref()
            .map(|a| !a.is_empty() && a != "#")
            .unwrap_or(false)
    });
    targets.is_empty() && !renders_wired_native_form
}

/// Prove every rendered request target resolves to a registered route for the
/// verb that will actually be sent.
///
/// Real verbs are mandatory: an `OPTIONS` probe answers 405 for any registered
/// path, so it cannot see a path-param route shadowing a dead action target.
/// The oracle self-checks prove both failure signals are live on the bare
/// router: an unknown path answers the fallback marker, and a known GET path
/// probed with DELETE answers 405.
///
/// Probes run real handlers, so callers pass a dedicated app instance and this
/// guard never touches the instance used for page-render assertions.
async fn assert_htmx_targets_are_wired(
    probe_app: &Router,
    page: &str,
    html: &str,
    concrete_ids_are_defects: bool,
) -> Result<(), String> {
    let (status, body) = send(
        probe_app,
        "GET",
        MISSING_ROUTE_PROBE,
        None,
        false,
        String::new(),
    )
    .await;
    if status != StatusCode::NOT_FOUND || !body.contains(ROUTE_FALLBACK_MARKER) {
        return Err(format!(
            "routing fallback oracle is missing: GET {MISSING_ROUTE_PROBE} answered {status} with {body:?}, expected 404 + {ROUTE_FALLBACK_MARKER:?}"
        ));
    }
    let (status, _) = send(probe_app, "DELETE", "/", None, false, String::new()).await;
    if status != StatusCode::METHOD_NOT_ALLOWED {
        return Err(format!(
            "method-mismatch oracle is missing: DELETE / answered {status}, expected 405 Method Not Allowed"
        ));
    }

    let targets = extract_htmx_targets(html);
    let forms = extract_rendered_forms(html).into_iter().collect::<Vec<_>>();
    // A page renders wiring either as htmx request attributes or as a native
    // form action (both are probed with real verbs below). A page with neither
    // renders nothing this guard can check, so it must not be guarded — but a
    // native-form-only page (the creation page posts the collection endpoint
    // with a plain action/method) is fully checked and never vacuous.
    if wiring_is_vacuous(&targets, &forms) {
        return Err(format!(
            "{page}: no hx-get/hx-post/hx-put/hx-patch/hx-delete targets or native form action rendered; guard would be vacuous"
        ));
    }
    for target in &targets {
        check_target_shape(
            page,
            &target.attr,
            &target.target,
            concrete_ids_are_defects,
            target.form_bound,
        )?;
        probe_or_fail(
            probe_app,
            page,
            &target.attr,
            &target.method,
            &target.target,
        )
        .await?;
    }

    for target in extract_script_targets(html).map_err(|e| format!("{page}: {e}"))? {
        check_target_shape(
            page,
            &target.attr,
            &target.target,
            concrete_ids_are_defects,
            false,
        )?;
        probe_or_fail(
            probe_app,
            page,
            &target.attr,
            &target.method,
            &target.target,
        )
        .await?;
    }

    for form in &forms {
        check_native_form(page, form, concrete_ids_are_defects)?;
        if let Some(action) = &form.action {
            if !action.is_empty() && action != "#" {
                probe_or_fail(probe_app, page, "form action", &form.method, action).await?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deliverable 1: routing fallback as a test oracle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unmatched_route_returns_recognisable_404_body() {
    let (app, _pool) = test_app().await;

    let (status, body) = get(&app, MISSING_ROUTE_PROBE).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains(ROUTE_FALLBACK_MARKER),
        "an unmatched path must be recognisable, got {body:?}"
    );

    // Handler-level 404s keep their own message; the marker stays unique to the
    // router fallback.
    let (status, body) = get(&app, "/api/sales/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = json_body(&body)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(error.contains("sale 999999 not found"), "{body}");
    assert!(!body.contains(ROUTE_FALLBACK_MARKER), "{body}");
}

// ---------------------------------------------------------------------------
// Deliverable 2: every seeded page only renders wired htmx targets
// ---------------------------------------------------------------------------

/// Minimal seeded world for the wiring guard: account + methods, product with
/// stock, supplier with satellite cost, one draft sale, one purchase and one
/// customer whose statement page the guard renders.
struct WiringFixture {
    account: i64,
    sale: i64,
    purchase: i64,
    customer: i64,
}

async fn seed_wiring_fixture(app: &Router, pool: &SqlitePool) -> WiringFixture {
    let cash = method_id(pool, "Cash").await;
    let account = create_account_via_web(app, pool, "GuardWallet", &[cash]).await;
    let (status, resp) = post_form(
        app,
        "/web/transactions",
        &format!("account_id={account}&type=Income&amount=250&description=seed&date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    let product = create_product_via_web(app, pool, "GUARD-P", "2", "20").await;
    record_stock_via_web(app, product, "2").await;

    let supplier = create_supplier_via_web(app, pool, "GuardSupplier").await;
    record_supplier_cost_via_web(app, product, supplier, "7.50").await;

    // Draft rows make the rendered list partials exercise their id-bearing
    // View/delete/edit targets.
    let sale = create_sale_draft_via_web(app, &pool, "GuardBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(app, sale, product, "1").await;

    let (status, resp) = post_form(
        app,
        "/web/purchases/from-suggestion",
        &format!("product_id={product}&payment_type=Cash&purchase_date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed purchase: {resp}");
    let purchase = find_only_purchase_id(app).await;

    let (status, resp) = post_form(
        app,
        "/web/customers",
        "name=GuardCustomer&phone=555-0100&credit_limit=500&due_days=30",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed customer: {resp}");
    let customer = customer_id_by_name(pool, "GuardCustomer").await;

    // A confirmed credit sale collected into a receipt, so the customer statement
    // renders the receipt list and the referenced-id rule covers that path too.
    let guard_sale = create_sale_draft_for_customer(app, customer, "Credit", "2024-06-02").await;
    add_sale_line_via_web(app, guard_sale, product, "1").await;
    confirm_sale_via_web(app, guard_sale, None).await;
    let (status, resp) = post_form(
        app,
        "/web/customer-receipts",
        &format!("customer_id={customer}&method_id={cash}&amount=10&date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed receipt: {resp}");

    WiringFixture {
        account,
        sale,
        purchase,
        customer,
    }
}

/// One seeded page under the wiring guard.
#[derive(Clone)]
struct GuardedPage {
    label: &'static str,
    path: String,
    /// True for a page whose URL carries no id and whose forms have no
    /// data-bound ids: a concrete numeric id in a form-bound request target is a
    /// defect there. Record pages and detail fragments carry the ids they render.
    concrete_ids_are_defects: bool,
    /// Selectors this page legitimately points at on a host document, each bound
    /// to the page that must render it (see `check_same_page_selectors`). Empty
    /// for full pages, which must resolve every selector in their own HTML.
    external_selectors: Vec<ExternalSelector>,
}

/// The seeded pages the wiring guard renders. Both the guard and the mutation pin
/// tests read this list, so each page's rules are declared once and cannot be
/// relaxed in passing.
fn guarded_pages(fixture: &WiringFixture) -> Vec<GuardedPage> {
    vec![
        GuardedPage {
            label: "dashboard",
            path: "/".to_string(),
            concrete_ids_are_defects: true,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "account detail",
            path: format!("/accounts/{}", fixture.account),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "products",
            path: "/products".to_string(),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "sales",
            path: "/sales".to_string(),
            concrete_ids_are_defects: true,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "documents",
            path: "/documents".to_string(),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        // The drawer fragments render the action block (draft delete +
        // annul/discard), so their hx-delete/hx-post targets are probed with
        // the real verbs too. The drawer carries the id it renders, so
        // concrete ids are its shape, not a defect.
        GuardedPage {
            label: "documents drawer (draft sale)",
            path: format!("/web/documents/detail/sale/{}", fixture.sale),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "documents drawer (purchase)",
            path: format!("/web/documents/detail/purchase/{}", fixture.purchase),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "purchases",
            path: "/purchases".to_string(),
            concrete_ids_are_defects: true,
            // The creation dialog (T3) renders its own ids — the picker's
            // results container and the dialog form's extra-fields wrapper —
            // so every hx-include target resolves on the page itself.
            external_selectors: vec![],
        },
        GuardedPage {
            label: "suppliers",
            path: "/suppliers".to_string(),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "customers",
            path: "/customers".to_string(),
            concrete_ids_are_defects: true,
            external_selectors: vec![],
        },
        // The list/detail fragments the pages refresh over HTMX carry more
        // targets (View buttons, inline line editors), so guard the seeded
        // details too.
        GuardedPage {
            label: "sale record page",
            path: format!("/sales/{}", fixture.sale),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "purchase record page",
            path: format!("/purchases/{}", fixture.purchase),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
        GuardedPage {
            label: "sale detail fragment",
            path: format!("/web/sales/{}", fixture.sale),
            concrete_ids_are_defects: false,
            external_selectors: vec![ExternalSelector {
                selector: "#sale-record",
                host: "sale record page",
            }],
        },
        GuardedPage {
            label: "purchase detail fragment",
            path: format!("/web/purchases/{}", fixture.purchase),
            concrete_ids_are_defects: false,
            external_selectors: vec![ExternalSelector {
                selector: "#purchase-record",
                host: "purchase record page",
            }],
        },
        GuardedPage {
            label: "customer statement",
            path: format!("/customers/{}", fixture.customer),
            concrete_ids_are_defects: false,
            external_selectors: vec![],
        },
    ]
}

/// Referenced entities the interface always shows by name: a product, account,
/// payment method, customer or supplier. A document's own id is exempt, so the
/// scan requires the entity noun before the digit, which keeps the legitimate
/// `Draft #12` and `2024-SALE-000012` allowed.
const BARE_REFERENCED_ID_PREFIXES: [&str; 5] = [
    "product #",
    "account #",
    "method #",
    "customer #",
    "supplier #",
];

/// The first `<entity noun> #<digits>` in a rendered page, case-insensitive, or
/// `None`. Pure, so the mutation pin can prove the scan bites on a page copy
/// without rendering one.
fn bare_referenced_id(html: &str) -> Option<String> {
    let lowered = html.to_ascii_lowercase();
    for prefix in BARE_REFERENCED_ID_PREFIXES {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(prefix) {
            let at = from + offset;
            let digits: String = lowered[at + prefix.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                return Some(format!("{prefix}{digits}"));
            }
            from = at + prefix.len();
        }
    }
    None
}

/// Fail when a rendered page prints a referenced entity's internal id instead of
/// its name. The entity nouns are explicit so a document id (`Draft #12`) never
/// fails, and the message names both the page and the offending fragment.
fn check_no_bare_referenced_ids(page: &str, html: &str) -> Result<(), String> {
    match bare_referenced_id(html) {
        Some(matched) => Err(format!(
            "{page}: renders a bare referenced-entity id {matched:?}; show the entity's name instead"
        )),
        None => Ok(()),
    }
}

/// Render every guarded page, then apply both rules to the same run: route
/// resolution per page, and selector declarations bound to the host page the run
/// rendered. A declaration cannot pass by membership alone.
async fn assert_guarded_pages_are_wired(
    app: &Router,
    probe_app: &Router,
    pages: &[GuardedPage],
) -> Result<(), String> {
    let mut rendered: RenderedPages = RenderedPages::new();
    for page in pages {
        let (status, html) = get(app, &page.path).await;
        if status != StatusCode::OK {
            return Err(format!("{} {}: {html:.400}", page.label, page.path));
        }
        check_no_bare_referenced_ids(page.label, &html)
            .map_err(|err| format!("{} {}: {err}", page.label, page.path))?;
        assert_htmx_targets_are_wired(probe_app, page.label, &html, page.concrete_ids_are_defects)
            .await?;
        rendered.insert(page.label, html);
    }
    for page in pages {
        let html = rendered
            .get(page.label)
            .ok_or_else(|| format!("{} was not rendered", page.label))?;
        check_same_page_selectors(page.label, html, &page.external_selectors, &rendered)?;
    }
    Ok(())
}

#[tokio::test]
async fn seeded_pages_render_only_wired_htmx_targets() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;

    // Real-verb probes run real handlers (a deactivate target really
    // deactivates), so probe a dedicated freshly seeded app: incidental effects
    // must never corrupt the render assertions or the other flows.
    let (probe_app, probe_pool) = test_app().await;
    let _probe_fixture = seed_wiring_fixture(&probe_app, &probe_pool).await;

    assert_guarded_pages_are_wired(&app, &probe_app, &guarded_pages(&fixture))
        .await
        .unwrap_or_else(|err| panic!("{err}"));
}

// ---------------------------------------------------------------------------
// Deliverable 3: business flows over HTTP
// ---------------------------------------------------------------------------

/// The exact setup path that was dead before the payment-method fix: an account
/// created through the web form with its allowlist, plus tracked product,
/// supplier and satellite cost.
#[tokio::test]
async fn web_setup_flow_persists_account_methods_product_and_supplier_cost() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    let account = create_account_via_web(&app, &pool, "SetupWallet", &[cash]).await;
    let catalog = payment_methods_catalog(&app, account).await;
    assert!(
        catalog["method_ids"]
            .as_array()
            .unwrap()
            .contains(&json!(cash)),
        "the created account must keep its allowlist: {catalog}"
    );

    let product = create_product_via_web(&app, &pool, "SETUP-P", "2", "20").await;
    let (status, body) = get(&app, &format!("/api/products/{product}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let product_json = json_body(&body);
    assert_eq!(product_json["track_stock"], json!(true));
    assert_eq!(dec(&product_json["min_stock"]), Decimal::from(2));
    assert_eq!(dec(&product_json["max_stock"]), Decimal::from(20));

    let supplier = create_supplier_via_web(&app, &pool, "SetupSupplier").await;
    record_supplier_cost_via_web(&app, product, supplier, "7.50").await;
    let (status, body) = get(
        &app,
        &format!("/api/product-supplier-costs?product_id={product}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let costs = json_body(&body);
    let cost = costs["costs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["supplier_id"] == json!(supplier))
        .unwrap_or_else(|| panic!("satellite cost for supplier {supplier}: {costs}"));
    assert_eq!(
        dec(&cost["current_cost"]),
        Decimal::from_str("7.50").unwrap()
    );

    // Rendered pages agree: the account is configured and the product is listed.
    let (status, page) = get(&app, &format!("/accounts/{account}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains("No payment methods configured"),
        "configured account must not be flagged: {page:.400}"
    );
    let (status, page) = get(&app, "/products").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("SETUP-P"), "products page lists the new sku");
}

#[tokio::test]
async fn cash_sale_confirm_deducts_stock_and_links_exactly_one_income() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    let account = create_account_via_web(&app, &pool, "CashWallet", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "CASH-P", "2", "50").await;
    record_stock_via_web(&app, product, "10").await;

    let sale = create_sale_draft_via_web(&app, &pool, "CashBuyer", "Cash", "").await;
    add_sale_line_via_web(&app, sale, product, "3").await;
    confirm_sale_via_web(&app, sale, Some(cash)).await;

    let detail = sale_detail(&app, sale).await;
    assert_eq!(detail["sale"]["status"], json!("Confirmed"));
    let sale_number = detail["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale has a number")
        .to_string();
    assert!(
        sale_number.starts_with("2024-SALE-"),
        "sale number shape: {sale_number}"
    );

    assert_eq!(stock_of(&app, product).await, Decimal::from(7));

    let payments = detail["payments"].as_array().unwrap();
    assert_eq!(payments.len(), 1, "{detail}");
    assert_eq!(payments[0]["method_id"].as_i64(), Some(cash));
    let payment_tx = payments[0]["transaction_id"]
        .as_i64()
        .expect("payment links to its transaction");

    let txs = transactions_for(&app, account).await;
    assert_eq!(
        txs.len(),
        1,
        "cash confirm posts exactly one movement: {txs:?}"
    );
    assert_eq!(txs[0]["kind"], json!("Income"));
    assert_eq!(txs[0]["id"].as_i64(), Some(payment_tx));
    assert_eq!(txs[0]["reference"].as_str(), Some(sale_number.as_str()));
    assert_eq!(dec(&txs[0]["amount"]), Decimal::from(75));

    // The rendered list fragment shows the confirmed, paid sale.
    let (status, html) = get(&app, "/web/sales").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&sale_number), "{html:.400}");
    assert!(html.contains("Paid"), "{html:.400}");
}

#[tokio::test]
async fn credit_sale_pay_overpay_and_cancel_reverses_stock_and_refunds() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    let account = create_account_via_web(&app, &pool, "CreditWallet", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "CREDIT-P", "2", "50").await;
    record_stock_via_web(&app, product, "10").await;

    let sale = create_sale_draft_via_web(&app, &pool, "CreditBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;

    let detail = sale_detail(&app, sale).await;
    let sale_number = detail["sale"]["sale_number"].as_str().unwrap().to_string();
    assert!(
        detail["payments"].as_array().unwrap().is_empty(),
        "credit confirm posts no finance row",
    );
    assert!(
        transactions_for(&app, account).await.is_empty(),
        "credit confirm touches no finance",
    );
    assert_eq!(dec(&detail["due"]), Decimal::from(50));
    assert_eq!(stock_of(&app, product).await, Decimal::from(8));

    // Partial payment: one Income linked from the payment.
    let (status, body) = pay_sale_via_web(&app, sale, cash, "30").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = sale_detail(&app, sale).await;
    assert_eq!(detail["payments"].as_array().unwrap().len(), 1);
    let original_tx = detail["payments"][0]["transaction_id"]
        .as_i64()
        .expect("payment links to its transaction");
    let txs = transactions_for(&app, account).await;
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0]["kind"], json!("Income"));
    assert_eq!(dec(&txs[0]["amount"]), Decimal::from(30));
    assert_eq!(txs[0]["reference"].as_str(), Some(sale_number.as_str()));

    // Overpaying is rejected without touching finance.
    let (status, body) = pay_sale_via_web(&app, sale, cash, "30").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("overpay rejected"), "{body}");
    assert_eq!(transactions_for(&app, account).await.len(), 1);
    assert_eq!(
        sale_detail(&app, sale).await["payments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Cancel: stock returns, the payment is refunded as a linked Expense, and the
    // original Income link stays intact.
    let (status, body) = post_form(
        &app,
        "/web/sales/cancel",
        &format!("sale_id={sale}&reason=smoke"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = sale_detail(&app, sale).await;
    assert_eq!(detail["sale"]["status"], json!("Cancelled"));
    assert_eq!(stock_of(&app, product).await, Decimal::from(10));

    let payments = detail["payments"].as_array().unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(
        payments[0]["transaction_id"].as_i64(),
        Some(original_tx),
        "original Income link must survive the refund"
    );
    let refund_tx = payments[0]["refund_transaction_id"]
        .as_i64()
        .expect("cancel links the refund");

    let txs = transactions_for(&app, account).await;
    assert_eq!(txs.len(), 2);
    let refund = txs
        .iter()
        .find(|t| t["id"].as_i64() == Some(refund_tx))
        .expect("refund row");
    assert_eq!(refund["kind"], json!("Expense"));
    assert_eq!(dec(&refund["amount"]), Decimal::from(30));
    assert_eq!(refund["reference"].as_str(), Some(sale_number.as_str()));
    let original = txs
        .iter()
        .find(|t| t["id"].as_i64() == Some(original_tx))
        .expect("original row");
    assert_eq!(original["kind"], json!("Income"));
}

/// K2 over HTTP: every creation path carries a customer, the credit rules hold at
/// the route boundary, and the due date default from the payment term is visible.
#[tokio::test]
async fn credit_rules_and_mandatory_customer_hold_over_http() {
    let (app, pool) = test_app().await;

    // AC2: a form post without a customer is a 400 and creates nothing.
    let (status, body) =
        post_form(&app, "/web/sales", "payment_type=Cash&sale_date=2024-05-02").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, list) = get(&app, "/api/sales").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(
        json_body(&list)["sales"].as_array().unwrap().is_empty(),
        "a rejected create must not insert a sale: {list}"
    );

    let product = create_product_via_web(&app, &pool, "K2-SMOKE", "1", "10").await;
    record_stock_via_web(&app, product, "100").await;

    // AC7: the due date defaults from the payment term (2024-05-02 + 30 days).
    let term_id = seed_customer(&pool, "Smoke Term", Some("50"), Some(30)).await;
    let sale = create_sale_draft_for_customer(&app, term_id, "Credit", "").await;
    let detail = sale_detail(&app, sale).await;
    assert_eq!(detail["sale"]["customer_id"].as_i64(), Some(term_id));
    assert_eq!(detail["sale"]["customer_name"], json!("Smoke Term"));
    assert_eq!(detail["sale"]["due_date"], json!("2024-06-01"), "{detail}");
    add_sale_line_via_web(&app, sale, product, "1").await;
    confirm_sale_via_web(&app, sale, None).await;
    assert_eq!(
        sale_detail(&app, sale).await["sale"]["status"],
        json!("Confirmed")
    );

    // AC7: no term and no due date is a 400 at creation.
    let no_term_id = seed_customer(&pool, "Smoke No Term", None, None).await;
    let (status, body) = post_form(
        &app,
        "/web/sales",
        &format!("customer_id={no_term_id}&payment_type=Credit&sale_date=2024-05-02&due_date="),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("due_date"), "actionable message: {body}");

    // AC4: over the limit is a 400 with the projected debt and no side effect.
    let over_id = seed_customer(&pool, "Smoke Over", Some("50"), Some(30)).await;
    let over_sale = create_sale_draft_for_customer(&app, over_id, "Credit", "").await;
    add_sale_line_via_web(&app, over_sale, product, "3").await;
    let (status, body) =
        post_form(&app, "/web/sales/confirm", &format!("sale_id={over_sale}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("75"), "projected debt in the message: {body}");
    assert!(body.contains("50"), "limit in the message: {body}");
    let over_detail = sale_detail(&app, over_sale).await;
    assert_eq!(over_detail["sale"]["status"], json!("Draft"));
    assert!(
        over_detail["sale"]["sale_number"].is_null(),
        "a blocked confirm assigns no number: {over_detail}"
    );

    // AC3: credit to the walk-in is rejected and leaves the draft untouched.
    let (walkin_id,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let walkin_sale = create_sale_draft_for_customer(&app, walkin_id, "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, walkin_sale, product, "1").await;
    let (status, body) = post_form(
        &app,
        "/web/sales/confirm",
        &format!("sale_id={walkin_sale}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_lowercase().contains("walk-in"),
        "actionable message: {body}"
    );
    assert_eq!(
        sale_detail(&app, walkin_sale).await["sale"]["status"],
        json!("Draft")
    );
}

/// M4 collection flow over HTTP: create a customer, sell on credit, collect
/// part of it through the form, then assert the derived balance, the ageing
/// buckets and that the receipt total equals the sum of its allocations
/// while every grouped payment keeps its own finance link.
#[tokio::test]
async fn collection_flow_derives_balance_ageing_and_receipt_total() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "CollectWallet", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "COLLECT-P", "1", "50").await;
    record_stock_via_web(&app, product, "10").await;

    // The customer is created through the same form the page renders.
    let (status, resp) = post_form(
        &app,
        "/web/customers",
        "name=Collect+Buyer&phone=555-0200&credit_limit=500&due_days=30",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create customer: {resp}");
    let customer = customer_id_by_name(&pool, "Collect Buyer").await;

    // Credit sale of 3 x 25 = 75, due 2024-06-15.
    let sale = create_sale_draft_for_customer(&app, customer, "Credit", "2024-06-15").await;
    add_sale_line_via_web(&app, sale, product, "3").await;
    confirm_sale_via_web(&app, sale, None).await;
    let detail = sale_detail(&app, sale).await;
    assert_eq!(dec(&detail["total"]), Decimal::from(75));
    assert_eq!(dec(&detail["due"]), Decimal::from(75));

    // Collect 30 through the collect form (the id travels in the body).
    let (status, resp) = post_form(
        &app,
        "/web/customer-receipts",
        &format!("customer_id={customer}&method_id={cash}&amount=30&date=2024-06-20&notes=part"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "collect: {resp}");

    // Derived balance and over-limit flag through the composed read.
    let (status, body) = get(&app, &format!("/api/customers/{customer}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v = json_body(&body);
    assert_eq!(dec(&v["balance"]), Decimal::from(45));
    assert_eq!(v["over_limit"], json!(false));
    assert_eq!(v["customer"]["name"], json!("Collect Buyer"));

    // Statement: one sale debit, one payment credit, balance 45, and the
    // whole balance 5 days overdue falls in 1-30.
    let (status, body) = get(
        &app,
        &format!("/api/customers/{customer}/statement?as_of=2024-06-20"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v = json_body(&body);
    let statement = &v["statement"];
    assert_eq!(dec(&statement["balance"]), Decimal::from(45));
    assert_eq!(dec(&statement["ageing"]["overdue_1_30"]), Decimal::from(45));
    assert_eq!(dec(&statement["ageing"]["current"]), Decimal::ZERO);
    let entries = statement["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "sale debit + payment credit: {entries:?}");
    assert!(entries
        .iter()
        .any(|e| e["kind"] == json!("Sale") && dec(&e["debit"]) == Decimal::from(75)));
    assert!(entries
        .iter()
        .any(|e| e["kind"] == json!("Payment") && dec(&e["credit"]) == Decimal::from(30)));

    // The receivables view ages the same balance.
    let (status, body) = get(&app, "/api/customers/ageing?as_of=2024-06-20").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v = json_body(&body);
    let row = v["ageing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["customer_id"] == json!(customer))
        .unwrap_or_else(|| panic!("customer {customer} missing from ageing: {v}"));
    assert_eq!(dec(&row["balance"]), Decimal::from(45));
    assert_eq!(dec(&row["ageing"]["overdue_1_30"]), Decimal::from(45));
    assert_eq!(row["name"], json!("Collect Buyer"));

    // Receipt total is derived from its allocations, never stored.
    let (status, body) = get(
        &app,
        &format!("/api/customer-receipts?customer_id={customer}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v = json_body(&body);
    let receipts = v["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1, "{v}");
    let receipt = &receipts[0];
    let receipt_id = receipt["receipt"]["id"].as_i64().unwrap();
    assert_eq!(dec(&receipt["total"]), Decimal::from(30));
    let allocations = receipt["allocations"].as_array().unwrap();
    assert_eq!(allocations.len(), 1);
    let summed: Decimal = allocations.iter().map(|a| dec(&a["amount"])).sum();
    assert_eq!(summed, dec(&receipt["total"]));
    for allocation in allocations {
        assert_eq!(allocation["receipt_id"].as_i64(), Some(receipt_id));
        assert_eq!(allocation["sale_id"].as_i64(), Some(sale));
        assert!(
            allocation["transaction_id"].as_i64().is_some(),
            "the grouped payment keeps its finance link: {allocation}"
        );
    }
    let raw: Vec<(String,)> =
        sqlx::query_as("SELECT amount FROM sale_payments WHERE receipt_id = ?")
            .bind(receipt_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    let raw_sum: Decimal = raw
        .iter()
        .map(|(amount,)| Decimal::from_str(amount).unwrap())
        .sum();
    assert_eq!(raw_sum, dec(&receipt["total"]));

    // The sale still shows the receipt-linked payment, and the money
    // invariant holds for the whole database built by the flow.
    let detail = sale_detail(&app, sale).await;
    assert_eq!(detail["payments"].as_array().unwrap().len(), 1);
    assert_eq!(
        detail["payments"][0]["receipt_id"].as_i64(),
        Some(receipt_id)
    );
    assert_eq!(dec(&detail["due"]), Decimal::from(45));
    assert_payment_links_are_traceable(&pool).await;

    // The statement page renders the collected customer and the new balance.
    let (status, html) = get(&app, &format!("/customers/{customer}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Collect Buyer"), "{html:.400}");
    assert!(
        html.contains("45"),
        "the page shows the derived balance: {html:.400}"
    );
}

#[tokio::test]
async fn purchase_flow_from_suggestion_confirms_cash_and_reverses_on_cancel() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    let account = create_account_via_web(&app, &pool, "PurchaseWallet", &[cash]).await;
    let (status, body) = post_form(
        &app,
        "/web/transactions",
        &format!("account_id={account}&type=Income&amount=1000&description=fund&date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let product = create_product_via_web(&app, &pool, "PURCH-P", "5", "20").await;
    record_stock_via_web(&app, product, "2").await;
    let supplier = create_supplier_via_web(&app, &pool, "PurchaseSupplier").await;
    record_supplier_cost_via_web(&app, product, supplier, "7.50").await;

    // Build the pedido from the suggestion endpoint the page renders.
    let (status, body) = post_form(
        &app,
        "/web/purchases/from-suggestion",
        &format!("product_id={product}&payment_type=Cash&purchase_date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed from suggestion: {body}");
    let purchase = find_only_purchase_id(&app).await;

    let detail = purchase_detail(&app, purchase).await;
    assert_eq!(detail["purchase"]["status"], json!("Draft"));
    assert_eq!(dec(&detail["lines"][0]["qty"]), Decimal::from(18));
    assert_eq!(
        dec(&detail["lines"][0]["unit_cost"]),
        Decimal::from_str("7.50").unwrap()
    );
    assert_eq!(stock_of(&app, product).await, Decimal::from(2));

    // Confirm Cash: stock in plus one Expense linked from the payment.
    let (status, body) = post_form(
        &app,
        "/web/purchases/confirm",
        &format!("purchase_id={purchase}&method_id={cash}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = purchase_detail(&app, purchase).await;
    assert_eq!(detail["purchase"]["status"], json!("Confirmed"));
    let purchase_number = detail["purchase"]["purchase_number"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        purchase_number.starts_with("2024-PURCH-"),
        "purchase number shape: {purchase_number}"
    );
    assert_eq!(stock_of(&app, product).await, Decimal::from(20));

    let payments = detail["payments"].as_array().unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0]["method_id"].as_i64(), Some(cash));
    let expense_tx = payments[0]["transaction_id"]
        .as_i64()
        .expect("payment links to its transaction");

    let txs = transactions_for(&app, account).await;
    assert_eq!(txs.len(), 2, "funding Income + purchase Expense: {txs:?}");
    let expense = txs
        .iter()
        .find(|t| t["id"].as_i64() == Some(expense_tx))
        .expect("expense row");
    assert_eq!(expense["kind"], json!("Expense"));
    assert_eq!(dec(&expense["amount"]), Decimal::from(135));
    assert_eq!(
        expense["reference"].as_str(),
        Some(purchase_number.as_str())
    );

    // Cancel the confirmed purchase: stock returns and the Expense is refunded.
    let (status, body) = post_form(
        &app,
        "/web/purchases/cancel",
        &format!("purchase_id={purchase}&reason=smoke"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = purchase_detail(&app, purchase).await;
    assert_eq!(detail["purchase"]["status"], json!("Cancelled"));
    assert_eq!(stock_of(&app, product).await, Decimal::from(2));

    let payments = detail["payments"].as_array().unwrap();
    assert_eq!(
        payments[0]["transaction_id"].as_i64(),
        Some(expense_tx),
        "original Expense link must survive the refund"
    );
    let refund_tx = payments[0]["refund_transaction_id"]
        .as_i64()
        .expect("cancel links the refund");
    let txs = transactions_for(&app, account).await;
    assert_eq!(txs.len(), 3);
    let refund = txs
        .iter()
        .find(|t| t["id"].as_i64() == Some(refund_tx))
        .expect("refund row");
    assert_eq!(refund["kind"], json!("Income"));
    assert_eq!(dec(&refund["amount"]), Decimal::from(135));
    assert_eq!(refund["reference"].as_str(), Some(purchase_number.as_str()));
}

/// A method with no owning account rejects the payment with the actionable
/// message; assigning it to the account afterwards makes the same payment
/// succeed.
#[tokio::test]
async fn payment_guard_rejects_then_succeeds_after_methods_configured() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    // REST-created accounts own nothing and Cash is unassigned: no account can
    // be derived, the realistic broken state.
    let (status, body) = post_json(&app, "/api/accounts", json!({ "name": "GuardAccount" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let account = json_body(&body)["id"].as_i64().unwrap();
    assert!(
        payment_methods_catalog(&app, account).await["method_ids"]
            .as_array()
            .unwrap()
            .is_empty(),
        "account starts without methods"
    );

    let product = create_product_via_web(&app, &pool, "GUARDFLOW-P", "1", "10").await;
    record_stock_via_web(&app, product, "5").await;
    let sale =
        create_sale_draft_via_web(&app, &pool, "GuardFlowBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, sale, product, "1").await;
    confirm_sale_via_web(&app, sale, None).await;

    let (status, body) = pay_sale_via_web(&app, sale, cash, "25").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("not assigned to any account"),
        "message must tell the user what to do: {body}"
    );
    assert!(
        transactions_for(&app, account).await.is_empty(),
        "rejected payment must not touch finance"
    );
    assert!(
        sale_detail(&app, sale).await["payments"]
            .as_array()
            .unwrap()
            .is_empty(),
        "rejected payment must not land on the sale"
    );

    // Configure through the same plain form the account detail page renders.
    let (status, body) = post_browser_form(
        &app,
        &format!("/accounts/{account}/payment-methods"),
        &format!("method_ids={cash}"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");

    let (status, body) = pay_sale_via_web(&app, sale, cash, "25").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "payment after configuration: {body}"
    );
    assert_eq!(
        sale_detail(&app, sale).await["payments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let txs = transactions_for(&app, account).await;
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0]["kind"], json!("Income"));
}

/// Money invariants over a database built by real flows: every account balance
/// is the signed sum of its transactions, and every payment row is traceable to
/// a real transaction whose `reference` is the document number.
#[tokio::test]
async fn money_invariants_hold_for_balances_and_payment_links() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;

    // Account A: cash sale posts Income; credit sale posts Income then a refund.
    let _account_a = create_account_via_web(&app, &pool, "InvA", &[cash]).await;
    let product_a = create_product_via_web(&app, &pool, "INV-A", "1", "50").await;
    record_stock_via_web(&app, product_a, "10").await;
    let cash_sale = create_sale_draft_via_web(&app, &pool, "InvCashBuyer", "Cash", "").await;
    add_sale_line_via_web(&app, cash_sale, product_a, "1").await;
    confirm_sale_via_web(&app, cash_sale, Some(cash)).await;

    let credit_sale =
        create_sale_draft_via_web(&app, &pool, "InvCreditBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, credit_sale, product_a, "1").await;
    confirm_sale_via_web(&app, credit_sale, None).await;
    let (status, body) = pay_sale_via_web(&app, credit_sale, cash, "25").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post_form(
        &app,
        "/web/sales/cancel",
        &format!("sale_id={credit_sale}&reason=invariant"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Account B: funded manually, purchase posts Expense, cancel refunds it.
    // InvB owns its own Cash duplicate (same name, different row), so the
    // purchase must name that row: the method id decides the account.
    let account_b = create_account_via_web(&app, &pool, "InvB", &[cash]).await;
    let cash_b = account_method_id(&pool, account_b, "Cash").await;
    let (status, body) = post_form(
        &app,
        "/web/transactions",
        &format!("account_id={account_b}&type=Income&amount=500&description=fund&date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let product_b = create_product_via_web(&app, &pool, "INV-B", "5", "20").await;
    record_stock_via_web(&app, product_b, "2").await;
    let supplier = create_supplier_via_web(&app, &pool, "InvSupplier").await;
    record_supplier_cost_via_web(&app, product_b, supplier, "4").await;
    let (status, body) = post_form(
        &app,
        "/web/purchases/from-suggestion",
        &format!("product_id={product_b}&payment_type=Cash&purchase_date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let purchase = find_only_purchase_id(&app).await;
    let (status, body) = post_form(
        &app,
        "/web/purchases/confirm",
        &format!("purchase_id={purchase}&method_id={cash_b}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post_form(
        &app,
        "/web/purchases/cancel",
        &format!("purchase_id={purchase}&reason=invariant"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Every account's derived balance equals the signed sum of its transactions.
    let (status, body) = get(&app, "/api/accounts").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let accounts_json = json_body(&body);
    let accounts = accounts_json["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2, "{accounts:?}");
    for account in accounts {
        let account_id = account["id"].as_i64().unwrap();
        let balance = dec(&account["balance"]);
        let mut sum = Decimal::ZERO;
        for tx in transactions_for(&app, account_id).await {
            let amount = dec(&tx["amount"]);
            if tx["kind"] == json!("Income") {
                sum += amount;
            } else {
                sum -= amount;
            }
        }
        assert_eq!(
            sum, balance,
            "account {account_id} balance must be the signed sum of its transactions"
        );
    }

    // The guard must exercise both payment tables and both refund links,
    // otherwise the refund half of the invariant would be vacuous.
    let link_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM sale_payments),
                (SELECT COUNT(*) FROM purchase_payments),
                (SELECT COUNT(*) FROM sale_payments WHERE refund_transaction_id IS NOT NULL),
                (SELECT COUNT(*) FROM purchase_payments WHERE refund_transaction_id IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        link_counts.0 > 0,
        "invariant would be vacuous: no sale payments"
    );
    assert!(
        link_counts.1 > 0,
        "invariant would be vacuous: no purchase payments"
    );
    assert!(
        link_counts.2 > 0,
        "invariant would be vacuous: no sale refund links"
    );
    assert!(
        link_counts.3 > 0,
        "invariant would be vacuous: no purchase refund links"
    );

    assert_payment_links_are_traceable(&pool).await;
}

/// Every payment row must point at a real transaction whose `reference`
/// equals the payment's document number. When a refund link exists it must
/// also resolve to a real transaction with that same reference, and the
/// original link must stay intact (a refund never replaces it).
/// Non-vacuous caller: the invariant test asserts both payment tables and both
/// refund links have rows before calling this. Returns an error so tests can
/// corrupt a link and prove the check fires.
async fn check_payment_links_are_traceable(pool: &SqlitePool) -> Result<(), String> {
    // Ownership can only be falsified by direct database tampering: the
    // application always creates a fresh transaction/refund per payment and no
    // route accepts `refund_transaction_id`. The global claim below keeps the
    // invariant honest even then.
    let mut owners: HashMap<i64, String> = HashMap::new();
    let sale_links: Vec<(i64, Option<i64>, Option<i64>, i64, String, Option<String>)> =
        sqlx::query_as(
            "SELECT sp.id, sp.transaction_id, sp.refund_transaction_id, sp.account_id, sp.amount, s.sale_number
             FROM sale_payments sp JOIN sales s ON s.id = sp.sale_id",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    for (payment_id, transaction_id, refund_transaction_id, account_id, amount_text, sale_number) in
        sale_links
    {
        let sale_number = sale_number.ok_or_else(|| {
            format!("sale payment {payment_id} belongs to a sale without a number")
        })?;
        let transaction_id = transaction_id
            .ok_or_else(|| format!("sale payment {payment_id} has no transaction_id"))?;
        let payment_amount = Decimal::from_str(&amount_text).map_err(|e| {
            format!("sale payment {payment_id} has invalid amount {amount_text}: {e}")
        })?;
        let original =
            check_original_transaction(pool, "sale", payment_id, transaction_id, &sale_number)
                .await?;
        claim_transaction(
            &mut owners,
            transaction_id,
            format!("sale payment {payment_id} transaction_id"),
        )?;
        if let Some(refund_id) = refund_transaction_id {
            if refund_id == transaction_id {
                return Err(format!(
                    "sale payment {payment_id} refund reuses the original transaction {transaction_id}"
                ));
            }
            check_refund_transaction(
                pool,
                "sale refund",
                payment_id,
                refund_id,
                &sale_number,
                account_id,
                payment_amount,
                &original,
            )
            .await?;
            claim_transaction(
                &mut owners,
                refund_id,
                format!("sale payment {payment_id} refund_transaction_id"),
            )?;
        }
    }

    let purchase_links: Vec<(i64, Option<i64>, Option<i64>, i64, String, Option<String>)> =
        sqlx::query_as(
            "SELECT pp.id, pp.transaction_id, pp.refund_transaction_id, pp.account_id, pp.amount, p.purchase_number
             FROM purchase_payments pp JOIN purchases p ON p.id = pp.purchase_id",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    for (
        payment_id,
        transaction_id,
        refund_transaction_id,
        account_id,
        amount_text,
        purchase_number,
    ) in purchase_links
    {
        let purchase_number = purchase_number.ok_or_else(|| {
            format!("purchase payment {payment_id} belongs to a purchase without a number")
        })?;
        let transaction_id = transaction_id
            .ok_or_else(|| format!("purchase payment {payment_id} has no transaction_id"))?;
        let payment_amount = Decimal::from_str(&amount_text).map_err(|e| {
            format!("purchase payment {payment_id} has invalid amount {amount_text}: {e}")
        })?;
        let original = check_original_transaction(
            pool,
            "purchase",
            payment_id,
            transaction_id,
            &purchase_number,
        )
        .await?;
        claim_transaction(
            &mut owners,
            transaction_id,
            format!("purchase payment {payment_id} transaction_id"),
        )?;
        if let Some(refund_id) = refund_transaction_id {
            if refund_id == transaction_id {
                return Err(format!(
                    "purchase payment {payment_id} refund reuses the original transaction {transaction_id}"
                ));
            }
            check_refund_transaction(
                pool,
                "purchase refund",
                payment_id,
                refund_id,
                &purchase_number,
                account_id,
                payment_amount,
                &original,
            )
            .await?;
            claim_transaction(
                &mut owners,
                refund_id,
                format!("purchase payment {payment_id} refund_transaction_id"),
            )?;
        }
    }

    // Orphan movements: every transaction whose reference looks like a document
    // number must be claimed by some payment, as original or refund. A failure
    // between creating the movement and inserting the payment row leaves one of
    // these (the project deliberately does not share transactions across
    // modules), and nothing else would notice it. Manual transactions keep a
    // NULL reference and stay exempt.
    let orphans: Vec<(i64,)> = sqlx::query_as(
        "SELECT t.id FROM transactions t \
         WHERE t.reference IS NOT NULL \
           AND (t.reference GLOB '[0-9][0-9][0-9][0-9]-SALE-[0-9][0-9][0-9][0-9][0-9][0-9]' \
             OR t.reference GLOB '[0-9][0-9][0-9][0-9]-PURCH-[0-9][0-9][0-9][0-9][0-9][0-9]') \
           AND NOT EXISTS (SELECT 1 FROM sale_payments sp \
                           WHERE sp.transaction_id = t.id OR sp.refund_transaction_id = t.id) \
           AND NOT EXISTS (SELECT 1 FROM purchase_payments pp \
                           WHERE pp.transaction_id = t.id OR pp.refund_transaction_id = t.id) \
         ORDER BY t.id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    if !orphans.is_empty() {
        let ids: Vec<i64> = orphans.into_iter().map(|(id,)| id).collect();
        return Err(format!(
            "transactions with a document reference are claimed by no payment: {ids:?} (every document movement must be linked as transaction_id or refund_transaction_id)"
        ));
    }
    Ok(())
}

/// Assert the traceability invariant with a panic (test-facing wrapper).
async fn assert_payment_links_are_traceable(pool: &SqlitePool) {
    if let Err(err) = check_payment_links_are_traceable(pool).await {
        panic!("{err}");
    }
}

/// Money facts a linked transaction must satisfy. Reference ties it to the
/// document; account/kind/amount tie a refund to its own payment.
struct MoneyTransaction {
    reference: Option<String>,
    account_id: i64,
    kind: String,
    amount: Decimal,
}

async fn fetch_money_transaction(
    pool: &SqlitePool,
    label: &str,
    payment_id: i64,
    transaction_id: i64,
) -> Result<MoneyTransaction, String> {
    let row: Option<(Option<String>, i64, String, String)> =
        sqlx::query_as("SELECT reference, account_id, kind, amount FROM transactions WHERE id = ?")
            .bind(transaction_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    let (reference, account_id, kind, amount) = row.ok_or_else(|| {
        format!("{label} payment {payment_id} points at missing transaction {transaction_id}")
    })?;
    let amount = Decimal::from_str(&amount).map_err(|e| {
        format!(
            "{label} payment {payment_id} transaction {transaction_id} has invalid amount {amount}: {e}"
        )
    })?;
    Ok(MoneyTransaction {
        reference,
        account_id,
        kind,
        amount,
    })
}

/// A transaction belongs to exactly one payment, as original or refund. The
/// claim map makes cross-payment swaps fail even when every identity fact
/// (reference, account, kind, amount) matches.
fn claim_transaction(
    owners: &mut HashMap<i64, String>,
    transaction_id: i64,
    owner: String,
) -> Result<(), String> {
    if let Some(existing) = owners.get(&transaction_id) {
        return Err(format!(
            "transaction {transaction_id} is claimed by both {existing} and {owner}; a transaction belongs to exactly one payment"
        ));
    }
    owners.insert(transaction_id, owner);
    Ok(())
}

/// The original transaction must exist and carry the document's reference.
async fn check_original_transaction(
    pool: &SqlitePool,
    label: &str,
    payment_id: i64,
    transaction_id: i64,
    document_number: &str,
) -> Result<MoneyTransaction, String> {
    let tx = fetch_money_transaction(pool, label, payment_id, transaction_id).await?;
    if tx.reference.as_deref() != Some(document_number) {
        return Err(format!(
            "{label} payment {payment_id} transaction {transaction_id} reference {:?} != document {document_number:?}",
            tx.reference
        ));
    }
    Ok(tx)
}

/// A refund must reverse its own payment: same document reference, the payment's
/// account, the opposite kind of the original transaction and the payment amount.
/// The reference alone is not enough because every payment of a document shares
/// it, so a cross-payment swap would otherwise pass.
#[allow(clippy::too_many_arguments)]
async fn check_refund_transaction(
    pool: &SqlitePool,
    label: &str,
    payment_id: i64,
    refund_id: i64,
    document_number: &str,
    payment_account_id: i64,
    payment_amount: Decimal,
    original: &MoneyTransaction,
) -> Result<(), String> {
    let refund = fetch_money_transaction(pool, label, payment_id, refund_id).await?;
    if refund.reference.as_deref() != Some(document_number) {
        return Err(format!(
            "{label} payment {payment_id} refund transaction {refund_id} reference {:?} != document {document_number:?}",
            refund.reference
        ));
    }
    if refund.account_id != payment_account_id {
        return Err(format!(
            "{label} payment {payment_id} refund transaction {refund_id} account {} != payment account {payment_account_id}",
            refund.account_id
        ));
    }
    let expected_kind = match original.kind.as_str() {
        "Income" => "Expense",
        "Expense" => "Income",
        other => {
            return Err(format!(
                "{label} payment {payment_id} original transaction has unknown kind {other}"
            ))
        }
    };
    if refund.kind != expected_kind {
        return Err(format!(
            "{label} payment {payment_id} refund transaction {refund_id} kind {} is not the opposite of the original kind {}",
            refund.kind, original.kind
        ));
    }
    if refund.amount != payment_amount {
        return Err(format!(
            "{label} payment {payment_id} refund transaction {refund_id} amount {} != payment amount {payment_amount}",
            refund.amount
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wiring guard: mutation tests. Each one reproduces a proven blind spot and
// fails loudly if the guard regresses to accepting it.
// ---------------------------------------------------------------------------

fn id_free_page_shape(html: &str) -> Result<(), String> {
    check_rendered_wiring_shape("mutation", html, true)
}

/// Blind spot 1: a hardcoded non-zero id on an id-free page used to pass
/// because only the literal `0` segment was rejected.
#[test]
fn wiring_guard_catches_hardcoded_numeric_id_on_id_free_page() {
    let err =
        id_free_page_shape(r#"<form hx-post="/web/sales/1/confirm"><input name="sale_id"></form>"#)
            .unwrap_err();
    eprintln!("mutation-1 rejected: {err}");
    assert!(err.contains("hardcodes a concrete id segment"), "{err}");

    // A final numeric segment is also a hardcoded id when the wiring is a form...
    let err = id_free_page_shape(r#"<form hx-post="/web/sales/7"></form>"#).unwrap_err();
    assert!(err.contains("hardcodes a concrete id segment"), "{err}");

    // ...while a data-bound record link (bare button, final segment) is fine.
    id_free_page_shape(r#"<button hx-get="/web/sales/7">View</button>"#).unwrap();

    // Record-bound detail fragments may target the record they render for.
    check_rendered_wiring_shape(
        "detail fragment",
        r#"<form hx-post="/web/sales/1/confirm"></form>"#,
        false,
    )
    .unwrap();
}

/// The verifier's mutation, pinned: adding a form with a hardcoded id to the
/// sales list must fail the guard. The only thing standing between that
/// mutation and a green run is the rule the page list carries for "sales", and
/// this test reads it from the same list the guard uses, so relaxing it again
/// makes this test fail before the mutation can ship.
#[tokio::test]
async fn wiring_guard_rejects_a_hardcoded_id_form_added_to_the_sales_list() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;

    let sales_page = guarded_pages(&fixture)
        .into_iter()
        .find(|page| page.label == "sales")
        .expect("the sales list must be declared in the guarded page list");
    let (status, html) = get(&app, &sales_page.path).await;
    assert_eq!(status, StatusCode::OK, "{}: {html:.400}", sales_page.path);

    // Exactly the verifier's mutation: one extra form with a concrete id.
    let mutant = format!(
        "{html}<form hx-post=\"/web/sales/1/lines\"><input name=\"qty\" value=\"1\" /></form>"
    );
    let err = check_rendered_wiring_shape(
        "sales list (mutated)",
        &mutant,
        sales_page.concrete_ids_are_defects,
    )
    .unwrap_err();
    assert!(
        err.contains("hardcodes a concrete id segment"),
        "the guard must reject a hardcoded id on the sales list: {err}"
    );

    // The mirror: on the record page a concrete id is legitimate — the URL
    // carries the record id and the line ids are data-bound.
    let record_page = guarded_pages(&fixture)
        .into_iter()
        .find(|page| page.label == "sale record page")
        .expect("the sale record page must be declared in the guarded page list");
    assert!(
        !record_page.concrete_ids_are_defects,
        "the record page carries the ids it renders and must stay out of the rule"
    );
    let (status, record_html) = get(&app, &record_page.path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}: {record_html:.400}",
        record_page.path
    );
    check_rendered_wiring_shape(
        "sale record page",
        &record_html,
        record_page.concrete_ids_are_defects,
    )
    .unwrap_or_else(|err| panic!("concrete ids must stay legitimate on the record page: {err}"));
}

/// The mirror pin: adding a form with a hardcoded id to the purchases list must
/// fail the guard. The list carries no legitimate concrete id, so its flag must
/// stay true; the new record page carries the ids it renders, so its flag must
/// stay false. Reading both from the same list the guard uses means a future flip
/// fails here before the mutation can ship.
#[tokio::test]
async fn wiring_guard_rejects_a_hardcoded_id_form_added_to_the_purchases_list() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;

    let purchases_page = guarded_pages(&fixture)
        .into_iter()
        .find(|page| page.label == "purchases")
        .expect("the purchases list must be declared in the guarded page list");
    assert!(
        purchases_page.concrete_ids_are_defects,
        "the purchases list carries no legitimate concrete id and must stay under the rule"
    );
    let (status, html) = get(&app, &purchases_page.path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}: {html:.400}",
        purchases_page.path
    );

    // Exactly the verifier's mutation: one extra form with a concrete id.
    let mutant = format!(
        "{html}<form hx-post=\"/web/purchases/1/lines\"><input name=\"qty\" value=\"1\" /></form>"
    );
    let err = check_rendered_wiring_shape(
        "purchases list (mutated)",
        &mutant,
        purchases_page.concrete_ids_are_defects,
    )
    .unwrap_err();
    assert!(
        err.contains("hardcodes a concrete id segment"),
        "the guard must reject a hardcoded id on the purchases list: {err}"
    );

    // The mirror: on the purchase record page a concrete id is legitimate — the URL
    // carries the record id and the line ids are data-bound.
    let record_page = guarded_pages(&fixture)
        .into_iter()
        .find(|page| page.label == "purchase record page")
        .expect("the purchase record page must be declared in the guarded page list");
    assert!(
        !record_page.concrete_ids_are_defects,
        "the record page carries the ids it renders and must stay out of the rule"
    );
    let (status, record_html) = get(&app, &record_page.path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}: {record_html:.400}",
        record_page.path
    );
    check_rendered_wiring_shape(
        "purchase record page",
        &record_html,
        record_page.concrete_ids_are_defects,
    )
    .unwrap_or_else(|err| {
        panic!("concrete ids must stay legitimate on the purchase record page: {err}")
    });
}

/// The dangling-selector shape, pinned. Route resolution cannot see a selector
/// that matches no rendered element, so this rule closes that gap: a control
/// aimed at the removed `#purchase-detail` panel must fail, and so must removing
/// the panel an existing control targets. The message names the page and the
/// missing selector.
#[tokio::test]
async fn wiring_guard_rejects_hx_target_selectors_that_no_element_matches() {
    // The rule in isolation: a selector that resolves passes, one that does not
    // fails and names both the page and the selector.
    check_same_page_selectors(
        "control",
        r##"<div id="panel"></div><button hx-target="#panel"></button>"##,
        &[],
        &RenderedPages::new(),
    )
    .unwrap();
    let err = check_same_page_selectors(
        "control",
        r##"<button hx-target="#purchase-detail"></button>"##,
        &[],
        &RenderedPages::new(),
    )
    .unwrap_err();
    assert!(err.contains("control"), "{err}");
    assert!(err.contains("#purchase-detail"), "{err}");

    let (app, pool) = test_app().await;
    let _fixture = seed_wiring_fixture(&app, &pool).await;

    // Mutation A: the verifier's shape — a control aimed at the removed panel.
    let (status, purchases) = get(&app, "/purchases").await;
    assert_eq!(status, StatusCode::OK, "{purchases:.400}");
    let dangling = format!("{purchases}<button hx-target=\"#purchase-detail\"></button>");
    let err =
        check_same_page_selectors("purchases", &dangling, &[], &RenderedPages::new()).unwrap_err();
    eprintln!("dangling selector rejected: {err}");
    assert!(err.contains("purchases"), "{err}");
    assert!(err.contains("#purchase-detail"), "{err}");

    // Mutation B: remove the panel an existing control targets.
    let (status, sales) = get(&app, "/sales").await;
    assert_eq!(status, StatusCode::OK, "{sales:.400}");
    let removed = sales.replacen("id=\"sale-debt\"", "", 1);
    assert_ne!(
        removed, sales,
        "the mutation must remove the targeted panel"
    );
    let err = check_same_page_selectors("sales", &removed, &[], &RenderedPages::new()).unwrap_err();
    eprintln!("removed panel rejected: {err}");
    assert!(err.contains("sales"), "{err}");
    assert!(err.contains("#sale-debt"), "{err}");
}

/// The page-external exemptions are load-bearing and real: each selector a
/// fragment may reach is rendered by the host page it swaps into, and the
/// fragment check fails without the declaration.
#[tokio::test]
async fn fragment_external_selectors_resolve_on_their_host_record_pages() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;

    let (status, sale_page) = get(&app, &format!("/sales/{}", fixture.sale)).await;
    assert_eq!(status, StatusCode::OK, "{sale_page:.400}");
    for id in ["sale-record", "sale-record-money", "line-picker"] {
        assert!(
            sale_page.contains(&format!("id=\"{id}\"")),
            "the sale host page must render #{id} for its fragments"
        );
    }

    let (status, purchase_page) = get(&app, &format!("/purchases/{}", fixture.purchase)).await;
    assert_eq!(status, StatusCode::OK, "{purchase_page:.400}");
    for id in ["purchase-record", "purchase-record-money", "line-picker"] {
        assert!(
            purchase_page.contains(&format!("id=\"{id}\"")),
            "the purchase host page must render #{id} for its fragments"
        );
    }

    // Without the declared exemption the fragment genuinely fails, so the
    // exemption is not decorative.
    let (_, fragment) = get(&app, &format!("/web/sales/{}", fixture.sale)).await;
    let err = check_same_page_selectors(
        "sale detail fragment",
        &fragment,
        &[],
        &RenderedPages::new(),
    )
    .unwrap_err();
    assert!(err.contains("#sale-record"), "{err}");
}

/// A declaration must be a bound check, not a trust list: declaring a selector
/// that no page renders must fail the guard, within the same run.
#[tokio::test]
async fn wiring_guard_rejects_an_external_selector_no_host_page_renders() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;
    let (probe_app, probe_pool) = test_app().await;
    let _probe_fixture = seed_wiring_fixture(&probe_app, &probe_pool).await;

    // The healthy declarations resolve on their host pages.
    let mut pages = guarded_pages(&fixture);
    assert_guarded_pages_are_wired(&app, &probe_app, &pages)
        .await
        .unwrap_or_else(|err| panic!("the declared exemptions must resolve: {err}"));

    // Mutation: declare a selector that exists nowhere, on a real page.
    let fragment = pages
        .iter_mut()
        .find(|page| page.label == "sale detail fragment")
        .expect("the sale detail fragment is guarded");
    fragment.external_selectors.push(ExternalSelector {
        selector: "#purchase-detail",
        host: "sale record page",
    });
    let err = assert_guarded_pages_are_wired(&app, &probe_app, &pages)
        .await
        .unwrap_err();
    eprintln!("bogus exemption rejected: {err}");
    assert!(err.contains("sale detail fragment"), "{err}");
    assert!(err.contains("#purchase-detail"), "{err}");
}

/// Blind spot 5: a colon in the query string is not a template placeholder.
#[test]
fn wiring_guard_allows_datetime_query_colons_but_rejects_path_colon() {
    id_free_page_shape(
        r#"<input hx-get="/web/transactions?from=2024-05-01T00:00&to=2024-05-02T23:59" />"#,
    )
    .unwrap();

    let err = id_free_page_shape(r#"<button hx-get="/web/sales/:id"></button>"#).unwrap_err();
    eprintln!("path colon rejected: {err}");
    assert!(err.contains("placeholder marker"), "{err}");
}

/// Blind spot 4a: a native form that rewrites `this.action` is dead under htmx
/// even when its `hx-*` attributes are wired.
#[test]
fn wiring_guard_catches_dead_native_form_action_rewrite() {
    let err = check_rendered_wiring_shape(
        "mutation",
        r#"<form method="post" action="/web/sales" onsubmit="this.action='/web/sales/1/confirm'; return true;"><button hx-post="/web/sales/confirm"></button></form>"#,
        true,
    )
    .unwrap_err();
    eprintln!("mutation-4a rejected: {err}");
    assert!(err.contains("this.action"), "{err}");
}

/// The widened non-vacuity boundary (S4): a native form with a real,
/// non-`#` action makes a target-free page guarded, not vacuous. Pinned on
/// all three sides of the boundary, asserting the guard's exact vacuity
/// text, so relaxing `wiring_is_vacuous` fails here before a page like
/// `/purchases/new` can silently lose its only wiring.
#[tokio::test]
async fn wiring_guard_pins_the_non_vacuity_boundary_of_native_form_actions() {
    let (app, _pool) = test_app().await;

    // The predicate directly, on all three boundaries.
    let form_with_action = extract_rendered_forms(r#"<form method="get" action="/login"></form>"#);
    assert!(!wiring_is_vacuous(&[], &form_with_action));
    let form_with_placeholder =
        extract_rendered_forms(r##"<form method="post" action="#"></form>"##);
    assert!(wiring_is_vacuous(&[], &form_with_placeholder));
    assert!(wiring_is_vacuous(&[], &[]));

    // And end-to-end through the guard, whose vacuity error text the
    // boundary pins assert verbatim.

    // (a) No hx-* target, but a form with a real action: guarded, not
    // vacuous — the form's action is probed and a real route answers.
    assert_htmx_targets_are_wired(
        &app,
        "mutation-5a",
        r#"<form method="get" action="/login"></form>"#,
        false,
    )
    .await
    .unwrap();

    // (b) A form whose action is the `#` placeholder posts nowhere, so it
    // checks nothing: vacuous.
    let err = assert_htmx_targets_are_wired(
        &app,
        "mutation-5b",
        r##"<form method="post" action="#"></form>"##,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        "mutation-5b: no hx-get/hx-post/hx-put/hx-patch/hx-delete targets or native form action rendered; guard would be vacuous",
        "{err}"
    );

    // (c) No form and no target at all: vacuous.
    let err = assert_htmx_targets_are_wired(&app, "mutation-5c", "<p>nothing wired</p>", false)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        "mutation-5c: no hx-get/hx-post/hx-put/hx-patch/hx-delete targets or native form action rendered; guard would be vacuous",
        "{err}"
    );
}

/// `data-action` names the failed action for the `#notice` region. It is not a
/// native form `action`, so the guard must not read it as one and probe
/// "Create product" as a URL.
#[test]
fn wiring_guard_does_not_read_data_action_as_a_native_action() {
    let html = r#"<form data-action="Create product" hx-post="/web/products"></form>"#;
    let forms = extract_rendered_forms(html);
    assert_eq!(forms.len(), 1);
    assert_eq!(
        forms[0].action, None,
        "data-action is a notice label, not an action URL"
    );
    check_rendered_wiring_shape("notice", html, false).unwrap();
}

/// Blind spot 2: the dead target `/web/sales/does-not-exist` matches the
/// path-param route `GET /web/sales/{id}`, so an OPTIONS probe saw 405 and the
/// guard called it wired. Probing the real POST verb sees 405 too, which fails.
#[tokio::test]
async fn wiring_guard_catches_dead_path_masked_by_path_param_route() {
    let (app, _pool) = test_app().await;
    let err = assert_htmx_targets_are_wired(
        &app,
        "mutation-2",
        r#"<button hx-post="/web/sales/does-not-exist"></button>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("mutation-2 rejected: {err}");
    assert!(err.contains("405"), "{err}");
}

/// Blind spot 3: the verb from the attribute is probed, so a GET-only path is
/// not a valid `hx-post` target even though the path exists.
#[tokio::test]
async fn wiring_guard_checks_the_real_verb_from_the_attribute() {
    let (app, _pool) = test_app().await;
    let err = assert_htmx_targets_are_wired(
        &app,
        "mutation-3",
        r#"<button hx-post="/web/sales/debt"></button>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("mutation-3 rejected: {err}");
    assert!(err.contains("405"), "{err}");

    // The control: the same path with its real verb passes.
    assert_htmx_targets_are_wired(
        &app,
        "control",
        r#"<button hx-get="/web/sales/debt"></button>"#,
        false,
    )
    .await
    .unwrap();
}

/// Blind spot 4b: native `action=` wiring is invisible to an hx-* scan, so the
/// guard probes it with the form method.
#[tokio::test]
async fn wiring_guard_probes_native_form_actions() {
    let (app, _pool) = test_app().await;
    let err = assert_htmx_targets_are_wired(
        &app,
        "mutation-4",
        r#"<form method="post" action="/web/sales/does-not-exist"><button hx-post="/web/sales/confirm"></button></form>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("mutation-4b rejected: {err}");
    assert!(err.contains("form action"), "{err}");
    assert!(err.contains("405"), "{err}");
}

/// Blind spot 6: the money invariant must validate `refund_transaction_id`, not
/// just the original link. A cancelled cash sale produces both links; corrupting
/// the refund pointer must make the check fail.
#[tokio::test]
async fn money_invariant_catches_broken_refund_link() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let account = create_account_via_web(&app, &pool, "RefundInv", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "REFUND-INV", "1", "10").await;
    record_stock_via_web(&app, product, "5").await;
    let sale = create_sale_draft_via_web(&app, &pool, "RefundInvBuyer", "Cash", "").await;
    add_sale_line_via_web(&app, sale, product, "1").await;
    confirm_sale_via_web(&app, sale, Some(cash)).await;
    let (status, body) = post_form(
        &app,
        "/web/sales/cancel",
        &format!("sale_id={sale}&reason=refund-invariant"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Healthy fixture: the invariant holds and is not vacuous.
    check_payment_links_are_traceable(&pool).await.unwrap();

    // Point the refund at a real transaction with a different (absent) reference.
    // The raw row is a fixture the suite plants directly, so the actor is the
    // migration's sentinel account, not any operator's principal.
    let bogus: (i64,) = sqlx::query_as(
        "INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by) \
         VALUES (?, 'Income', '1', 'bogus', NULL, '2024-05-01', ?) RETURNING id",
    )
    .bind(account)
    .bind(test_support::audit_actor_id(&pool).await.unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE sale_payments SET refund_transaction_id = ? \
         WHERE refund_transaction_id IS NOT NULL",
    )
    .bind(bogus.0)
    .execute(&pool)
    .await
    .unwrap();

    let err = check_payment_links_are_traceable(&pool).await.unwrap_err();
    eprintln!("mutation-6 rejected: {err}");
    assert!(err.contains("refund"), "{err}");
    assert!(err.contains("reference"), "{err}");
}

// ---------------------------------------------------------------------------
// JS handler URLs and refund identity: mutation tests.
// ---------------------------------------------------------------------------

/// Hole 1: URLs written inside `hx-on` bodies and inline scripts are probed
/// with the same oracle as `hx-*` targets, with the verb from the call.
#[tokio::test]
async fn wiring_guard_probes_js_handler_urls() {
    let (app, _pool) = test_app().await;

    // Dead URL in an hx-on body: routing fallback.
    let err = assert_htmx_targets_are_wired(
        &app,
        "js-hx-on",
        r#"<button hx-get="/web/accounts"></button><form hx-on::after-request="htmx.ajax('GET','/web/category-options-removed','#filter-category');"></form>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("js hx-on url rejected: {err}");
    assert!(err.contains("/web/category-options-removed"), "{err}");
    assert!(err.contains("hit the routing fallback"), "{err}");

    // Dead URL in an inline script body: routing fallback.
    let err = assert_htmx_targets_are_wired(
        &app,
        "js-script",
        r#"<button hx-get="/web/accounts"></button><script>htmx.ajax('GET','/web/account-options-removed','#x');</script>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("js script url rejected: {err}");
    assert!(err.contains("/web/account-options-removed"), "{err}");

    // Explicit call verb: POST to a GET-only path is 405 even though the URL
    // itself is live.
    let err = assert_htmx_targets_are_wired(
        &app,
        "js-verb",
        r#"<button hx-get="/web/accounts"></button><script>htmx.ajax('POST','/web/sales/debt','#x');</script>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("js verb rejected: {err}");
    assert!(err.contains("405"), "{err}");

    // Selectors and event names are not URLs: nothing extra is probed.
    assert_htmx_targets_are_wired(
        &app,
        "js-control",
        r#"<button hx-get="/web/accounts"></button><script>htmx.trigger('#account-list','refresh'); document.body.addEventListener('transaction-created', function(){});</script>"#,
        false,
    )
    .await
    .unwrap();
}

/// Non-path string literals inside scripts must not become probes.
#[test]
fn wiring_guard_ignores_non_path_js_strings() {
    check_rendered_wiring_shape(
        "js-control",
        r#"<script>
            htmx.trigger('#sale-list','refresh');
            document.body.addEventListener('sale-created', function(){});
            let cdn = '//cdn.example.com/app.js';
            let note = '/* not a path */';
            let spaced = '/web/not a path';
        </script>"#,
        false,
    )
    .unwrap();
}

/// The live JS-handler URLs must actually be extracted from the real pages, or
/// the probe for them would be vacuous.
#[tokio::test]
async fn seeded_pages_expose_their_js_handler_urls_to_the_guard() {
    let (app, _pool) = test_app().await;

    let (status, html) = get(&app, "/products").await;
    assert_eq!(status, StatusCode::OK);
    let targets: Vec<(String, String)> = extract_script_targets(&html)
        .unwrap()
        .into_iter()
        .map(|t| (t.method, t.target))
        .collect();
    assert!(
        targets
            .iter()
            .any(|(m, t)| m == "GET" && t == "/web/category-options"),
        "products page must expose htmx.ajax('/web/category-options'): {targets:?}"
    );

    let (status, html) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    let targets: Vec<(String, String)> = extract_script_targets(&html)
        .unwrap()
        .into_iter()
        .map(|t| (t.method, t.target))
        .collect();
    assert!(
        targets
            .iter()
            .any(|(m, t)| m == "GET" && t == "/web/account-options"),
        "dashboard must expose htmx.ajax('/web/account-options'): {targets:?}"
    );
}

/// Hole 2: a cross-payment refund swap shares the sale reference, so the
/// reference-only check accepted it. A refund must reverse its own payment.
#[tokio::test]
async fn money_invariant_catches_cross_payment_refund_swap() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "SwapInv", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "SWAP-INV", "1", "10").await;
    record_stock_via_web(&app, product, "5").await;
    let sale = create_sale_draft_via_web(&app, &pool, "SwapInvBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;
    let (status, body) = pay_sale_via_web(&app, sale, cash, "30").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = pay_sale_via_web(&app, sale, cash, "20").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post_form(
        &app,
        "/web/sales/cancel",
        &format!("sale_id={sale}&reason=swap"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Healthy fixture: every refund reverses its own payment.
    check_payment_links_are_traceable(&pool).await.unwrap();

    // Point payment 1's refund at payment 2's original transaction: same sale
    // reference, but not this payment's refund.
    let payments: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT id, transaction_id, refund_transaction_id FROM sale_payments ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(payments.len(), 2);
    sqlx::query("UPDATE sale_payments SET refund_transaction_id = ? WHERE id = ?")
        .bind(payments[1].1.unwrap())
        .bind(payments[0].0)
        .execute(&pool)
        .await
        .unwrap();

    let err = check_payment_links_are_traceable(&pool).await.unwrap_err();
    eprintln!("cross-payment refund swap rejected: {err}");
    assert!(err.contains("refund"), "{err}");
    assert!(err.contains("opposite"), "{err}");
}

// ---------------------------------------------------------------------------
// JS URL styles + global payment ownership: mutation tests.
// ---------------------------------------------------------------------------

/// Claim A part 2: a URL built dynamically cannot be verified, so the guard
/// enforces the plain-literal convention instead of silently missing it.
#[test]
fn wiring_guard_rejects_dynamic_js_urls() {
    let err = check_rendered_wiring_shape(
        "dynamic",
        r#"<script>htmx.ajax('GET','/web/category-options'+'-removed','#x');</script>"#,
        false,
    )
    .unwrap_err();
    eprintln!("dynamic concat rejected: {err}");
    assert!(
        err.contains("plain single or double quoted literal"),
        "{err}"
    );

    let err = check_rendered_wiring_shape(
        "dynamic",
        r#"<script>htmx.ajax('GET',`/web/sales/${id}/confirm`,`#x`);</script>"#,
        false,
    )
    .unwrap_err();
    eprintln!("dynamic template rejected: {err}");
    assert!(
        err.contains("plain single or double quoted literal"),
        "{err}"
    );

    // The enforced convention: a plain literal still passes.
    check_rendered_wiring_shape(
        "dynamic-control",
        r#"<script>htmx.ajax('GET','/web/category-options','#x');</script>"#,
        false,
    )
    .unwrap();
}

/// Claim A part 1: decoded entities, single-quoted attributes and plain
/// backticks are all scanned and probed.
#[tokio::test]
async fn wiring_guard_probes_js_url_styles() {
    let (app, _pool) = test_app().await;

    // Plain backtick literal.
    let err = assert_htmx_targets_are_wired(
        &app,
        "backtick",
        r#"<button hx-get="/web/accounts"></button><script>htmx.ajax('GET',`/web/account-options-removed`,`#x`);</script>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("backtick url rejected: {err}");
    assert!(err.contains("/web/account-options-removed"), "{err}");

    // Single-quoted HTML attribute containing JS double quotes.
    let err = assert_htmx_targets_are_wired(
        &app,
        "single-attr",
        r##"<button hx-get="/web/accounts"></button><form hx-on::after-request='htmx.ajax("GET","/web/category-options-removed","#x")'></form>"##,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("single-quoted attr url rejected: {err}");
    assert!(err.contains("/web/category-options-removed"), "{err}");

    // HTML-entity-encoded quotes.
    let err = assert_htmx_targets_are_wired(
        &app,
        "entities",
        r#"<button hx-get="/web/accounts"></button><form hx-on::after-request="htmx.ajax(&quot;GET&quot;,&quot;/web/account-options-removed&quot;,&quot;#x&quot;)"></form>"#,
        false,
    )
    .await
    .unwrap_err();
    eprintln!("entity-encoded url rejected: {err}");
    assert!(err.contains("/web/account-options-removed"), "{err}");

    // The live plain literals keep passing.
    assert_htmx_targets_are_wired(
        &app,
        "plain-control",
        r#"<button hx-get="/web/accounts"></button><form hx-on::after-request="htmx.ajax('GET','/web/account-options','#x')"></form>"#,
        false,
    )
    .await
    .unwrap();
}

/// Claim B: equal-amount payments are interchangeable to the identity checks,
/// so global ownership must reject the refund-to-refund and both-pointers
/// swaps.
#[tokio::test]
async fn money_invariant_catches_equal_amount_pointer_swaps() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "EqualInv", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "EQUAL-INV", "1", "10").await;
    record_stock_via_web(&app, product, "5").await;
    let sale =
        create_sale_draft_via_web(&app, &pool, "EqualInvBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;
    let (status, body) = pay_sale_via_web(&app, sale, cash, "25").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = pay_sale_via_web(&app, sale, cash, "25").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post_form(
        &app,
        "/web/sales/cancel",
        &format!("sale_id={sale}&reason=equal-swap"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    check_payment_links_are_traceable(&pool).await.unwrap();

    let payments: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT id, transaction_id, refund_transaction_id FROM sale_payments ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(payments.len(), 2);

    // Variant 1: refund-to-refund (identity facts all match).
    sqlx::query("UPDATE sale_payments SET refund_transaction_id = ? WHERE id = ?")
        .bind(payments[1].2.unwrap())
        .bind(payments[0].0)
        .execute(&pool)
        .await
        .unwrap();
    let err = check_payment_links_are_traceable(&pool).await.unwrap_err();
    eprintln!("equal-amount refund swap rejected: {err}");
    assert!(err.contains("claimed by both"), "{err}");

    // Variant 2: both pointers of payment 1 point at payment 2's pair.
    sqlx::query(
        "UPDATE sale_payments SET transaction_id = ?, refund_transaction_id = ? WHERE id = ?",
    )
    .bind(payments[1].1.unwrap())
    .bind(payments[1].2.unwrap())
    .bind(payments[0].0)
    .execute(&pool)
    .await
    .unwrap();
    let err = check_payment_links_are_traceable(&pool).await.unwrap_err();
    eprintln!("both-pointers swap rejected: {err}");
    assert!(err.contains("claimed by both"), "{err}");
}

/// The payment-traceability invariant must also see movements that no payment
/// claims: a failure between creating the finance movement and inserting the
/// payment leaves an orphan Income that inflates the account while the sale
/// stays unpaid. The collection is deliberately not transactional across
/// modules, so the invariant detects the residual instead.
#[tokio::test]
async fn money_invariant_catches_orphan_document_movement() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "OrphanInv", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "ORPHAN-INV", "1", "10").await;
    record_stock_via_web(&app, product, "5").await;
    let sale = create_sale_draft_via_web(&app, &pool, "OrphanBuyer", "Credit", "2024-06-02").await;
    add_sale_line_via_web(&app, sale, product, "1").await;
    confirm_sale_via_web(&app, sale, None).await;
    let sale_number = sale_detail(&app, sale).await["sale"]["sale_number"]
        .as_str()
        .unwrap()
        .to_string();

    // Injected failure between the movement and the payment row.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER injected_payment_failure BEFORE INSERT ON sale_payments \
         WHEN NEW.sale_id = {sale} BEGIN SELECT RAISE(ABORT, 'injected payment failure'); END"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = pay_sale_via_web(&app, sale, cash, "10").await;
    assert!(status.is_server_error(), "{status} {body}");

    let orphan: (i64,) = sqlx::query_as(
        "SELECT t.id FROM transactions t WHERE t.reference = ? \
         AND NOT EXISTS (SELECT 1 FROM sale_payments sp \
                         WHERE sp.transaction_id = t.id OR sp.refund_transaction_id = t.id)",
    )
    .bind(&sale_number)
    .fetch_one(&pool)
    .await
    .unwrap();

    let err = check_payment_links_are_traceable(&pool).await.unwrap_err();
    eprintln!("orphan invariant error: {err}");
    assert!(err.contains("claimed by no payment"), "{err}");
    assert!(
        err.contains(&orphan.0.to_string()),
        "the invariant must report the orphan id: {err}"
    );

    // The orphan inflated the account while the sale stayed unpaid.
    let tx: (String, String) = sqlx::query_as("SELECT kind, amount FROM transactions WHERE id = ?")
        .bind(orphan.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tx.0, "Income");
    assert_eq!(
        Decimal::from_str(&tx.1).unwrap(),
        Decimal::from_str("10").unwrap()
    );
    let detail = sale_detail(&app, sale).await;
    assert_eq!(
        dec(&detail["paid"]),
        Decimal::ZERO,
        "the orphan paid nothing"
    );
    assert_eq!(dec(&detail["due"]), dec(&detail["total"]));
}

// ---------------------------------------------------------------------------
// Shell: sidebar, page header and non-blocking feedback (redesign-interface N1a)
// ---------------------------------------------------------------------------

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// `alert(` as a call, not the `price_alert(` method name.
fn contains_bare_alert(text: &str) -> bool {
    let mut rest = text;
    while let Some(position) = rest.find("alert(") {
        let preceded_by_identifier = rest[..position]
            .chars()
            .next_back()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false);
        if !preceded_by_identifier {
            return true;
        }
        rest = &rest[position + "alert(".len()..];
    }
    false
}

/// The opening tag of the sidebar entry the server marked active.
fn active_nav_tag(html: &str) -> &str {
    let marker = html
        .find("aria-current=\"page\"")
        .unwrap_or_else(|| panic!("no sidebar entry is marked active: {html:.600}"));
    let start = html[..marker]
        .rfind('<')
        .expect("the active marker must sit inside a tag");
    let end = marker
        + html[marker..]
            .find('>')
            .expect("unterminated active nav tag");
    &html[start..=end]
}

/// Value of `data-nav` on the active sidebar entry.
fn active_nav_key(html: &str) -> String {
    let tag = active_nav_tag(html);
    let start = tag
        .find("data-nav=\"")
        .expect("the active entry must carry a data-nav key")
        + "data-nav=\"".len();
    let rest = &tag[start..];
    let end = rest.find('"').expect("unterminated data-nav attribute");
    rest[..end].to_string()
}

#[test]
fn template_suite_never_calls_the_blocking_alert() {
    fn collect(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("templates directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(&path, offenders);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("html") {
                let text = std::fs::read_to_string(&path).expect("read template");
                if contains_bare_alert(&text) {
                    offenders.push(path.display().to_string());
                }
            }
        }
    }
    let mut offenders = Vec::new();
    collect(std::path::Path::new("templates"), &mut offenders);
    assert!(
        offenders.is_empty(),
        "alert() is blocking and must be replaced by the #notice region: {offenders:?}"
    );
}

// Mint actions: the blue button overrides retire (purchases-create-and-header T1c)
// -------------------------------------------------------------------------------

/// The opening tag of the element whose markup contains `needle`.
fn opening_tag_containing<'a>(html: &'a str, needle: &str) -> &'a str {
    let marker = html
        .find(needle)
        .unwrap_or_else(|| panic!("nothing carries {needle}: {html:.600}"));
    let start = html[..marker]
        .rfind('<')
        .expect("the needle must sit inside a tag");
    let end = marker + html[marker..].find('>').expect("unterminated opening tag");
    &html[start..=end]
}

/// T1c drift pin: the base `button` element is already mint with a dark label
/// (`assets/tailwind.css`, the `button { @apply ... bg-accent ... }` rule, with
/// its own hover and disabled states), so any template still writing
/// `bg-accent2` on a button is an override fighting the base style, not a
/// second palette. Walk the whole template tree so an eleventh blue button
/// cannot be added unnoticed.
#[test]
fn no_template_still_writes_the_bg_accent2_button_override() {
    fn collect(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("templates directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(&path, offenders);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("html") {
                let text = std::fs::read_to_string(&path).expect("read template");
                if text.contains("bg-accent2") {
                    offenders.push(path.display().to_string());
                }
            }
        }
    }
    let mut offenders = Vec::new();
    collect(std::path::Path::new("templates"), &mut offenders);
    assert!(
        offenders.is_empty(),
        "the base button is already mint: retire the bg-accent2 button overrides: {offenders:?}"
    );
}

/// T1c rendered assertion: buttons that used to carry `bg-accent2 text-white`
/// must now inherit the base mint button style — their tags name neither
/// override — while every identifying attribute (type, id, label, the
/// disabled state) survives untouched.
#[tokio::test]
async fn rendered_action_buttons_inherit_the_base_mint_not_the_accent2_override() {
    let (app, pool) = test_app().await;

    // The dashboard's Add Transaction button: a plain submit button with no id,
    // so pin its type and its label.
    let (status, html) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    let tag = opening_tag_containing(&html, ">Add transaction<");
    assert!(
        tag.contains("<button"),
        "the action must stay a button element: {tag}"
    );
    assert!(
        tag.contains("type=\"submit\""),
        "the action's type must survive: {tag}"
    );
    assert!(
        !tag.contains("bg-accent2"),
        "the action must not override the mint base style: {tag}"
    );
    assert!(
        !tag.contains("text-white"),
        "the label must not override the dark base label: {tag}"
    );

    // A purchase draft's record page: the action bar's Confirm button carries
    // an id and a disabled state (no lines yet) that must both survive.
    let supplier = create_supplier_via_web(&app, &pool, "MintSur").await;
    let draft =
        create_purchase_draft_with_due(&app, supplier, "Credit", "2024-05-02", "2024-12-31").await;
    let (status, html) = get(&app, &format!("/purchases/{draft}")).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    let tag = opening_tag_containing(&html, "id=\"open-confirm\"");
    let tag_end = html.find(tag).expect("the tag came from this html") + tag.len();
    assert!(
        tag.contains("<button"),
        "the action must stay a button element: {tag}"
    );
    assert!(
        html[tag_end..].starts_with("Confirm \u{25be}"),
        "the action's label must survive: {tag}..."
    );
    assert!(
        tag.contains("disabled"),
        "a draft with no lines keeps its disabled Confirm: {tag}"
    );
    assert!(
        !tag.contains("bg-accent2"),
        "the action must not override the mint base style: {tag}"
    );
    assert!(
        !tag.contains("text-white"),
        "the label must not override the dark base label: {tag}"
    );
}

#[tokio::test]
async fn sidebar_marks_the_active_entry_from_the_server_on_every_page() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;
    let pages = [
        ("dashboard", "/".to_string(), "dashboard"),
        ("products", "/products".to_string(), "products"),
        ("sales", "/sales".to_string(), "sales"),
        ("purchases", "/purchases".to_string(), "purchases"),
        ("suppliers", "/suppliers".to_string(), "suppliers"),
        ("customers", "/customers".to_string(), "customers"),
        (
            "account detail",
            format!("/accounts/{}", fixture.account),
            "accounts",
        ),
        (
            "customer statement",
            format!("/customers/{}", fixture.customer),
            "customers",
        ),
        (
            "sale record page",
            format!("/sales/{}", fixture.sale),
            "sales",
        ),
        (
            "purchase record page",
            format!("/purchases/{}", fixture.purchase),
            "purchases",
        ),
    ];
    for (label, path, expected) in pages {
        let (status, html) = get(&app, &path).await;
        assert_eq!(status, StatusCode::OK, "{label} {path}: {html:.400}");
        assert_eq!(
            count_occurrences(&html, "aria-current=\"page\""),
            1,
            "{label}: exactly one sidebar entry must be active"
        );
        let active = active_nav_tag(&html);
        assert!(
            active.contains("data-nav-active=\"true\""),
            "{label}: the active entry needs a machine-checkable marker: {active}"
        );
        assert!(
            active.contains("text-accent"),
            "{label}: the active entry must be visually distinct: {active}"
        );
        assert_eq!(active_nav_key(&html), expected, "{label} {path}");
    }
}

#[tokio::test]
async fn sidebar_groups_navigation_into_operation_catalogue_and_cash() {
    let (app, _pool) = test_app().await;
    let (status, html) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);

    // Groups render in the documented order.
    let mut cursor = 0;
    for group in ["operation", "catalogue", "cash"] {
        let needle = format!("data-nav-group=\"{group}\"");
        let found = html[cursor..]
            .find(&needle)
            .unwrap_or_else(|| panic!("group {group} missing or out of order: {html:.600}"));
        cursor += found + needle.len();
    }

    // Every destination renders exactly once.
    for key in [
        "dashboard",
        "sales",
        "purchases",
        "products",
        "suppliers",
        "customers",
        "accounts",
    ] {
        assert_eq!(
            count_occurrences(&html, &format!("data-nav=\"{key}\"")),
            1,
            "nav key {key} must render exactly once"
        );
    }

    // Secondary shell facts stay available but below navigation.
    assert!(html.contains("local · SQLite"), "environment line missing");
    assert!(
        html.contains("href=\"/api/accounts\""),
        "REST API link missing"
    );
    assert!(
        html.contains("href=\"/#accounts\""),
        "accounts has no page yet: the entry must point at the dashboard section"
    );
}

/// The dashboard is the last page still on the one-action page_header component.
/// Products left it: the redesign needs two modal buttons in the header slot,
/// which the single-action header cannot host, so it moved to the parties-style
/// title row (covered by the products redesign test below).
#[tokio::test]
async fn dashboard_uses_the_page_header_component() {
    let (app, _pool) = test_app().await;
    let (path, title, action) = ("/", "Dashboard", "#new-transaction");
    {
        let (status, html) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            count_occurrences(&html, "data-page-header"),
            1,
            "{path}: exactly one page header"
        );
        assert!(
            html.contains(&format!("data-page-title>{title}</h1>")),
            "{path}: page title missing: {html:.400}"
        );
        assert_eq!(
            count_occurrences(&html, "data-page-action"),
            1,
            "{path}: exactly one primary action"
        );
        assert!(
            html.contains(&format!("href=\"{action}\"")),
            "{path}: primary action must target {action}"
        );
        assert!(
            html.contains(&format!("id=\"{}\"", &action[1..])),
            "{path}: primary action target {action} must exist on the page"
        );
    }

    // The dashboard is top level: no breadcrumb.
    assert!(
        !get(&app, "/").await.1.contains("data-page-breadcrumb"),
        "dashboard is top level: no breadcrumb"
    );
}

/// The products redesign (odd/tasks/redesign-products.md T3): the permanent
/// New Category / New Product cards became `<dialog>` modals opened by header
/// buttons, the Stock Movement and REST API cards left the page, and a
/// right-hand drawer opens on click. This test pins the page-level contract of
/// that redesign; the e2e browser suite exercises the interactions.
#[tokio::test]
async fn products_page_uses_modals_drawer_and_clickable_rows() {
    let (app, pool) = test_app().await;
    let _a = create_product_full_via_web(&app, &pool, "REDESIGN-A", "Widget A", None).await;
    let _b = create_product_full_via_web(&app, &pool, "REDESIGN-B", "Widget B", None).await;

    let (status, products) = get(&app, "/products").await;
    assert_eq!(status, StatusCode::OK);

    // The creation flows are modals now: both dialog elements exist and exactly
    // the two header buttons open them with showModal().
    assert!(
        products.contains("id=\"new-category-dialog\""),
        "the New category dialog must be rendered"
    );
    assert!(
        products.contains("id=\"new-product-dialog\""),
        "the New product dialog must be rendered"
    );
    assert_eq!(
        count_occurrences(&products, ".showModal()"),
        2,
        "exactly two buttons may open modals"
    );
    assert!(
        products.contains("document.getElementById('new-category-dialog').showModal()"),
        "the New category button must open its dialog"
    );
    assert!(
        products.contains("document.getElementById('new-product-dialog').showModal()"),
        "the New product button must open its dialog"
    );

    // The right-hand drawer shell is always present but empty on load: detail
    // content is fetched on click, never pre-rendered.
    assert!(
        products.contains("id=\"product-drawer\""),
        "the product drawer must be rendered"
    );
    assert!(
        products.contains("id=\"product-drawer-body\""),
        "the product drawer body must be rendered"
    );
    assert!(
        products.contains("function closeProductDrawer()"),
        "the page must define closeProductDrawer"
    );

    // The permanent New Product card is gone. `id="new-product"` with the
    // closing quote cannot match `id="new-product-dialog"`, so this stays exact.
    assert!(
        !products.contains("id=\"new-product\""),
        "the permanent New Product card must not exist"
    );

    // The REST API card is gone: the endpoints stay, the page no longer
    // advertises them. The sidebar's `/api` link renders the same phrase on every
    // page, so the pin is the card heading, not the phrase anywhere in the shell.
    assert!(
        !products.contains(">REST API</h2>"),
        "the REST API card must not be rendered"
    );

    // Every row binds its name to the drawer: one detail link per seeded row.
    assert_eq!(
        count_occurrences(&products, "/web/products/detail/"),
        2,
        "each product row must open the drawer once"
    );
}

/// The purchases-index redesign S1 (odd/tasks/redesign-purchases-index.md):
/// the four REST API cards left the operator's pages (`/`, `/purchases`,
/// `/sales`, `/suppliers`). The endpoints themselves stay; the shell's
/// `REST API ↗` link (`partials/sidebar.html`) remains the surviving API
/// surface and is pinned separately by
/// `sidebar_groups_navigation_into_operation_catalogue_and_cash`.
#[tokio::test]
async fn purchases_page_drops_the_rest_api_card() {
    let (app, _pool) = test_app().await;
    let (status, purchases) = get(&app, "/purchases").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !purchases.contains(">REST API</h2>"),
        "the REST API card must not be rendered on /purchases"
    );
}

/// The dialog creation flow (purchases-create-and-header T3): the `New
/// purchase` primary action opens the creation dialog — a `<button>` with the
/// mint classes the header component renders, onclick the dialog's
/// `showModal()` — and the dialog holds the T2 supplier picker pre-filled
/// with the LAST USED supplier plus the creation date. The form posts the
/// existing `POST /web/purchases`, whose non-htmx branch lands the browser on
/// the new draft's record.
#[tokio::test]
async fn purchases_dialog_offers_the_last_used_supplier_on_the_list_page() {
    let (app, pool) = test_app().await;
    let first = create_supplier_via_web(&app, &pool, "FirstUsedSup").await;
    let last = create_supplier_via_web(&app, &pool, "LastUsedSup").await;
    // Two prior purchases, so the LAST used supplier is the one the dialog
    // pre-fills. Created over the web with explicit dates to keep the rows
    // deterministic; the drafts need no lines.
    for (supplier, date) in [(first, "2024-05-01"), (last, "2024-05-02")] {
        let body = format!("supplier_id={supplier}&purchase_date={date}");
        let (status, location) = post_form_plain(&app, "/web/purchases", &body).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "create draft must redirect: {location}"
        );
    }

    let (status, page) = get(&app, "/purchases").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    // The primary action is a button that opens the dialog — never a link to
    // a creation page (AC2: none exists).
    let tag = element_tag_containing(page.as_str(), "data-page-action");
    // `contains("bg-accent")` also matched `bg-accent2`, so the old assertion
    // could never have caught the blue regression it names. The guard now pins
    // the component instead: the colour is asserted by the visual net.
    assert!(
        tag.contains("<button")
            && tag
                .contains("onclick=\"document.getElementById('new-purchase-dialog').showModal()\"")
            && tag.contains("btn-primary"),
        "the primary action must open the creation dialog as the primary button component: {tag}"
    );
    assert!(
        page.contains("data-page-action>New purchase</button>"),
        "the action keeps its label: {page:.600}"
    );
    assert!(
        !page.contains("/purchases/new"),
        "nothing may link to the deleted creation page: {page:.600}"
    );
    // The dialog holds the shared supplier picker pre-filled with the LAST
    // used supplier (the picker's text field carries the name — its form
    // carries NO hidden id, so Enter resolves the field's text server-side).
    assert!(
        page.contains("id=\"new-purchase-dialog\""),
        "the creation dialog must render: {page:.600}"
    );
    let field = element_tag_containing(page.as_str(), "id=\"new-purchase-supplier\"");
    assert!(
        field.contains("value=\"LastUsedSup\""),
        "the picker must be pre-filled with the last used supplier's name: {field}"
    );
    assert!(
        !page.contains("name=\"supplier_id\""),
        "the picker's own form must not carry a hidden current id: {page:.600}"
    );
    assert!(
        page.contains("name=\"purchase_date\""),
        "the dialog must render the creation date: {page:.600}"
    );
}

/// With NO purchases yet there is no last used supplier: the dialog renders
/// with an EMPTY supplier field and the operator must choose (never a silent
/// guess — the feature doc's hazard).
#[tokio::test]
async fn purchases_dialog_with_no_purchases_opens_with_an_empty_supplier() {
    let (app, _pool) = test_app().await;
    let (status, page) = get(&app, "/purchases").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        page.contains("id=\"new-purchase-dialog\""),
        "the creation dialog must still render: {page:.600}"
    );
    let field = element_tag_containing(page.as_str(), "id=\"new-purchase-supplier\"");
    assert!(
        field.contains("value=\"\""),
        "with no purchases yet the picker must render empty: {field}"
    );
}

/// A plain (non-htmx) create posts the existing `POST /web/purchases` and
/// lands the browser on the new draft's record (a 303 Location, not the htmx
/// `HX-Redirect` header — that branch belongs to htmx callers and stays
/// untouched). The explicit `supplier_id` here models a clicked result, the
/// only path where an id wins.
#[tokio::test]
async fn dialog_create_lands_on_the_record() {
    let (app, pool) = test_app().await;
    let supplier = create_supplier_via_web(&app, &pool, "LandingSup").await;
    let body = format!(
        "supplier_id={supplier}&purchase_date=2024-05-02&supplier_invoice_no=INV-9&notes=via+the+dialog"
    );
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "a plain full-page create must redirect"
    );
    let location = resp
        .headers()
        .get("location")
        .expect("a plain create must redirect to the record")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.starts_with("/purchases/"),
        "the redirect must land on the new record: {location}"
    );
    let purchase_id: i64 = location["/purchases/".len()..]
        .parse()
        .unwrap_or_else(|_| panic!("the redirect must end in the purchase id: {location}"));
    // The two optional fields the dialog sends persist server-side.
    let (invoice, notes): (Option<String>, String) =
        sqlx::query_as("SELECT supplier_invoice_no, notes FROM purchases WHERE id = ?")
            .bind(purchase_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(invoice.as_deref(), Some("INV-9"), "invoice no must persist");
    assert_eq!(notes, "via the dialog", "notes must persist");
}

/// The two resolution refusals the dialog can hit (T3): a typed name that is
/// not an exact supplier name is a 400 naming the value, and neither an id
/// nor a name is the required-field refusal — in both cases NO purchase is
/// created. The supplier is never silently guessed (the feature doc's
/// hazard): an exact name resolves, anything else refuses.
#[tokio::test]
async fn web_create_purchase_resolves_a_typed_name_and_refuses_unknown_or_absent_suppliers() {
    let (app, pool) = test_app().await;
    create_supplier_via_web(&app, &pool, "Typed Name Sup").await;

    // An exact typed name with no id resolves and creates.
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from("supplier_name=Typed+Name+Sup"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "an exact typed name must resolve and create"
    );
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let purchase_id: i64 = location["/purchases/".len()..].parse().unwrap();
    let (supplier_id,): (i64,) = sqlx::query_as("SELECT supplier_id FROM purchases WHERE id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (name,): (String,) = sqlx::query_as("SELECT name FROM suppliers WHERE id = ?")
        .bind(supplier_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        name, "Typed Name Sup",
        "the draft must belong to the resolved supplier"
    );

    // An unknown name refuses with the 400 naming the value.
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from("supplier_name=Missing+Supplier"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an unknown name must refuse"
    );
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&body).to_string();
    assert!(
        body.contains("Missing Supplier"),
        "the refusal must name the value the operator typed: {body:.400}"
    );

    // Neither id nor name: the required-field refusal.
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from("purchase_date=2024-05-02"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a post with neither id nor name must refuse"
    );
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "the refusals must have created nothing: only the resolved-name draft exists"
    );
}

/// The explicit supplier_id wins over a typed name (the picker's host
/// contract, T2): a clicked result posts both, and the draft must belong to
/// the id's supplier.
#[tokio::test]
async fn web_create_purchase_an_explicit_supplier_id_wins_over_the_typed_name() {
    let (app, pool) = test_app().await;
    let id_sup = create_supplier_via_web(&app, &pool, "IdSup").await;
    create_supplier_via_web(&app, &pool, "NameSup").await;

    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let body = format!("supplier_id={id_sup}&supplier_name=NameSup");
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "the post must succeed: {resp:?}"
    );
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let purchase_id: i64 = location["/purchases/".len()..].parse().unwrap();
    let (supplier_id,): (i64,) = sqlx::query_as("SELECT supplier_id FROM purchases WHERE id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        supplier_id, id_sup,
        "the explicit id must win over the typed name"
    );
}

/// The picker's own rendered form (the Enter path, T3's hazard): the dialog
/// arrives pre-filled with supplier A, the operator types a DIFFERENT exact
/// name B and presses Enter inside the text field, and the created draft
/// must belong to B. The body is built from the inputs the widget's own form
/// actually carries — exactly what the browser submits — so a hidden
/// `supplier_id` riding along in the picker's form would be seen here, and
/// the pre-filled supplier must never win over the typed name.
#[tokio::test]
async fn dialog_enter_path_assigns_the_typed_supplier_not_the_pre_filled_one() {
    let (app, pool) = test_app().await;
    let prefilled = create_supplier_via_web(&app, &pool, "PrefilledEnterSup").await;
    let typed = create_supplier_via_web(&app, &pool, "TypedEnterSup").await;
    // A prior purchase, so the dialog pre-fills PrefilledEnterSup.
    let (status, _) = post_form_plain(
        &app,
        "/web/purchases",
        &format!("supplier_id={prefilled}&purchase_date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, page) = get(&app, "/purchases").await;
    // The picker's OWN form (data-action "Save supplier") is what Enter
    // submits; the Create draft button is the separate include form.
    let pos = page
        .find("data-action=\"Save\"")
        .expect("the picker's form renders in the dialog");
    let form_start = page[..pos].rfind("<form").expect("the picker's form opens");
    let form_end = form_start + page[form_start..].find("</form>").expect("the form closes");
    let form = &page[form_start..form_end];

    // Collect the form's inputs as (name, value) — the request Enter sends.
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut rest = form;
    while let Some(i) = rest.find("<input") {
        rest = &rest[i..];
        let tag_end = rest.find('>').expect("unterminated input tag");
        let tag = &rest[..=tag_end];
        let name = tag
            .split("name=\"")
            .nth(1)
            .map(|s| s.split('"').next().unwrap());
        if let Some(name) = name {
            let value = tag
                .split("value=\"")
                .nth(1)
                .map(|s| s.split('"').next().unwrap())
                .unwrap_or_default();
            fields.push((name.to_string(), value.to_string()));
        }
        rest = &rest[tag_end..];
    }
    assert!(
        fields.iter().any(|(n, _)| n == "supplier_name"),
        "the picker's form must carry the text field: {fields:?}"
    );

    // The operator typed a different exact supplier: the text field's value
    // is B; everything else travels as the widget rendered it.
    let body = fields
        .iter()
        .map(|(name, value)| {
            let value = if name == "supplier_name" {
                "TypedEnterSup"
            } else {
                value
            };
            format!("{name}={value}")
        })
        .collect::<Vec<_>>()
        .join("&");
    let (status, location) = post_form_plain(&app, "/web/purchases", &body).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "Enter must create: {location}"
    );
    let purchase_id: i64 = location["/purchases/".len()..].parse().unwrap();
    let (supplier_id,): (i64,) = sqlx::query_as("SELECT supplier_id FROM purchases WHERE id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        supplier_id, typed,
        "the draft must belong to the supplier the operator typed, never the pre-filled one"
    );
}

/// The same Enter path with NO edit: the pre-filled text is the stored
/// supplier's name and the form resolves it to that same supplier — the
/// default keeps working without an id in the picker's form.
#[tokio::test]
async fn dialog_enter_path_with_the_unchanged_pre_fill_assigns_the_same_supplier() {
    let (app, pool) = test_app().await;
    let prefilled = create_supplier_via_web(&app, &pool, "UnchangedPrefillSup").await;
    let (status, _) = post_form_plain(
        &app,
        "/web/purchases",
        &format!("supplier_id={prefilled}&purchase_date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, page) = get(&app, "/purchases").await;
    let field = element_tag_containing(page.as_str(), "id=\"new-purchase-supplier\"");
    assert!(
        field.contains("value=\"UnchangedPrefillSup\""),
        "the dialog must be pre-filled with the last used supplier: {field}"
    );

    // Enter submits the picker's own form carrying the pre-filled name —
    // and nothing else but what the widget rendered.
    let (status, location) =
        post_form_plain(&app, "/web/purchases", "supplier_name=UnchangedPrefillSup").await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "Enter must create: {location}"
    );
    let purchase_id: i64 = location["/purchases/".len()..].parse().unwrap();
    let (supplier_id,): (i64,) = sqlx::query_as("SELECT supplier_id FROM purchases WHERE id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        supplier_id, prefilled,
        "the unchanged pre-fill must resolve to the same supplier"
    );
}

/// The dialog may not carry a date, so an omitted `purchase_date` defaults to
/// today server-side (the same default the collection endpoint always had —
/// now load-bearing for the dialog).
#[tokio::test]
async fn web_create_purchase_omitting_the_date_defaults_to_today() {
    let (app, pool) = test_app().await;
    let supplier = create_supplier_via_web(&app, &pool, "DatelessSup").await;
    let builder = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded"),
    );
    let today_before = chrono::Local::now().date_naive();
    let resp = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(format!("supplier_id={supplier}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "an omitted date must still create"
    );
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let purchase_id: i64 = location["/purchases/".len()..].parse().unwrap();
    let (date,): (String,) = sqlx::query_as("SELECT purchase_date FROM purchases WHERE id = ?")
        .bind(purchase_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let today_after = chrono::Local::now().date_naive();
    let utc_today = chrono::Utc::now().date_naive();
    assert!(
        date == today_before.to_string()
            || date == today_after.to_string()
            || date == utc_today.to_string(),
        "an omitted purchase_date must default to today: {date}"
    );
}

/// `/purchases/new` is DELETED (AC2): the route answers 404 and nothing can
/// link to a creation page that no longer exists.
#[tokio::test]
async fn purchases_new_page_is_deleted() {
    let (app, _pool) = test_app().await;
    let (status, body) = get(&app, "/purchases/new").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:.400}");
}

#[tokio::test]
async fn sales_page_drops_the_rest_api_card() {
    let (app, _pool) = test_app().await;
    let (status, sales) = get(&app, "/sales").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !sales.contains(">REST API</h2>"),
        "the REST API card must not be rendered on /sales"
    );
}

#[tokio::test]
async fn suppliers_page_drops_the_rest_api_card() {
    let (app, _pool) = test_app().await;
    let (status, suppliers) = get(&app, "/suppliers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !suppliers.contains(">REST API</h2>"),
        "the REST API card must not be rendered on /suppliers"
    );
}

#[tokio::test]
async fn dashboard_drops_the_rest_api_card() {
    let (app, _pool) = test_app().await;
    let (status, dashboard) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !dashboard.contains(">REST API</h2>"),
        "the REST API card must not be rendered on /"
    );
}

#[tokio::test]
async fn converted_pages_expose_the_notice_region_and_named_actions() {
    let (app, _pool) = test_app().await;

    let (status, dashboard) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(dashboard.contains("id=\"notice\""), "notice region missing");
    assert!(
        dashboard.contains("data-action=\"Create account\""),
        "the create-account form must name its action: {dashboard:.400}"
    );
    assert!(
        dashboard.contains("data-action=\"Add transaction\""),
        "the add-transaction form must name its action"
    );

    let (status, products) = get(&app, "/products").await;
    assert_eq!(status, StatusCode::OK);
    assert!(products.contains("id=\"notice\""), "notice region missing");
    // `Record movement` no longer renders on the page: the redesign moved that
    // form into the product drawer, which is fetched on click and therefore not
    // present in the served shell.
    for action in ["Create category", "Create product"] {
        assert!(
            products.contains(&format!("data-action=\"{action}\"")),
            "form action {action:?} must be named for the notice"
        );
    }

    assert!(
        !contains_bare_alert(&dashboard),
        "the served shell must not call alert()"
    );
}

// ---------------------------------------------------------------------------
// N4: the picker loads a sale from the keyboard and the scanner alone
// ---------------------------------------------------------------------------

/// The add-line response must bring the picker back out of band, empty and
/// focused, so the next scan lands without a click.
fn assert_oob_picker_is_empty_and_focused(html: &str) {
    // The response can carry more than one OOB element (the purchase action
    // bar rides out of band on add-line too) and more than one `#line-picker`
    // (the in-place picker plus its OOB copy): locate the OOB picker by ITS
    // tag carrying `hx-swap-oob`, never by the first OOB in the document.
    let mut from = 0usize;
    let (tag_start, tag_end) = loop {
        let rel = html[from..]
            .find("id=\"line-picker\"")
            .unwrap_or_else(|| panic!("the out-of-band picker must render: {html:.800}"));
        let pos = from + rel;
        let start = html[..pos]
            .rfind('<')
            .expect("the id must sit inside a tag");
        let end_rel = html[start..].find('>').expect("unterminated tag");
        if html[start..=start + end_rel].contains("hx-swap-oob") {
            break (start, start + end_rel);
        }
        from = pos + 1;
    };
    let oob_tag = &html[tag_start..=tag_end];
    assert!(oob_tag.contains("id=\"line-picker\""), "{oob_tag}");
    let oob = &html[tag_start..];
    assert!(
        oob.contains("autofocus"),
        "the picker must come back focused: {oob:.400}"
    );
    let input_pos = oob
        .find("id=\"product-picker\"")
        .expect("the out-of-band picker renders its field");
    let input_start = oob[..input_pos].rfind('<').unwrap();
    let input_end = input_pos + oob[input_pos..].find('>').unwrap();
    let input_tag = &oob[input_start..=input_end];
    assert!(
        !input_tag.contains("value="),
        "the picker must come back empty: {input_tag}"
    );
}

/// The whole loop over HTTP with the series of requests a USB reader produces:
/// type (the debounced search) then Enter (the line form), with the response
/// re-focusing an empty picker for the next scan. No request in the loop needs a
/// click, name, SKU and barcode all find the product, an exact barcode adds in
/// one step, removing a line updates the total, and an unknown value is a clear
/// 400 that adds nothing.
#[tokio::test]
async fn line_picker_loads_a_sale_without_a_click() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "SCAN-P", "1", "50").await;
    record_stock_via_web(&app, product, "20").await;
    let (status, body) = post_json(
        &app,
        &format!("/api/products/{product}/barcodes"),
        json!({ "code": "7791234567890" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed barcode: {body}");
    let sale = create_sale_draft_via_web(&app, &pool, "ScanBuyer", "Cash", "").await;
    let base = format!("/web/sales/{sale}");

    // The record page offers the picker island, its sibling results container
    // and no catalogue select. The island owns the search now, so the page
    // carries no declarative transport: the field carries no hx-get, trigger,
    // target, vals or keyup handler, and the debounce lives in
    // static/picker.js, not in markup. Escape is base.html's document-level
    // keydown handler, which serves both pickers.
    let (status, page) = get(&app, &format!("/sales/{sale}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains("<select name=\"product_id\""),
        "the catalogue select must be gone: {page:.600}"
    );

    // The island's mount point carries its calling context: one picker,
    // priced for a sale.
    assert_eq!(
        page.matches("id=\"line-picker\"").count(),
        1,
        "one island mount point: {page:.600}"
    );
    let container_pos = page
        .find("id=\"line-picker\"")
        .expect("the record page renders the island mount point");
    let container_start = page[..container_pos]
        .rfind('<')
        .expect("the attribute must sit inside a tag");
    let container_end =
        container_start + page[container_start..].find('>').expect("unterminated tag");
    let container_tag = &page[container_start..=container_end];
    assert!(
        container_tag.contains("id=\"line-picker\"")
            && container_tag.contains("data-price-kind=\"sale\""),
        "{container_tag}"
    );

    // The field carries no declarative search transport.
    let input_pos = page
        .find("id=\"product-picker\"")
        .expect("the record page renders the picker field");
    let input_start = page[..input_pos]
        .rfind('<')
        .expect("the id must sit inside a tag");
    let input_end = input_pos + page[input_pos..].find('>').expect("unterminated tag");
    let input_tag = &page[input_start..=input_end];
    for transport in [
        "hx-get",
        "hx-trigger",
        "hx-target",
        "hx-vals",
        "hx-on:keyup",
    ] {
        assert!(
            !input_tag.contains(transport),
            "the field must not carry {transport}: {input_tag}"
        );
    }

    // The single add-line form keeps the server's contract: the post, the
    // hidden island-owned product id and the default quantity.
    let form_pos = page[..input_pos]
        .rfind("<form")
        .expect("the field sits in the add-line form");
    let form_end = form_pos + page[form_pos..].find("</form>").expect("unterminated form");
    let form = &page[form_pos..form_end];
    assert!(
        form.contains(&format!("hx-post=\"/web/sales/{sale}/lines\"")),
        "{form:.600}"
    );
    assert!(form.contains("name=\"product_id\""), "{form:.600}");
    assert!(
        form.contains("name=\"qty\"") && form.contains("value=\"1\""),
        "a scan and a click must both carry the default quantity: {form:.600}"
    );

    // The results container is a sibling of the form, never inside it.
    assert!(
        !form.contains("id=\"product-search-results\""),
        "the results container must be a sibling of the picker form, never inside it: {form:.600}"
    );
    assert!(
        page.contains("id=\"product-search-results\""),
        "{page:.600}"
    );

    // The debounce moved with the island: the island file declares it.
    assert!(
        include_str!("../static/picker.js").contains("DEBOUNCE_MS = 250"),
        "the search must be debounced by static/picker.js"
    );

    // Scan 1: the reader types the barcode and presses Enter. The form carries the
    // field and the quantity, never a product id.
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567890&qty=2&unit_price=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("product SCAN-P"), "{added:.600}");
    assert!(
        added.contains("50 USD"),
        "running total after the scan: {added:.800}"
    );
    assert_oob_picker_is_empty_and_focused(&added);

    // Scan 2: the same series, and the picker comes back ready again.
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567890&qty=1&unit_price=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("75 USD"), "running total: {added:.800}");
    assert_oob_picker_is_empty_and_focused(&added);

    // A clicked result is the same form plus its own product id; the quantity
    // typed in the field still travels.
    let (status, clicked) = post_form(
        &app,
        &format!("{base}/lines"),
        &format!("product=scan&qty=3&unit_price=&product_id={product}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{clicked}");
    assert!(clicked.contains("150 USD"), "running total: {clicked:.800}");

    // Removing a line updates the running total from the same response: 150 - 50.
    let detail = sale_detail(&app, sale).await;
    let line_id = detail["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["qty"] == json!("2"))
        .and_then(|line| line["id"].as_i64())
        .expect("the scanned line");
    let (status, removed) = send(
        &app,
        "DELETE",
        &format!("{base}/lines/{line_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(
        removed.contains("100 USD"),
        "running total after removal: {removed:.800}"
    );
    assert!(
        !removed.contains(&format!("id=\"sale-line-{line_id}\"")),
        "the removed line is gone: {removed:.800}"
    );

    // An unknown value is a clear 400 naming the search count, and adds nothing.
    let before = sale_detail(&app, sale).await;
    let (status, err) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=does-not-exist&qty=1&unit_price=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.contains("no exact match"), "{err}");
    assert!(err.contains("0 matches"), "{err}");
    let after = sale_detail(&app, sale).await;
    assert_eq!(
        after["lines"].as_array().unwrap().len(),
        before["lines"].as_array().unwrap().len(),
        "a failed resolution adds nothing"
    );
    assert_eq!(after["total"], before["total"]);
}

/// The same loop as the sale page, against the purchase record: the picker posts
/// the typed value to the purchase line endpoint, the response carries the updated
/// lines, the running total and the entry row — persistent inside the swapped
/// money region, empty and focused for the next scan (no out-of-band picker on
/// purchases) — and the repeated-product rule surfaces as a clear 400 instead of
/// a crash.

/// The purchase add response must bring the entry row back inside the swapped
/// money region, empty and ready for the next scan. The sale record keeps the
/// out-of-band picker, so purchases get their own contract here: the entry
/// row renders once, its tag carries NO `hx-swap-oob`, the product field is
/// empty and `autofocus`, and the qty and cost fields travel with it.
fn assert_purchase_entry_row_is_empty_and_ready(html: &str) {
    assert_eq!(
        html.matches("id=\"line-picker\"").count(),
        1,
        "the entry row renders exactly once, inside the money region: {html:.800}"
    );
    let row_pos = html
        .find("id=\"line-picker\"")
        .expect("the entry row renders on the add-line response");
    let row_start = html[..row_pos]
        .rfind('<')
        .expect("the id must sit inside a tag");
    let tag_end = row_start + html[row_start..].find('>').expect("unterminated tag");
    let row_tag = &html[row_start..=tag_end];
    assert!(
        !row_tag.contains("hx-swap-oob"),
        "the purchase picker is no longer out of band; it travels inside the money region: {row_tag}"
    );
    let row = &html[row_pos..];
    let money_pos = html
        .find("id=\"purchase-record-money\"")
        .expect("the add response renders the money region");
    assert!(
        money_pos < row_pos,
        "the entry row must render inside the swapped money region: money={money_pos} row={row_pos}"
    );
    let input_pos = row
        .find("id=\"product-picker\"")
        .expect("the entry row renders its product field");
    let input_start = row[..input_pos].rfind('<').unwrap();
    let input_end = input_pos + row[input_pos..].find('>').unwrap();
    let input_tag = &row[input_start..=input_end];
    assert!(
        input_tag.contains("autofocus"),
        "the entry row must come back focused: {input_tag}"
    );
    assert!(
        !input_tag.contains("value="),
        "the entry row must come back empty: {input_tag}"
    );
    for id in ["id=\"line-qty\"", "id=\"line-unit-cost\""] {
        assert!(
            row.contains(id),
            "the entry row carries the qty and the cost field: {id}: {row:.600}"
        );
    }
}

#[tokio::test]
async fn purchase_line_picker_adds_lines_without_a_click() {
    let (app, pool) = test_app().await;
    let product_a = create_product_via_web(&app, &pool, "PSCAN-A", "1", "50").await;
    let product_b = create_product_via_web(&app, &pool, "PSCAN-B", "1", "50").await;
    record_stock_via_web(&app, product_a, "20").await;
    record_stock_via_web(&app, product_b, "5").await;
    let (status, body) = post_json(
        &app,
        &format!("/api/products/{product_a}/barcodes"),
        json!({ "code": "7791234567891" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed barcode A: {body}");
    let (status, body) = post_json(
        &app,
        &format!("/api/products/{product_b}/barcodes"),
        json!({ "code": "7791234567892" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed barcode B: {body}");
    let supplier = create_supplier_via_web(&app, &pool, "ScanSupplier").await;

    let (status, body) = post_form(
        &app,
        "/web/purchases",
        &format!("supplier_id={supplier}&payment_type=Cash&purchase_date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let purchase = find_only_purchase_id(&app).await;
    let base = format!("/web/purchases/{purchase}");

    // The record page offers the picker island's entry row, its sibling
    // results container and no catalogue select. The island owns the search
    // now, so the page carries no declarative transport: the field carries no
    // hx-get, trigger, target, vals or keyup handler, and the debounce lives
    // in static/picker.js, not in markup. Escape is base.html's
    // document-level keydown handler, which serves both pickers.
    let (status, page) = get(&app, &format!("/purchases/{purchase}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        !page.contains("<select name=\"product_id\""),
        "the catalogue select must be gone: {page:.600}"
    );

    // The island's mount point carries its calling context: one picker,
    // priced for a purchase — the entry row quotes cost, not the sale price.
    assert_eq!(
        page.matches("data-picker=\"product\"").count(),
        1,
        "one island mount point: {page:.600}"
    );
    let container_pos = page
        .find("data-picker")
        .expect("the record page renders the island mount point");
    let container_start = page[..container_pos]
        .rfind('<')
        .expect("the attribute must sit inside a tag");
    let container_end =
        container_start + page[container_start..].find('>').expect("unterminated tag");
    let container_tag = &page[container_start..=container_end];
    assert!(
        container_tag.contains("id=\"line-picker\"")
            && container_tag.contains("data-price-kind=\"cost\""),
        "{container_tag}"
    );

    // The field carries no declarative search transport.
    let input_pos = page
        .find("id=\"product-picker\"")
        .expect("the record page renders the picker field");
    let input_start = page[..input_pos]
        .rfind('<')
        .expect("the id must sit inside a tag");
    let input_end = input_pos + page[input_pos..].find('>').expect("unterminated tag");
    let input_tag = &page[input_start..=input_end];
    for transport in [
        "hx-get",
        "hx-trigger",
        "hx-target",
        "hx-vals",
        "hx-on:keyup",
    ] {
        assert!(
            !input_tag.contains(transport),
            "the field must not carry {transport}: {input_tag}"
        );
    }

    // The single add-line form keeps the server's contract: the post, the
    // hidden island-owned product id and the default quantity.
    let form_pos = page[..input_pos]
        .rfind("<form")
        .expect("the field sits in the add-line form");
    let form_end = form_pos + page[form_pos..].find("</form>").expect("unterminated form");
    let form = &page[form_pos..form_end];
    assert!(
        form.contains(&format!("hx-post=\"/web/purchases/{purchase}/lines\"")),
        "{form:.600}"
    );
    assert!(form.contains("name=\"product_id\""), "{form:.600}");
    assert!(
        form.contains("name=\"qty\"") && form.contains("value=\"1\""),
        "a scan and a click must both carry the default quantity: {form:.600}"
    );

    // The results container is a sibling of the form, never inside it.
    assert!(
        !form.contains("id=\"product-search-results\""),
        "the results container must be a sibling of the picker form, never inside it: {form:.600}"
    );
    assert!(
        page.contains("id=\"product-search-results\""),
        "{page:.600}"
    );

    // The debounce moved with the island: the island file declares it.
    assert!(
        include_str!("../static/picker.js").contains("DEBOUNCE_MS = 250"),
        "the search must be debounced by static/picker.js"
    );

    assert!(page.contains("id=\"purchase-record-money\""), "{page:.600}");

    // Scan 1: the reader types the barcode and presses Enter. The form carries the
    // field and the quantity, never a product id. The fixture's supplier has no
    // satellite row for the product, so the empty cost uses the product cost price
    // (10).
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567891&qty=2&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("product PSCAN-A"), "{added:.600}");
    assert!(
        added.contains("20 USD"),
        "running total after the scan: {added:.800}"
    );
    assert_purchase_entry_row_is_empty_and_ready(&added);

    // Scan 2: a different product, and the entry row comes back ready again.
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567892&qty=3&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("50 USD"), "running total: {added:.800}");
    assert_purchase_entry_row_is_empty_and_ready(&added);

    // Removing a line updates the running total from the same response.
    let detail = purchase_detail(&app, purchase).await;
    let line_id = detail["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["qty"] == json!("2"))
        .and_then(|line| line["id"].as_i64())
        .expect("the scanned line");
    let (status, removed) = send(
        &app,
        "DELETE",
        &format!("{base}/lines/{line_id}"),
        None,
        true,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(
        removed.contains("30 USD"),
        "running total after removal: {removed:.800}"
    );
    assert!(
        !removed.contains(&format!("id=\"purchase-line-{line_id}\"")),
        "the removed line is gone: {removed:.800}"
    );

    // S5b on the scan path: a repeat of product B with the same resolved cost
    // (the product column again — the fixture supplier has no satellite row)
    // MERGES into the existing line instead of answering 400: one line, the
    // summed quantity, and a visible server notice naming the product. A
    // silent quantity change would be magic.
    let before = purchase_detail(&app, purchase).await;
    let (status, merged) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567892&qty=1&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert!(
        merged.contains("data-notice-server"),
        "the merge announces itself: {merged:.600}"
    );
    assert!(
        merged.contains("merged"),
        "the merge notice says what happened: {merged:.600}"
    );
    assert!(
        merged.contains("product PSCAN-B"),
        "the merge notice names the product: {merged:.600}"
    );
    let after = purchase_detail(&app, purchase).await;
    assert_eq!(
        after["lines"].as_array().unwrap().len(),
        before["lines"].as_array().unwrap().len(),
        "the merge keeps exactly one line for the product"
    );
    let b_line = after["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["product_id"] == json!(product_b))
        .expect("product B's line");
    assert_eq!(b_line["qty"], json!("4"), "qty 3 + 1 scanned = 4: {b_line}");
    assert_eq!(b_line["unit_cost"], json!("10"));
    assert_eq!(
        after["total"],
        json!("40"),
        "the merged total is 4 x $10: {after}"
    );

    // The strict rule keeps its bite where it matters: a repeat at a DIFFERENT
    // explicit cost is the clear 400, because one product cannot carry two
    // prices on one purchase and a merge would silently discard one of them.
    // The picker form still names its action so the notice region can say
    // which action failed. Product B's merged line is untouched by the refusal.
    let (status, repeated) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567892&qty=1&unit_cost=999",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{repeated}");
    assert!(repeated.contains("already has a line"), "{repeated}");
    assert!(repeated.contains("separate purchase"), "{repeated}");
    assert!(
        repeated.contains("product PSCAN-B"),
        "the rejection must name the product, not its id: {repeated}"
    );
    assert!(
        !repeated.contains(&format!("product {product_b} already has a line")),
        "the rejection must not leak the bare product id: {repeated}"
    );
    assert!(
        page.contains("data-action=\"Add line\""),
        "the notice must be able to name the failed action: {page:.600}"
    );
    let after = purchase_detail(&app, purchase).await;
    assert_eq!(
        after["lines"].as_array().unwrap().len(),
        before["lines"].as_array().unwrap().len(),
        "the refused repeat adds nothing"
    );
    let b_line = after["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["product_id"] == json!(product_b))
        .expect("product B's line");
    assert_eq!(
        b_line["qty"],
        json!("4"),
        "the refusal leaves the merge intact"
    );
    assert_eq!(after["total"], json!("40"));

    // An unknown value is a clear 400 naming the search count, and adds nothing.
    let (status, err) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=does-not-exist&qty=1&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.contains("no exact match"), "{err}");
    assert!(err.contains("0 matches"), "{err}");
    assert_eq!(
        purchase_detail(&app, purchase).await["lines"]
            .as_array()
            .unwrap()
            .len(),
        after["lines"].as_array().unwrap().len(),
        "a failed resolution adds nothing"
    );
}

/// The merge notice renders the product name through Askama's HTML escaping:
/// a name made of markup characters must reach the operator as text, not HTML.
/// Mirrors `create_notice_escapes_html_specials_in_the_product_name`, which
/// pins the same guarantee for the create-under-filter box; this pins it for
/// the S5b merge notice (`partials/purchase_merge_notice.html`), which no
/// other test exercises with a markup-laden name.
#[tokio::test]
async fn merge_notice_escapes_html_specials_in_the_product_name() {
    let (app, pool) = test_app().await;

    // "Agua <500ml> & \"especial\"" URL-encoded, exactly what a browser form
    // sends for that name.
    let body = "sku=ESC-M&name=Agua+%3C500ml%3E+%26+%22especial%22&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50";
    let (status, resp) = post_form(&app, "/web/products", body).await;
    assert_eq!(status, StatusCode::OK, "create product ESC-M: {resp}");

    let supplier = create_supplier_via_web(&app, &pool, "EscMergeSupplier").await;
    let (status, body) = post_form(
        &app,
        "/web/purchases",
        &format!("supplier_id={supplier}&payment_type=Cash&purchase_date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let purchase = find_only_purchase_id(&app).await;

    // The first add creates the line at the product cost price (10): the
    // fixture supplier has no satellite row, so the empty cost falls back to
    // the product column.
    let (status, added) = post_form(
        &app,
        &format!("/web/purchases/{purchase}/lines"),
        "product=ESC-M&qty=2&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(
        !added.contains("scanned again"),
        "the first add is not a merge: {added:.400}"
    );

    // The repeat resolves to the SAME cost, so the merge notice renders —
    // with the escaped name, never the raw markup.
    let (status, merged) = post_form(
        &app,
        &format!("/web/purchases/{purchase}/lines"),
        "product=ESC-M&qty=1&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert!(
        merged.contains("data-notice-server=\"true\""),
        "the merge notice must be present: {merged:.600}"
    );
    assert!(
        merged.contains("Agua &lt;500ml&gt; &amp; &quot;especial&quot; scanned again"),
        "the notice must carry the escaped name: {merged:.600}"
    );
    assert!(
        !merged.contains("<500ml>"),
        "the raw markup must never reach the notice: {merged:.600}"
    );
}

// The price context moved to the island (`data-price-kind` picks it client-side); e2e's test_the_picker_island_owns_the_purchase_search and its sale sibling carry this coverage.

// ---------------------------------------------------------------------------
// N4 accessibility: named controls and a polite announcement for the picker
// ---------------------------------------------------------------------------

/// The opening tag that encloses byte `pos`, from its `<` to its `>`. The `<`
/// itself may sit at `pos` (a control found by its `<input` marker), so the
/// search includes that byte.
fn enclosing_tag(html: &str, pos: usize) -> &str {
    let start = html[..=pos]
        .rfind('<')
        .unwrap_or_else(|| panic!("no tag opens before byte {pos}"));
    let end = pos
        + html[pos..]
            .find('>')
            .unwrap_or_else(|| panic!("unterminated tag at byte {pos}"));
    &html[start..=end]
}

/// The earliest native form control in `body`, if any.
fn first_control(body: &str) -> Option<usize> {
    ["<input", "<select", "<textarea"]
        .iter()
        .filter_map(|tag| body.find(tag))
        .min()
}

/// Native controls with no accessible name, resolved the way a screen reader
/// resolves one for these forms: a `<label>` that wraps the control, or a label
/// whose `for` matches the control's id. Hidden controls are ignored; buttons
/// carry their own text and are not in scope.
fn controls_without_accessible_name(html: &str) -> Vec<String> {
    use std::collections::HashSet;

    let mut named_by_for: HashSet<String> = HashSet::new();
    let mut wrapped: HashSet<usize> = HashSet::new();
    let mut from = 0usize;
    while let Some(rel) = html[from..].find("<label") {
        let start = from + rel;
        let open_end = start + html[start..].find('>').expect("unterminated <label>");
        let label_tag = &html[start..=open_end];
        if let Some(id) = attr_value(label_tag, "for") {
            named_by_for.insert(id.to_string());
        } else {
            let content_start = open_end + 1;
            if let Some(close_rel) = html[content_start..].find("</label>") {
                let body = &html[content_start..content_start + close_rel];
                if let Some(control_pos) = first_control(body) {
                    wrapped.insert(content_start + control_pos);
                }
            }
        }
        from = open_end + 1;
    }

    let mut offenders = Vec::new();
    for tag in ["<input", "<select", "<textarea"] {
        let mut from = 0usize;
        while let Some(rel) = html[from..].find(tag) {
            let pos = from + rel;
            from = pos + 1;
            let opening = enclosing_tag(html, pos);
            if attr_value(opening, "type") == Some("hidden") {
                continue;
            }
            let named = wrapped.contains(&pos)
                || attr_value(opening, "id")
                    .map(|id| named_by_for.contains(id))
                    .unwrap_or(false);
            if named {
                continue;
            }
            let name = attr_value(opening, "name").unwrap_or("?");
            let id = attr_value(opening, "id").unwrap_or("none");
            offenders.push(format!("{tag} name={name:?} id={id:?}"));
        }
    }
    offenders
}

/// The resolver accepts both patterns a screen reader accepts, and still
/// rejects a control whose label is only visual text.
#[test]
fn accessible_name_resolution_accepts_wrapping_and_for_labels() {
    let named = r#"
        <label>Wrapped <input type="number" name="wrapped" /></label>
        <label for="picked">Picked</label><input type="range" name="picked" id="picked" />
    "#;
    assert_eq!(
        controls_without_accessible_name(named),
        Vec::<String>::new()
    );

    let offenders = controls_without_accessible_name(
        r#"<label>Qty</label><input type="number" name="qty" id="qty" />"#,
    );
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(offenders[0].contains("qty"), "{offenders:?}");

    // A hidden control is not announced, so it needs no name.
    assert!(controls_without_accessible_name(r#"<input type="hidden" name="id" />"#).is_empty());
}

/// Every control on the sale record page resolves an accessible name. The
/// picker's Qty and Unit price fields were the verified defect; the same
/// resolver covers the confirm, edit-header, discard, payment and cancel forms
/// in both document states.
#[tokio::test]
async fn sale_record_controls_resolve_accessible_names() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "Caja", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "A11Y-L", "1", "50").await;
    let sale = create_sale_draft_via_web(&app, &pool, "A11yBuyer", "Cash", "").await;
    add_sale_line_via_web(&app, sale, product, "1").await;

    let (status, page) = get(&app, &format!("/sales/{sale}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    for id in ["line-qty", "line-unit-price"] {
        assert!(
            page.contains(&format!("for=\"{id}\"")),
            "the {id} label must point at its input: {page:.600}"
        );
    }
    let unnamed = controls_without_accessible_name(&page);
    assert!(
        unnamed.is_empty(),
        "draft record page has unlabelled controls: {unnamed:?}"
    );

    confirm_sale_via_web(&app, sale, Some(cash)).await;
    let (status, page) = get(&app, &format!("/sales/{sale}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    let unnamed = controls_without_accessible_name(&page);
    assert!(
        unnamed.is_empty(),
        "confirmed record page has unlabelled controls: {unnamed:?}"
    );
}

// The match count and the polite announcement are derived in the island's render now; e2e's test_the_results_announce_the_match_count carries this coverage.

// ---------------------------------------------------------------------------
// N5 — the referenced-id guard
// ---------------------------------------------------------------------------

/// The scan bites on every entity noun, and a document's own id stays exempt in
/// both shapes the lists print.
#[test]
fn referenced_id_scan_bites_on_every_entity_noun() {
    for noun in BARE_REFERENCED_ID_PREFIXES {
        let mutant = format!("<div>{noun}3</div>");
        let err = check_no_bare_referenced_ids("mutation", &mutant).unwrap_err();
        assert!(err.contains(noun), "{err}");
    }
    check_no_bare_referenced_ids("mutation", "<div>Draft #12</div>").unwrap();
    check_no_bare_referenced_ids("mutation", "<div>2024-SALE-000012</div>").unwrap();
    check_no_bare_referenced_ids("mutation", "<div>sale #12</div>").unwrap();
}

/// A guarded page that is clean passes, and the same page with a leaked referenced
/// id is rejected by the exact check the guard runs over every seeded page.
#[tokio::test]
async fn referenced_id_guard_rejects_a_bare_id_added_to_a_guarded_page_copy() {
    let (app, pool) = test_app().await;
    let fixture = seed_wiring_fixture(&app, &pool).await;
    let products_page = guarded_pages(&fixture)
        .into_iter()
        .find(|page| page.label == "products")
        .expect("the products page is guarded");
    let (status, html) = get(&app, &products_page.path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}: {html:.400}",
        products_page.path
    );
    check_no_bare_referenced_ids(products_page.label, &html)
        .unwrap_or_else(|err| panic!("the guarded products page must be clean: {err}"));

    let mutant = format!("{html}<div>product #3</div>");
    let err = check_no_bare_referenced_ids(products_page.label, &mutant).unwrap_err();
    assert!(err.contains("products"), "{err}");
    assert!(err.contains("product #3"), "{err}");
}

/// The customer statement's receipt list shows the account and method names; the
/// referenced-id rule covers the path now that the guard fixture collects a
/// receipt, so this ordinary assertion replaces the old defect pin.
#[tokio::test]
async fn customer_statement_resolves_receipt_account_and_method_names() {
    let (app, pool) = test_app().await;
    let cash = method_id(&pool, "Cash").await;
    let _account = create_account_via_web(&app, &pool, "GapWallet", &[cash]).await;
    let product = create_product_via_web(&app, &pool, "GAP-P", "1", "50").await;
    record_stock_via_web(&app, product, "10").await;
    let customer = seed_customer(&pool, "GapBuyer", None, None).await;
    let sale =
        create_sale_draft_on_date(&app, customer, "Credit", "2024-05-02", "2024-06-01").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;
    let (status, resp) = post_form(
        &app,
        "/web/customer-receipts",
        &format!("customer_id={customer}&method_id={cash}&amount=10&date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "collect: {resp}");

    let (status, page) = get(&app, &format!("/customers/{customer}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        bare_referenced_id(&page).is_none(),
        "the receipt list must resolve account and method names: {page:.800}"
    );
    assert!(
        page.contains("GapWallet • Cash"),
        "the receipt shows the resolved account and method: {page:.800}"
    );
    assert!(
        page.contains("Receipt #"),
        "the receipt's own identifier stays visible: {page:.800}"
    );

    // The allocation names the sale the way the user does: its number, not its id.
    let sale_number = sale_detail(&app, sale).await["sale"]["sale_number"]
        .as_str()
        .expect("the confirmed sale number")
        .to_string();
    assert!(
        page.contains(&format!("{sale_number} •")),
        "the receipt allocation shows the sale number: {page:.800}"
    );
    assert!(
        !page.contains("sale #"),
        "the receipt allocation must not print the sale's internal id: {page:.800}"
    );
}

// ---------------------------------------------------------------------------
// N5 — list filters, catalogue search and name resolution
// ---------------------------------------------------------------------------

/// The products list fragment as the browser's filter form fetches it.
async fn product_list_html(app: &Router, query: &str) -> String {
    let (status, html) = get(app, &format!("/web/products{query}")).await;
    assert_eq!(status, StatusCode::OK, "/web/products{query}: {html}");
    html
}

/// The sales list fragment as the browser's filter form fetches it.
async fn sale_list_html(app: &Router, query: &str) -> String {
    let (status, html) = get(app, &format!("/web/sales{query}")).await;
    assert_eq!(status, StatusCode::OK, "/web/sales{query}: {html}");
    html
}

/// The purchases list fragment as the browser's filter form fetches it.
async fn purchase_list_html(app: &Router, query: &str) -> String {
    let (status, html) = get(app, &format!("/web/purchases{query}")).await;
    assert_eq!(status, StatusCode::OK, "/web/purchases{query}: {html}");
    html
}

/// The documents list fragment as the browser's filter form fetches it.
async fn document_list_html(app: &Router, query: &str) -> String {
    let (status, html) = get(app, &format!("/web/documents{query}")).await;
    assert_eq!(status, StatusCode::OK, "/web/documents{query}: {html}");
    html
}

async fn category_id_by_name(pool: &SqlitePool, name: &str) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT id FROM categories WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

async fn create_category_via_web(app: &Router, pool: &SqlitePool, name: &str) -> i64 {
    let (status, resp) = post_form(app, "/web/categories", &format!("name={name}")).await;
    assert_eq!(status, StatusCode::OK, "create category {name}: {resp}");
    category_id_by_name(pool, name).await
}

/// Create a product with an explicit display name and optional category, through
/// the same web form the browser uses.
async fn create_product_full_via_web(
    app: &Router,
    pool: &SqlitePool,
    sku: &str,
    name: &str,
    category_id: Option<i64>,
) -> i64 {
    let encoded_name = name.replace(' ', "+");
    let category = category_id.map(|c| c.to_string()).unwrap_or_default();
    // The wire key is `product_category_id`, not `category_id`: the web form
    // carries `hx-include="#product-filters"`, so the filter owns `category_id`
    // and the product's own category rides the renamed key (issue #37).
    let body = format!(
        "sku={sku}&name={encoded_name}&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&product_category_id={category}"
    );
    let (status, resp) = post_form(app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "create product {sku}: {resp}");
    product_id_by_sku(pool, sku).await
}

/// Create a sale draft on an explicit date through the web form, and return its id.
async fn create_sale_draft_on_date(
    app: &Router,
    customer_id: i64,
    payment_type: &str,
    sale_date: &str,
    due_date: &str,
) -> i64 {
    let body = format!(
        "customer_id={customer_id}&payment_type={payment_type}&sale_date={sale_date}&due_date={due_date}"
    );
    let (status, resp) = post_form(app, "/web/sales", &body).await;
    assert_eq!(status, StatusCode::OK, "create sale: {resp}");
    let (status, body) = get(app, "/api/sales").await;
    assert_eq!(status, StatusCode::OK, "list sales: {body}");
    let v = json_body(&body);
    v["sales"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["sale"]["customer_id"] == json!(customer_id))
        .last()
        .and_then(|d| d["sale"]["id"].as_i64())
        .unwrap_or_else(|| panic!("sale for customer {customer_id} not found: {v}"))
}

/// Create a purchase draft for an explicit supplier and date through the web form.
async fn create_purchase_draft_on_date(app: &Router, supplier_id: i64, purchase_date: &str) -> i64 {
    create_purchase_draft_with_due(app, supplier_id, "Credit", purchase_date, "2024-12-31").await
}

/// Create a purchase draft with an explicit payment type and due date through
/// the web form, and return its id.
async fn create_purchase_draft_with_due(
    app: &Router,
    supplier_id: i64,
    payment_type: &str,
    purchase_date: &str,
    due_date: &str,
) -> i64 {
    let body = format!(
        "supplier_id={supplier_id}&payment_type={payment_type}&purchase_date={purchase_date}&due_date={due_date}"
    );
    let (status, resp) = post_form(app, "/web/purchases", &body).await;
    assert_eq!(status, StatusCode::OK, "create purchase: {resp}");
    let (status, body) = get(app, "/api/purchases").await;
    assert_eq!(status, StatusCode::OK, "list purchases: {body}");
    let v = json_body(&body);
    v["purchases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["purchase"]["supplier_id"] == json!(supplier_id))
        .last()
        .and_then(|d| d["purchase"]["id"].as_i64())
        .unwrap_or_else(|| panic!("purchase for supplier {supplier_id} not found: {v}"))
}

/// The one purchase row's inner HTML, cut from the list fragment by the row's
/// stable id (`id="purchase-{id}"`). Rows are anchors, so the cut ends at the
/// first `</a>` — no nested anchor may live inside a row.
fn purchase_row_html(html: &str, purchase_id: i64) -> String {
    let start = html
        .find(&format!("id=\"purchase-{purchase_id}\""))
        .unwrap_or_else(|| panic!("purchase row {purchase_id} missing from the list"));
    let end = html[start..]
        .find("</a>")
        .map(|i| start + i)
        .expect("a purchase row is an anchor");
    html[start..end].to_string()
}

/// The row anchor's OPENING tag, cut around the row's stable id. Colour
/// assertions must read this tag, not a span inside the row: the whole row is
/// an anchor, so its inherited or declared text colour paints the identifier,
/// the supplier and the meta line — and a class on one inner span (the
/// total's) once passed for the row's colour.
fn purchase_row_opening_tag(html: &str, purchase_id: i64) -> String {
    let id_pos = html
        .find(&format!("id=\"purchase-{purchase_id}\""))
        .unwrap_or_else(|| panic!("purchase row {purchase_id} missing from the list"));
    let start = html[..id_pos]
        .rfind("<a ")
        .unwrap_or_else(|| panic!("a purchase row is an anchor"));
    let end = id_pos + html[id_pos..].find('>').expect("unterminated anchor tag");
    html[start..=end].to_string()
}

async fn add_purchase_line_via_web(app: &Router, purchase_id: i64, product_id: i64, qty: &str) {
    let body = format!("purchase_id={purchase_id}&product_id={product_id}&qty={qty}");
    let (status, resp) = post_form(app, "/web/purchases/lines", &body).await;
    assert_eq!(status, StatusCode::OK, "add purchase line: {resp}");
}

async fn confirm_purchase_via_web(app: &Router, purchase_id: i64) {
    let body = format!("purchase_id={purchase_id}");
    let (status, resp) = post_form(app, "/web/purchases/confirm", &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "confirm purchase {purchase_id}: {resp}"
    );
}

/// AC13: every sales filter works alone and combined; an empty filter is no
/// constraint and a filter matching nothing is an empty list, never an error.
#[tokio::test]
async fn sales_list_filters_by_status_customer_number_and_date() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "FILT-S", "1", "50").await;
    record_stock_via_web(&app, product, "20").await;

    let ana = seed_customer(&pool, "FiltAna", None, None).await;
    let beto = seed_customer(&pool, "FiltBeto", None, None).await;

    let _draft = create_sale_draft_on_date(&app, ana, "Cash", "2024-05-02", "").await;
    let ana_confirmed =
        create_sale_draft_on_date(&app, ana, "Credit", "2024-05-02", "2024-06-01").await;
    add_sale_line_via_web(&app, ana_confirmed, product, "1").await;
    confirm_sale_via_web(&app, ana_confirmed, None).await;
    let beto_confirmed =
        create_sale_draft_on_date(&app, beto, "Credit", "2024-07-15", "2024-08-15").await;
    add_sale_line_via_web(&app, beto_confirmed, product, "1").await;
    confirm_sale_via_web(&app, beto_confirmed, None).await;

    let ana_number = sale_detail(&app, ana_confirmed).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();
    let beto_number = sale_detail(&app, beto_confirmed).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();

    // No filter returns everything.
    let all = sale_list_html(&app, "").await;
    assert!(
        all.contains("Draft #"),
        "the draft stays in the unfiltered list: {all}"
    );
    assert!(
        all.contains(&ana_number) && all.contains(&beto_number),
        "{all}"
    );

    // Status alone.
    let confirmed = sale_list_html(&app, "?status=Confirmed").await;
    assert!(confirmed.contains(&ana_number), "{confirmed}");
    assert!(confirmed.contains(&beto_number), "{confirmed}");
    assert!(!confirmed.contains("Draft #"), "{confirmed}");

    let drafts = sale_list_html(&app, "?status=Draft").await;
    assert!(drafts.contains("Draft #"), "{drafts}");
    assert!(!drafts.contains(&ana_number), "{drafts}");

    // Customer alone, case-insensitive over the name the list shows.
    let ana_only = sale_list_html(&app, "?customer=filtana").await;
    assert!(ana_only.contains("FiltAna"), "{ana_only}");
    assert!(!ana_only.contains("FiltBeto"), "{ana_only}");

    // Number matches partially: the user remembers a fragment, not the whole number.
    let fragment = &ana_number[ana_number.len() - 6..];
    let by_number = sale_list_html(&app, &format!("?number={fragment}")).await;
    assert!(by_number.contains(&ana_number), "{by_number}");
    assert!(!by_number.contains(&beto_number), "{by_number}");

    // Date range is inclusive on sale_date.
    let by_date = sale_list_html(&app, "?from=2024-07-01&to=2024-07-31").await;
    assert!(by_date.contains(&beto_number), "{by_date}");
    assert!(!by_date.contains(&ana_number), "{by_date}");
    assert!(!by_date.contains("Draft #"), "{by_date}");

    // Combined filters narrow further.
    let combined = sale_list_html(&app, "?status=Confirmed&customer=FiltBeto").await;
    assert!(combined.contains(&beto_number), "{combined}");
    assert!(!combined.contains(&ana_number), "{combined}");

    // Empty values are no constraint, not an error.
    let blank = sale_list_html(&app, "?status=&customer=&number=&from=&to=").await;
    assert!(
        blank.contains("Draft #") && blank.contains(&ana_number) && blank.contains(&beto_number),
        "{blank}"
    );

    // Matching nothing is an empty list, not an error.
    let none = sale_list_html(&app, "?number=NOPE-0000").await;
    assert!(none.contains("Nothing here yet."), "{none}");
    assert!(!none.contains(&ana_number), "{none}");

    // A status or date the picker never sends is treated as absent, not an error.
    let lenient = sale_list_html(&app, "?status=bogus&from=not-a-date").await;
    assert!(
        lenient.contains("Draft #")
            && lenient.contains(&ana_number)
            && lenient.contains(&beto_number),
        "{lenient}"
    );

    // An inverted range matches nothing rather than failing.
    let inverted = sale_list_html(&app, "?from=2024-07-01&to=2024-05-01").await;
    assert!(inverted.contains("Nothing here yet."), "{inverted}");

    // The full page is filtered too, so the filtered view is bookmarkable.
    let (status, page) = get(&app, "/sales?status=Confirmed").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains(&ana_number), "{page:.600}");
    assert!(!page.contains("Draft #"), "{page:.600}");

    // The form reflects the URL, so a shared link re-opens with the same filters.
    let (status, page) = get(
        &app,
        "/sales?status=Draft&customer=FiltAna&number=0000&from=2024-05-01&to=2024-05-31",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        page.contains("name=\"customer\" placeholder=\"Customer\" value=\"FiltAna\""),
        "the form must reflect the bookmarkable URL: {page:.600}"
    );
    assert!(
        page.contains("<option value=\"Draft\" selected>Draft</option>"),
        "{page:.600}"
    );
    assert!(
        page.contains("name=\"from\" value=\"2024-05-01\""),
        "{page:.600}"
    );
}

/// AC13: the purchases list carries the same filter shape.
#[tokio::test]
async fn purchases_list_filters_by_status_supplier_number_and_date() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "FILT-P2", "1", "50").await;
    record_stock_via_web(&app, product, "20").await;

    let sur = create_supplier_via_web(&app, &pool, "FiltSur").await;
    let norte = create_supplier_via_web(&app, &pool, "FiltNorte").await;

    let _draft = create_purchase_draft_on_date(&app, sur, "2024-05-02").await;
    let sur_confirmed = create_purchase_draft_on_date(&app, sur, "2024-05-02").await;
    add_purchase_line_via_web(&app, sur_confirmed, product, "1").await;
    confirm_purchase_via_web(&app, sur_confirmed).await;
    let norte_confirmed = create_purchase_draft_on_date(&app, norte, "2024-07-15").await;
    add_purchase_line_via_web(&app, norte_confirmed, product, "1").await;
    confirm_purchase_via_web(&app, norte_confirmed).await;

    let sur_number = purchase_detail(&app, sur_confirmed).await["purchase"]["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();
    let norte_number = purchase_detail(&app, norte_confirmed).await["purchase"]["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();

    let all = purchase_list_html(&app, "").await;
    assert!(all.contains("Draft #"), "{all}");
    assert!(
        all.contains(&sur_number) && all.contains(&norte_number),
        "{all}"
    );

    let confirmed = purchase_list_html(&app, "?status=Confirmed").await;
    assert!(
        confirmed.contains(&sur_number) && confirmed.contains(&norte_number),
        "{confirmed}"
    );
    assert!(!confirmed.contains("Draft #"), "{confirmed}");

    let drafts = purchase_list_html(&app, "?status=Draft").await;
    assert!(drafts.contains("Draft #"), "{drafts}");
    assert!(!drafts.contains(&sur_number), "{drafts}");

    // Supplier alone, case-insensitive over the name the list shows.
    let norte_only = purchase_list_html(&app, "?supplier=filtnorte").await;
    assert!(norte_only.contains("FiltNorte"), "{norte_only}");
    assert!(!norte_only.contains("FiltSur"), "{norte_only}");

    let fragment = &sur_number[sur_number.len() - 6..];
    let by_number = purchase_list_html(&app, &format!("?number={fragment}")).await;
    assert!(by_number.contains(&sur_number), "{by_number}");
    assert!(!by_number.contains(&norte_number), "{by_number}");

    let by_date = purchase_list_html(&app, "?from=2024-07-01&to=2024-07-31").await;
    assert!(by_date.contains(&norte_number), "{by_date}");
    assert!(!by_date.contains(&sur_number), "{by_date}");

    let combined = purchase_list_html(&app, "?status=Confirmed&supplier=FiltNorte").await;
    assert!(combined.contains(&norte_number), "{combined}");
    assert!(!combined.contains(&sur_number), "{combined}");

    let blank = purchase_list_html(&app, "?status=&supplier=&number=&from=&to=").await;
    assert!(
        blank.contains("Draft #") && blank.contains(&sur_number) && blank.contains(&norte_number),
        "{blank}"
    );

    let none = purchase_list_html(&app, "?supplier=NoSuchSupplier").await;
    assert!(none.contains("Nothing here yet."), "{none}");

    let (status, page) = get(&app, "/purchases?status=Confirmed").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains(&sur_number), "{page:.600}");
    assert!(!page.contains("Draft #"), "{page:.600}");

    // The form reflects the URL, so a shared link re-opens with the same filters.
    let (status, page) = get(
        &app,
        "/purchases?status=Draft&supplier=FiltSur&number=0000&from=2024-05-01&to=2024-05-31",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        page.contains("name=\"supplier\" placeholder=\"Supplier\" value=\"FiltSur\""),
        "the form must reflect the bookmarkable URL: {page:.600}"
    );
    assert!(
        page.contains("<option value=\"Draft\" selected>Draft</option>"),
        "{page:.600}"
    );
}

/// S6: one purchase row, one reading order (identifier → supplier → money),
/// one status chip. The chip carries the state colour the total used to
/// shout: Draft/Cancelled stay muted, Confirmed+settled shows Paid, owed and
/// not yet past due shows Due (warning), owed past the due date shows
/// Overdue (expense). The owed chips also print the amount still owed as a
/// bare number — what the old badge cloud's `payable …` used to say — while
/// Paid stays a bare word because nothing is owed. The total itself is
/// neutral text, the badge cloud (`payable …`, `settled`, `Cash`,
/// `Confirmed`) is gone, and the row keeps the S3 peek contract (`hx-get`
/// into the drawer, never a full navigation).
///
/// As T2b, the guard pins the component, not the colour: Rust cannot compute
/// a style, so the colours themselves are asserted by the visual-neutrality
/// net's purchase-list-paid/due/overdue states.
#[tokio::test]
async fn purchase_list_row_reads_identifier_supplier_money_with_one_status_chip() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "ROW-S6", "1", "50").await;
    record_stock_via_web(&app, product, "20").await;
    let sur = create_supplier_via_web(&app, &pool, "RowSur").await;

    // Fund an account so the Cash confirm can pay the total immediately.
    let cash = method_id(&pool, "Cash").await;
    let account = create_account_via_web(&app, &pool, "RowWallet", &[cash]).await;
    let account_cash = account_method_id(&pool, account, "Cash").await;
    let (status, resp) = post_form(
        &app,
        "/web/transactions",
        &format!("account_id={account}&type=Income&amount=1000&description=seed&date=2024-05-01"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fund account: {resp}");

    let draft =
        create_purchase_draft_with_due(&app, sur, "Credit", "2024-05-02", "2024-12-31").await;
    let due_row = create_purchase_draft_with_due(
        &app,
        sur,
        "Credit",
        "2024-05-02",
        &(chrono::Local::now().date_naive() + chrono::Duration::days(1)).to_string(),
    )
    .await;
    let overdue =
        create_purchase_draft_with_due(&app, sur, "Credit", "2024-05-02", "2024-12-31").await;
    // Cash purchases carry no due date (the domain rejects one), and the Cash
    // confirm pays the total immediately, so this row settles fully paid.
    let paid = create_purchase_draft_with_due(&app, sur, "Cash", "2024-05-02", "").await;
    for (id, qty) in [(due_row, "2"), (overdue, "1"), (paid, "1")] {
        add_purchase_line_via_web(&app, id, product, qty).await;
    }
    confirm_purchase_via_web(&app, due_row).await;
    confirm_purchase_via_web(&app, overdue).await;
    let (status, resp) = post_form(
        &app,
        "/web/purchases/confirm",
        &format!("purchase_id={paid}&method_id={account_cash}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm Cash purchase: {resp}");
    // A cancelled confirmed purchase still owes money past its due date; its
    // chip must stay the muted Cancelled one — the lifecycle outranks money.
    let cancelled =
        create_purchase_draft_with_due(&app, sur, "Credit", "2024-05-02", "2024-12-31").await;
    add_purchase_line_via_web(&app, cancelled, product, "1").await;
    confirm_purchase_via_web(&app, cancelled).await;
    let (status, resp) = post_form(
        &app,
        "/web/purchases/cancel",
        &format!("purchase_id={cancelled}&reason=triangulation"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cancel purchase: {resp}");
    // A partial payment keeps the due row owed (due > 0) while some money is
    // already down, so its meta line may name what was paid so far.
    let (status, resp) = post_form(
        &app,
        "/web/purchases/payments",
        &format!("purchase_id={due_row}&method_id={account_cash}&amount=1&date=2024-05-10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "partial payment: {resp}");

    let all = purchase_list_html(&app, "").await;
    let draft_row = purchase_row_html(&all, draft);
    let due_row_html = purchase_row_html(&all, due_row);
    let overdue_row = purchase_row_html(&all, overdue);
    let paid_row = purchase_row_html(&all, paid);
    let cancelled_row = purchase_row_html(&all, cancelled);

    // Identifier zone first (AC4): the stable handle a draft actually has.
    assert!(
        draft_row.contains(&format!("Draft #{draft}")),
        "the draft identifier must be the row's handle: {draft_row}"
    );
    assert!(
        draft_row.contains("font-semibold tabular-nums"),
        "the identifier reads first and is tabular: {draft_row}"
    );

    // The S3 peek contract must survive the row rewrite.
    assert!(
        draft_row.contains(&format!(
            "hx-get=\"/web/documents/detail/purchase/{draft}\""
        )),
        "{draft_row}"
    );
    assert!(
        draft_row.contains("hx-target=\"#purchase-drawer-body\"")
            && draft_row.contains("hx-swap=\"innerHTML\""),
        "{draft_row}"
    );
    assert!(
        draft_row.contains(&format!("href=\"/purchases/{draft}\"")),
        "{draft_row}"
    );

    // AC1b: the row anchor names the normal text colour itself. The
    // `@layer base` `a` rule is deleted, so an anchor can no longer inherit a
    // colour at all; the row's computed colour is asserted by the
    // visual-neutrality net, which covers `/purchases`. Read on the
    // row's OPENING tag, not a span inside it: the total's class once passed
    // for the row's colour.
    let draft_tag = purchase_row_opening_tag(&all, draft);
    assert!(
        draft_tag.contains("text-text"),
        "the row anchor must name the normal text colour so identifier, supplier \
         and meta line stop inheriting the anchor blue: {draft_tag}"
    );

    // Exactly one chip per row, and each state gets its own colour. The owed
    // chips print the localized amount still owed, and a partially paid row
    // shows what remains, not the total.
    let pending_due = purchase_detail(&app, due_row).await["due"]
        .as_str()
        .expect("purchase due")
        .to_string();
    let overdue_due = purchase_detail(&app, overdue).await["due"]
        .as_str()
        .expect("purchase due")
        .to_string();
    let pending_total = purchase_detail(&app, due_row).await["total"]
        .as_str()
        .expect("purchase total")
        .to_string();
    assert!(
        paid_row.contains(">Paid</span>") && paid_row.contains("chip-income"),
        "a fully paid row carries the bare-word Paid chip in income colour: {paid_row}"
    );
    assert!(
        due_row_html.contains(&format!(">Due {pending_due} USD</span>"))
            && due_row_html.contains("chip-warning"),
        "owed and not yet past due carries the Due chip with the amount owed in warning colour: {due_row_html}"
    );
    assert!(
        pending_due != pending_total
            && !due_row_html.contains(&format!(">Due {pending_total} USD</span>")),
        "a partially paid row's Due chip names the remainder, not the total: {due_row_html}"
    );
    assert!(
        overdue_row.contains(&format!(">Overdue {overdue_due} USD</span>"))
            && overdue_row.contains("chip-expense"),
        "owed past the due date carries the Overdue chip with the amount owed in expense colour: {overdue_row}"
    );
    assert!(
        draft_row.contains(">Draft</span>")
            && !draft_row.contains("text-income")
            && !draft_row.contains("text-warning"),
        "a draft chip is muted, never coloured: {draft_row}"
    );
    assert!(
        cancelled_row.contains(">Cancelled</span>")
            && cancelled_row.contains("text-muted")
            && !cancelled_row.contains("text-expense"),
        "a cancelled row keeps the muted Cancelled chip, never the Overdue red: {cancelled_row}"
    );

    // AC3: the total is neutral — it never borrows income or expense colour
    // (the old row painted it `text-expense">{total}` whenever due > 0).
    for (id, row) in [
        (draft, &draft_row),
        (due_row, &due_row_html),
        (overdue, &overdue_row),
        (paid, &paid_row),
    ] {
        let total = purchase_detail(&app, id).await["total"]
            .as_str()
            .expect("purchase total")
            .to_string();
        assert!(
            row.contains(&format!(
                "font-bold tabular-nums text-text\">{total} USD</span>"
            )),
            "the total must be bold, tabular and neutral: {row}"
        );
        assert!(
            !row.contains(&format!("text-expense\">{total} USD</span>"))
                && !row.contains(&format!("text-income\">{total} USD</span>")),
            "the total must not be painted by payment state: {row}"
        );
    }

    // AC5: the money is printed once — the old muted `total … paid … due`
    // echo line is gone, and the total appears exactly once in the row.
    let paid_total = purchase_detail(&app, paid).await["total"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        paid_row.matches(&format!("{paid_total} USD")).count(),
        1,
        "the total must render once per row: {paid_row}"
    );

    // The badge cloud is gone: no payable/settled badge, no payment-type chip,
    // and no `Confirmed` chip — the normal state is not a status.
    assert!(
        !all.contains(">payable ") && !all.contains(">settled<"),
        "{all}"
    );
    assert!(!all.contains(">Cash<"), "{all}");
    assert!(!all.contains(">Confirmed<"), "{all}");
    // The partial payment shows what is already down, once, in the meta line.
    assert!(
        due_row_html.matches("· paid ").count() == 1,
        "partial payment meta renders once: {due_row_html}"
    );
    // Credit is the meta line's only payment-type mention (Cash is the default).
    assert!(overdue_row.contains("· Credit"), "{overdue_row}");
}

/// S6: the per-page global-config echo is gone from both document lists (AC6).
/// Nothing else on either page depends on it.
#[tokio::test]
async fn purchases_and_sales_pages_render_no_config_subtitle() {
    let (app, _pool) = test_app().await;
    for path in ["/purchases", "/sales"] {
        let (status, page) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK, "{page:.400}");
        assert!(!page.contains("negative stock"), "{page:.600}");
        assert!(!page.contains("overdraft"), "{page:.600}");
    }
}

/// AC14: the products list matches name, SKU and barcode through the same search
/// the picker uses, and combines with the existing category filter.
#[tokio::test]
async fn products_list_searches_name_sku_and_barcode_and_combines_with_category() {
    let (app, pool) = test_app().await;
    let yerba_cat = create_category_via_web(&app, &pool, "FiltBeverages").await;
    let other_cat = create_category_via_web(&app, &pool, "FiltSnacks").await;

    let yerba = create_product_full_via_web(
        &app,
        &pool,
        "FILT-YERBA-500",
        "Yerba Filt 500g",
        Some(yerba_cat),
    )
    .await;
    let gal = create_product_full_via_web(
        &app,
        &pool,
        "FILT-GAL-100",
        "Galletitas Filt",
        Some(other_cat),
    )
    .await;
    record_stock_via_web(&app, yerba, "10").await;
    record_stock_via_web(&app, gal, "10").await;

    let (status, body) = post_json(
        &app,
        &format!("/api/products/{yerba}/barcodes"),
        json!({ "code": "7791234567001" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed barcode: {body}");

    // An empty query is not a search: it keeps the whole catalogue.
    let all = product_list_html(&app, "?q=").await;
    assert!(
        all.contains("Yerba Filt 500g") && all.contains("Galletitas Filt"),
        "{all}"
    );

    // By name, partial and case-insensitive.
    let by_name = product_list_html(&app, "?q=yerba").await;
    assert!(by_name.contains("Yerba Filt 500g"), "{by_name}");
    assert!(!by_name.contains("Galletitas Filt"), "{by_name}");

    // By SKU.
    let by_sku = product_list_html(&app, "?q=FILT-GAL-100").await;
    assert!(by_sku.contains("Galletitas Filt"), "{by_sku}");
    assert!(!by_sku.contains("Yerba Filt 500g"), "{by_sku}");

    // By barcode: the matching path the picker already uses.
    let by_barcode = product_list_html(&app, "?q=7791234567001").await;
    assert!(by_barcode.contains("Yerba Filt 500g"), "{by_barcode}");
    assert!(!by_barcode.contains("Galletitas Filt"), "{by_barcode}");

    // The existing category filter alone still works.
    let by_category = product_list_html(&app, &format!("?category_id={yerba_cat}")).await;
    assert!(by_category.contains("Yerba Filt 500g"), "{by_category}");
    assert!(!by_category.contains("Galletitas Filt"), "{by_category}");

    // Search and category combine.
    let combined = product_list_html(&app, &format!("?q=Filt&category_id={other_cat}")).await;
    assert!(combined.contains("Galletitas Filt"), "{combined}");
    assert!(!combined.contains("Yerba Filt 500g"), "{combined}");

    // A barcode that matches a product in another category combines to nothing.
    let crossed =
        product_list_html(&app, &format!("?q=7791234567001&category_id={other_cat}")).await;
    assert!(crossed.contains("No products yet"), "{crossed}");

    // Matching nothing is an empty list, not an error.
    let none = product_list_html(&app, "?q=does-not-exist").await;
    assert!(none.contains("No products yet"), "{none}");

    // The full page is filtered too, so the view is bookmarkable, and the form
    // reflects the URL a shared link carries.
    let (status, page) = get(&app, &format!("/products?q=yerba&category_id={yerba_cat}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains("Yerba Filt 500g"), "{page:.600}");
    assert!(!page.contains("Galletitas Filt"), "{page:.600}");
    assert!(
        page.contains("name=\"q\" placeholder=\"Name, SKU or barcode\" value=\"yerba\""),
        "the search box must reflect the bookmarkable URL: {page:.600}"
    );
    assert!(
        page.contains(&format!("<option value=\"{yerba_cat}\" selected>")),
        "the category select must reflect the bookmarkable URL: {page:.600}"
    );
}

/// Issue #37: the four product mutations answer their non-drawer HTMX caller
/// with the list the caller is looking at, not the whole catalogue. These posts
/// ride the shape the browser's forms send: `HX-Request` without an
/// `HX-Target`, plus the catalogue filter the forms merge from
/// `#product-filters`. Each filtered answer is checked against a seeded row the
/// filter must exclude, and the same handler is re-posted without a filter so
/// the change cannot simply have narrowed everything.
#[tokio::test]
async fn product_create_answer_honours_the_active_filter() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "MutCat Alpha").await;
    let beta_cat = create_category_via_web(&app, &pool, "MutCat Beta").await;
    create_product_full_via_web(&app, &pool, "MUT-A", "Alpha Widget", Some(alpha_cat)).await;
    create_product_full_via_web(&app, &pool, "MUT-B", "Beta Widget", Some(beta_cat)).await;

    // The body carries both the product's own category (renamed to
    // `product_category_id` because the filter owns `category_id`) and the
    // active filter, exactly what `hx-include` would merge: only the alpha row
    // may come back, even though the created product is a beta one.
    let body = format!(
        "sku=MUT-C&name=Gamma+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&product_category_id={beta_cat}&category_id={alpha_cat}&q=Alpha"
    );
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget"),
        "filtered answer must hold the matching row: {html:.400}"
    );
    assert!(
        !html.contains("Beta Widget"),
        "filtered answer must not hold the other row: {html:.400}"
    );

    // The rename must not re-home the product's own category: Gamma lives in
    // the category the modal selected, not in the filter's category.
    let gamma = product_id_by_sku(&pool, "MUT-C").await;
    let (gamma_cat,): (Option<i64>,) =
        sqlx::query_as("SELECT category_id FROM products WHERE id = ?")
            .bind(gamma)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        gamma_cat,
        Some(beta_cat),
        "the product's own category must ride product_category_id"
    );

    // Without a filter the answer is still the whole catalogue.
    let body = "sku=MUT-D&name=Delta+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50";
    let (status, html) = post_form(&app, "/web/products", body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget") && html.contains("Beta Widget"),
        "unfiltered answer must cover the catalogue: {html:.400}"
    );
}

// Issue #37 T2, create-under-filter: when the active filter keeps the fresh
// product out of the correctly filtered list, the answer must say so — a
// server-rendered notice that names the product, swapped out of band into the
// page's `#notice` region, with a Clear filter way out. Everything below rides
// the shape the browser's create form sends: `HX-Request` plus the filter the
// modal merges from `#product-filters`.
/// The notice IS present, naming the product and marked for the client-side
/// precedence guard, when an active filter excludes the created product. The
/// list fragment itself must stay the filtered rows the caller is looking at.
#[tokio::test]
async fn create_hidden_by_filter_answers_the_named_product_notice() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "NoticeCat Alpha").await;
    create_product_full_via_web(&app, &pool, "NTC-A", "Alpha Widget", Some(alpha_cat)).await;

    // The filter (q=Alpha, matching only the seeded row) excludes the product
    // being created, exactly what the merged body looks like in the browser.
    let body = "sku=NTC-B&name=Beta+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&product_category_id=&category_id=&q=Alpha";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("hx-swap-oob=\"innerHTML:#notice\""),
        "the notice must travel out of band into #notice: {html:.400}"
    );
    assert!(
        html.contains("data-notice-server=\"true\""),
        "the server box must carry the precedence-guard marker: {html:.400}"
    );
    assert!(
        html.contains("Beta Widget created"),
        "the notice must name the created product: {html:.400}"
    );
    assert!(
        html.contains("the active catalogue filter is keeping it out of the list"),
        "the notice must say why the row is not there: {html:.400}"
    );
    assert!(
        html.contains(">Clear filter</a>"),
        "the notice must offer the one-click way out: {html:.400}"
    );
    assert!(
        html.contains("data-notice=\"success\"") && html.contains("role=\"status\""),
        "the box must mirror base.html notice()'s success markup: {html:.400}"
    );
    assert!(
        html.contains("data-notice-dismiss=\"true\"") && html.contains("aria-label=\"Dismiss\""),
        "the global dismiss handler must be able to remove the box: {html:.400}"
    );
    // The list fragment is still the filtered view: the seeded matching row is
    // there, the created product's row is not (the notice names the product —
    // that mention is the only occurrence — and rows carry the SKU, which the
    // notice does not).
    assert!(
        html.contains("Alpha Widget") && html.contains("NTC-A"),
        "the filtered answer must keep the matching seeded row: {html:.400}"
    );
    assert_eq!(
        html.matches("Beta Widget").count(),
        1,
        "the created product must appear only in the notice, not as a row: {html:.400}"
    );
    assert!(
        !html.contains("NTC-B"),
        "the created row must not render in the filtered answer: {html:.400}"
    );
}

/// The marker rides the response body exactly when the notice does: base.html
/// skips the generic notice on this marker alone, so the guard and the box
/// must not drift apart.
#[tokio::test]
async fn create_notice_marker_appears_exactly_when_the_notice_does() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "MarkCat Alpha").await;
    create_product_full_via_web(&app, &pool, "MRK-A", "Alpha Widget", Some(alpha_cat)).await;

    // Hidden case: the marker rides along with the notice.
    let body = "sku=MRK-B&name=Beta+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=Alpha";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("data-notice-server=\"true\"") && html.contains("hx-swap-oob"),
        "the marker must be present when the notice is: {html:.400}"
    );

    // Matching filter: the created product is IN the answer, so no notice and
    // no marker — out loud, no false alarm.
    let body = "sku=MRK-C&name=Alpha+Junior&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=Alpha";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        !html.contains("data-notice-server") && !html.contains("hx-swap-oob"),
        "a matching filter must not raise the notice: {html:.400}"
    );

    // No filter: the lenient empty path must not trip the notice either.
    let body = "sku=MRK-D&name=Delta+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        !html.contains("data-notice-server") && !html.contains("hx-swap-oob"),
        "an unfiltered create must stay silent beyond the generic notice: {html:.400}"
    );
    // (The two negative cases above also pin the empty-string filter keys:
    // `q=` and `category_id=` must parse as inactive, not as constraints.)
    let body = "sku=MRK-E&name=Echo+Widget&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=&category_id=";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        !html.contains("data-notice-server") && !html.contains("hx-swap-oob"),
        "empty filter keys must parse as no filter: {html:.400}"
    );
}

/// A matching filter keeps the answer notice-free: the generic "Create product
/// saved" notice is already honest when the row lands in the list.
#[tokio::test]
async fn create_matching_filter_answer_holds_the_new_row_without_a_notice() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "MatchCat Alpha").await;
    create_product_full_via_web(&app, &pool, "MTC-A", "Alpha Widget", Some(alpha_cat)).await;

    let body = "sku=MTC-B&name=Alpha+Junior&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=Alpha";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Junior"),
        "the created row must be in the filtered answer: {html:.400}"
    );
    assert!(
        !html.contains("data-notice-server") && !html.contains("hx-swap-oob"),
        "a matching filter must not raise the notice: {html:.400}"
    );
}

/// A product's stored row, read back through the JSON API the same way the
/// service sees it: the derived `sale_price` and the `markup_pct` column.
async fn product_json(app: &Router, product_id: i64) -> Value {
    let (status, body) = get(app, &format!("/api/products/{product_id}")).await;
    assert_eq!(status, StatusCode::OK, "read back product: {body}");
    json_body(&body)
}

/// The drawer's save path over markup-derived pricing (product-markup T4–T7).
/// Until now no smoke test ever posted `/web/products/edit`, so the drawer's
/// save path had page-level coverage in neither direction. This test drives it
/// end to end: a create with a markup and an EMPTY price must store the
/// server-derived price; the drawer fragment must render the stored markup and
/// the readonly derived price; a save with a changed markup must re-derive the
/// price from the cost (the stale readonly value the browser re-submits must be
/// ignored); and a save with the markup cleared must keep the last stored price
/// standing while the markup column goes back to NULL (NULL is manual, not 0).
#[tokio::test]
async fn products_drawer_edit_derives_the_price_from_the_markup_and_clearing_it_keeps_the_price() {
    let (app, pool) = test_app().await;

    // Create the way the create modal sends it: markup present, price empty —
    // the server derives 10 * (1 + 50/100) = 15.00 and ignores the price field.
    let body = "sku=MKP-A&name=Markup+Widget&kind=Product&unit=un&sale_price=&cost_price=10&track_stock=1&min_stock=1&max_stock=50&markup_pct=50";
    let (status, resp) = post_form(&app, "/web/products", body).await;
    assert_eq!(status, StatusCode::OK, "create with markup: {resp:.400}");
    let product = product_id_by_sku(&pool, "MKP-A").await;
    let stored = product_json(&app, product).await;
    assert_eq!(
        dec(&stored["sale_price"]),
        Decimal::from_str("15.00").unwrap(),
        "the derived price must be stored, not an echoed one: {stored}"
    );
    assert_eq!(
        stored["markup_pct"],
        json!("50"),
        "the markup must be stored with the product: {stored}"
    );

    // The drawer fragment mirrors the stored state: the markup value in its
    // input, the derived price readonly with the last-stored value, and the
    // derivation hint the operator reads while the field is locked.
    let (status, drawer) = get(&app, &format!("/web/products/detail/{product}")).await;
    assert_eq!(status, StatusCode::OK, "drawer fragment: {drawer:.400}");
    assert!(
        drawer.contains(
            "name=\"markup_pct\" step=\"0.01\" min=\"-99\" placeholder=\"25\" value=\"50\""
        ),
        "the drawer must render the stored markup value: {drawer:.400}"
    );
    assert!(
        drawer.contains("step=\"0.01\" readonly value=\"15.00\""),
        "the drawer's price input must be readonly and show the derived price: {drawer:.400}"
    );
    let localization = crate::localization::load_context(&pool).await.unwrap();
    let derivation_hint = format!(
        "{} {}",
        localization.tr(crate::localization::MessageKey::ProductMarkupRecalculated),
        localization.format_percentage(Decimal::from(50))
    );
    assert!(
        drawer.contains(&derivation_hint),
        "the drawer must carry the derivation hint: {drawer:.400}"
    );

    // An edit that changes the markup re-derives the price. The price field
    // re-submits its stale readonly value (15.00) exactly as the browser's
    // readonly input would: the server must ignore it and store 10 * 2 = 20.00.
    let edit = format!(
        "id={product}&sku=MKP-A&name=Markup+Widget&kind=Product&unit=un&sale_price=15.00&cost_price=10&category_id=&track_stock=1&min_stock=1&max_stock=50&location=&notes=&markup_pct=100"
    );
    let (status, html) = post_drawer_form(&app, "/web/products/edit", &edit).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "edit with a new markup: {html:.400}"
    );
    // The drawer branch answers with the fresh detail fragment, so the operator
    // sees the re-derived price and the new markup without a reload.
    assert!(
        html.contains(
            "name=\"markup_pct\" step=\"0.01\" min=\"-99\" placeholder=\"25\" value=\"100\""
        ),
        "the drawer answer must render the new markup: {html:.400}"
    );
    assert!(
        html.contains("step=\"0.01\" readonly value=\"20.00\""),
        "the drawer answer must render the re-derived price: {html:.400}"
    );
    let stored = product_json(&app, product).await;
    assert_eq!(
        dec(&stored["sale_price"]),
        Decimal::from_str("20.00").unwrap(),
        "the re-derived price must be stored over the stale submitted one: {stored}"
    );
    assert_eq!(
        stored["markup_pct"],
        json!("100"),
        "the changed markup must be stored: {stored}"
    );

    // An edit that clears the markup hands the price back to the operator: the
    // price SURVIVES the clear (the last derived value becomes the manual one)
    // and the markup column goes back to NULL, not to 0.
    let edit = format!(
        "id={product}&sku=MKP-A&name=Markup+Widget&kind=Product&unit=un&sale_price=20.00&cost_price=10&category_id=&track_stock=1&min_stock=1&max_stock=50&location=&notes=&markup_pct="
    );
    let (status, html) = post_drawer_form(&app, "/web/products/edit", &edit).await;
    assert_eq!(status, StatusCode::OK, "clear the markup: {html:.400}");
    assert!(
        html.contains("step=\"0.01\" required value=\"20.00\"") && !html.contains("readonly"),
        "the cleared drawer must render the price editable again, value standing: {html:.400}"
    );
    assert!(
        !html.contains("Recalculated from the cost"),
        "the derivation hint must leave with the markup: {html:.400}"
    );
    let stored = product_json(&app, product).await;
    assert_eq!(
        dec(&stored["sale_price"]),
        Decimal::from_str("20.00").unwrap(),
        "the price must survive the markup clear: {stored}"
    );
    assert!(
        stored["markup_pct"].is_null(),
        "a cleared markup must store NULL, not zero: {stored}"
    );
}

/// Regression for the trap the body transport exists to avoid: a product name
/// with non-ASCII characters (and one with an emoji, outside the BMP) must
/// create successfully under a filter that hides it — no panic, no 5xx — and
/// the notice must name it verbatim. The body is UTF-8 and Askama escapes it;
/// a header payload would have needed the name re-encoded by hand, and a raw
/// non-ASCII header value reaches the client as mojibake, because XHR decodes
/// header bytes as ISO-8859-1. `http`'s `HeaderValue` is NOT the blocker:
/// `is_valid` (http 1.5.0) accepts any byte >= 32 except 127, so non-ASCII
/// passes validation and the damage happens on the wire instead.
#[tokio::test]
async fn create_non_ascii_names_under_filter_stay_2xx_with_the_notice() {
    let (app, pool) = test_app().await;
    create_product_full_via_web(&app, &pool, "UTF-A", "Alpha Widget", None).await;

    // "Yerba Ñandú" and "Yerba 🧉 Mate" URL-encoded, exactly what a browser
    // form sends for those names.
    for (sku, encoded, decoded) in [
        ("UTF-B", "Yerba+%C3%91and%C3%BA", "Yerba Ñandú"),
        ("UTF-C", "Yerba+%F0%9F%A7%89+Mate", "Yerba 🧉 Mate"),
    ] {
        let body = format!(
            "sku={sku}&name={encoded}&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=Alpha"
        );
        let (status, html) = post_form(&app, "/web/products", &body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a non-ASCII name must not turn a create into a 5xx: {html:.400}"
        );
        // html_escape leaves non-ASCII untouched, so the raw name must appear
        // verbatim in the notice.
        assert!(
            html.contains(&format!("{decoded} created")),
            "the notice must name the created product verbatim: {html:.400}"
        );
    }
}

/// The notice renders the product name through Askama's HTML escaping: a name
/// made of markup characters must reach the operator as text, not HTML. This
/// pins the escaping guarantee the template took over from the hand
/// `html_escape` call when the box moved into `templates/partials/notice.html`.
#[tokio::test]
async fn create_notice_escapes_html_specials_in_the_product_name() {
    let (app, pool) = test_app().await;
    create_product_full_via_web(&app, &pool, "ESC-A", "Alpha Widget", None).await;

    // "Agua <500ml> & \"especial\"" URL-encoded, exactly what a browser form
    // sends for that name, under a filter that hides the created product.
    let body = "sku=ESC-B&name=Agua+%3C500ml%3E+%26+%22especial%22&kind=Product&unit=un&sale_price=25&cost_price=10&track_stock=1&min_stock=1&max_stock=50&q=Alpha";
    let (status, html) = post_form(&app, "/web/products", &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a markup-laden name must not turn a create into a 5xx: {html:.400}"
    );
    assert!(
        html.contains("data-notice-server=\"true\""),
        "the notice must be present for the escaped-name case: {html:.400}"
    );
    assert!(
        html.contains("Agua &lt;500ml&gt; &amp; &quot;especial&quot; created"),
        "the notice must carry the escaped name: {html:.400}"
    );
    assert!(
        !html.contains("<500ml>"),
        "the raw markup must never reach the notice: {html:.400}"
    );
}

#[tokio::test]
async fn product_movement_answer_honours_the_active_filter() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "MovCat Alpha").await;
    let beta_cat = create_category_via_web(&app, &pool, "MovCat Beta").await;
    let alpha =
        create_product_full_via_web(&app, &pool, "MOV-A", "Alpha Widget", Some(alpha_cat)).await;
    create_product_full_via_web(&app, &pool, "MOV-B", "Beta Widget", Some(beta_cat)).await;

    let body = format!(
        "product_id={alpha}&type=In&qty=3&reason=Initial&date=2024-05-01&category_id={alpha_cat}&q=Alpha"
    );
    let (status, html) = post_form(&app, "/web/stock-movements", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget"),
        "filtered answer must hold the matching row: {html:.400}"
    );
    assert!(
        !html.contains("Beta Widget"),
        "filtered answer must not hold the other row: {html:.400}"
    );

    // Without a filter the answer is still the whole catalogue.
    let body = format!("product_id={alpha}&type=In&qty=1&reason=Sale&date=2024-05-02");
    let (status, html) = post_form(&app, "/web/stock-movements", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget") && html.contains("Beta Widget"),
        "unfiltered answer must cover the catalogue: {html:.400}"
    );
}

#[tokio::test]
async fn product_cost_answer_honours_the_active_filter() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "CostCat Alpha").await;
    let beta_cat = create_category_via_web(&app, &pool, "CostCat Beta").await;
    let alpha =
        create_product_full_via_web(&app, &pool, "PCOST-A", "Alpha Widget", Some(alpha_cat)).await;
    create_product_full_via_web(&app, &pool, "PCOST-B", "Beta Widget", Some(beta_cat)).await;
    let supplier = create_supplier_via_web(&app, &pool, "Cost Mut Sup").await;

    let body = format!(
        "product_id={alpha}&supplier_id={supplier}&cost=12.50&date=2024-05-01&category_id={alpha_cat}&q=Alpha"
    );
    let (status, html) = post_form(&app, "/web/product-costs", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget"),
        "filtered answer must hold the matching row: {html:.400}"
    );
    assert!(
        !html.contains("Beta Widget"),
        "filtered answer must not hold the other row: {html:.400}"
    );
    // The mutation itself still happened behind the filtered list answer.
    let (rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM product_supplier_costs WHERE product_id = ?")
            .bind(alpha)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "the web post must still create the satellite row");

    // Without a filter the answer is still the whole catalogue.
    let body = format!("product_id={alpha}&supplier_id={supplier}&cost=13&date=2024-05-02");
    let (status, html) = post_form(&app, "/web/product-costs", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget") && html.contains("Beta Widget"),
        "unfiltered answer must cover the catalogue: {html:.400}"
    );
}

#[tokio::test]
async fn product_preferred_cost_answer_honours_the_active_filter() {
    let (app, pool) = test_app().await;
    let alpha_cat = create_category_via_web(&app, &pool, "PrefCat Alpha").await;
    let beta_cat = create_category_via_web(&app, &pool, "PrefCat Beta").await;
    let alpha =
        create_product_full_via_web(&app, &pool, "PREF-A", "Alpha Widget", Some(alpha_cat)).await;
    create_product_full_via_web(&app, &pool, "PREF-B", "Beta Widget", Some(beta_cat)).await;
    let supplier = create_supplier_via_web(&app, &pool, "Pref Mut Sup").await;
    record_supplier_cost_via_web(&app, alpha, supplier, "9").await;

    let body = format!("product_id={alpha}&supplier_id={supplier}&category_id={alpha_cat}&q=Alpha");
    let (status, html) = post_form(&app, "/web/product-costs/preferred", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget"),
        "filtered answer must hold the matching row: {html:.400}"
    );
    assert!(
        !html.contains("Beta Widget"),
        "filtered answer must not hold the other row: {html:.400}"
    );
    // The marker itself still moved, behind the filtered list answer.
    let (preferred,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM product_supplier_costs WHERE product_id = ? AND is_preferred = 1",
    )
    .bind(alpha)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(preferred, 1, "the web post must still move the marker");

    // Without a filter the answer is still the whole catalogue.
    let body = format!("product_id={alpha}&supplier_id={supplier}");
    let (status, html) = post_form(&app, "/web/product-costs/preferred", &body).await;
    assert_eq!(status, StatusCode::OK, "{html:.400}");
    assert!(
        html.contains("Alpha Widget") && html.contains("Beta Widget"),
        "unfiltered answer must cover the catalogue: {html:.400}"
    );
}

/// AC4/N5: each of the three lists resolves the referenced entity to a name and
/// prints no internal id for it.
#[tokio::test]
async fn lists_resolve_referenced_names_and_print_no_internal_ids() {
    let (app, pool) = test_app().await;
    let product = create_product_full_via_web(&app, &pool, "NAMES-P", "Named Widget", None).await;
    let customer = seed_customer(&pool, "NamesBuyer", None, None).await;
    let _sale = create_sale_draft_on_date(&app, customer, "Cash", "2024-05-02", "").await;
    let supplier = create_supplier_via_web(&app, &pool, "NamesSupplier").await;
    let _purchase = create_purchase_draft_on_date(&app, supplier, "2024-05-02").await;

    let sales = sale_list_html(&app, "").await;
    assert!(
        sales.contains("NamesBuyer"),
        "the sales list shows the customer name: {sales}"
    );
    assert!(!sales.contains("customer #"), "{sales}");

    let purchases = purchase_list_html(&app, "").await;
    assert!(
        purchases.contains("NamesSupplier"),
        "the purchases list shows the supplier name: {purchases}"
    );
    assert!(!purchases.contains("supplier #"), "{purchases}");

    let products = product_list_html(&app, "").await;
    assert!(
        products.contains("Named Widget"),
        "the products list shows the product name: {products}"
    );
    assert!(
        !products.contains(&format!("#{product}")),
        "the products list must not print the product's internal id: {products}"
    );
}

/// The README must not describe the receipt-list referenced-id gap as open: the
/// list resolves account and method names and the guard fixture now collects a
/// receipt, so the old pin is gone.
#[test]
fn readme_does_not_describe_the_receipt_list_gap_as_open() {
    let readme = std::fs::read_to_string("README.md").expect("README.md is readable");
    assert!(
        !readme.contains("A separate pin records the known"),
        "the README still claims the receipt-list gap is pinned instead of fixed"
    );
    assert!(
        readme.contains("receipt list resolves"),
        "the README should state that the receipt list resolves names"
    );
}

/// N6: the shared normalizer folds Unicode case and the Spanish diacritics.
#[test]
fn normalize_search_folds_case_and_spanish_diacritics() {
    use crate::models::normalize_search;
    for (raw, folded) in [
        ("Pérez", "perez"),
        ("pérez", "perez"),
        ("PÉREZ", "perez"),
        ("Perez", "perez"),
        ("PEREZ", "perez"),
        ("Ñandú", "nandu"),
        ("ñandú", "nandu"),
        ("ÑANDÚ", "nandu"),
        ("Café", "cafe"),
        ("CAFÉ", "cafe"),
        ("ÀÉÎÕÜ", "aeiou"),
    ] {
        assert_eq!(normalize_search(raw), folded, "{raw:?}");
    }
}

/// N6: search ignores accents and case on both sides, for parties and the
/// catalogue. A phone keyboard capitalising the first letter, or a name typed
/// without accents, must still find the record.
#[tokio::test]
async fn search_matches_ignore_accents_and_case() {
    let (app, pool) = test_app().await;

    // Sales: a customer named with accents, found from every typed form.
    let perez = seed_customer(&pool, "Pérez", None, None).await;
    let andu = seed_customer(&pool, "Ñandú", None, None).await;
    create_sale_draft_for_customer(&app, perez, "Cash", "").await;
    create_sale_draft_for_customer(&app, andu, "Cash", "").await;
    for needle in ["Pérez", "pérez", "PÉREZ", "Perez", "PEREZ"] {
        let html = sale_list_html(&app, &format!("?customer={needle}")).await;
        assert!(html.contains("Pérez"), "{needle:?} must find Pérez: {html}");
        assert!(
            !html.contains("Ñandú"),
            "{needle:?} must not match Ñandú: {html}"
        );
    }
    for needle in ["Ñandú", "ñandú", "Nandu", "ÑANDÚ"] {
        let html = sale_list_html(&app, &format!("?customer={needle}")).await;
        assert!(html.contains("Ñandú"), "{needle:?} must find Ñandú: {html}");
        assert!(
            !html.contains("Pérez"),
            "{needle:?} must not match Pérez: {html}"
        );
    }

    // Purchases: a supplier named with accents, the same both ways.
    let cafe_sup = create_supplier_via_web(&app, &pool, "Café").await;
    let andu_sup = create_supplier_via_web(&app, &pool, "Ñandú").await;
    create_purchase_draft_on_date(&app, cafe_sup, "2024-05-02").await;
    create_purchase_draft_on_date(&app, andu_sup, "2024-05-02").await;
    for needle in ["Nandu", "ÑANDÚ", "ñandú"] {
        let html = purchase_list_html(&app, &format!("?supplier={needle}")).await;
        assert!(html.contains("Ñandú"), "{needle:?} must find Ñandú: {html}");
        assert!(
            !html.contains("Café"),
            "{needle:?} must not match Café: {html}"
        );
    }
    for needle in ["CAFE", "café", "Café"] {
        let html = purchase_list_html(&app, &format!("?supplier={needle}")).await;
        assert!(html.contains("Café"), "{needle:?} must find Café: {html}");
        assert!(
            !html.contains("Ñandú"),
            "{needle:?} must not match Ñandú: {html}"
        );
    }

    // Catalogue: the picker reads the JSON route now, and it folds accents and
    // case on both sides, like the list.
    create_product_full_via_web(&app, &pool, "CAFE-P", "Café", None).await;
    for needle in ["CAFE", "CAFÉ"] {
        let (status, search) = get(&app, &format!("/web/product-search.json?q={needle}")).await;
        assert_eq!(status, StatusCode::OK, "{needle}: {search}");
        let body = json_body(&search);
        let names: Vec<&str> = body["products"]
            .as_array()
            .expect("products array")
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert!(
            names.contains(&"Café"),
            "the picker must find Café by {needle}: {search}"
        );
    }
    let list = product_list_html(&app, "?q=cafe").await;
    assert!(
        list.contains("Café"),
        "the catalogue must find Café by cafe: {list}"
    );
    let list = product_list_html(&app, "?q=CAFÉ").await;
    assert!(
        list.contains("Café"),
        "the catalogue must find Café by CAFÉ: {list}"
    );
}

// ---------------------------------------------------------------------------
// AC18 (finance audit, M5 Phase B slice S9): the actor the interface renders
// ---------------------------------------------------------------------------

/// The finance detail view shows the actor as a DISPLAY NAME, never an id:
/// the account header names who registered it, each history row names its
/// movement's actor, and an edit adds "Actualizado por" without erasing the
/// creator. Two principals drive the flow so the two names are distinct —
/// the shared session user creates the account, a second probe session edits
/// the transaction.
#[tokio::test]
async fn audit_finance_detail_view_shows_the_actor_display_name() {
    let (app, pool) = test_app().await;
    let method = method_id(&pool, "Cash").await;
    let account = create_account_via_web(&app, &pool, "AuditWallet", &[method]).await;

    // The shared session user ("Test Admin") records a transaction.
    let (status, resp) = post_form(
        &app,
        "/web/transactions",
        &format!("account_id={account}&type=Income&amount=250&description=venta&date=2024-05-01",),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    // A second principal (display name "Test Probe") edits the same movement.
    let probe_token = test_support::seed_session_with_permissions(&pool, &["finance.write"])
        .await
        .unwrap();
    let tx_id: i64 = sqlx::query_scalar("SELECT id FROM transactions WHERE account_id = ?")
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (status, resp) = post_json_with_cookie(
        &app,
        &format!("/api/transactions/{tx_id}"),
        serde_json::json!({ "amount": "300" }),
        &test_support::cookie_for(&probe_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    let localization = crate::localization::load_context(&pool).await.unwrap();
    let registered_by = format!(
        "{} Test Admin",
        localization.tr(crate::localization::MessageKey::AuditRegisteredBy)
    );
    let updated_by = format!(
        "{} Test Probe",
        localization.tr(crate::localization::MessageKey::AuditUpdatedBy)
    );
    let (status, page) = get(&app, &format!("/accounts/{account}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    // The name appears in TWO places (the account header and the movement
    // row), so a single missing render cannot satisfy the count.
    assert_eq!(
        page.matches(&registered_by).count(),
        2,
        "the account header AND the movement row show the creator's display name: {page}"
    );
    // The editor renders once — on the movement row — because the account
    // itself has no edit path yet (its `updated_by` is still NULL).
    assert_eq!(
        page.matches(&updated_by).count(),
        1,
        "the edit names its editor on the movement row: {page}"
    );
    assert!(
        !page.contains("Registrado por 1"),
        "the interface never renders a raw user id: {page}"
    );
}

/// The migration's sentinel account is a system account, not a back door: an
/// attempt to LOG IN as `sistema` with any password answers the same generic
/// failure any bad credential gets, with no session and nothing that would
/// distinguish it from an unknown username. The comparison half proves the
/// path is the inactive-user path: a deactivated (roleless) account's failed
/// login is byte-for-byte the same shape.
#[tokio::test]
async fn audit_the_system_sentinel_cannot_log_in_like_any_inactive_account() {
    let (app, pool) = test_app().await;

    // The control: one ordinary roleless account, deactivated the way the
    // users screen does it.
    let teller = test_support::seed_audit_user(&pool, "teller", "Teller")
        .await
        .unwrap();
    sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let sessions_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Three attempts as the sentinel: a plausible password, a paste of its
    // display name, and pure garbage. Every one is the SAME generic refusal.
    for password in ["Sistema (anterior al registro)", "admin", "x"] {
        let resp = post_form(
            &app,
            "/login",
            &format!("username=sistema&password={password}"),
        )
        .await;
        let (status, body) = resp;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the sentinel must not log in with any password: {body}"
        );
        assert!(
            body.contains("Incorrect username or password"),
            "the refusal is the generic one, not a special case: {body}"
        );
    }

    // The same shape for the deactivated control account.
    let resp = post_form(&app, "/login", "username=teller&password=whatever").await;
    let (status, body) = resp;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an inactive user's failed login: {body}"
    );
    assert!(
        body.contains("Incorrect username or password"),
        "same generic message: {body}"
    );

    // No attempt minted a session, and the refusal never wrote one.
    let sessions_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sessions_after.0, sessions_before.0,
        "no session was created"
    );
}

// ---------------------------------------------------------------------------
// AC19 (finance audit, M5 Phase B slice S9): the upgrade sequence, proven on a
// database that has business rows and no users — the upgrade case of a real
// installation — and the FK refusal that holds every actor row in place.
// ---------------------------------------------------------------------------

/// A pool with the migration chain stopped just before the audit migration
/// (the pre-30 schema is real) plus legacy business data, planted the way
/// pre-audit code wrote it: no user anywhere in the database.
async fn upgraded_pool_with_legacy_rows() -> (axum::Router, sqlx::SqlitePool) {
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run_to(20240101000029, &pool)
        .await
        .unwrap();

    let account_id: (i64,) = sqlx::query_as(
        "INSERT INTO accounts (name, cached_balance) VALUES ('legacy', '0') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO transactions (account_id, kind, amount, description, date) \
         VALUES (?, 'Income', '10', 'legacy movement', '2024-01-01')",
    )
    .bind(account_id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO payment_methods (name) VALUES ('Legacy Method')")
        .execute(&pool)
        .await
        .unwrap();
    let state = test_support::app_state(pool.clone());
    (crate::routes::router(state.clone()), pool)
}

#[tokio::test]
async fn ac19_the_upgrade_attributes_every_legacy_row_to_the_system_sentinel() {
    let (app, pool) = upgraded_pool_with_legacy_rows().await;
    let migrator = sqlx::migrate!("./migrations");
    migrator.run(&pool).await.unwrap();

    // The sentinel exists: inactive, roleless, unusable credential, and it
    // is NOT an administrator the bootstrap would collide with.
    let sentinel: (i64, i64, i64) = sqlx::query_as(
        "SELECT id, is_active, must_change_password FROM users WHERE username = 'sistema'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sentinel.1, 0, "the sentinel account is inactive");
    let sentinel_roles: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE user_id = ?")
            .bind(sentinel.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel_roles.0, 0, "the sentinel holds no role");

    // Every pre-existing row points at the sentinel and nothing was lost.
    let accounts: (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(*) FROM accounts WHERE created_by = ?")
            .bind(sentinel.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(accounts.0, accounts.1, "no row lost, all attributed");
    assert_eq!(accounts.0, 1, "the legacy account survived");
    let transactions: (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(*) FROM transactions WHERE created_by = ?")
            .bind(sentinel.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        transactions.0, transactions.1,
        "no row lost, all attributed"
    );
    assert_eq!(transactions.0, 1, "the legacy movement survived");
    let methods: (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(*) FROM payment_methods WHERE created_by = ?")
            .bind(sentinel.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(methods.0, methods.1, "no row lost, all attributed");
    assert_eq!(methods.0, 6, "the five seeds plus the legacy one survived");

    // `created_by` is NOT NULL afterwards: a write that omits the actor
    // is refused by the database, which is what makes every future insert
    // explicit.
    let refused = sqlx::query("INSERT INTO accounts (name) VALUES ('no-actor')")
        .execute(&pool)
        .await;
    assert!(refused.is_err(), "created_by is NOT NULL after the rebuild");

    // The service keeps working: a write through the real web route lands,
    // attributed to the acting user like every future row. The session is
    // seeded after the migration (the same way a real install starts).
    test_support::seed_session(&pool).await.unwrap();
    let method = method_id(&pool, "Cash").await;
    let account = create_account_via_web(&app, &pool, "post-migration", &[method]).await;
    let session_user: (i64,) = sqlx::query_as("SELECT id FROM users WHERE username = ?")
        .bind(test_support::TEST_USERNAME)
        .fetch_one(&pool)
        .await
        .unwrap();
    let created_by: (i64,) = sqlx::query_as("SELECT created_by FROM accounts WHERE id = ?")
        .bind(account)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        created_by.0, session_user.0,
        "the route's actor, not a fresh one"
    );
}

/// The upgrade's second half: the bootstrap still creates the ONE real
/// administrator through its ordinary creation path (never the recovery
/// path), grants the protected role, and the sentinel stays what it was.
#[tokio::test]
async fn ac19_the_bootstrap_creates_exactly_one_active_administrator_after_the_upgrade() {
    // The state the upgrade ends on: the whole chain has already run, the
    // legacy rows are attributed, and the application is about to start.
    let (app, pool) = upgraded_pool_with_legacy_rows().await;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let _app = app;
    // The shared identity service construction (light hasher): the same
    // bootstrap path production runs at startup.
    let state = test_support::app_state(pool.clone());

    let outcome = state
        .identity_service
        .bootstrap_admin(Some("upgrade pw 123"))
        .await
        .unwrap();
    assert!(outcome.created, "the bootstrap created the administrator");
    assert!(
        outcome.generated_password.is_none(),
        "the env password is used as-is"
    );

    let admins: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users u \
         JOIN user_roles ur ON ur.user_id = u.id \
         JOIN roles r ON r.id = ur.role_id \
         WHERE u.is_active = 1 AND r.is_system = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(admins.0, 1, "exactly one active administrator exists");

    let sentinel: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = 'sistema' AND is_active = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        sentinel.0, 0,
        "the sentinel account is not an administrator"
    );

    // A second bootstrap run is the ordinary no-op: the active protected
    // holder already exists.
    let again = state
        .identity_service
        .bootstrap_admin(Some("another pw 123"))
        .await
        .unwrap();
    assert!(!again.created, "the bootstrap no-ops once an admin exists");
}

/// AC19: the audit foreign key holds a user row in place. Deleting the
/// sentinel — referenced by every attributed row — is refused (the same
/// `ON DELETE RESTRICT` the identity grant trail already relies on), so
/// history cannot lose its actor.
#[tokio::test]
async fn ac19_deleting_the_system_actor_is_refused_by_the_audit_foreign_key() {
    let (_s, pool) = upgraded_pool_with_legacy_rows().await;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();

    let refused = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("FOREIGN KEY constraint failed"),
        "the audit FK refuses the deletion: {refused}"
    );

    // The row survives: history still explains itself.
    let still_there: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there.0, 1);
}

// ---------------------------------------------------------------------------
// AC19 (inventory audit, M5 Phase B slice S10): the upgrade sequence on the
// inventory tables — a database built with the migrations up to 30, business
// rows in categories/products/stock_movements and no user beyond the sentinel.
// ---------------------------------------------------------------------------

/// A pool with the migration chain stopped just after the finance audit (the
/// pre-31 inventory schema is real) plus legacy inventory rows, planted the
/// way pre-audit code wrote them: no created_by column exists to fill.
async fn upgraded_pool_with_legacy_inventory_rows() -> (Vec<i64>, sqlx::SqlitePool) {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run_to(20240101000030, &pool)
        .await
        .unwrap();

    let category_id: (i64,) =
        sqlx::query_as("INSERT INTO categories (name) VALUES ('legacy-cat') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let product_id: (i64,) = sqlx::query_as(
        "INSERT INTO products (sku, name, kind, category_id, unit, sale_price, cost_price, track_stock) \
         VALUES ('LEGACY-P', 'Legacy', 'Product', ?, 'un', '1', '0', 1) RETURNING id",
    )
    .bind(category_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let movement_id: (i64,) = sqlx::query_as(
        "INSERT INTO stock_movements (product_id, qty, type, reason, reference, date) \
         VALUES (?, '5', 'In', 'Purchase', '', '2024-01-01') RETURNING id",
    )
    .bind(product_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    (vec![category_id.0, product_id.0, movement_id.0], pool)
}

/// The upgrade attributes every pre-existing inventory row to the sentinel it
/// REUSES (migration 30 created it on this database), loses no row and no id,
/// and leaves `created_by` NOT NULL — a future write that omits the actor is
/// refused by the database. The RESTRICT foreign key holds the sentinel in
/// place for the inventory rows the same way it already does for finance.
#[tokio::test]
async fn ac19_the_upgrade_attributes_every_inventory_row_to_the_system_sentinel() {
    let (legacy_ids, pool) = upgraded_pool_with_legacy_inventory_rows().await;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();

    // Rows preserved, ids preserved, zero unattributed: every legacy row is
    // exactly where it was, pointing at the sentinel.
    for (table, id) in [
        ("categories", legacy_ids[0]),
        ("products", legacy_ids[1]),
        ("stock_movements", legacy_ids[2]),
    ] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the legacy row survived with its id, attributed to the sentinel"
        );
    }
    // The sentinel attribution is total: no row of the three tables points
    // anywhere else.
    for table in ["categories", "products", "stock_movements"] {
        let unattributed: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE created_by IS NULL OR created_by != ?"
        )))
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unattributed.0, 0, "{table}: no row lost its actor");
    }

    // `created_by` is NOT NULL on all three rebuilt tables: a write that
    // omits the actor is refused by the database.
    for (table, sql) in [
        (
            "categories",
            "INSERT INTO categories (name) VALUES ('no-actor')",
        ),
        (
            "products",
            "INSERT INTO products (sku, name, kind, unit, sale_price, cost_price, track_stock) \
             VALUES ('NO-ACTOR', 'x', 'Product', 'un', '1', '0', 0)",
        ),
        (
            "stock_movements",
            "INSERT INTO stock_movements (product_id, qty, type, reason, date) \
             VALUES (1, '1', 'In', 'Initial', '2024-01-01')",
        ),
    ] {
        let refused = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&pool)
            .await;
        assert!(
            refused.is_err(),
            "{table}: created_by is NOT NULL after the rebuild"
        );
    }

    // The re-enabled foreign keys find the same graph that existed before:
    // no violation anywhere.
    let violations: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        violations.0, 0,
        "the upgrade leaves no foreign-key violation"
    );

    // The RESTRICT audit foreign key holds the sentinel in place for the
    // inventory rows too: deleting it is refused and the row survives.
    let refused = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("FOREIGN KEY constraint failed"),
        "the audit FK refuses the deletion: {refused}"
    );
    let still_there: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there.0, 1);
}

/// The defensive path: the migration REUSES the sentinel migration 30
/// created, and only creates one if it is somehow absent. This test makes it
/// absent — the finance tables are emptied and the sentinel deleted after
/// migration 30 — plants legacy inventory rows, and runs the rest of the
/// chain: migration 31 must create its own sentinel and attribute the rows
/// to it.
#[tokio::test]
async fn ac19_the_inventory_migration_recreates_a_missing_sentinel() {
    let (legacy_ids, pool) = upgraded_pool_with_legacy_inventory_rows().await;
    // Make the sentinel absent: empty the finance tables that reference it
    // (RESTRICT refuses a direct delete), then delete the account.
    sqlx::query("DELETE FROM payment_methods")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM transactions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM accounts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users")
        .execute(&pool)
        .await
        .unwrap();
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before.0, 0, "the sentinel is gone when migration 31 runs");

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();
    for (table, id) in [
        ("categories", legacy_ids[0]),
        ("products", legacy_ids[1]),
        ("stock_movements", legacy_ids[2]),
    ] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the defensive sentinel exists and owns the legacy row"
        );
    }
    // The defensive sentinel is the same shape migration 30's is: inactive,
    // roleless, unusable credential.
    let sentinel_row: (i64, i64) =
        sqlx::query_as("SELECT is_active, must_change_password FROM users WHERE id = ?")
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel_row.0, 0, "the recreated sentinel is inactive");
    let roles: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE user_id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(roles.0, 0, "the recreated sentinel holds no role");
}

// ---------------------------------------------------------------------------
// AC19 (sales/customers audit, M5 Phase B slice S11): the upgrade sequence on
// the four rebuilt tables — a database built with the migrations up to 31,
// business rows in customers/sales/sale_payments/customer_receipts and no user
// beyond the sentinel.
// ---------------------------------------------------------------------------

/// A pool with the migration chain stopped just after the inventory audit (the
/// pre-32 sales/customers schema is real) plus legacy business rows planted
/// the way pre-audit code wrote them: no created_by column exists to fill.
async fn upgraded_pool_with_legacy_sales_and_customer_rows(
) -> ((i64, i64, i64, i64), sqlx::SqlitePool) {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run_to(20240101000031, &pool)
        .await
        .unwrap();

    // The sentinel exists by now (the seeded payment methods made migration 30
    // attribute something): the finance rows the audit needs are real.
    let sentinel: (i64,) =
        sqlx::query_as("SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE")
            .fetch_one(&pool)
            .await
            .unwrap();
    let users_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        users_before.0, 1,
        "no user beyond the sentinel before the upgrade"
    );

    let account_id: (i64,) = sqlx::query_as(
        "INSERT INTO accounts (name, created_by) VALUES ('legacy wallet', ?) RETURNING id",
    )
    .bind(sentinel.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let customer_id: (i64,) = sqlx::query_as(
        "INSERT INTO customers (name, is_walkin, is_active) \
         VALUES ('Legacy Client', 0, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let sale_id: (i64,) = sqlx::query_as(
        "INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date) \
         VALUES ('2024-SALE-000001', 'Confirmed', 'Credit', ?, 'Legacy Client', '2024-05-02', '2024-06-01') \
         RETURNING id",
    )
    .bind(customer_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) \
         SELECT ?, id, '1', '10' FROM products LIMIT 1",
    )
    .bind(sale_id.0)
    .execute(&pool)
    .await
    .unwrap();
    let payment_id: (i64,) = sqlx::query_as(
        "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date) \
         VALUES (?, ?, 1, '5', '2024-05-10') RETURNING id",
    )
    .bind(sale_id.0)
    .bind(account_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let receipt_id: (i64,) = sqlx::query_as(
        "INSERT INTO customer_receipts (customer_id, account_id, method_id, date, notes) \
         VALUES (?, ?, 1, '2024-06-20', NULL) RETURNING id",
    )
    .bind(customer_id.0)
    .bind(account_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    ((customer_id.0, sale_id.0, payment_id.0, receipt_id.0), pool)
}

/// The upgrade attributes every pre-existing row of the four tables to the
/// sentinel it REUSES, loses no row and no id, and leaves `created_by` NOT NULL
/// on all four — a future write that omits the actor is refused by the
/// database. The RESTRICT audit foreign key holds the sentinel in place.
#[tokio::test]
async fn ac19_the_upgrade_attributes_every_sales_and_customer_row_to_the_system_sentinel() {
    let (legacy, pool) = upgraded_pool_with_legacy_sales_and_customer_rows().await;
    let (customer_id, sale_id, payment_id, receipt_id) = legacy;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();

    // Rows preserved, ids preserved, zero unattributed: every legacy row is
    // exactly where it was, pointing at the reused sentinel.
    for (table, id) in [
        ("customers", customer_id),
        ("sales", sale_id),
        ("sale_payments", payment_id),
        ("customer_receipts", receipt_id),
    ] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the legacy row survived with its id, attributed to the reused sentinel"
        );
    }
    for table in ["customers", "sales", "sale_payments", "customer_receipts"] {
        let unattributed: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE created_by IS NULL OR created_by != ?"
        )))
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unattributed.0, 0, "{table}: no row lost its actor");
    }
    // The seeded walk-in predates the audit too, so it points at the sentinel.
    let walkin_attributed: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM customers WHERE is_walkin = 1 AND created_by = ?")
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(walkin_attributed.0, 1, "the seeded walk-in is attributed");

    // Exactly one sentinel: the REUSE path never duplicated the account.
    let sentinels: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = 'sistema' COLLATE NOCASE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        sentinels.0, 1,
        "the reuse path never created a second sentinel"
    );

    // `created_by` is NOT NULL on all four rebuilt tables: a write that omits
    // the actor is refused by the database.
    for (table, sql) in [
        (
            "customers",
            "INSERT INTO customers (name) VALUES ('no-actor')",
        ),
        (
            "sales",
            "INSERT INTO sales (status, payment_type, customer_id, sale_date) \
             VALUES ('Draft', 'Cash', 1, '2024-01-01')",
        ),
        (
            "sale_payments",
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date) \
             VALUES (1, 1, 1, '1', '2024-01-01')",
        ),
        (
            "customer_receipts",
            "INSERT INTO customer_receipts (customer_id, account_id, method_id, date) \
             VALUES (1, 1, 1, '2024-01-01')",
        ),
    ] {
        let refused = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&pool)
            .await;
        assert!(
            refused.is_err(),
            "{table}: created_by is NOT NULL after the rebuild"
        );
    }

    // The re-enabled foreign keys find the same graph that existed before, and
    // the walk-in backstops survived the customers rebuild: the three triggers
    // still refuse the erase.
    let violations: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        violations.0, 0,
        "the upgrade leaves no foreign-key violation"
    );
    let refused_deactivate = sqlx::query("UPDATE customers SET is_active = 0 WHERE is_walkin = 1")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        refused_deactivate
            .to_string()
            .contains("cannot be deactivated"),
        "the walk-in deactivation trigger survived the rebuild: {refused_deactivate}"
    );

    // The RESTRICT audit foreign key holds the sentinel in place for these
    // tables too: deleting it is refused and the row survives.
    let refused = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("FOREIGN KEY constraint failed"),
        "the audit FK refuses the deletion: {refused}"
    );
    let still_there: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there.0, 1);
}

/// The defensive path: migration 32 REUSES the sentinel migration 30 created
/// and only creates one if it is somehow absent. This test makes it absent —
/// every table that references the sentinel is emptied and the account deleted
/// after migration 31 — plants legacy sales/customers rows (the only two of the
/// four that can exist without any user: a payment row and a receipt need an
/// account, and an account needs a user to attribute itself to), and runs the
/// rest of the chain.
#[tokio::test]
async fn ac19_the_sales_migration_recreates_a_missing_sentinel() {
    let pool = upgraded_pool_with_legacy_sales_and_customer_rows().await.1;
    // Make the sentinel absent: empty the tables that reference it (RESTRICT
    // refuses a direct delete), children of the business rows first.
    sqlx::query("DELETE FROM sale_payments")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sale_lines")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM customer_receipts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sales")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM transactions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM payment_methods")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM accounts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stock_movements")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM products")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM categories")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users")
        .execute(&pool)
        .await
        .unwrap();
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before.0, 0, "the sentinel is gone when migration 32 runs");

    // The legacy rows this path can attribute: a customer and its sale. Neither
    // references a user before migration 32 runs, so both survive the erasure
    // and give the migration something to attribute.
    let (customer_id,): (i64,) = sqlx::query_as(
        "INSERT INTO customers (name, is_walkin, is_active) \
         VALUES ('Defensive Client', 0, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let (sale_id,): (i64,) = sqlx::query_as(
        "INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date) \
         VALUES ('2024-SALE-000009', 'Confirmed', 'Credit', ?, 'Defensive Client', '2024-05-02', '2024-06-01') \
         RETURNING id",
    )
    .bind(customer_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();
    for (table, id) in [("customers", customer_id), ("sales", sale_id)] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the defensive sentinel exists and owns the legacy row"
        );
    }
    // The defensive sentinel is the same shape migration 30's is: inactive,
    // roleless, unusable credential.
    let sentinel_row: (i64, i64) =
        sqlx::query_as("SELECT is_active, must_change_password FROM users WHERE id = ?")
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel_row.0, 0, "the recreated sentinel is inactive");
    let roles: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE user_id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(roles.0, 0, "the recreated sentinel holds no role");
}

// ---------------------------------------------------------------------------
// AC18 (sales/customers audit, slice S11): the display, at the wiring layer.
// ---------------------------------------------------------------------------

/// The sale record page shows the actor as a DISPLAY NAME, never an id: the
/// shared session user creates the draft, a second probe user confirms it, and
/// the page renders both names — "Registrado por" for the creator and
/// "Actualizado por" for the confirming edit.
#[tokio::test]
async fn audit_the_sale_record_shows_the_actor_display_name() {
    let (app, pool) = test_app().await;
    let customer = seed_customer(&pool, "Audit Sale Client", None, Some(30)).await;
    let product = create_product_via_web(&app, &pool, "AUD-SALE-P", "0", "100").await;
    record_stock_via_web(&app, product, "10").await;
    let sale_id = create_sale_draft_for_customer(&app, customer, "Credit", "").await;
    add_sale_line_via_web(&app, sale_id, product, "1").await;

    // A second principal (display name "Test Probe") holds the draft-lifecycle
    // permission and confirms the sale.
    let probe_token =
        test_support::seed_session_with_permissions(&pool, &["sales.read", "sales.create"])
            .await
            .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        &format!("/web/sales/{sale_id}/confirm"),
        "method_id=",
        &test_support::cookie_for(&probe_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    let (status, page) = get(&app, &format!("/sales/{sale_id}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page.matches("Registered by Test Admin").count(),
        1,
        "the sale names its creator: {page:.600}"
    );
    assert_eq!(
        page.matches("Updated by Test Probe").count(),
        1,
        "the edit names its editor: {page:.600}"
    );
    assert!(
        !page.contains("Registrado por 1"),
        "the interface never renders a raw user id: {page}"
    );
}

/// The customer statement shows the CUSTOMER row's attribution as a display
/// name, labelled "Cliente registrado por" so the documents below cannot be
/// misread as this person's work.
#[tokio::test]
async fn audit_the_customer_statement_shows_the_actor_display_name() {
    let (app, pool) = test_app().await;
    // The customer is created through the web form, so its actor is the shared
    // session user (the raw fixture seeds the sentinel, which is for upgrade
    // tests, not for a display test that must name a person).
    let (status, resp) = post_form(&app, "/web/customers", "name=Audit+Statement+Client").await;
    assert_eq!(status, StatusCode::OK, "{resp:.400}");
    let customer = customer_id_by_name(&pool, "Audit Statement Client").await;

    // A second principal (display name "Test Probe") edits the customer.
    let probe_token =
        test_support::seed_session_with_permissions(&pool, &["customers.read", "customers.write"])
            .await
            .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/customers/edit",
        &format!("customer_id={customer}&name=Renamed+Client&phone=&address=&tax_id=&notes=&credit_limit=&due_days="),
        &test_support::cookie_for(&probe_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    let localization = crate::localization::load_context(&pool).await.unwrap();
    let registered_by = format!(
        "{} Test Admin",
        localization.tr(crate::localization::MessageKey::AuditRegisteredBy)
    );
    let updated_by = format!(
        "{} Test Probe",
        localization.tr(crate::localization::MessageKey::AuditUpdatedBy)
    );
    let (status, page) = get(&app, &format!("/customers/{customer}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page.matches(&registered_by).count(),
        1,
        "the statement names the customer's creator: {page:.600}"
    );
    assert_eq!(
        page.matches(&updated_by).count(),
        1,
        "the edit names its editor: {page:.600}"
    );
    assert!(
        !page.contains(&registered_by.replace("Test Admin", "1")),
        "the interface never renders a raw user id: {page}"
    );
}

// ---------------------------------------------------------------------------
// AC19 (purchases/suppliers audit, M5 Phase B slice S12): the upgrade sequence
// on the four rebuilt tables — a database built with the migrations up to 32,
// business rows in suppliers/product_supplier_costs/purchases/purchase_payments
// and no user beyond the sentinel.
// ---------------------------------------------------------------------------

/// A pool with the migration chain stopped just after the sales/customers
/// audit (the pre-33 purchases/suppliers schema is real) plus legacy business
/// rows planted the way pre-audit code wrote them: no created_by column
/// exists to fill.
async fn upgraded_pool_with_legacy_purchases_and_supplier_rows(
) -> ((i64, i64, i64, i64), sqlx::SqlitePool) {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run_to(20240101000032, &pool)
        .await
        .unwrap();

    // The sentinel exists by now (the seeded payment methods made migration 30
    // attribute something): the finance rows the audit needs are real.
    let sentinel: (i64,) =
        sqlx::query_as("SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE")
            .fetch_one(&pool)
            .await
            .unwrap();
    let users_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        users_before.0, 1,
        "no user beyond the sentinel before the upgrade"
    );

    // A product for the cost satellite (migration 31 already gave products
    // their audit columns, so the plant carries the sentinel).
    let product_id: (i64,) = sqlx::query_as(
        "INSERT INTO products (sku, name, kind, unit, sale_price, cost_price, track_stock, created_by) \
         VALUES ('LEGACY-P', 'legacy product', 'Product', 'un', '10', '5', 0, ?) RETURNING id",
    )
    .bind(sentinel.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let supplier_id: (i64,) = sqlx::query_as(
        "INSERT INTO suppliers (name, phone, notes, is_active) \
         VALUES ('Legacy Supplier', '555', 'legacy', 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let cost_id: (i64,) = sqlx::query_as(
        "INSERT INTO product_supplier_costs \
         (product_id, supplier_id, current_cost, current_cost_updated_at) \
         VALUES (?, ?, '7.50', '2024-05-01') RETURNING id",
    )
    .bind(product_id.0)
    .bind(supplier_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let account_id: (i64,) = sqlx::query_as(
        "INSERT INTO accounts (name, created_by) VALUES ('legacy purchase wallet', ?) RETURNING id",
    )
    .bind(sentinel.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let purchase_id: (i64,) = sqlx::query_as(
        "INSERT INTO purchases (purchase_number, supplier_id, status, payment_type, purchase_date, due_date) \
         VALUES ('2024-PURCH-000001', ?, 'Confirmed', 'Credit', '2024-05-02', '2024-06-01') RETURNING id",
    )
    .bind(supplier_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) \
         VALUES (?, ?, '2', '7.50')",
    )
    .bind(purchase_id.0)
    .bind(product_id.0)
    .execute(&pool)
    .await
    .unwrap();
    let payment_id: (i64,) = sqlx::query_as(
        "INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date) \
         VALUES (?, ?, 1, '5', '2024-05-10') RETURNING id",
    )
    .bind(purchase_id.0)
    .bind(account_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    (
        (supplier_id.0, cost_id.0, purchase_id.0, payment_id.0),
        pool,
    )
}

/// The upgrade attributes every pre-existing row of the four tables to the
/// sentinel it REUSES, loses no row and no id, and leaves `created_by` NOT NULL
/// on all four — a future write that omits the actor is refused by the
/// database. The rebuilt tables keep their backstops: the name/number/pair
/// UNIQUEs, the CHECKs, the RESTRICT foreign keys and the partial unique
/// preferred index all still refuse what they refused before.
#[tokio::test]
async fn ac19_the_upgrade_attributes_every_purchases_and_suppliers_row_to_the_system_sentinel() {
    let (legacy, pool) = upgraded_pool_with_legacy_purchases_and_supplier_rows().await;
    let (supplier_id, cost_id, purchase_id, payment_id) = legacy;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();

    // Rows preserved, ids preserved, zero unattributed: every legacy row is
    // exactly where it was, pointing at the reused sentinel.
    for (table, id) in [
        ("suppliers", supplier_id),
        ("product_supplier_costs", cost_id),
        ("purchases", purchase_id),
        ("purchase_payments", payment_id),
    ] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the legacy row survived with its id, attributed to the reused sentinel"
        );
    }
    for table in [
        "suppliers",
        "product_supplier_costs",
        "purchases",
        "purchase_payments",
    ] {
        let unattributed: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE created_by IS NULL OR created_by != ?"
        )))
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unattributed.0, 0, "{table}: no row lost its actor");
    }

    // Exactly one sentinel: the REUSE path never duplicated the account.
    let sentinels: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE username = 'sistema' COLLATE NOCASE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        sentinels.0, 1,
        "the reuse path never created a second sentinel"
    );

    // `created_by` is NOT NULL on all four rebuilt tables: a write that omits
    // the actor is refused by the database.
    for (table, sql) in [
        (
            "suppliers",
            "INSERT INTO suppliers (name) VALUES ('no-actor')",
        ),
        (
            "purchases",
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date) \
             VALUES (1, 'Draft', 'Cash', '2024-01-01')",
        ),
        (
            "purchase_payments",
            "INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date) \
             VALUES (1, 1, 1, '1', '2024-01-01')",
        ),
        (
            "product_supplier_costs",
            "INSERT INTO product_supplier_costs \
             (product_id, supplier_id, current_cost, current_cost_date) \
             VALUES (1, 1, '1', '2024-01-01')",
        ),
    ] {
        let refused = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&pool)
            .await;
        assert!(
            refused.is_err(),
            "{table}: created_by is NOT NULL after the rebuild"
        );
    }

    // The rebuilt tables keep their live constraints: a runtime probe for each
    // declaration the originals carried.
    // suppliers: the UNIQUE name and the is_active CHECK.
    let dup_name =
        sqlx::query("INSERT INTO suppliers (name, created_by) VALUES ('Legacy Supplier', 1)")
            .execute(&pool)
            .await;
    assert!(
        dup_name.is_err(),
        "suppliers: the UNIQUE name survived the rebuild"
    );
    let bad_active = sqlx::query(
        "INSERT INTO suppliers (name, is_active, created_by) VALUES ('bad-active', 2, 1)",
    )
    .execute(&pool)
    .await;
    assert!(
        bad_active.is_err(),
        "suppliers: the is_active CHECK survived the rebuild"
    );
    // product_supplier_costs: the (product, supplier) UNIQUE pair, the CHECK on
    // is_preferred and the partial unique one-preferred index.
    let dup_pair = sqlx::query(
        "INSERT INTO product_supplier_costs \
         (product_id, supplier_id, current_cost, current_cost_date, created_by) \
         VALUES (1, 1, '9', '2024-06-01', ?)",
    )
    .bind(sentinel)
    .execute(&pool)
    .await;
    assert!(
        dup_pair.is_err(),
        "product_supplier_costs: the (product, supplier) UNIQUE pair survived"
    );
    let second_supplier: (i64,) = sqlx::query_as(
        "INSERT INTO suppliers (name, created_by) VALUES ('legacy supplier two', ?) RETURNING id",
    )
    .bind(sentinel)
    .fetch_one(&pool)
    .await
    .unwrap();
    let bad_preferred =
        sqlx::query("UPDATE product_supplier_costs SET is_preferred = 2 WHERE id = ?")
            .bind(cost_id)
            .execute(&pool)
            .await;
    assert!(
        bad_preferred.is_err(),
        "product_supplier_costs: the is_preferred CHECK survived"
    );
    sqlx::query("UPDATE product_supplier_costs SET is_preferred = 1 WHERE id = ?")
        .bind(cost_id)
        .execute(&pool)
        .await
        .unwrap();
    // A second preferred row for the SAME product hits the partial unique
    // index (the product keeps at most one preferred supplier, now rebuilt).
    sqlx::query(
        "INSERT INTO product_supplier_costs \
         (product_id, supplier_id, current_cost, current_cost_date, created_by) \
         VALUES (1, ?, '9', '2024-06-01', ?)",
    )
    .bind(second_supplier.0)
    .bind(sentinel)
    .execute(&pool)
    .await
    .unwrap();
    let second_preferred = sqlx::query(
        "UPDATE product_supplier_costs SET is_preferred = 1 \
         WHERE product_id = 1 AND supplier_id = ?",
    )
    .bind(second_supplier.0)
    .execute(&pool)
    .await;
    assert!(
        second_preferred.is_err(),
        "product_supplier_costs: the one-preferred-per-product partial unique index survived"
    );
    // purchases: the status/payment_type CHECKs and the UNIQUE purchase_number.
    let bad_status = sqlx::query(
        "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by) \
         VALUES (1, 'Shipped', 'Cash', '2024-01-01', ?)",
    )
    .bind(sentinel)
    .execute(&pool)
    .await;
    assert!(
        bad_status.is_err(),
        "purchases: the status CHECK survived the rebuild"
    );
    let bad_payment_type = sqlx::query(
        "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by) \
         VALUES (1, 'Draft', 'Barter', '2024-01-01', ?)",
    )
    .bind(sentinel)
    .execute(&pool)
    .await;
    assert!(
        bad_payment_type.is_err(),
        "purchases: the payment_type CHECK survived the rebuild"
    );
    let dup_number = sqlx::query(
        "INSERT INTO purchases (purchase_number, supplier_id, status, payment_type, purchase_date, created_by) \
         VALUES ('2024-PURCH-000001', 1, 'Draft', 'Cash', '2024-01-01', ?)",
    )
    .bind(sentinel)
    .execute(&pool)
    .await;
    assert!(
        dup_number.is_err(),
        "purchases: the UNIQUE purchase_number survived the rebuild"
    );
    // purchase_payments: the RESTRICT purchase/account/method references.
    let unknown_purchase = sqlx::query(
        "INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, created_by) \
         VALUES (99999, 1, 1, '1', '2024-01-01', ?)",
    )
    .bind(sentinel)
    .execute(&pool)
    .await;
    assert!(
        unknown_purchase.is_err(),
        "purchase_payments: the purchase FK survived the rebuild"
    );
    let delete_referenced_supplier = sqlx::query("DELETE FROM suppliers WHERE id = ?")
        .bind(supplier_id)
        .execute(&pool)
        .await;
    assert!(
        delete_referenced_supplier.is_err(),
        "suppliers: the RESTRICT from purchases/cost rows survived the rebuild"
    );

    // The re-enabled foreign keys find the same graph that existed before:
    // no violation anywhere.
    let violations: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        violations.0, 0,
        "the upgrade leaves no foreign-key violation"
    );

    // The RESTRICT audit foreign key holds the sentinel in place for these
    // tables too: deleting it is refused and the row survives.
    let refused = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("FOREIGN KEY constraint failed"),
        "the audit FK refuses the deletion: {refused}"
    );
    let still_there: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_there.0, 1);
}

/// The defensive path: migration 33 REUSES the sentinel migration 30 created
/// and only creates one if it is somehow absent. This test makes it absent —
/// every table that references the sentinel is emptied and the account deleted
/// after migration 32 — plants legacy purchases/suppliers rows (the only ones
/// of the four that can exist without any user: a purchase needs a supplier,
/// and neither references a user), and runs the rest of the chain.
#[tokio::test]
async fn ac19_the_purchases_migration_recreates_a_missing_sentinel() {
    let pool = upgraded_pool_with_legacy_purchases_and_supplier_rows()
        .await
        .1;
    // Make the sentinel absent: empty the tables that reference it (RESTRICT
    // refuses a direct delete), children of the business rows first.
    sqlx::query("DELETE FROM purchase_payments")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM purchase_lines")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM product_supplier_costs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM purchases")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM suppliers")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sale_payments")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sale_lines")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM customer_receipts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sales")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM transactions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM payment_methods")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM accounts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stock_movements")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM product_barcodes")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM products")
        .execute(&pool)
        .await
        .unwrap();
    // The category tree is self-referencing: unparent first, then erase.
    sqlx::query("UPDATE categories SET parent_id = NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM categories")
        .execute(&pool)
        .await
        .unwrap();
    // Customers reference the sentinel too (migration 32), and the walk-in
    // trigger refuses its deletion, so the trigger goes first.
    sqlx::query("DROP TRIGGER IF EXISTS trg_customers_walkin_no_delete")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM customers")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users")
        .execute(&pool)
        .await
        .unwrap();
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before.0, 0, "the sentinel is gone when migration 33 runs");
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before.0, 0, "the sentinel is gone when migration 33 runs");

    // The legacy rows this path can attribute: a supplier and a purchase on
    // it. Neither references a user before migration 33 runs, so both survive
    // the erasure and give the migration something to attribute.
    let (supplier_id,): (i64,) = sqlx::query_as(
        "INSERT INTO suppliers (name, phone, notes, is_active) \
         VALUES ('Defensive Supplier', '555', NULL, 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let (purchase_id,): (i64,) = sqlx::query_as(
        "INSERT INTO purchases (purchase_number, supplier_id, status, payment_type, purchase_date, due_date) \
         VALUES ('2024-PURCH-000009', ?, 'Confirmed', 'Credit', '2024-05-02', '2024-06-01') RETURNING id",
    )
    .bind(supplier_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = test_support::audit_actor_id(&pool).await.unwrap();
    for (table, id) in [("suppliers", supplier_id), ("purchases", purchase_id)] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE id = ? AND created_by = ?"
        )))
        .bind(id)
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, 1,
            "{table}: the defensive sentinel exists and owns the legacy row"
        );
    }
    // The defensive sentinel is the same shape migration 30's is: inactive,
    // roleless, unusable credential.
    let sentinel_row: (i64, i64) =
        sqlx::query_as("SELECT is_active, must_change_password FROM users WHERE id = ?")
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel_row.0, 0, "the recreated sentinel is inactive");
    let roles: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE user_id = ?")
        .bind(sentinel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(roles.0, 0, "the recreated sentinel holds no role");
}

// ---------------------------------------------------------------------------
// AC18 (purchases/suppliers audit, slice S12): the display, at the wiring layer.
// ---------------------------------------------------------------------------

/// The purchase record page shows the actor as a DISPLAY NAME, never an id:
/// the shared session user creates the draft and its line, a second probe
/// user confirms it, and the page renders both names — "Registered by" for
/// the creator and "Updated by" for the confirming edit (the purchase
/// surfaces are English, receiving-desk T1).
#[tokio::test]
async fn audit_the_purchase_record_shows_the_actor_display_name() {
    let (app, pool) = test_app().await;
    let supplier = create_supplier_via_web(&app, &pool, "Audit Purchase Supplier").await;
    let product = create_product_via_web(&app, &pool, "AUD-PURCH-P", "0", "100").await;

    // A Credit draft needs a due date; the confirm then carries no method.
    let body = format!(
        "supplier_id={supplier}&payment_type=Credit&purchase_date=2024-05-02&due_date=2024-06-02&supplier_invoice_no=&notes="
    );
    let (status, resp) = post_form(&app, "/web/purchases", &body).await;
    assert_eq!(status, StatusCode::OK, "create purchase draft: {resp:.400}");
    let purchase_id = purchase_id_by_supplier(&pool, supplier).await;

    let (status, resp) = post_form(
        &app,
        &format!("/web/purchases/{purchase_id}/lines"),
        &format!("product_id={product}&qty=2&unit_cost=7.50"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add line: {resp:.400}");

    // A second principal (display name "Test Probe") holds the purchase
    // permission and confirms the draft.
    let probe_token =
        test_support::seed_session_with_permissions(&pool, &["purchases.read", "purchases.create"])
            .await
            .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        &format!("/web/purchases/{purchase_id}/confirm"),
        "method_id=",
        &test_support::cookie_for(&probe_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    let (status, page) = get(&app, &format!("/purchases/{purchase_id}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page.matches("Registered by Test Admin").count(),
        1,
        "the purchase names its creator: {page:.600}"
    );
    assert_eq!(
        page.matches("Updated by Test Probe").count(),
        1,
        "the confirm names its editor: {page:.600}"
    );
    assert!(
        !page.contains("Registered by 1"),
        "the interface never renders a raw user id: {page}"
    );
}

/// The supplier drawer shows the SUPPLIER row's attribution as a display
/// name, labelled "Proveedor registrado por" so the balance and the purchases
/// below cannot be misread as this person's work.
#[tokio::test]
async fn audit_the_supplier_detail_shows_the_actor_display_name() {
    let (app, pool) = test_app().await;
    let supplier = create_supplier_via_web(&app, &pool, "Audit Drawer Supplier").await;

    // A second principal (display name "Test Probe") edits the supplier.
    let probe_token =
        test_support::seed_session_with_permissions(&pool, &["suppliers.read", "suppliers.write"])
            .await
            .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/suppliers/edit",
        &format!("id={supplier}&name=Renamed+Supplier&phone=555&notes="),
        &test_support::cookie_for(&probe_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    let (status, page) = get(&app, &format!("/web/suppliers/{supplier}/detail")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page.matches("Supplier registered by Test Admin").count(),
        1,
        "the drawer names the supplier's creator: {page:.600}"
    );
    assert_eq!(
        page.matches("Updated by Test Probe").count(),
        1,
        "the edit names its editor: {page:.600}"
    );
    assert!(
        !page.contains("Proveedor registrado por 1"),
        "the interface never renders a raw user id: {page}"
    );
}

/// Helper: the newest purchase for a supplier id, resolved through the
/// supplier-side read the drawer already uses.
async fn purchase_id_by_supplier(pool: &sqlx::SqlitePool, supplier_id: i64) -> i64 {
    let row: (i64,) =
        sqlx::query_as("SELECT id FROM purchases WHERE supplier_id = ? ORDER BY id DESC LIMIT 1")
            .bind(supplier_id)
            .fetch_one(pool)
            .await
            .unwrap();
    row.0
}

// ---------------------------------------------------------------------------
// S13 (identity audit, M5 Phase B): the actor on `users`, `roles` and
// `permissions`, and the grant trail (`user_roles.granted_by`/`granted_at`)
// finally surfaced. AC18/AC19 on the identity surface, over the HTTP paths
// the screens drive.
// ---------------------------------------------------------------------------

/// The audit columns of one user row, read straight from the database.
async fn user_audit(pool: &SqlitePool, username: &str) -> (Option<i64>, Option<i64>) {
    sqlx::query_as("SELECT created_by, updated_by FROM users WHERE username = ? COLLATE NOCASE")
        .bind(username)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The audit columns of one role row, read straight from the database.
async fn role_audit(pool: &SqlitePool, code: &str) -> (Option<i64>, Option<i64>) {
    sqlx::query_as("SELECT created_by, updated_by FROM roles WHERE code = ?")
        .bind(code)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The id of the migration's sentinel account (the Phase B system actor).
async fn sentinel_id(pool: &SqlitePool) -> i64 {
    test_support::audit_actor_id(pool).await.unwrap()
}

/// The grant trail row of one (username, role code) pair.
async fn grant_trail_row(pool: &SqlitePool, username: &str, role_code: &str) -> (i64, i64) {
    sqlx::query_as(
        "SELECT ur.granted_by, u.id FROM user_roles ur \
         JOIN users u ON u.id = ur.user_id \
         JOIN roles r ON r.id = ur.role_id \
         WHERE u.username = ? COLLATE NOCASE AND r.code = ?",
    )
    .bind(username)
    .bind(role_code)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// AC18 (users): a user created by one actor and edited by another carries
/// both — the creation records `created_by` on the row the screen made, the
/// activation toggle and the administrator password reset stamp the TARGET's
/// `updated_by` with the acting administrator. The bootstrap-created rows
/// (sentinel, bootstrap administrator) carry NULL, the honest "the system"
/// value the interface renders as such.
#[tokio::test]
async fn ac18_a_user_records_two_different_actors_and_the_system_rows_stay_null() {
    let (app, pool) = test_app().await;

    // The shared fixture user (Test Admin) creates a user through the screen.
    let (status, body) = post_form(
        &app,
        "/web/users",
        "username=teller&display_name=Teller&password=initial password 1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    // A second principal (Test Probe) holds the users tier: it toggles the
    // target's activation, then resets the target's password.
    let probe_cookie = test_support::cookie_for(
        &test_support::seed_session_with_permissions(
            &pool,
            &["identity.users.read", "identity.users.manage"],
        )
        .await
        .unwrap(),
    );
    let target_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'teller'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/users/deactivate",
        &format!("user_id={target_id}"),
        &probe_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");
    // The toggle is an edit: its own stamp is asserted HERE, before the
    // reset's write could mask it (the reset is by the same actor, so the
    // final column alone cannot tell the two writes apart).
    let probe_id = probe_id_by_suffix(&pool).await;
    let (_, toggled_by) = user_audit(&pool, "teller").await;
    assert_eq!(
        toggled_by,
        Some(probe_id),
        "the activation toggle records its actor"
    );
    // A third principal (the second probe) performs the administrator reset:
    // a DIFFERENT actor, so the reset's own stamp is observable against the
    // toggle's.
    let reset_cookie = test_support::cookie_for(
        &test_support::seed_session_with_permissions(
            &pool,
            &["identity.users.read", "identity.users.manage"],
        )
        .await
        .unwrap(),
    );
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/users/password",
        &format!("user_id={target_id}&new_password=temp password 34"),
        &reset_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    // The actor columns: created by Test Admin's principal, edited last by
    // the probe (both the toggle and the reset are edits of the target).
    let fixture_id = user_id_by_username_smoke(&pool, test_support::TEST_USERNAME).await;
    let (created_by, updated_by) = user_audit(&pool, "teller").await;
    assert_eq!(
        created_by,
        Some(fixture_id),
        "the creation records the acting principal"
    );
    // The LAST edit is the reset, by the second probe — the column keeps the
    // latest editor, exactly what the display shows.
    let resetter_id = probe_id_by_suffix(&pool).await;
    assert_eq!(
        updated_by,
        Some(resetter_id),
        "the reset records the acting administrator"
    );

    // The sentinel is the system's work: NULL, honestly, on both audit
    // columns. (The bootstrap administrator's NULLs are asserted at the
    // service level, in the AC1 bootstrap tests.)
    let (sentinel_created, sentinel_updated) = user_audit(&pool, "sistema").await;
    assert_eq!(
        sentinel_created, None,
        "the sentinel has no creator and the schema says so"
    );
    assert_eq!(sentinel_updated, None);

    // The RESTRICT foreign keys hold: deleting a user whose id another
    // user's audit columns name is refused.
    let refused = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(fixture_id)
        .execute(&pool)
        .await;
    assert!(
        refused.is_err(),
        "users.created_by is ON DELETE RESTRICT like every audited table"
    );
}

/// AC18 (roles): a role created by one actor and re-permissioned by another —
/// the details edit and the matrix replacement both stamp the role's
/// `updated_by` with whoever made them. The seeded roles are the migration's
/// work: attributed to the sentinel, `updated_by` NULL.
#[tokio::test]
async fn ac18_a_role_records_two_different_actors_and_the_seeds_carry_the_sentinel() {
    let (app, pool) = test_app().await;

    // The shared fixture user (Test Admin) creates a role through the screen.
    let (status, body) = post_form(
        &app,
        "/web/roles",
        "code=auditado&name=Auditado&description=Creado+para+la+prueba",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    // A second principal (Test Probe) edits the role's details: the row's
    // `updated_by` becomes the editor, asserted BEFORE the next write so the
    // details edit's own stamp is what the assertion observes.
    let details_cookie = test_support::cookie_for(
        &test_support::seed_session_with_permissions(&pool, &["identity.roles.manage"])
            .await
            .unwrap(),
    );
    let role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'auditado'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/roles/edit",
        &format!("role_id={role_id}&name=Auditado+II&description="),
        &details_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");
    let details_editor = probe_id_by_suffix(&pool).await;
    let (created_by, updated_by) = role_audit(&pool, "auditado").await;
    let fixture_id = user_id_by_username_smoke(&pool, test_support::TEST_USERNAME).await;
    assert_eq!(
        created_by,
        Some(fixture_id),
        "the create records its author"
    );
    assert_eq!(
        updated_by,
        Some(details_editor),
        "the details edit records its editor"
    );

    // A third principal (the second probe) re-permissions the role: the
    // matrix replacement stamps the role's `updated_by` in the same
    // transaction as the matrix change.
    let matrix_cookie = test_support::cookie_for(
        &test_support::seed_session_with_permissions(&pool, &["identity.roles.manage"])
            .await
            .unwrap(),
    );
    let dashboard: i64 =
        sqlx::query_scalar("SELECT id FROM permissions WHERE code = 'dashboard.read'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/roles/matrix",
        &format!("role_id={role_id}&permission_ids={dashboard}"),
        &matrix_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");
    let matrix_editor = probe_id_by_suffix(&pool).await;
    assert_ne!(
        matrix_editor, details_editor,
        "precondition: the two probes are different users"
    );
    let (_, updated_by) = role_audit(&pool, "auditado").await;
    assert_eq!(
        updated_by,
        Some(matrix_editor),
        "the matrix edit records its editor (the stamp lives in the matrix transaction)"
    );

    // The four seeded roles carry the migration's sentinel as their creator
    // and no editor: the seeds were nobody's screen work.
    let sentinel = sentinel_id(&pool).await;
    for code in ["admin", "vendedor", "cajero", "deposito"] {
        let (seeded_created, seeded_updated) = role_audit(&pool, code).await;
        assert_eq!(
            seeded_created,
            Some(sentinel),
            "{code}: the pre-existing role is attributed to the sentinel"
        );
        assert_eq!(seeded_updated, None, "{code}: the seed was never edited");
    }
    // Same for the seeded catalog: every one of the 23 rows.
    let unattributed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM permissions WHERE created_by IS NULL OR created_by != ?",
    )
    .bind(sentinel)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unattributed.0, 0, "every seeded permission is attributed");
}

/// The grant trail display (slice S13): the users screen renders, for each
/// granted role, who granted it and when — the `user_roles` data the RBAC
/// slice has carried since S2 — plus the rows' own audit attribution with
/// NULL shown honestly as the system. Names, never ids.
#[tokio::test]
async fn audit_the_users_screen_shows_the_grant_trail_and_the_actor_names() {
    let (app, pool) = test_app().await;

    // Test Admin creates the target through the screen...
    let (status, body) = post_form(
        &app,
        "/web/users",
        "username=caja1&display_name=Caja+Uno&password=initial password 1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    // ...and Test Probe grants the target the vendedor role through the
    // assignment endpoint, then edits the target (deactivation) so both
    // attributions are distinct.
    let probe_token = test_support::seed_session_with_permissions(
        &pool,
        &[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ],
    )
    .await
    .unwrap();
    let probe_cookie = test_support::cookie_for(&probe_token);
    use crate::repositories::role_repo::RoleRepository;
    let vendedor = crate::repositories::SqliteRoleRepository::new(pool.clone())
        .find_by_code("vendedor")
        .await
        .unwrap()
        .unwrap();
    let target_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'caja1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor_id = user_id_by_username_smoke(&pool, test_support::TEST_USERNAME).await;
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/users/roles",
        &format!("user_id={target_id}&role_ids={}", vendedor.id),
        &probe_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");
    let (status, body) = post_form_with_cookie(
        &app,
        "/web/users/deactivate",
        &format!("user_id={target_id}"),
        &probe_cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    // The trail is in the database first: the granter is the probing
    // principal, not the creator.
    let (granted_by, _) = grant_trail_row(&pool, "caja1", "vendedor").await;
    assert_ne!(granted_by, actor_id, "the grant records the granting actor");
    assert_eq!(
        granted_by,
        probe_id_by_suffix(&pool).await,
        "the grant trail carries the granting actor"
    );

    // The page: names, never ids.
    let (status, page) = get(&app, "/users").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert_eq!(
        page.matches("Salesperson: granted by Test Probe on ")
            .count(),
        1,
        "the trail names the granting actor: {page:.900}"
    );
    assert_eq!(
        page.matches("Registered by Test Admin").count(),
        1,
        "the created user names its creator: {page:.900}"
    );
    assert_eq!(
        page.matches("Updated by Test Probe").count(),
        1,
        "the edit names its editor: {page:.900}"
    );
    // The system-created rows (sentinel, bootstrap-era accounts): the honest
    // label, not a blank and not an id.
    let system_rows = page.matches("Registered by (system)").count();
    assert!(
        system_rows >= 2,
        "the sentinel and the shared fixture account are the system's work: {page:.900}"
    );
    assert!(
        !page.contains("Creado por 1") && !page.contains("otorgado por 1"),
        "the interface never renders a raw user id: {page:.900}"
    );
}

/// The roles screen names the roles' authors (slice S13): the seeds render
/// the sentinel's display name; the screen-created role names its author.
#[tokio::test]
async fn audit_the_roles_screen_shows_the_role_authors() {
    let (app, _pool) = test_app().await;

    let (status, body) = post_form(
        &app,
        "/web/roles",
        "code=supervisor&name=Supervisor&description=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:.400}");

    let (status, page) = get(&app, "/roles").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert_eq!(
        page.matches("Registered by Sistema (anterior al registro)")
            .count(),
        4,
        "the four seeds name the sentinel: {page:.900}"
    );
    assert!(
        page.matches("Registered by Test Admin").count() >= 1,
        "the screen-created role names its author: {page:.900}"
    );
    assert!(
        !page.contains("Creado por 1"),
        "the interface never renders a raw user id: {page:.900}"
    );
}

/// AC19 (identity tables): the upgrade attributes the pre-existing roles and
/// permissions to the sentinel it reuses, leaves the users rows' audit
/// columns NULL (the honest "created by the system"), preserves every row and
/// id, and leaves the foreign-key graph clean. The guard triggers recreated
/// by the rebuild still bite immediately after the migration.
#[tokio::test]
async fn ac19_the_upgrade_attributes_the_identity_rows_to_the_system_sentinel() {
    // Build the database with migrations up to 33: identity rows exist (the
    // four seeded roles, the permissions present at that migration, the sentinel) plus business rows
    // with their audit columns already attributed.
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
    run_migrations_up_to_33(&pool).await;

    // Pre-34 identity state: a legacy role and user created by direct SQL —
    // exactly the shape the chain produces (no audit columns yet).
    sqlx::query("INSERT INTO roles (code, name, description) VALUES ('legacy', 'Legacy', NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (username, display_name, password_hash) \
         VALUES ('legacy-op', 'Legacy Op', 'placeholder-not-a-real-argon2-hash')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let legacy_user: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'legacy-op'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let legacy_role: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'legacy'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_roles (user_id, role_id, granted_by) VALUES (?, ?, ?)")
        .bind(legacy_user)
        .bind(legacy_role)
        .bind(legacy_user)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let sentinel = sentinel_id(&pool).await;

    // Every pre-existing role and permission is attributed to the sentinel.
    for sql in [
        "SELECT COUNT(*) FROM roles WHERE created_by IS NULL OR created_by != ?",
        "SELECT COUNT(*) FROM permissions WHERE created_by IS NULL OR created_by != ?",
    ] {
        let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0, 0,
            "no pre-existing identity row lost its attribution: {sql}"
        );
    }
    // The legacy role and user kept their ids and now carry the sentinel.
    let attributed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM roles WHERE id = ? AND created_by = ? AND updated_by IS NULL",
    )
    .bind(legacy_role)
    .bind(sentinel)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        attributed.0, 1,
        "the legacy role survived with its id, attributed"
    );
    let attributed: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM permissions WHERE id > 0 AND created_by = ?")
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attributed.0, 24, "the whole catalog is attributed");

    // users: NULL means the system — the sentinel and the legacy operator
    // predate the audit and no person created them.
    let unattributed_users: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE created_by IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        unattributed_users.0, 0,
        "every pre-existing user is the system's work: NULL is the honest value"
    );

    // The rebuild preserved the identity rows the interface renders: four
    // seeded roles plus the legacy one.
    let roles: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM roles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(roles.0, 5, "no role was lost in the rebuild");

    // The upgrade leaves a consistent graph.
    let violations: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        violations.0, 0,
        "the upgrade leaves no foreign-key violation"
    );

    // The recreated guard triggers bite immediately: the five refusal
    // families of the identity guarantees, after the migration, with the
    // triggers' own text.
    for (sql, refusal) in [
        (
            "DELETE FROM roles WHERE id = ?",
            "protected role cannot be deleted",
        ),
        (
            "UPDATE roles SET code = 'raiz' WHERE id = ?",
            "protected role code cannot change",
        ),
        (
            "DELETE FROM role_permissions WHERE role_id = ?",
            "protected role permissions cannot be removed",
        ),
        (
            "UPDATE roles SET is_system = 0 WHERE id = ?",
            "protected status is decided at seed time and cannot change",
        ),
    ] {
        let admin_role: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let err = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(admin_role)
            .execute(&pool)
            .await
            .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(
                db.message(),
                refusal,
                "the recreated trigger must refuse: {sql}"
            ),
            other => panic!("expected the guard trigger for {sql}, got {other:?}"),
        }
    }
    // The last-administrator arithmetic on users: the legacy operator holds
    // the protected role; deactivate, delete the row, remove the grant — all
    // refused while it is the only active holder.
    sqlx::query(
        "INSERT INTO user_roles (user_id, role_id, granted_by) \
         VALUES (?, (SELECT id FROM roles WHERE code = 'admin'), ?)",
    )
    .bind(legacy_user)
    .bind(legacy_user)
    .execute(&pool)
    .await
    .unwrap();
    for (sql, refusal) in [
        (
            "UPDATE users SET is_active = 0 WHERE id = ?",
            "cannot deactivate the last active user holding a protected role",
        ),
        (
            "DELETE FROM users WHERE id = ?",
            "cannot delete the last active user holding a protected role",
        ),
    ] {
        let err = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(legacy_user)
            .execute(&pool)
            .await
            .unwrap_err();
        match err {
            sqlx::Error::Database(db) => assert_eq!(db.message(), refusal, "{sql}"),
            other => panic!("expected the guard trigger for {sql}, got {other:?}"),
        }
    }
    let admin_role: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let err = sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
        .bind(legacy_user)
        .bind(admin_role)
        .execute(&pool)
        .await
        .unwrap_err();
    match err {
        sqlx::Error::Database(db) => assert_eq!(
            db.message(),
            "cannot remove the last grant of a protected role to an active user",
        ),
        other => panic!("expected the grant guard, got {other:?}"),
    }

    // The session and receipt triggers survived untouched.
    let (status, page) = get(
        &crate::routes::router(crate::routes::AppState::new(pool.clone(), false, true)),
        "/login",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page:.200}");
}

// ---------------------------------------------------------------------------
// The /documents index: any-of reachability, narrowed content, filters, cap.
// Every document below is seeded through the same web-flow helpers the
// neighbouring list tests use; nothing is inserted by hand except the row-cap
// test, which says so in its own comment.
// ---------------------------------------------------------------------------

/// The any-of screen opens for any ONE of the four read permissions, and the
/// content narrows to the families that code owns: the option list and the
/// rows both obey the permission, so a `sales.read` principal never even sees
/// the purchases option, and a seeded purchase never leaks into its list.
#[tokio::test]
async fn documents_any_single_tier_opens_and_the_content_narrows_to_its_families() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "DOC-P", "1", "50").await;
    record_stock_via_web(&app, product, "10").await;

    let cash = method_id(&pool, "Cash").await;
    let _wallet = create_account_via_web(&app, &pool, "DocWallet", &[cash]).await;
    let buyer = seed_customer(&pool, "DocBuyer", None, None).await;
    // Credit, so the customer owes and the receipt below has a receivable to
    // collect against (a collection never exceeds the outstanding debt).
    let sale = create_sale_draft_on_date(&app, buyer, "Credit", "2024-05-02", "2024-06-30").await;
    add_sale_line_via_web(&app, sale, product, "1").await;
    confirm_sale_via_web(&app, sale, None).await;
    let sale_number = sale_detail(&app, sale).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();

    let supplier = create_supplier_via_web(&app, &pool, "DocSupplier").await;
    let purchase = create_purchase_draft_on_date(&app, supplier, "2024-05-10").await;
    add_purchase_line_via_web(&app, purchase, product, "1").await;
    confirm_purchase_via_web(&app, purchase).await;
    let purchase_number = purchase_detail(&app, purchase).await["purchase"]["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();

    let (status, resp) = post_form(
        &app,
        "/web/customer-receipts",
        &format!("customer_id={buyer}&method_id={cash}&amount=5&date=2024-06-01"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed receipt: {resp}");

    // sales.read ALONE opens the page, sees only its groups' options and only
    // the sale rows — the purchase and its option never render.
    let sales = test_support::seed_session_with_permissions(&pool, &["sales.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&sales);
    let (status, page) = get_with_cookie(&app, "/documents", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        page.contains(r#"data-document-group="sales""#),
        "{page:.600}"
    );
    assert!(
        !page.contains(r#"data-document-group="purchases""#),
        "{page:.600}"
    );
    assert!(page.contains(&sale_number), "{page:.600}");
    assert!(!page.contains(&purchase_number), "{page:.600}");
    let (status, fragment) = get_fragment_with_cookie(&app, "/web/documents", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{fragment:.400}");
    assert!(fragment.contains(&sale_number), "{fragment:.600}");
    assert!(!fragment.contains(&purchase_number), "{fragment:.600}");

    // customers.read ALONE: the payments families open, the sales option and
    // the sale rows never render, and the receipt row does.
    let customers = test_support::seed_session_with_permissions(&pool, &["customers.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&customers);
    let (status, page) = get_with_cookie(&app, "/documents", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        page.contains(r#"data-document-group="payments""#),
        "{page:.600}"
    );
    assert!(
        !page.contains(r#"data-document-group="sales""#),
        "{page:.600}"
    );
    assert!(!page.contains(&sale_number), "{page:.600}");
    let (status, fragment) = get_fragment_with_cookie(&app, "/web/documents", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{fragment:.400}");
    assert!(
        fragment.contains("Customer receipt"),
        "the receipt row must render for the customers reader: {fragment:.600}"
    );
}

/// Deny by default: a principal holding none of the four codes is refused the
/// page AND the fragment request the browser's filter form makes. The empty
/// set must never open an any-of screen.
#[tokio::test]
async fn documents_deny_by_default_refuses_the_page_and_the_fragment() {
    let (app, pool) = test_app().await;
    let none = test_support::seed_session_with_permissions(&pool, &[])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&none);
    let (status, _) = get_with_cookie(&app, "/documents", &cookie).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the empty set must not open /documents"
    );
    let (status, _) = get_fragment_with_cookie(&app, "/web/documents", &cookie).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the empty set must not open /web/documents"
    );
}

/// The fragment is a fragment: the browser's filter form swaps it into the
/// list region, so it must never carry the page shell.
#[tokio::test]
async fn documents_fragment_is_a_fragment_not_a_page() {
    let (app, pool) = test_app().await;
    let sales = test_support::seed_session_with_permissions(&pool, &["sales.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&sales);
    let (status, fragment) = get_fragment_with_cookie(&app, "/web/documents", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{fragment:.400}");
    assert!(
        !fragment.contains("<html"),
        "the fragment must not render the shell: {fragment:.400}"
    );
}

/// The filters narrow: type, actor, number search and the inclusive date
/// range each work alone; a filter matching nothing is the empty state, never
/// an error; and the full page honours the same filters, so a filtered view is
/// bookmarkable. The session here holds all four codes (the shared fixture),
/// plus a second actor holding the same four, so the user filter has two
/// actors to tell apart.
#[tokio::test]
async fn documents_filters_narrow_by_group_user_text_and_date() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "DOC-F", "1", "50").await;
    record_stock_via_web(&app, product, "10").await;

    let ana = seed_customer(&pool, "DocFiltAna", None, None).await;
    let beto = seed_customer(&pool, "DocFiltBeto", None, None).await;

    // Two actors: the shared fixture session registers the May sale; a probe
    // principal holding the same four codes registers the July one. The Cash
    // method needs an account allowlist before a cash sale confirms.
    let cash = method_id(&pool, "Cash").await;
    let _wallet = create_account_via_web(&app, &pool, "DocFiltWallet", &[cash]).await;
    let may_sale = create_sale_draft_on_date(&app, ana, "Cash", "2024-05-02", "").await;
    add_sale_line_via_web(&app, may_sale, product, "1").await;
    confirm_sale_via_web(&app, may_sale, Some(cash)).await;
    let may_number = sale_detail(&app, may_sale).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();

    let probe = test_support::seed_session_with_permissions(
        &pool,
        &[
            "sales.read",
            "sales.create",
            "purchases.read",
            "inventory.read",
            "customers.read",
        ],
    )
    .await
    .unwrap();
    let probe_cookie = test_support::cookie_for(&probe);
    let july_sale = create_sale_draft_on_date_as(&app, &probe_cookie, beto, "2024-07-15").await;
    add_sale_line_via_web(&app, july_sale, product, "1").await;
    confirm_sale_via_web(&app, july_sale, Some(cash)).await;
    let july_number = sale_detail(&app, july_sale).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();

    let supplier = create_supplier_via_web(&app, &pool, "DocFiltSupplier").await;
    let purchase = create_purchase_draft_on_date(&app, supplier, "2024-05-10").await;
    add_purchase_line_via_web(&app, purchase, product, "1").await;
    confirm_purchase_via_web(&app, purchase).await;
    let purchase_number = purchase_detail(&app, purchase).await["purchase"]["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();

    // Type alone: the purchases option shows the purchase and neither sale.
    let purchases_only = document_list_html(&app, "?group=purchases").await;
    assert!(
        purchases_only.contains(&purchase_number),
        "{purchases_only:.600}"
    );
    assert!(
        !purchases_only.contains(&may_number),
        "{purchases_only:.600}"
    );
    assert!(
        !purchases_only.contains(&july_number),
        "{purchases_only:.600}"
    );

    // Number: a fragment the operator remembers finds its document; combined
    // with the type it excludes the other families' numbers too.
    let may_tail = &may_number[may_number.len() - 6..];
    let by_number = document_list_html(&app, &format!("?group=sales&q={may_tail}")).await;
    assert!(by_number.contains(&may_number), "{by_number:.600}");
    assert!(!by_number.contains(&july_number), "{by_number:.600}");
    assert!(!by_number.contains(&purchase_number), "{by_number:.600}");
    let july_tail = &july_number[july_number.len() - 6..];
    let other_number = document_list_html(&app, &format!("?group=sales&q={july_tail}")).await;
    assert!(other_number.contains(&july_number), "{other_number:.600}");
    assert!(!other_number.contains(&may_number), "{other_number:.600}");

    // User: the seeder's display name keeps that actor's documents and drops
    // the other actor's. The assertions read the SALE rows specifically: a
    // confirm also writes a stock movement that carries the sale number as its
    // reference, so a naive substring check would see the number on the
    // movement row another actor legitimately owns.
    let by_user = document_list_html(&app, "?user=Test%20Admin").await;
    assert!(row_having(&by_user, "sale", &may_number), "{by_user:.600}");
    assert!(
        !row_having(&by_user, "sale", &july_number),
        "{by_user:.600}"
    );
    let by_probe_user = document_list_html(&app, "?user=Test%20Probe").await;
    assert!(
        row_having(&by_probe_user, "sale", &july_number),
        "{by_probe_user:.600}"
    );
    assert!(
        !row_having(&by_probe_user, "sale", &may_number),
        "{by_probe_user:.600}"
    );

    // Dates bound inclusively on both ends.
    let july = document_list_html(&app, "?from=2024-07-01&to=2024-07-31").await;
    assert!(row_having(&july, "sale", &july_number), "{july:.600}");
    assert!(!row_having(&july, "sale", &may_number), "{july:.600}");
    assert!(!july.contains(&purchase_number), "{july:.600}");
    let inclusive = document_list_html(&app, "?from=2024-07-15&to=2024-07-15").await;
    assert!(
        row_having(&inclusive, "sale", &july_number),
        "{inclusive:.600}"
    );
    assert!(
        !row_having(&inclusive, "sale", &may_number),
        "{inclusive:.600}"
    );

    // A filter matching nothing is the empty state, never an error.
    let none = document_list_html(&app, "?user=NoSuchOperator").await;
    assert!(none.contains("Nothing here yet."), "{none:.600}");
    assert!(!none.contains(&may_number), "{none:.600}");

    // The full page honours the same filters, so a filtered view is
    // bookmarkable and re-opens exactly as shared.
    let (status, page) = get(&app, "/documents?group=purchases").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains(&purchase_number), "{page:.600}");
    assert!(!page.contains(&may_number), "{page:.600}");
    let (status, page) = get(&app, &format!("/documents?user=Test%20Admin&q={may_tail}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains(&may_number), "{page:.600}");
    assert!(!page.contains(&july_number), "{page:.600}");
}

/// The cap is disclosed: a feed that hits `DOCUMENTS_PAGE_LIMIT` says so, and
/// a narrower date range that selects fewer rows does not claim truncation.
/// The rows are inserted with direct SQL inside one transaction — 205 form
/// posts would dominate the suite's runtime for what is a row-count property.
#[tokio::test]
async fn documents_page_limit_is_disclosed_when_the_feed_is_cut() {
    use crate::models::DOCUMENTS_PAGE_LIMIT;
    let (app, pool) = test_app().await;
    let actor = test_support::audit_actor_id(&pool).await.unwrap();
    let walkin: i64 = sqlx::query_scalar("SELECT id FROM customers WHERE is_walkin = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for _ in 0..(DOCUMENTS_PAGE_LIMIT + 5) {
        sqlx::query(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, \
             sale_date, created_by) \
             VALUES ('Confirmed', 'Cash', ?, 'CapFill', '2024-09-01', ?)",
        )
        .bind(walkin)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let sales = test_support::seed_session_with_permissions(&pool, &["sales.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&sales);
    let (status, fragment) =
        get_fragment_with_cookie(&app, "/web/documents?group=sales", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{fragment:.400}");
    assert!(
        fragment.contains(r#"data-document-truncated="true""#),
        "a feed cut at {DOCUMENTS_PAGE_LIMIT} rows must say so: {fragment:.600}"
    );

    let (status, narrower) = get_fragment_with_cookie(
        &app,
        "/web/documents?group=sales&from=2024-01-01&to=2024-01-31",
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{narrower:.400}");
    assert!(
        !narrower.contains(r#"data-document-truncated="true""#),
        "a date range that selects fewer rows must not claim truncation: {narrower:.600}"
    );
}

// ---------------------------------------------------------------------------
// The /documents drawer: GET /web/documents/detail/{kind}/{id}. Every family
// below is seeded through the same web-flow helpers the neighbouring tests
// use; nothing is inserted by hand except the two ids the routes do not
// return (the movement's and the receipt's), read back from the pool.
// ---------------------------------------------------------------------------

/// One fixture with every family in it: a confirmed credit sale with a
/// payment, a completed receipt collecting it, a stock movement, and a
/// confirmed cash purchase (its confirm creates the purchase payment).
struct DrawerFixture {
    sale: i64,
    sale_payment: i64,
    sale_number: String,
    receipt: i64,
    movement: i64,
    purchase: i64,
    purchase_payment: i64,
    purchase_number: String,
    product: i64,
    account: i64,
    /// An unconfirmed sale: the family's destructive delete control (a bare
    /// `hx-delete` button, not a form) is only rendered for drafts.
    draft_sale: i64,
}

async fn seed_drawer_fixture(app: &Router, pool: &SqlitePool) -> DrawerFixture {
    let cash = method_id(pool, "Cash").await;
    let account = create_account_via_web(app, pool, "DrawerWallet", &[cash]).await;
    let product = create_product_via_web(app, pool, "DRAWER-P", "1", "50").await;
    record_stock_via_web(app, product, "10").await;
    let buyer = seed_customer(pool, "DrawerBuyer", None, None).await;

    // Confirmed credit sale + its payment (the SALE-PAYMENTS family).
    let sale = create_sale_draft_on_date(app, buyer, "Credit", "2024-05-02", "2024-06-30").await;
    add_sale_line_via_web(app, sale, product, "2").await;
    confirm_sale_via_web(app, sale, None).await;
    let sale_number = sale_detail(app, sale).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();
    let (status, resp) = pay_sale_via_web(app, sale, cash, "10").await;
    assert_eq!(status, StatusCode::OK, "pay sale: {resp}");
    let payments = sale_detail(app, sale).await["payments"]
        .as_array()
        .cloned()
        .unwrap();
    let sale_payment = payments[0]["id"].as_i64().expect("sale payment id");

    // The receipt that collects the payment (the RECEIPTS family).
    let (status, resp) = post_form(
        app,
        "/web/customer-receipts",
        &format!("customer_id={buyer}&method_id={cash}&amount=10&date=2024-05-11"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed receipt: {resp}");
    let receipt: i64 = sqlx::query_scalar(
        "SELECT id FROM customer_receipts WHERE customer_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(buyer)
    .fetch_one(pool)
    .await
    .unwrap();

    // One explicit stock movement (the STOCK family).
    let (status, resp) = post_form(
        app,
        "/web/stock-movements",
        &format!(
            "product_id={product}&type=In&qty=5&reason=Adjust&reference=drawer-fix&date=2024-05-12"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed movement: {resp}");
    let movement: i64 = sqlx::query_scalar(
        "SELECT id FROM stock_movements WHERE reference = 'drawer-fix' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap();

    // Confirmed cash purchase: its confirm writes the purchase payment.
    let supplier = create_supplier_via_web(app, pool, "DrawerSupplier").await;
    let purchase = create_purchase_draft_on_date(app, supplier, "2024-05-14").await;
    add_purchase_line_via_web(app, purchase, product, "1").await;
    // A supplier payment after the credit confirm: the purchase payment this
    // test reads back.
    let (status, resp) = post_form(
        app,
        "/web/purchases/confirm",
        &format!("purchase_id={purchase}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm purchase: {resp}");
    let (status, resp) = post_form(
        app,
        &format!("/web/purchases/{purchase}/payments"),
        &format!("method_id={cash}&amount=5&date=2024-05-15"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pay purchase: {resp}");
    let detail = purchase_detail(app, purchase).await;
    let purchase_number = detail["purchase"]["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();
    let purchase_payment = detail["payments"][0]["id"]
        .as_i64()
        .expect("purchase payment id");

    // One unconfirmed sale: the drawer's delete control is only rendered for
    // drafts, so the error-handler label assertion needs one of its own.
    let draft_sale =
        create_sale_draft_on_date(app, buyer, "Credit", "2024-05-13", "2024-07-13").await;

    DrawerFixture {
        sale,
        sale_payment,
        sale_number,
        receipt,
        movement,
        purchase,
        purchase_payment,
        purchase_number,
        product,
        account,
        draft_sale,
    }
}

/// The drawer renders for EVERY family with its decisive facts, an unknown
/// kind token is a 404 and so is a known family with no such document.
#[tokio::test]
async fn documents_drawer_renders_every_family_with_its_decisive_facts() {
    let (app, pool) = test_app().await;
    let f = seed_drawer_fixture(&app, &pool).await;

    // Sale: identifier, customer, its line (product name and quantity),
    // totals, actors and the link to the owning page.
    let (status, body) = get(&app, &format!("/web/documents/detail/sale/{}", f.sale)).await;
    assert_eq!(status, StatusCode::OK, "sale drawer: {body:.400}");
    assert!(body.contains(&f.sale_number), "{body:.800}");
    assert!(body.contains("DrawerBuyer"), "{body:.800}");
    assert!(body.contains("product DRAWER-P"), "{body:.800}");
    // The line's quantity cell: the drawer's line table renders the Cant.
    // the sale line carries — there is no SKU column any more, and the
    // product's name alone does not prove a line row rendered.
    assert!(body.contains(">2</td>"), "{body:.800}");
    assert!(body.contains("Registered by"), "{body:.800}");
    assert!(body.contains("Test Admin"), "{body:.800}");
    assert!(body.contains(&format!("/sales/{}", f.sale)), "{body:.800}");
    // The confirmed sale's annul form carries the id the cancel endpoint
    // reads from the body: a broken hidden field would POST an invalid id.
    assert!(
        body.contains(&format!(r#"name="sale_id" value="{}""#, f.sale)),
        "the annul form must carry the document's id: {body:.800}"
    );

    // Draft sale: the destructive delete control is a bare `<button
    // hx-delete ...>` and carries the server-rendered `data-action` label the
    // page's global error handler reads from the element itself when the
    // request fails (a button has no form for `closest('form[data-action]')`
    // to find). The JS announce behaviour itself is covered by reading the
    // handler, not by a browser test.
    let (status, body) = get(
        &app,
        &format!("/web/documents/detail/sale/{}", f.draft_sale),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "draft sale drawer: {body:.400}");
    assert!(
        body.contains(r#"data-action="Delete draft""#),
        "the draft delete button must render the error-handler action label: {body:.800}"
    );
    assert!(
        body.contains(&format!(r#"hx-delete="/web/sales/{}""#, f.draft_sale)),
        "the draft delete must be the hx-delete button for the draft's own path: {body:.800}"
    );
    assert_eq!(
        body.matches("Delete draft").count(),
        3, // impact-preview header + button text + data-action attribute
        "the draft drawer must name the delete action exactly three times (label twice + data-action):\n{body}"
    );

    // Sale payment: the payment's own amount, account, method and ledger
    // transaction, plus the parent sale's summary as a sub-block.
    let (status, body) = get(
        &app,
        &format!("/web/documents/detail/sale_payment/{}", f.sale_payment),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "sale payment drawer: {body:.400}");
    assert!(body.contains("10"), "{body:.800}");
    assert!(body.contains("DrawerWallet"), "{body:.800}");
    assert!(body.contains("Cash"), "{body:.800}");
    assert!(body.contains(&f.sale_number), "{body:.800}");
    assert!(body.contains(&format!("/sales/{}", f.sale)), "{body:.800}");
    assert!(body.contains("/accounts/"), "{body:.800}");

    // Purchase: supplier name and the link to the owning page.
    let (status, body) = get(
        &app,
        &format!("/web/documents/detail/purchase/{}", f.purchase),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "purchase drawer: {body:.400}");
    assert!(body.contains(&f.purchase_number), "{body:.800}");
    assert!(body.contains("DrawerSupplier"), "{body:.800}");
    assert!(
        body.contains(&format!("/purchases/{}", f.purchase)),
        "{body:.800}"
    );
    // The purchase twin: the annul form's hidden field carries the id too.
    assert!(
        body.contains(&format!(r#"name="purchase_id" value="{}""#, f.purchase)),
        "the purchase annul form must carry the document's id: {body:.800}"
    );

    // Purchase payment: amount and parent purchase summary.
    let (status, body) = get(
        &app,
        &format!(
            "/web/documents/detail/purchase_payment/{}",
            f.purchase_payment
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "purchase payment drawer: {body:.400}"
    );
    assert!(body.contains(&f.purchase_number), "{body:.800}");
    assert!(body.contains("DrawerWallet"), "{body:.800}");
    assert!(
        body.contains(&format!("/purchases/{}", f.purchase)),
        "{body:.800}"
    );

    // Stock movement: product, reason, the product's current derived stock,
    // and the append-only sentence the action slice relies on.
    let (status, body) = get(
        &app,
        &format!("/web/documents/detail/stock_movement/{}", f.movement),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "movement drawer: {body:.400}");
    assert!(body.contains("product DRAWER-P"), "{body:.800}");
    assert!(body.contains(">Stock</span>"), "{body:.800}");
    assert!(body.contains("append-only"), "{body:.800}");
    assert!(
        body.contains(&format!("/products#product-{}", f.product)),
        "{body:.800}"
    );

    // Receipt: customer, account, total and the allocations table with the
    // sale number it applied.
    let (status, body) = get(
        &app,
        &format!("/web/documents/detail/receipt/{}", f.receipt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "receipt drawer: {body:.400}");
    assert!(body.contains("DrawerBuyer"), "{body:.800}");
    assert!(body.contains("DrawerWallet"), "{body:.800}");
    assert!(body.contains(&f.sale_number), "{body:.800}");
    assert!(body.contains(&format!("/customers/")), "{body:.800}");
    assert!(body.contains(&format!("/sales/{}", f.sale)), "{body:.800}");

    // An unknown kind token is a 404 naming the token, not a panic or a
    // silent empty fragment.
    let (status, body) = get(&app, "/web/documents/detail/nonsense/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown kind: {body:.400}");
    assert!(body.contains("nonsense"), "{body:.400}");

    // A known family with an unknown id is a 404 naming the family.
    let (status, body) = get(&app, "/web/documents/detail/sale/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown sale: {body:.400}");
}

/// Per-family narrowing: the drawer obeys the same rule the rows obey — a
/// principal never opens another tier's document. A `sales.read`-only
/// principal gets the sale and sale-payment drawers but a 403 for the
/// purchase, purchase-payment, movement and receipt drawers; a
/// `customers.read`-only principal gets only the receipt drawer.
#[tokio::test]
async fn documents_drawer_refuses_families_the_principal_cannot_read() {
    let (app, pool) = test_app().await;
    let f = seed_drawer_fixture(&app, &pool).await;

    let sales = test_support::seed_session_with_permissions(&pool, &["sales.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&sales);
    for (kind, id, expected) in [
        ("sale", f.sale, StatusCode::OK),
        ("sale_payment", f.sale_payment, StatusCode::OK),
        ("purchase", f.purchase, StatusCode::FORBIDDEN),
        (
            "purchase_payment",
            f.purchase_payment,
            StatusCode::FORBIDDEN,
        ),
        ("stock_movement", f.movement, StatusCode::FORBIDDEN),
        ("receipt", f.receipt, StatusCode::FORBIDDEN),
    ] {
        let (status, body) =
            get_fragment_with_cookie(&app, &format!("/web/documents/detail/{kind}/{id}"), &cookie)
                .await;
        assert_eq!(status, expected, "{kind} as sales.read: {body:.400}");
        if expected == StatusCode::FORBIDDEN {
            assert!(
                body.contains("Se necesita el permiso"),
                "{kind} refusal names the code: {body:.400}"
            );
        }
    }

    let customers = test_support::seed_session_with_permissions(&pool, &["customers.read"])
        .await
        .unwrap();
    let cookie = test_support::cookie_for(&customers);
    for (kind, id, expected) in [
        ("sale", f.sale, StatusCode::FORBIDDEN),
        ("sale_payment", f.sale_payment, StatusCode::FORBIDDEN),
        ("purchase", f.purchase, StatusCode::FORBIDDEN),
        (
            "purchase_payment",
            f.purchase_payment,
            StatusCode::FORBIDDEN,
        ),
        ("stock_movement", f.movement, StatusCode::FORBIDDEN),
        ("receipt", f.receipt, StatusCode::OK),
    ] {
        let (status, body) =
            get_fragment_with_cookie(&app, &format!("/web/documents/detail/{kind}/{id}"), &cookie)
                .await;
        assert_eq!(status, expected, "{kind} as customers.read: {body:.400}");
    }
}

/// The row of exactly one document family that carries `needle` in its markup
/// (reference, party, date line or pill), reading the rendered rows by their
/// `data-document-kind` marker so a number quoted on a DIFFERENT family's row
/// (a stock movement references the sale that produced it) never confuses the
/// assertion.
fn row_having(html: &str, kind: &str, needle: &str) -> bool {
    html.split(r#"data-document-kind=""#)
        .skip(1)
        .any(|chunk| chunk.split('"').next().unwrap_or("") == kind && chunk.contains(needle))
}

/// Create a sale draft as the principal the cookie carries, on an explicit
/// date, through the same web form endpoint the browser uses; returns the id
/// (the last sale for that customer, as the other draft helpers resolve it).
async fn create_sale_draft_on_date_as(
    app: &Router,
    cookie: &str,
    customer_id: i64,
    sale_date: &str,
) -> i64 {
    let body = format!("customer_id={customer_id}&payment_type=Cash&sale_date={sale_date}");
    let (status, resp) = post_form_with_cookie(app, "/web/sales", &body, cookie).await;
    assert_eq!(status, StatusCode::OK, "create sale as probe: {resp}");
    let (status, body) = get(app, "/api/sales").await;
    assert_eq!(status, StatusCode::OK, "list sales: {body}");
    let v = json_body(&body);
    v["sales"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["sale"]["customer_id"] == json!(customer_id))
        .last()
        .and_then(|d| d["sale"]["id"].as_i64())
        .unwrap_or_else(|| panic!("sale for customer {customer_id} not found: {v}"))
}

/// Helper: a user's id by username, through the real table.
async fn user_id_by_username_smoke(pool: &SqlitePool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = ? COLLATE NOCASE")
        .bind(username)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Helper: the id of the latest `seed_session_with_permissions` probe user
/// (the display name is fixed at "Test Probe").
async fn probe_id_by_suffix(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar(
        "SELECT id FROM users WHERE display_name = 'Test Probe' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Run migrations up to 33 only: the pre-slice-13 state the upgrade test
/// starts from (sqlx's migrator stops at the version, exactly like the S12
/// upgrade fixture did).
async fn run_migrations_up_to_33(pool: &SqlitePool) {
    sqlx::migrate!("./migrations")
        .run_to(20240101000033, pool)
        .await
        .unwrap();
}

// The /documents drawer actions: the two end-to-end flows the action block
// offers. Everything runs through the real web endpoints, the way the
// operator's browser would.

/// Delete a draft from the drawer's route: the feed stops listing it and the
/// database holds neither the sale nor its lines afterwards.
#[tokio::test]
async fn documents_drawer_delete_removes_a_draft_and_the_feed_stops_listing_it() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "DRAW-D", "1", "50").await;
    let buyer = seed_customer(&pool, "Drawer Delete Buyer", None, None).await;
    let sale = create_sale_draft_on_date(&app, buyer, "Credit", "2024-05-02", "2024-06-30").await;
    add_sale_line_via_web(&app, sale, product, "2").await;

    // The draft is in the feed before the delete.
    let (_, feed) = get(&app, "/web/documents").await;
    assert!(
        feed.contains(&format!("Draft #{sale}")),
        "the draft must be listed before the delete: {feed:.600}"
    );

    // DELETE through the drawer's target, with the header the page listens for.
    let req = test_support::with_cookie(
        Request::builder()
            .method("DELETE")
            .uri(format!("/web/sales/{sale}")),
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .map(|v| v.to_str().unwrap().to_string());
    assert_eq!(
        trigger.as_deref(),
        Some("sale-changed"),
        "the delete must tell the page to re-read the feed"
    );

    // The feed no longer lists it and the tables hold nothing for the id.
    let (_, feed) = get(&app, "/web/documents").await;
    assert!(
        !feed.contains(&format!("Draft #{sale}")),
        "the deleted draft must leave the feed: {feed:.600}"
    );
    let (sales,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sales WHERE id = ?")
        .bind(sale)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (lines,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_lines WHERE sale_id = ?")
        .bind(sale)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sales, 0, "the sale row must be gone");
    assert_eq!(lines, 0, "the lines must be gone with the draft");
}

/// The annulment flow: a confirmed sale is annulled through the EXISTING
/// cancel collection endpoint with a reason, the answer triggers
/// `sale-changed`, and the designed inverse is observable — the sale is
/// Cancelled, a refund Expense exists on the paying account and one
/// `In · Sale-return` movement restores the stock.
#[tokio::test]
async fn documents_drawer_annul_flows_through_the_existing_cancel_endpoint() {
    let (app, pool) = test_app().await;
    let product = create_product_via_web(&app, &pool, "DRAW-A", "1", "50").await;
    record_stock_via_web(&app, product, "10").await;
    let cash = method_id(&pool, "Cash").await;
    // The migration's Cash method starts unassigned: creating the wallet with
    // `method_ids` assigns it, exactly how the neighbouring flows seed it.
    let _wallet = create_account_via_web(&app, &pool, "DrawerAnnulWallet", &[cash]).await;
    let buyer = seed_customer(&pool, "Drawer Annul Buyer", None, None).await;
    let sale = create_sale_draft_on_date(&app, buyer, "Credit", "2024-05-02", "2024-06-30").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;
    let (status, resp) = pay_sale_via_web(&app, sale, cash, "5").await;
    assert_eq!(status, StatusCode::OK, "seed payment: {resp}");
    let sale_number = sale_detail(&app, sale).await["sale"]["sale_number"]
        .as_str()
        .expect("confirmed sale number")
        .to_string();

    // POST the cancel collection endpoint with a reason, exactly as the
    // drawer's action form does.
    let body = format!("sale_id={sale}&reason=annulled from the drawer");
    let req = test_support::with_cookie(
        Request::builder()
            .method("POST")
            .uri("/web/sales/cancel")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true"),
    )
    .body(Body::from(body))
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trigger = resp
        .headers()
        .get("HX-Trigger")
        .map(|v| v.to_str().unwrap().to_string());
    assert_eq!(trigger.as_deref(), Some("sale-changed"));

    // The document is annulled.
    let detail = sale_detail(&app, sale).await;
    assert_eq!(
        detail["sale"]["status"].as_str(),
        Some("Cancelled"),
        "{}",
        detail
    );

    // One refund Expense on the paying account.
    let (account,): (i64,) = sqlx::query_as("SELECT account_id FROM payment_methods WHERE id = ?")
        .bind(cash)
        .fetch_one(&pool)
        .await
        .unwrap();
    let refunds: Vec<Value> = transactions_for(&app, account)
        .await
        .into_iter()
        .filter(|t| t["kind"].as_str() == Some("Expense"))
        .collect();
    assert!(
        refunds
            .iter()
            .any(|t| t["amount"].as_str() == Some("5") || t["amount"].as_f64() == Some(5.0)),
        "one Expense refund of 5 must exist: {refunds:?}"
    );

    // One In · Sale-return movement for the tracked line.
    let (movements,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM stock_movements \
         WHERE product_id = ? AND type = 'In' AND reason = 'Sale-return' AND reference = ?",
    )
    .bind(product)
    .bind(&sale_number)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        movements, 1,
        "the tracked line's return movement must exist"
    );
}

// ---------------------------------------------------------------------------
// T4: customer/supplier due-day wiring
// ---------------------------------------------------------------------------

#[tokio::test]
async fn due_days_customer_web_and_api_create_edit_round_trip() {
    let (app, pool) = test_app().await;

    let (status, body) = post_form(&app, "/web/customers", "name=Term+Web&due_days=15").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create customer through web: {body}"
    );
    let web_customer = customer_id_by_name(&pool, "Term Web").await;
    let (status, edit_form) = get(&app, &format!("/web/customers/edit-form/{web_customer}")).await;
    assert_eq!(status, StatusCode::OK, "{edit_form}");
    assert!(
        element_tag_containing(&edit_form, "name=\"due_days\"").contains("value=\"15\""),
        "the edit form must show the stored term: {edit_form}"
    );
    let (status, body) = post_form(
        &app,
        "/web/customers/edit",
        &format!("customer_id={web_customer}&name=Term+Web&due_days=20"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "edit customer through web: {body}");

    let api_customer = json!({
        "name": "Term API",
        "phone": null,
        "address": null,
        "tax_id": null,
        "notes": null,
        "is_walkin": false,
        "credit_limit": null,
        "due_days": 30
    });
    let (status, body) = post_json(&app, "/api/customers", api_customer).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create customer through API: {body}"
    );
    let api_customer_id = json_body(&body)["customer"]["id"].as_i64().unwrap();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/customers/{api_customer_id}"),
        Some("application/json"),
        false,
        json!({ "due_days": 45 }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "edit customer through API: {body}");

    let (status, body) = get(&app, "/api/customers").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let customers = json_body(&body);
    let web_due = customers["customers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|customer| customer["customer"]["id"] == json!(web_customer))
        .unwrap()["customer"]["due_days"]
        .as_i64();
    let api_due = customers["customers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|customer| customer["customer"]["id"] == json!(api_customer_id))
        .unwrap()["customer"]["due_days"]
        .as_i64();
    assert_eq!(web_due, Some(20));
    assert_eq!(api_due, Some(45));
}

#[tokio::test]
async fn due_days_supplier_web_and_api_create_edit_round_trip() {
    let (app, pool) = test_app().await;

    let (status, body) = post_form(&app, "/web/suppliers", "name=Term+Sup&due_days=15").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create supplier through web: {body}"
    );
    let web_supplier = supplier_id_by_name(&pool, "Term Sup").await;
    let (status, edit_form) = get(&app, &format!("/web/suppliers/{web_supplier}/edit-form")).await;
    assert_eq!(status, StatusCode::OK, "{edit_form}");
    assert!(
        element_tag_containing(&edit_form, "name=\"due_days\"").contains("value=\"15\""),
        "the edit form must show the stored term: {edit_form}"
    );
    let (status, body) = post_form(
        &app,
        "/web/suppliers/edit",
        &format!("id={web_supplier}&name=Term+Sup&due_days=20"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "edit supplier through web: {body}");

    let (status, body) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Term API Sup", "due_days": 30 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create supplier through API: {body}"
    );
    let api_supplier = json_body(&body);
    let api_supplier_id = api_supplier["id"].as_i64().unwrap();
    assert_eq!(api_supplier["due_days"].as_i64(), Some(30));
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/suppliers/{api_supplier_id}"),
        Some("application/json"),
        false,
        json!({ "due_days": 45 }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "edit supplier through API: {body}");
    assert_eq!(json_body(&body)["due_days"].as_i64(), Some(45));

    let web_due: (Option<i64>,) = sqlx::query_as("SELECT due_days FROM suppliers WHERE id = ?")
        .bind(web_supplier)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(web_due, (Some(20),));
}

#[tokio::test]
async fn due_days_customer_credit_sale_defaults_and_explicit_date_wins() {
    let (app, pool) = test_app().await;
    let (status, body) = post_form(&app, "/web/customers", "name=Net+15&due_days=15").await;
    assert_eq!(status, StatusCode::OK, "create customer: {body}");
    let customer = customer_id_by_name(&pool, "Net 15").await;

    let defaulted = create_sale_draft_on_date(&app, customer, "Credit", "2024-05-02", "").await;
    assert_eq!(
        sale_detail(&app, defaulted).await["sale"]["due_date"],
        json!("2024-05-17")
    );

    let manual =
        create_sale_draft_on_date(&app, customer, "Credit", "2024-05-02", "2024-06-20").await;
    assert_eq!(
        sale_detail(&app, manual).await["sale"]["due_date"],
        json!("2024-06-20")
    );
}

#[tokio::test]
async fn due_days_supplier_credit_confirm_prefills_without_storing_and_manual_wins() {
    let (app, pool) = test_app().await;
    let (status, body) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Net 30 Supplier", "due_days": 30 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let supplier = json_body(&body)["id"].as_i64().unwrap();
    let product = create_product_via_web(&app, &pool, "DUE-P", "1", "50").await;
    let purchase = create_purchase_draft_with_due(&app, supplier, "Cash", "2024-05-02", "").await;
    add_purchase_line_via_web(&app, purchase, product, "1").await;

    let (status, page) = get(&app, &format!("/purchases/{purchase}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let due_input = element_tag_containing(&page, "id=\"confirm-due-date\"");
    assert!(
        due_input.contains("value=\"2024-06-01\""),
        "the confirm field must visibly prefill purchase_date + supplier.due_days: {due_input}"
    );
    let stored_before: (Option<String>,) =
        sqlx::query_as("SELECT due_date FROM purchases WHERE id = ?")
            .bind(purchase)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored_before,
        (None,),
        "prefill must not write a document fact"
    );

    let (status, body) = post_form(
        &app,
        &format!("/web/purchases/{purchase}/confirm"),
        "payment_type=Credit&due_date=2024-06-15",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm Credit purchase: {body}");
    assert_eq!(
        purchase_detail(&app, purchase).await["purchase"]["due_date"],
        json!("2024-06-15")
    );
}

#[tokio::test]
async fn due_days_supplier_cash_confirm_remains_without_a_due_date() {
    let (app, pool) = test_app().await;
    let (status, body) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Immediate Supplier", "due_days": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let supplier = json_body(&body)["id"].as_i64().unwrap();
    let product = create_product_via_web(&app, &pool, "DUE-CASH", "1", "50").await;
    let purchase = create_purchase_draft_with_due(&app, supplier, "Cash", "2024-05-02", "").await;
    add_purchase_line_via_web(&app, purchase, product, "1").await;
    let (status, page) = get(&app, &format!("/purchases/{purchase}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let due_input = element_tag_containing(&page, "id=\"confirm-due-date\"");
    assert!(due_input.contains("value=\"2024-05-02\""));
    assert!(
        due_input.contains("disabled"),
        "Cash must not submit the visible Credit suggestion: {due_input}"
    );

    let cash = method_id(&pool, "Cash").await;
    let account = create_account_via_web(&app, &pool, "Due Cash Account", &[cash]).await;
    let (status, body) = post_form(
        &app,
        "/web/transactions",
        &format!(
            "account_id={account}&type=Income&amount=1000&description=fund+due-days+cash&date=2024-05-01"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fund Cash account: {body}");

    let (status, body) = post_form(
        &app,
        &format!("/web/purchases/{purchase}/confirm"),
        &format!("payment_type=Cash&method_id={cash}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm Cash purchase: {body}");
    let detail = purchase_detail(&app, purchase).await;
    assert_eq!(detail["purchase"]["payment_type"], json!("Cash"));
    assert_eq!(detail["purchase"]["due_date"], json!(null));
}

#[tokio::test]
async fn due_days_negative_terms_are_refused_on_create_and_edit() {
    let (app, pool) = test_app().await;

    let (status, _) = post_form(&app, "/web/customers", "name=Bad+Web&due_days=-1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let customer_body = json!({
        "name": "Valid API Customer",
        "phone": null,
        "address": null,
        "tax_id": null,
        "notes": null,
        "is_walkin": false,
        "credit_limit": null,
        "due_days": 5
    });
    let (status, body) = post_json(&app, "/api/customers", customer_body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let customer_id = json_body(&body)["customer"]["id"].as_i64().unwrap();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/customers/{customer_id}"),
        Some("application/json"),
        false,
        json!({ "due_days": -1 }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_form(&app, "/web/customers", "name=Valid+Web&due_days=5").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let web_customer = customer_id_by_name(&pool, "Valid Web").await;
    let (status, body) = post_form(
        &app,
        "/web/customers/edit",
        &format!("customer_id={web_customer}&name=Valid+Web&due_days=-1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = post_form(&app, "/web/suppliers", "name=Bad+Sup&due_days=-1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_json(
        &app,
        "/api/suppliers",
        json!({ "name": "Valid API Sup", "due_days": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let supplier_id = json_body(&body)["id"].as_i64().unwrap();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/api/suppliers/{supplier_id}"),
        Some("application/json"),
        false,
        json!({ "due_days": -1 }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_form(&app, "/web/suppliers", "name=Valid+Web+Sup&due_days=5").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let web_supplier = supplier_id_by_name(&pool, "Valid Web Sup").await;
    let (status, body) = post_form(
        &app,
        "/web/suppliers/edit",
        &format!("id={web_supplier}&name=Valid+Web+Sup&due_days=-1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
