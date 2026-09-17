// Slice G (T13): purchases web `/purchases` Askama + HTMX, parity with the
// sales/products pages. Thin handlers over PurchasesService; fragments live in
// partials/purchase_*.html and cross-page refresh uses HX-Trigger events.
//
// Typed-id actions (add line, confirm, pay, cancel) post to collection web
// endpoints with the purchase id in the form body, because HTMX cannot
// interpolate a path from an input value. Fragment actions on a known purchase
// (line save/remove) use the `/web/purchases/{id}/lines/{line_id}` paths.
use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewPurchase, PaymentType, PurchaseDetail, PurchaseSuggestions};
use crate::repositories::ProductRepository;
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// A purchase detail plus the resolved supplier name (purchases store only the id).
#[derive(Clone)]
pub struct PurchaseView {
    pub detail: PurchaseDetail,
    pub supplier_name: String,
}

#[derive(Template)]
#[template(path = "purchases.html")]
struct PurchasesTemplate {
    title: String,
    purchases: Vec<PurchaseView>,
    suggestions: PurchaseSuggestions,
    has_suggestions: bool,
    products: Vec<crate::models::Product>,
    suppliers: Vec<crate::models::Supplier>,
    accounts: Vec<crate::models::AccountWithBalance>,
    methods: Vec<crate::models::PaymentMethod>,
    allow_negative: bool,
    allow_negative_stock: bool,
    today: String,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "partials/purchase_list.html")]
struct PurchaseListPartial {
    title: String,
    purchases: Vec<PurchaseView>,
}

#[derive(Template)]
#[template(path = "partials/purchase_detail.html")]
struct PurchaseDetailPartial {
    view: PurchaseView,
}

#[derive(Template)]
#[template(path = "partials/suggestion_list.html")]
struct SuggestionListPartial {
    suggestions: PurchaseSuggestions,
    has_suggestions: bool,
    today: String,
}

// ---------------------------------------------------------------------------
// Helpers (mirror sales_web)
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn parse_required_decimal(s: &str, field: &str) -> AppResult<Decimal> {
    Decimal::from_str(s.trim()).map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_decimal(s: &str, field: &str) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    Decimal::from_str(t)
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_i64(s: &str, field: &str) -> AppResult<Option<i64>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_date_or_today(s: &str) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(chrono::Local::now().date_naive());
    }
    t.parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))
}

fn parse_opt_date(s: &str, field: &str) -> AppResult<Option<NaiveDate>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field} (YYYY-MM-DD)")))
}

fn clean_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

async fn purchase_view(state: &AppState, id: i64) -> AppResult<PurchaseView> {
    let detail = state.purchases_service.get_detail(id).await?;
    let supplier = state
        .supplier_service
        .get_supplier(detail.purchase.supplier_id)
        .await?;
    Ok(PurchaseView {
        detail,
        supplier_name: supplier.name,
    })
}

async fn purchase_views(state: &AppState) -> AppResult<Vec<PurchaseView>> {
    let details = state.purchases_service.list_details().await?;
    let mut out = Vec::with_capacity(details.len());
    for detail in details {
        let supplier = state
            .supplier_service
            .get_supplier(detail.purchase.supplier_id)
            .await?;
        out.push(PurchaseView {
            detail,
            supplier_name: supplier.name,
        });
    }
    Ok(out)
}

fn triggered(html: String, event: &str) -> Response {
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", event.parse().unwrap());
    resp
}

