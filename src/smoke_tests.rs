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

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    send(app, "GET", uri, None, false, String::new()).await
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
    send(app, "POST", uri, Some("application/json"), false, body.to_string()).await
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
    let row: (i64,) = sqlx::query_as(
        "SELECT id FROM payment_methods WHERE account_id = ? AND name = ?",
    )
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
    let body =
        format!("product_id={product_id}&type=In&qty={qty}&reason=Initial&date=2024-05-01");
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
    payment_days: Option<i64>,
) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO customers (name, credit_limit, payment_days) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(credit_limit)
    .bind(payment_days)
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

async fn confirm_sale_via_web(
    app: &Router,
    sale_id: i64,
    method_id: Option<i64>,
) {
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
    let body = format!(
        "sale_id={sale_id}&method_id={method_id}&amount={amount}&date=2024-05-10"
    );
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
    assert_eq!(status, StatusCode::OK, "purchase {purchase_id} detail: {body}");
    json_body(&body)
}

async fn stock_of(app: &Router, product_id: i64) -> Decimal {
    let (status, body) = get(app, &format!("/api/products/{product_id}/stock")).await;
    assert_eq!(status, StatusCode::OK, "stock for {product_id}: {body}");
    dec(&json_body(&body)["stock"])
}

async fn transactions_for(app: &Router, account_id: i64) -> Vec<Value> {
    let (status, body) = get(app, &format!("/api/transactions?account_id={account_id}")).await;
    assert_eq!(status, StatusCode::OK, "transactions for {account_id}: {body}");
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
                b' ' | b'\t' | b'\n' | b'\'' | b'"' | b'`' | b'(' | b',' | b'=' | b'{' | b'}' | b':' | b'?' | b';'
            );
        let path_char = bytes
            .get(index + 1)
            .map(|next| {
                next.is_ascii_alphanumeric() || matches!(next, b'_' | b'-' | b'/' | b'.')
            })
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
        check_target_shape(page, &target.attr, &target.target, concrete_ids_are_defects, false)?;
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
        let Some(id) = declared.selector.strip_prefix('#').filter(|id| !id.is_empty()) else {
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
    let (status, body) = send(probe_app, method, target, None, false, String::new()).await;
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
    if targets.is_empty() {
        return Err(format!(
            "{page}: no hx-get/hx-post/hx-put/hx-patch/hx-delete targets rendered; guard would be vacuous"
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
        check_target_shape(page, &target.attr, &target.target, concrete_ids_are_defects, false)?;
        probe_or_fail(
            probe_app,
            page,
            &target.attr,
            &target.method,
            &target.target,
        )
        .await?;
    }

    for form in extract_rendered_forms(html) {
        check_native_form(page, &form, concrete_ids_are_defects)?;
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
        "name=GuardCustomer&phone=555-0100&credit_limit=500&payment_days=30",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed customer: {resp}");
    let customer = customer_id_by_name(pool, "GuardCustomer").await;

    // A confirmed credit sale collected into a receipt, so the customer statement
    // renders the receipt list and the referenced-id rule covers that path too.
    let guard_sale =
        create_sale_draft_for_customer(app, customer, "Credit", "2024-06-02").await;
    add_sale_line_via_web(app, guard_sale, product, "1").await;
    confirm_sale_via_web(app, guard_sale, None).await;
    let (status, resp) = post_form(
        app,
        "/web/customer-receipts",
        &format!(
            "customer_id={customer}&method_id={cash}&amount=10&date=2024-05-10"
        ),
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
            label: "purchases",
            path: "/purchases".to_string(),
            concrete_ids_are_defects: true,
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
            label: "product search fragment",
            path: format!(
                "/web/product-search?q=GUARD-P&price=sale&line_action=/web/sales/{}/lines&line_target=%23sale-record-money",
                fixture.sale
            ),
            concrete_ids_are_defects: false,
            external_selectors: vec![
                ExternalSelector {
                    selector: "#line-picker",
                    host: "sale record page",
                },
                ExternalSelector {
                    selector: "#sale-record-money",
                    host: "sale record page",
                },
            ],
        },
        GuardedPage {
            label: "purchase product search fragment",
            path: format!(
                "/web/product-search?q=GUARD-P&price=cost&line_action=/web/purchases/{}/lines&line_target=%23purchase-record-money",
                fixture.purchase
            ),
            concrete_ids_are_defects: false,
            external_selectors: vec![
                ExternalSelector {
                    selector: "#line-picker",
                    host: "purchase record page",
                },
                ExternalSelector {
                    selector: "#purchase-record-money",
                    host: "purchase record page",
                },
            ],
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
/// `draft #12` and `2024-SALE-000012` allowed.
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
/// its name. The entity nouns are explicit so a document id (`draft #12`) never
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
    let (status, body) = get(&app, &format!("/api/product-supplier-costs?product_id={product}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let costs = json_body(&body);
    let cost = costs["costs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["supplier_id"] == json!(supplier))
        .unwrap_or_else(|| panic!("satellite cost for supplier {supplier}: {costs}"));
    assert_eq!(dec(&cost["current_cost"]), Decimal::from_str("7.50").unwrap());

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
    let sale_number = detail["sale"]["sale_number"]
        .as_str()
        .unwrap()
        .to_string();
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
    let (status, body) = post_form(
        &app,
        "/web/sales",
        "payment_type=Cash&sale_date=2024-05-02",
    )
    .await;
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
        &format!(
            "customer_id={no_term_id}&payment_type=Credit&sale_date=2024-05-02&due_date="
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("due_date"), "actionable message: {body}");

    // AC4: over the limit is a 400 with the projected debt and no side effect.
    let over_id = seed_customer(&pool, "Smoke Over", Some("50"), Some(30)).await;
    let over_sale = create_sale_draft_for_customer(&app, over_id, "Credit", "").await;
    add_sale_line_via_web(&app, over_sale, product, "3").await;
    let (status, body) = post_form(
        &app,
        "/web/sales/confirm",
        &format!("sale_id={over_sale}"),
    )
    .await;
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
    let walkin_sale =
        create_sale_draft_for_customer(&app, walkin_id, "Credit", "2024-06-02").await;
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
            "name=Collect+Buyer&phone=555-0200&credit_limit=500&payment_days=30",
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
            &format!(
                "customer_id={customer}&method_id={cash}&amount=30&date=2024-06-20&notes=part"
            ),
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
        assert!(entries.iter().any(|e| e["kind"] == json!("Payment")
            && dec(&e["credit"]) == Decimal::from(30)));

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
        assert!(html.contains("45"), "the page shows the derived balance: {html:.400}");
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
    assert_eq!(dec(&detail["lines"][0]["unit_cost"]), Decimal::from_str("7.50").unwrap());
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
    assert_eq!(expense["reference"].as_str(), Some(purchase_number.as_str()));

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
    let sale = create_sale_draft_via_web(&app, &pool, "GuardFlowBuyer", "Credit", "2024-06-02").await;
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
    assert_eq!(status, StatusCode::OK, "payment after configuration: {body}");
    assert_eq!(sale_detail(&app, sale).await["payments"].as_array().unwrap().len(), 1);
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
        let original = check_original_transaction(
            pool,
            "sale",
            payment_id,
            transaction_id,
            &sale_number,
        )
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
    for (payment_id, transaction_id, refund_transaction_id, account_id, amount_text, purchase_number) in
        purchase_links
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
    let row: Option<(Option<String>, i64, String, String)> = sqlx::query_as(
        "SELECT reference, account_id, kind, amount FROM transactions WHERE id = ?",
    )
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
    let err = id_free_page_shape(
        r#"<form hx-post="/web/sales/1/confirm"><input name="sale_id"></form>"#,
    )
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
    assert_eq!(status, StatusCode::OK, "{}: {html:.400}", purchases_page.path);

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
    let err = check_same_page_selectors("purchases", &dangling, &[], &RenderedPages::new()).unwrap_err();
    eprintln!("dangling selector rejected: {err}");
    assert!(err.contains("purchases"), "{err}");
    assert!(err.contains("#purchase-detail"), "{err}");

    // Mutation B: remove the panel an existing control targets.
    let (status, sales) = get(&app, "/sales").await;
    assert_eq!(status, StatusCode::OK, "{sales:.400}");
    let removed = sales.replacen("id=\"sale-debt\"", "", 1);
    assert_ne!(removed, sales, "the mutation must remove the targeted panel");
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

    // The shared search fragment is guarded in its purchase context too: it adds
    // against the purchase money region and shows the cost.
    let (status, purchase_search) = get(
        &app,
        &format!(
            "/web/product-search?q=GUARD-P&price=cost&line_action=/web/purchases/{}/lines&line_target=%23purchase-record-money",
            fixture.purchase
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{purchase_search}");
    assert!(
        purchase_search.contains("hx-target=\"#purchase-record-money\""),
        "the purchase results add against the purchase money region: {purchase_search}"
    );
    assert!(
        purchase_search.contains("cost $10"),
        "the purchase results show the cost: {purchase_search}"
    );

    // Without the declared exemption the fragment genuinely fails, so the
    // exemption is not decorative.
    let (_, fragment) = get(&app, &format!("/web/sales/{}", fixture.sale)).await;
    let err = check_same_page_selectors("sale detail fragment", &fragment, &[], &RenderedPages::new()).unwrap_err();
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
    let bogus: (i64,) = sqlx::query_as(
        "INSERT INTO transactions (account_id, kind, amount, description, reference, date) \
         VALUES (?, 'Income', '1', 'bogus', NULL, '2024-05-01') RETURNING id",
    )
    .bind(account)
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
    assert!(err.contains("plain single or double quoted literal"), "{err}");

    let err = check_rendered_wiring_shape(
        "dynamic",
        r#"<script>htmx.ajax('GET',`/web/sales/${id}/confirm`,`#x`);</script>"#,
        false,
    )
    .unwrap_err();
    eprintln!("dynamic template rejected: {err}");
    assert!(err.contains("plain single or double quoted literal"), "{err}");

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
    let sale = create_sale_draft_via_web(&app, &pool, "EqualInvBuyer", "Credit", "2024-06-02").await;
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
    assert_eq!(Decimal::from_str(&tx.1).unwrap(), Decimal::from_str("10").unwrap());
    let detail = sale_detail(&app, sale).await;
    assert_eq!(dec(&detail["paid"]), Decimal::ZERO, "the orphan paid nothing");
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
    let oob_pos = html
        .find("hx-swap-oob=\"true\"")
        .unwrap_or_else(|| panic!("the picker must come back out of band: {html:.800}"));
    let tag_start = html[..oob_pos].rfind('<').unwrap();
    let tag_end = oob_pos + html[oob_pos..].find('>').unwrap();
    let oob_tag = &html[tag_start..=tag_end];
    assert!(oob_tag.contains("id=\"line-picker\""), "{oob_tag}");
    let oob = &html[tag_start..];
    assert!(oob.contains("autofocus"), "the picker must come back focused: {oob:.400}");
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

    // Typing a name, a SKU or a barcode each find the product, through the same
    // search path the field uses.
    let search = format!(
        "/web/product-search?price=sale&line_action={base}/lines&line_target=%23sale-record-money"
    );
    for needle in ["scan", "SCAN-P", "7791234567890"] {
        let (status, fragment) = get(&app, &format!("{search}&q={needle}")).await;
        assert_eq!(status, StatusCode::OK, "{fragment}");
        assert!(fragment.contains("product SCAN-P"), "{needle}: {fragment}");
        assert!(fragment.contains("SCAN-P"), "{needle}: {fragment}");
        assert!(fragment.contains("$25"), "{needle}: price travels: {fragment}");
        assert!(fragment.contains("stock 20"), "{needle}: stock travels: {fragment}");
        // A result is its own add action: it includes the picker form and carries
        // its own product id.
        assert!(
            fragment.contains(&format!("hx-post=\"{base}/lines\"")),
            "{fragment}"
        );
        assert!(fragment.contains("hx-include=\"#line-picker\""), "{fragment}");
        assert!(
            fragment.contains(&format!("hx-vals='{{\"product_id\": {product}}}'")),
            "{fragment}"
        );
    }

    // An empty query returns nothing, not the whole catalogue.
    let (status, empty) = get(&app, "/web/product-search?q=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!empty.contains("SCAN-P"), "{empty}");

    // The record page offers the field, its debounced search, the sibling results
    // container and no catalogue select.
    let (status, page) = get(&app, &format!("/sales/{sale}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains("<select name=\"product_id\""),
        "the catalogue select must be gone: {page:.600}"
    );
    assert!(page.contains("hx-get=\"/web/product-search\""), "{page:.600}");
    assert!(page.contains("id=\"product-search-results\""), "{page:.600}");
    assert!(page.contains("delay:"), "the search must be debounced");
    assert!(page.contains("Escape"), "Escape must clear the field");

    // Scan 1: the reader types the barcode and presses Enter. The form carries the
    // field and the quantity, never a product id.
    let (status, added) = post_form(&app, &format!("{base}/lines"), "product=7791234567890&qty=2&unit_price=").await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("product SCAN-P"), "{added:.600}");
    assert!(added.contains("$50"), "running total after the scan: {added:.800}");
    assert_oob_picker_is_empty_and_focused(&added);

    // Scan 2: the same series, and the picker comes back ready again.
    let (status, added) = post_form(&app, &format!("{base}/lines"), "product=7791234567890&qty=1&unit_price=").await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("$75"), "running total: {added:.800}");
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
    assert!(clicked.contains("$150"), "running total: {clicked:.800}");

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
    assert!(removed.contains("$100"), "running total after removal: {removed:.800}");
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
/// lines, the running total and the out-of-band picker, and the repeated-product
/// rule surfaces as a clear 400 instead of a crash.
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

    // Typing a name, a SKU or a barcode each find the product through the same
    // search path the field uses; the result is its own add action against the
    // purchase line endpoint and carries the current stock.
    let search = format!(
        "/web/product-search?price=cost&line_action={base}/lines&line_target=%23purchase-record-money"
    );
    for needle in ["PSCAN-A", "7791234567891"] {
        let (status, fragment) = get(&app, &format!("{search}&q={needle}")).await;
        assert_eq!(status, StatusCode::OK, "{fragment}");
        assert!(fragment.contains("product PSCAN-A"), "{needle}: {fragment}");
        assert!(
            fragment.contains("stock 20"),
            "{needle}: stock travels: {fragment}"
        );
        assert!(
            fragment.contains(&format!("hx-post=\"{base}/lines\"")),
            "{needle}: {fragment}"
        );
        assert!(fragment.contains("hx-include=\"#line-picker\""), "{fragment}");
        assert!(
            fragment.contains(&format!("hx-vals='{{\"product_id\": {product_a}}}'")),
            "{fragment}"
        );
        assert!(
            fragment.contains("cost $10"),
            "{needle}: the purchase picker must show the cost: {fragment}"
        );
        assert!(
            !fragment.contains("$25"),
            "{needle}: the purchase picker must not show the sale price: {fragment}"
        );
    }

    // The record page offers the field, its debounced search and the sibling
    // results container, and no catalogue select.
    let (status, page) = get(&app, &format!("/purchases/{purchase}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(
        !page.contains("<select name=\"product_id\""),
        "the catalogue select must be gone: {page:.600}"
    );
    assert!(
        page.contains("hx-get=\"/web/product-search\""),
        "{page:.600}"
    );
    assert!(
        page.contains("id=\"purchase-record-money\""),
        "{page:.600}"
    );

    // Scan 1: the reader types the barcode and presses Enter. The form carries the
    // field and the quantity, never a product id. The empty cost falls back to the
    // product cost price (10).
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567891&qty=2&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("product PSCAN-A"), "{added:.600}");
    assert!(
        added.contains("$20"),
        "running total after the scan: {added:.800}"
    );
    assert_oob_picker_is_empty_and_focused(&added);

    // Scan 2: a different product, and the picker comes back ready again.
    let (status, added) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567892&qty=3&unit_cost=",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert!(added.contains("$50"), "running total: {added:.800}");
    assert_oob_picker_is_empty_and_focused(&added);

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
        removed.contains("$30"),
        "running total after removal: {removed:.800}"
    );
    assert!(
        !removed.contains(&format!("id=\"purchase-line-{line_id}\"")),
        "the removed line is gone: {removed:.800}"
    );

    // The repeated-product rule surfaces as a clear 400 with the actionable
    // message, and the picker form names its action so the notice region can say
    // which action failed. Product B is still on the purchase after the removal.
    let before = purchase_detail(&app, purchase).await;
    let (status, repeated) = post_form(
        &app,
        &format!("{base}/lines"),
        "product=7791234567892&qty=1&unit_cost=",
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
        "the repeated product adds nothing"
    );
    assert_eq!(after["total"], before["total"]);

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

/// The shared results fragment shows the price the calling context works in:
/// a sale line is sold at the sale price, a purchase line is bought at the
/// cost. The endpoint takes the price kind from the picker, so the number can
/// never be the other context's price.
#[tokio::test]
async fn product_search_shows_the_context_price() {
    let (app, pool) = test_app().await;
    create_product_via_web(&app, &pool, "PRICE-P", "1", "50").await;

    let (status, sale) = get(
        &app,
        "/web/product-search?q=PRICE-P&price=sale&line_action=/web/sales/1/lines&line_target=%23sale-record-money",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sale}");
    assert!(sale.contains("$25"), "a sale shows its sale price: {sale}");
    assert!(!sale.contains("$10"), "a sale must not show the cost: {sale}");

    let (status, purchase) = get(
        &app,
        "/web/product-search?q=PRICE-P&price=cost&line_action=/web/purchases/1/lines&line_target=%23purchase-record-money",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{purchase}");
    assert!(
        purchase.contains("cost $10"),
        "a purchase shows the cost price: {purchase}"
    );
    assert!(
        !purchase.contains("$25"),
        "a purchase must not show the sale price: {purchase}"
    );
}

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
    let end = pos + html[pos..]
        .find('>')
        .unwrap_or_else(|| panic!("unterminated tag at byte {pos}"));
    &html[start..=end]
}

/// The full element carrying `id`, opening tag through closing tag, for the
/// small elements this check inspects.
fn element_with_id<'a>(html: &'a str, id: &str) -> &'a str {
    let pos = html
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("no element renders id={id:?}"));
    let start = html[..pos].rfind('<').expect("an id must sit inside a tag");
    let name_start = start + 1;
    let name_end = name_start
        + html[name_start..]
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .expect("unterminated opening tag");
    let name = &html[name_start..name_end];
    let close = format!("</{name}>");
    let end = html[start..]
        .find(&close)
        .unwrap_or_else(|| panic!("no {close} for id={id:?}"));
    &html[start..start + end + close.len()]
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
    assert_eq!(controls_without_accessible_name(named), Vec::<String>::new());

    let offenders = controls_without_accessible_name(
        r#"<label>Qty</label><input type="number" name="qty" id="qty" />"#,
    );
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(offenders[0].contains("qty"), "{offenders:?}");

    // A hidden control is not announced, so it needs no name.
    assert!(
        controls_without_accessible_name(r#"<input type="hidden" name="id" />"#).is_empty()
    );
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

/// The picker's results container is a polite live region the input is wired
/// to, and only the match count is announced; the visual list is explicitly
/// not live, so typing does not read the catalogue out loud on every keystroke.
#[tokio::test]
async fn product_search_results_announce_a_polite_match_count() {
    let (app, pool) = test_app().await;
    create_product_via_web(&app, &pool, "A11Y-P", "1", "50").await;
    create_product_via_web(&app, &pool, "A11Y-Q", "1", "50").await;
    let sale = create_sale_draft_via_web(&app, &pool, "A11yBuyer", "Cash", "").await;
    let base = format!("/web/sales/{sale}");

    let (status, page) = get(&app, &format!("/sales/{sale}")).await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");

    let results_pos = page
        .find("id=\"product-search-results\"")
        .expect("the picker renders its results container");
    let results = enclosing_tag(&page, results_pos);
    assert!(
        results.contains("aria-live=\"polite\""),
        "the results container must be a polite live region: {results}"
    );
    assert!(results.contains("role=\"status\""), "{results}");
    assert!(
        results.contains("aria-atomic=\"false\""),
        "the region must announce the count, not replace its whole content: {results}"
    );

    let input_pos = page
        .find("id=\"product-picker\"")
        .expect("the picker input");
    let input = enclosing_tag(&page, input_pos);
    assert!(
        input.contains("aria-controls=\"product-search-results\""),
        "the input must say what it controls: {input}"
    );
    assert!(
        input.contains("aria-describedby=\"product-search-status\""),
        "the input must point at the announced state: {input}"
    );
    assert!(
        page.contains("id=\"product-search-status\""),
        "the described status element must exist on the page"
    );

    let search = |query: &str| {
        format!(
            "/web/product-search?q={query}&line_action={base}/lines&line_target=%23sale-record-money"
        )
    };

    // One match: the announced text is the count.
    let (status, fragment) = get(&app, &search("A11Y-P")).await;
    assert_eq!(status, StatusCode::OK, "{fragment}");
    let status_text = element_with_id(&fragment, "product-search-status");
    assert!(status_text.contains("1 match"), "{status_text}");
    assert!(
        fragment.contains("aria-live=\"off\""),
        "the visual list must stay out of the live announcement: {fragment}"
    );

    // Two matches pluralize.
    let (status, fragment) = get(&app, &search("A11Y")).await;
    assert_eq!(status, StatusCode::OK, "{fragment}");
    let status_text = element_with_id(&fragment, "product-search-status");
    assert!(status_text.contains("2 matches"), "{status_text}");

    // No matches is a state, not silence.
    let (status, fragment) = get(&app, &search("does-not-exist")).await;
    assert_eq!(status, StatusCode::OK, "{fragment}");
    let status_text = element_with_id(&fragment, "product-search-status");
    assert!(status_text.contains("No products match"), "{status_text}");
}

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
    check_no_bare_referenced_ids("mutation", "<div>draft #12</div>").unwrap();
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
    assert_eq!(status, StatusCode::OK, "{}: {html:.400}", products_page.path);
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
    let sale = create_sale_draft_on_date(&app, customer, "Credit", "2024-05-02", "2024-06-01").await;
    add_sale_line_via_web(&app, sale, product, "2").await;
    confirm_sale_via_web(&app, sale, None).await;
    let (status, resp) = post_form(
        &app,
        "/web/customer-receipts",
        &format!(
            "customer_id={customer}&method_id={cash}&amount=10&date=2024-05-10"
        ),
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
    let body = format!(
        "supplier_id={supplier_id}&payment_type=Credit&purchase_date={purchase_date}&due_date=2024-12-31"
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

async fn add_purchase_line_via_web(app: &Router, purchase_id: i64, product_id: i64, qty: &str) {
    let body = format!("purchase_id={purchase_id}&product_id={product_id}&qty={qty}");
    let (status, resp) = post_form(app, "/web/purchases/lines", &body).await;
    assert_eq!(status, StatusCode::OK, "add purchase line: {resp}");
}

async fn confirm_purchase_via_web(app: &Router, purchase_id: i64) {
    let body = format!("purchase_id={purchase_id}");
    let (status, resp) = post_form(app, "/web/purchases/confirm", &body).await;
    assert_eq!(status, StatusCode::OK, "confirm purchase {purchase_id}: {resp}");
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
        all.contains("draft #"),
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
    assert!(!confirmed.contains("draft #"), "{confirmed}");

    let drafts = sale_list_html(&app, "?status=Draft").await;
    assert!(drafts.contains("draft #"), "{drafts}");
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
    assert!(!by_date.contains("draft #"), "{by_date}");

    // Combined filters narrow further.
    let combined = sale_list_html(&app, "?status=Confirmed&customer=FiltBeto").await;
    assert!(combined.contains(&beto_number), "{combined}");
    assert!(!combined.contains(&ana_number), "{combined}");

    // Empty values are no constraint, not an error.
    let blank = sale_list_html(&app, "?status=&customer=&number=&from=&to=").await;
    assert!(
        blank.contains("draft #")
            && blank.contains(&ana_number)
            && blank.contains(&beto_number),
        "{blank}"
    );

    // Matching nothing is an empty list, not an error.
    let none = sale_list_html(&app, "?number=NOPE-0000").await;
    assert!(none.contains("Nothing here yet."), "{none}");
    assert!(!none.contains(&ana_number), "{none}");

    // A status or date the picker never sends is treated as absent, not an error.
    let lenient = sale_list_html(&app, "?status=bogus&from=not-a-date").await;
    assert!(
        lenient.contains("draft #")
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
    assert!(!page.contains("draft #"), "{page:.600}");

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
    assert!(page.contains("name=\"from\" value=\"2024-05-01\""), "{page:.600}");
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
    let norte_number = purchase_detail(&app, norte_confirmed).await["purchase"]
        ["purchase_number"]
        .as_str()
        .expect("confirmed purchase number")
        .to_string();

    let all = purchase_list_html(&app, "").await;
    assert!(all.contains("draft #"), "{all}");
    assert!(
        all.contains(&sur_number) && all.contains(&norte_number),
        "{all}"
    );

    let confirmed = purchase_list_html(&app, "?status=Confirmed").await;
    assert!(
        confirmed.contains(&sur_number) && confirmed.contains(&norte_number),
        "{confirmed}"
    );
    assert!(!confirmed.contains("draft #"), "{confirmed}");

    let drafts = purchase_list_html(&app, "?status=Draft").await;
    assert!(drafts.contains("draft #"), "{drafts}");
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
        blank.contains("draft #")
            && blank.contains(&sur_number)
            && blank.contains(&norte_number),
        "{blank}"
    );

    let none = purchase_list_html(&app, "?supplier=NoSuchSupplier").await;
    assert!(none.contains("Nothing here yet."), "{none}");

    let (status, page) = get(&app, "/purchases?status=Confirmed").await;
    assert_eq!(status, StatusCode::OK, "{page:.400}");
    assert!(page.contains(&sur_number), "{page:.600}");
    assert!(!page.contains("draft #"), "{page:.600}");

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
    let crossed = product_list_html(&app, &format!("?q=7791234567001&category_id={other_cat}")).await;
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
    let (rows,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM product_supplier_costs WHERE product_id = ?",
    )
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

    let body = format!(
        "product_id={alpha}&supplier_id={supplier}&category_id={alpha_cat}&q=Alpha"
    );
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
    let product =
        create_product_full_via_web(&app, &pool, "NAMES-P", "Named Widget", None).await;
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
        assert!(!html.contains("Ñandú"), "{needle:?} must not match Ñandú: {html}");
    }
    for needle in ["Ñandú", "ñandú", "Nandu", "ÑANDÚ"] {
        let html = sale_list_html(&app, &format!("?customer={needle}")).await;
        assert!(html.contains("Ñandú"), "{needle:?} must find Ñandú: {html}");
        assert!(!html.contains("Pérez"), "{needle:?} must not match Pérez: {html}");
    }

    // Purchases: a supplier named with accents, the same both ways.
    let cafe_sup = create_supplier_via_web(&app, &pool, "Café").await;
    let andu_sup = create_supplier_via_web(&app, &pool, "Ñandú").await;
    create_purchase_draft_on_date(&app, cafe_sup, "2024-05-02").await;
    create_purchase_draft_on_date(&app, andu_sup, "2024-05-02").await;
    for needle in ["Nandu", "ÑANDÚ", "ñandú"] {
        let html = purchase_list_html(&app, &format!("?supplier={needle}")).await;
        assert!(html.contains("Ñandú"), "{needle:?} must find Ñandú: {html}");
        assert!(!html.contains("Café"), "{needle:?} must not match Café: {html}");
    }
    for needle in ["CAFE", "café", "Café"] {
        let html = purchase_list_html(&app, &format!("?supplier={needle}")).await;
        assert!(html.contains("Café"), "{needle:?} must find Café: {html}");
        assert!(!html.contains("Ñandú"), "{needle:?} must not match Ñandú: {html}");
    }

    // Catalogue: the picker and the list both fold accents and case.
    create_product_full_via_web(&app, &pool, "CAFE-P", "Café", None).await;
    let (status, search) = get(&app, "/web/product-search?q=CAFE").await;
    assert_eq!(status, StatusCode::OK, "{search}");
    assert!(search.contains("Café"), "the picker must find Café by CAFE: {search}");
    let list = product_list_html(&app, "?q=cafe").await;
    assert!(list.contains("Café"), "the catalogue must find Café by cafe: {list}");
    let list = product_list_html(&app, "?q=CAFÉ").await;
    assert!(list.contains("Café"), "the catalogue must find Café by CAFÉ: {list}");
}