fn render_list(view: Vec<PurchaseView>, title: &str) -> AppResult<Html<String>> {
    let html = PurchaseListPartial {
        title: title.to_string(),
        purchases: view,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// List fragment + `HX-Trigger` refresh event, for mutating web handlers.
async fn list_response(state: &AppState, event: &str) -> AppResult<Response> {
    let view = purchase_views(state).await?;
    let html = render_list(view, "All purchases")?;
    Ok(triggered(html.0, event))
}

async fn changed(state: &AppState, id: i64) -> AppResult<Response> {
    let view = purchase_view(state, id).await?;
    let html = PurchaseDetailPartial { view }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(triggered(html, "purchase-changed"))
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

async fn purchases_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let purchases = purchase_views(&state).await?;
    let suggestions = state.purchases_service.suggestions().await?;
    let has_suggestions =
        !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
    let products = state.inventory_service.products.list().await?;
    let suppliers = state.supplier_service.list_suppliers().await?;
    let accounts = state.account_service.list_with_balances().await?;
    let methods = state.payment_method_service.list().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = PurchasesTemplate {
        title: "All purchases".to_string(),
        purchases,
        suggestions,
        has_suggestions,
        products,
        suppliers,
        accounts,
        methods,
        allow_negative: state.allow_negative,
        allow_negative_stock: state.allow_negative_stock,
        today,
        nav_key: "purchases",
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_purchase_list(State(state): State<AppState>) -> AppResult<Response> {
    let view = purchase_views(&state).await?;
    Ok(render_list(view, "All purchases")?.into_response())
}

async fn web_purchase_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let view = purchase_view(&state, id).await?;
    let html = PurchaseDetailPartial { view }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html).into_response())
}

async fn web_purchase_suggestions(State(state): State<AppState>) -> AppResult<Html<String>> {
    let suggestions = state.purchases_service.suggestions().await?;
    let has_suggestions =
        !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let html = SuggestionListPartial {
        suggestions,
        has_suggestions,
        today,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Forms (HTMX, mirror sales_web patterns)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePurchaseForm {
    pub supplier_id: i64,
    #[serde(default)]
    pub payment_type: String,
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub due_date: String,
    #[serde(default)]
    pub supplier_invoice_no: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct AddLineForm {
    pub purchase_id: i64,
    pub product_id: i64,
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub unit_cost: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineForm {
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub unit_cost: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmPurchaseForm {
    pub purchase_id: i64,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub method_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    pub purchase_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelPurchaseForm {
    pub purchase_id: i64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct SeedSuggestionForm {
    pub product_id: i64,
    #[serde(default)]
    pub payment_type: String,
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub due_date: String,
}

fn parse_payment_type(raw: &str) -> AppResult<PaymentType> {
    if raw.trim().is_empty() {
        Ok(PaymentType::Cash)
    } else {
        raw.parse().map_err(AppError::Validation)
    }
}

async fn web_create_purchase(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreatePurchaseForm>,
) -> AppResult<Response> {
    state
        .purchases_service
        .create_draft(NewPurchase {
            supplier_id: form.supplier_id,
            payment_type: parse_payment_type(&form.payment_type)?,
            purchase_date: parse_date_or_today(&form.purchase_date)?,
            due_date: parse_opt_date(&form.due_date, "due_date")?,
            supplier_invoice_no: clean_opt(&form.supplier_invoice_no),
            notes: clean_opt(&form.notes),
        })
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "purchase-created").await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

async fn web_add_line(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddLineForm>,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_cost = parse_opt_decimal(&form.unit_cost, "unit_cost")?;
    state
        .purchases_service
        .add_line(form.purchase_id, form.product_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, form.purchase_id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

async fn web_update_line(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
    Form(form): Form<UpdateLineForm>,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_cost = parse_required_decimal(&form.unit_cost, "unit_cost")?;
    state
        .purchases_service
        .update_line(line_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, purchase_id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

async fn web_remove_line(
    State(state): State<AppState>,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    state.purchases_service.remove_line(line_id).await?;
    changed(&state, purchase_id).await
}

async fn web_confirm_purchase(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConfirmPurchaseForm>,
) -> AppResult<Response> {
    let account_id = parse_opt_i64(&form.account_id, "account_id")?;
    let method_id = parse_opt_i64(&form.method_id, "method_id")?;
    state
        .purchases_service
        .confirm(form.purchase_id, account_id, method_id)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, form.purchase_id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

async fn web_record_payment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RecordPaymentForm>,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .purchases_service
        .record_payment(form.purchase_id, form.account_id, form.method_id, amount, date)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, form.purchase_id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

async fn web_cancel_purchase(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CancelPurchaseForm>,
) -> AppResult<Response> {
    state
        .purchases_service
        .cancel(form.purchase_id, clean_opt(&form.reason))
        .await?;
    if is_htmx(&headers) {
        return changed(&state, form.purchase_id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

/// Seed a Draft pedido from one suggested low-stock product: the service
/// re-derives the suggestion (chosen supplier, qty, satellite cost) so the form
/// never decides business values.
async fn web_seed_from_suggestion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SeedSuggestionForm>,
) -> AppResult<Response> {
    let suggestions = state.purchases_service.suggestions().await?;
    let item = suggestions
        .suggestions
        .into_iter()
        .find(|s| s.product.id == form.product_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "suggestion for product {} not found",
                form.product_id
            ))
        })?;
    if item.suggested_qty <= Decimal::ZERO {
        return Err(AppError::Validation(
            "suggested qty must be > 0 to seed a draft".into(),
        ));
    }
    let purchase = state
        .purchases_service
        .create_draft(NewPurchase {
            supplier_id: item.supplier_id,
            payment_type: parse_payment_type(&form.payment_type)?,
            purchase_date: parse_date_or_today(&form.purchase_date)?,
            due_date: parse_opt_date(&form.due_date, "due_date")?,
            supplier_invoice_no: None,
            notes: None,
        })
        .await?;
    state
        .purchases_service
        .add_line(
            purchase.id,
            item.product.id,
            item.suggested_qty,
            Some(item.unit_cost),
        )
        .await?;
    if is_htmx(&headers) {
        return changed(&state, purchase.id).await;
    }
    Ok(Redirect::to("/purchases").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/purchases", get(purchases_page))
        .route(
            "/web/purchases",
            get(web_purchase_list).post(web_create_purchase),
        )
        .route("/web/purchases/suggestions", get(web_purchase_suggestions))
        .route(
            "/web/purchases/from-suggestion",
            post(web_seed_from_suggestion),
        )
        .route("/web/purchases/lines", post(web_add_line))
        .route("/web/purchases/confirm", post(web_confirm_purchase))
        .route("/web/purchases/payments", post(web_record_payment))
        .route("/web/purchases/cancel", post(web_cancel_purchase))
        .route("/web/purchases/{id}", get(web_purchase_detail))
        .route(
            "/web/purchases/{purchase_id}/lines/{line_id}",
            post(web_update_line).delete(web_remove_line),
        )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::routes::AppState;

    async fn test_state() -> AppState {
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
        AppState::new(pool, false, true)
    }

    async fn get_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn post_form(app: axum::Router, uri: &str, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    async fn delete_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("HX-Request", "true")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn web_draft_line_editor_add_update_remove() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Editor Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();
        let (st, v) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "EDIT-P", "name": "prod EDIT-P", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed product: {v}");
        let pid = v["id"].as_i64().unwrap();

        let body = format!("supplier_id={sup}&payment_type=Cash&purchase_date=2024-05-02");
        assert_eq!(
            post_form(app.clone(), "/web/purchases", &body).await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), "/api/purchases").await;
        let purchase_id = v["purchases"][0]["purchase"]["id"].as_i64().unwrap();

        // Add a line through the Draft editor endpoint.
        let body = format!("purchase_id={purchase_id}&product_id={pid}&qty=2&unit_cost=5");
        assert_eq!(
            post_form(app.clone(), "/web/purchases/lines", &body).await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        let line_id = v["lines"][0]["id"].as_i64().unwrap();
        assert_eq!(v["lines"][0]["qty"].as_str().unwrap(), "2");

        // Inline save edits qty/unit_cost in place.
        let body = "qty=3&unit_cost=6";
        assert_eq!(
            post_form(
                app.clone(),
                &format!("/web/purchases/{purchase_id}/lines/{line_id}"),
                body
            )
            .await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        assert_eq!(v["lines"][0]["qty"].as_str().unwrap(), "3");
        assert_eq!(v["lines"][0]["unit_cost"].as_str().unwrap(), "6");

        // Inline remove empties the Draft again.
        let (st, html) = delete_html(
            app.clone(),
            &format!("/web/purchases/{purchase_id}/lines/{line_id}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            html.contains("No lines yet"),
            "detail should show the empty state: {html:.300}"
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        assert!(v["lines"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn web_purchases_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Purchases"), "page should mention Purchases");
        assert!(html.contains("Sugerido"), "page should have the suggestion panel");
    }

    #[tokio::test]
    async fn web_create_purchase_then_list_shows_it() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Web Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();

        let body = format!("supplier_id={sup}&payment_type=Cash&purchase_date=2024-05-02");
        assert_eq!(
            post_form(app.clone(), "/web/purchases", &body).await,
            StatusCode::OK
        );

        let (status, html) = get_html(app.clone(), "/web/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Web Sup"),
            "list should contain the new draft: {html:.400}"
        );
        assert!(
            html.contains("draft #"),
            "draft without number should show as draft #id: {html:.400}"
        );
    }

    #[tokio::test]
    async fn web_seed_from_suggestion_creates_draft_with_line() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "WEB-SUG", "name": "prod WEB-SUG", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed product: {v}");
        let pid = v["id"].as_i64().unwrap();
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            serde_json::json!({
                "product_id": pid, "qty": "2", "type": "In",
                "reason": "Initial", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Seed Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();
        // A second low-stock product with no satellite row lands in `without_supplier`.
        let (st, _) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "WEB-NOSUP", "name": "prod WEB-NOSUP", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/product-supplier-costs",
            serde_json::json!({
                "product_id": pid, "supplier_id": sup, "cost": "7.50", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        // The fragment endpoint renders the same suggestion the panel shows.
        let (st, html) = get_html(app.clone(), "/web/purchases/suggestions").await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            html.contains("Seed Sup") && html.contains("48") && html.contains("Without supplier cost"),
            "suggestion fragment should render costed and unsourced rows: {html:.400}"
        );

        let body = format!("product_id={pid}&payment_type=Cash&purchase_date=2024-05-02");
        assert_eq!(
            post_form(app.clone(), "/web/purchases/from-suggestion", &body).await,
            StatusCode::OK
        );

        let (status, html) = get_html(app.clone(), "/web/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Seed Sup"),
            "seeded draft should appear in the list: {html:.400}"
        );

        // The seeded draft exposes the suggested line in its detail fragment.
        let req = Request::builder()
            .method("GET")
            .uri("/api/purchases")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let purchase_id = v["purchases"][0]["purchase"]["id"].as_i64().unwrap();
        let (st, detail) = get_html(app.clone(), &format!("/web/purchases/{purchase_id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            detail.contains("product #") && detail.contains("48"),
            "seeded line should show suggested qty: {detail:.400}"
        );
    }
}
