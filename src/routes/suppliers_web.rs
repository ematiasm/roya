// Slice G (T13): suppliers web `/suppliers` Askama + HTMX. The list page
// shows names only; the per-supplier detail (header, Confirmed-due balance,
// purchases) lives in `partials/supplier_detail.html` and renders as the
// `/web/suppliers/{id}/detail` fragment the slide-over drawer loads.
// Supplier CRUD plus the per-product cost satellite (record cost, derived
// price alert, preferred marker). Thin handlers over SupplierService; the
// list fragment lives in partials/supplier_list.html.
use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewSupplier, PaymentMethodWithAccount, Product, ProductSupplierCost, PurchaseDetail,
    PurchaseListFilter, PurchaseStatus, Supplier, UpdateSupplier,
};
use crate::repositories::{ProductRepository, ProductSupplierCostRepository};
use crate::routes::AppState;
// S7 enforcement: every registered handler declares the permission its action
// needs (AC10). The drawer detail carries a double gate — it renders
// per-supplier cost rows, the same data the S5 product drawer gates
// `purchases.costs.read`, so one dataset never answers two permissions. The
// mapping and its judgement calls are recorded in
// openspec/changes/2026-09-18-add-identity-module/tasks.md (S7 section).
use crate::security::authz::{
    Nav, PurchasesCostsRead, PurchasesCostsWrite, PurchasesCreate, Require, SuppliersRead,
    SuppliersWrite,
};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One satellite cost row with the product fields the page needs to render it.
#[derive(Clone)]
pub struct SupplierCostView {
    pub cost: ProductSupplierCost,
    pub product_name: String,
    pub product_sku: String,
    pub product_unit: String,
}

#[derive(Clone)]
pub struct SupplierView {
    pub supplier: Supplier,
    pub costs: Vec<SupplierCostView>,
}

#[derive(Template)]
#[template(path = "suppliers.html")]
struct SuppliersTemplate {
    suppliers: Vec<SupplierView>,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/supplier_list.html")]
struct SupplierListPartial {
    suppliers: Vec<SupplierView>,
}

/// The slide-over drawer body: the supplier header, the outstanding balance
/// (sum of `due` over Confirmed purchases), the pay action (card A, mirror of
/// the customer "Collect payment" card), the record-cost action (card B,
/// supplier fixed) and that supplier's purchases.
#[derive(Template)]
#[template(path = "partials/supplier_detail.html")]
struct SupplierDetailPartial {
    supplier: Supplier,
    balance: Decimal,
    purchases: Vec<PurchaseDetail>,
    products: Vec<Product>,
    method_options: Vec<PaymentMethodWithAccount>,
    today: String,
}

/// Prefilled edit form for the row-level ✎ button: the list rows carry only
/// the name, so this read-only fragment loads the entity's data into the
/// `<dialog>` modal. It posts to the existing `/web/suppliers/edit` backend.
#[derive(Template)]
#[template(path = "partials/supplier_edit_form.html")]
struct SupplierEditFormPartial {
    supplier: Supplier,
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

fn parse_date_or_today(s: &str) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(chrono::Local::now().date_naive());
    }
    t.parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))
}

fn clean_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

async fn supplier_views(state: &AppState) -> AppResult<Vec<SupplierView>> {
    let suppliers = state.supplier_service.list_suppliers().await?;
    let products = state.inventory_service.products.list().await?;
    let mut views = Vec::with_capacity(suppliers.len());
    for supplier in suppliers {
        let costs = state
            .supplier_service
            .costs
            .list_by_supplier(supplier.id)
            .await?;
        let mut cost_views = Vec::with_capacity(costs.len());
        for cost in costs {
            let (product_name, product_sku, product_unit) = products
                .iter()
                .find(|p| p.id == cost.product_id)
                .map(|p| (p.name.clone(), p.sku.clone(), p.unit.clone()))
                .unwrap_or_else(|| (format!("product #{}", cost.product_id), String::new(), String::new()));
            cost_views.push(SupplierCostView {
                cost,
                product_name,
                product_sku,
                product_unit,
            });
        }
        views.push(SupplierView {
            supplier,
            costs: cost_views,
        });
    }
    Ok(views)
}

fn triggered(html: String, event: &str) -> Response {
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", event.parse().unwrap());
    resp
}

async fn render_list(suppliers: Vec<SupplierView>) -> AppResult<Html<String>> {
    let html = SupplierListPartial { suppliers }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Supplier fragment + `HX-Trigger` refresh event, for mutating web handlers.
async fn list_response(state: &AppState, event: &str) -> AppResult<Response> {
    let suppliers = supplier_views(state).await?;
    let html = render_list(suppliers).await?;
    Ok(triggered(html.0, event))
}

// ---------------------------------------------------------------------------
// Page + fragment
// ---------------------------------------------------------------------------

/// The page is names-only (cost rows live in the drawer), so the page and the
/// list fragment carry `suppliers.read` alone; the drawer — the surface that
/// renders the per-supplier costs — is where the costs gate joins in.
async fn suppliers_page(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
) -> Result<Html<String>, AppError> {
    let suppliers = supplier_views(&state).await?;
    let tmpl = SuppliersTemplate {
        suppliers,
        nav_key: "suppliers",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_supplier_list(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
) -> AppResult<Response> {
    let suppliers = supplier_views(&state).await?;
    Ok(render_list(suppliers).await?.into_response())
}

/// Drawer detail for one supplier: the header, the outstanding balance (sum
/// of `due` over Confirmed purchases, narrowed to this supplier at the
/// repository), the pay context (account-owning methods, same source as the
/// customer collect form), the record-cost context (product list + today) and
/// that supplier's purchases, each linking to its record.
///
/// Double gate, the S5 product drawer's shape: the fragment renders the
/// supplier's per-supplier cost data, so it needs `suppliers.read` AND
/// `purchases.costs.read` — the same data reached from the product drawer
/// answers to the same code. The cost, recorded deliberately: a
/// suppliers-only principal reads the list but is refused the drawer; the
/// drawer's purchases card renders that supplier's own documents (the S6
/// statement-mirror) without a `purchases.read` gate.
async fn web_supplier_detail(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
    _: Require<PurchasesCostsRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    supplier_detail_html(&state, id).await
}

/// The drawer body with fresh derived data. Both the detail fragment and the
/// pay/record-cost actions answer it, so paying or recording refreshes the
/// drawer in place without the client rebuilding a URL.
async fn supplier_detail_html(state: &AppState, id: i64) -> AppResult<Html<String>> {
    let supplier = state.supplier_service.get_supplier(id).await?;
    let filter = PurchaseListFilter {
        supplier_ids: Some(vec![id]),
        ..Default::default()
    };
    let purchases = state
        .purchases_service
        .list_details_filtered(&filter)
        .await?;
    let balance: Decimal = purchases
        .iter()
        .filter(|d| d.purchase.status == PurchaseStatus::Confirmed)
        .map(|d| d.due)
        .sum();
    let products = state.inventory_service.products.list().await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let html = SupplierDetailPartial {
        supplier,
        balance,
        purchases,
        products,
        method_options,
        today,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_supplier_edit_form(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let supplier = state.supplier_service.get_supplier(id).await?;
    let html = SupplierEditFormPartial { supplier }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Forms (HTMX)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SupplierForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct EditSupplierForm {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordCostForm {
    pub product_id: i64,
    pub supplier_id: i64,
    #[serde(default)]
    pub cost: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PaySupplierForm {
    #[serde(default)]
    pub supplier_id: i64,
    #[serde(default)]
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

async fn web_create_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    headers: HeaderMap,
    Form(form): Form<SupplierForm>,
) -> AppResult<Response> {
    state
        .supplier_service
        .create_supplier(NewSupplier {
            name: form.name,
            phone: clean_opt(&form.phone),
            notes: clean_opt(&form.notes),
        })
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "supplier-created").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

async fn web_update_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    headers: HeaderMap,
    Form(form): Form<EditSupplierForm>,
) -> AppResult<Response> {
    state
        .supplier_service
        .update_supplier(
            form.id,
            UpdateSupplier {
                name: Some(form.name),
                phone: Some(clean_opt(&form.phone)),
                notes: Some(clean_opt(&form.notes)),
            },
        )
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "supplier-changed").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

async fn web_activate_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.set_active(id, true).await?;
    list_response(&state, "supplier-changed").await
}

async fn web_deactivate_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.set_active(id, false).await?;
    list_response(&state, "supplier-changed").await
}

async fn web_delete_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.delete_supplier(id).await?;
    list_response(&state, "supplier-deleted").await
}

async fn web_record_cost(
    State(state): State<AppState>,
    _: Require<PurchasesCostsWrite>,
    headers: HeaderMap,
    Form(form): Form<RecordCostForm>,
) -> AppResult<Response> {
    let cost = parse_required_decimal(&form.cost, "cost")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .supplier_service
        .record_cost(form.product_id, form.supplier_id, cost, date)
        .await?;
    if is_htmx(&headers) {
        // Drawer submissions target `#supplier-drawer-body`: answer the fresh
        // detail fragment (costs/balance reloaded) and reuse the existing
        // `supplier-cost-recorded` trigger so the page listener refreshes
        // `#supplier-list`. Any other HTMX caller keeps the historical
        // list-fragment answer.
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("supplier-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = supplier_detail_html(&state, form.supplier_id).await?;
            return Ok(triggered(html.0, "supplier-cost-recorded"));
        }
        return list_response(&state, "supplier-cost-recorded").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

/// Pay a supplier across their Confirmed purchases, oldest debt first (the
/// supplier-side mirror of `web_collect_receipt`, without a grouping receipt:
/// suppliers have no such document). Judgement call (S7 mapping): the
/// payment is a purchase-side movement — it writes `purchase_payments` rows
/// against purchases and posts the Expense — so the gate is the recording
/// tier `purchases.create`, NOT `suppliers.write`. The cost to the suppliers
/// tier is written in the mapping: a principal that manages suppliers but
/// cannot record purchases sees the pay card (its read gates hold) and is
/// refused on submit; no seeded matrix separates the pair.
async fn web_pay_supplier(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<PaySupplierForm>,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .purchases_service
        .pay_supplier(principal.user_id, form.supplier_id, form.method_id, amount, date)
        .await?;
    if is_htmx(&headers) {
        // Drawer submissions target `#supplier-drawer-body`: answer the fresh
        // detail fragment (balance and purchases reloaded) and fire
        // `supplier-paid` so the page listener refreshes `#supplier-list`, the
        // same way the record-cost action reuses its trigger.
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("supplier-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = supplier_detail_html(&state, form.supplier_id).await?;
            return Ok(triggered(html.0, "supplier-paid"));
        }
        return list_response(&state, "supplier-paid").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/suppliers", get(suppliers_page))
        .route(
            "/web/suppliers",
            get(web_supplier_list).post(web_create_supplier),
        )
        .route("/web/suppliers/edit", post(web_update_supplier))
        .route("/web/suppliers/{id}", delete(web_delete_supplier))
        .route("/web/suppliers/{id}/detail", get(web_supplier_detail))
        .route("/web/suppliers/{id}/edit-form", get(web_supplier_edit_form))
        .route("/web/suppliers/{id}/activate", post(web_activate_supplier))
        .route(
            "/web/suppliers/{id}/deactivate",
            post(web_deactivate_supplier),
        )
        .route("/web/supplier-costs", post(web_record_cost))
        .route("/web/supplier-payments", post(web_pay_supplier))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{HeaderMap, Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::routes::AppState;
    use crate::security::test_support;

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests seed their own users instead, because there
    /// the point is telling two actors apart.
    async fn audit_actor(state: &AppState) -> i64 {
        test_support::audit_actor_id(&state.pool).await.unwrap()
    }

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
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    async fn get_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_form(app: axum::Router, uri: &str, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    async fn post_form_full(
        app: axum::Router,
        uri: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE);
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).to_string(),
        )
    }

    #[tokio::test]
    async fn web_suppliers_page_is_names_only_with_drawer_and_modal() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/suppliers").await;
        assert_eq!(status, StatusCode::OK);
        for expected in [
            "Suppliers",
            "New supplier",
            "<dialog",
            "new-supplier-dialog",
            "supplier-drawer",
            "supplier-drawer-body",
        ] {
            assert!(html.contains(expected), "page must show {expected}: {html:.600}");
        }
        // The cost form lives in the drawer now (card B), not on the page.
        assert!(
            !html.contains("Record Product Cost"),
            "record-cost card must not render on the page: {html:.600}"
        );
        assert!(
            !html.contains("Edit Supplier"),
            "no edit card may render: {html:.600}"
        );
    }

    #[tokio::test]
    async fn web_create_supplier_then_list_shows_name_only() {
        let app = crate::routes::router(test_state().await);
        let body = "name=WebSupplier&phone=555&notes=nota";
        assert_eq!(
            post_form(app.clone(), "/web/suppliers", body).await,
            StatusCode::OK
        );
        let (status, html) = get_html(app.clone(), "/web/suppliers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("WebSupplier"),
            "fragment should contain the new supplier: {html:.400}"
        );
        // The names-only list carries no phone, notes or cost rows: those
        // live in the drawer detail fragment.
        for absent in ["555", "nota", "Costs ("] {
            assert!(
                !html.contains(absent),
                "names-only fragment must not show {absent}: {html:.400}"
            );
        }
        // Each name loads its detail fragment into the drawer.
        assert!(
            html.contains("hx-get=\"/web/suppliers/"),
            "names must open the drawer through the detail fragment: {html:.400}"
        );
        assert!(
            html.contains("/detail\""),
            "names must target the detail endpoint: {html:.400}"
        );
    }

    #[tokio::test]
    async fn web_supplier_detail_reports_confirmed_balance_and_links_documents() {
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        use crate::models::{
            NewProduct, NewPurchase, NewSupplier, PaymentType, ProductKind,
        };

        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Web Detail Supplier".into(),
                phone: Some("555-0100".into()),
                notes: Some("drawer notes".into()),
            })
            .await
            .unwrap();
        let other = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Unrelated Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: "WEB-SUP-P".into(),
                name: "prod WEB-SUP-P".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::ZERO),
                max_stock: Some(Decimal::from(100)),
                location: None,
                notes: None,
            })
            .await
            .unwrap();
        // One Confirmed credit purchase (2 × 10 = 20 due) plus one Draft that
        // must not move the balance.
        let confirmed = state
            .purchases_service
            .create_draft(NewPurchase {
                supplier_id: supplier.id,
                payment_type: PaymentType::Credit,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(confirmed.id, product.id, Decimal::from(2), Some(Decimal::from(10)))
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(audit_actor(&state).await, confirmed.id, None)
            .await
            .unwrap();
        let draft = state
            .purchases_service
            .create_draft(NewPurchase {
                supplier_id: supplier.id,
                payment_type: PaymentType::Credit,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 3).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 16).unwrap()),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(draft.id, product.id, Decimal::from(5), Some(Decimal::from(10)))
            .await
            .unwrap();
        // Another supplier's purchase must not leak into this detail.
        let foreign = state
            .purchases_service
            .create_draft(NewPurchase {
                supplier_id: other.id,
                payment_type: PaymentType::Credit,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 4).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 17).unwrap()),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(foreign.id, product.id, Decimal::from(1), Some(Decimal::from(7)))
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(audit_actor(&state).await, foreign.id, None)
            .await
            .unwrap();

        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/suppliers/{}/detail", supplier.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        // The balance counts only the Confirmed purchase (2 × 10 = 20); the
        // 5 × 10 Draft stays out of it.
        assert!(
            html.contains(">20 <"),
            "balance must be exactly the Confirmed due: {html:.600}"
        );
        for expected in [
            "Web Detail Supplier",
            "555-0100",
            "drawer notes",
            "Confirmed",
            "Draft",
            &format!("/purchases/{}", confirmed.id),
            &format!("/purchases/{}", draft.id),
        ] {
            assert!(html.contains(expected), "detail must show {expected}: {html:.600}");
        }
        assert!(
            !html.contains(&format!("/purchases/{}", foreign.id)),
            "another supplier's purchase must not leak in: {html:.600}"
        );

        // Unknown supplier reads 404, not an empty drawer.
        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/suppliers/999/detail").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_supplier_edit_form_is_prefilled_and_posts_to_edit() {
        use crate::models::NewSupplier;

        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Web Edit Supplier".into(),
                phone: Some("555-0100".into()),
                notes: Some("edit notes".into()),
            })
            .await
            .unwrap();
        let app = crate::routes::router(state);

        // Each list row carries the ✎ button loading this fragment, next to
        // the name button opening the drawer: a div row with two buttons,
        // never a nested button.
        let (status, html) = get_html(app.clone(), "/web/suppliers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!(
                "hx-get=\"/web/suppliers/{}/edit-form\"",
                supplier.id
            )),
            "rows must offer the edit form: {html:.600}"
        );
        assert!(
            html.contains("/detail\""),
            "the name button must keep opening the drawer: {html:.400}"
        );

        // The fragment prefills the entity's data and posts to the existing
        // edit backend with the id in the body.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/suppliers/{}/edit-form", supplier.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "hx-post=\"/web/suppliers/edit\"",
            &format!("name=\"id\" value=\"{}\"", supplier.id),
            "value=\"Web Edit Supplier\"",
            "value=\"555-0100\"",
            "value=\"edit notes\"",
        ] {
            assert!(html.contains(expected), "edit form must show {expected}: {html:.600}");
        }

        let (status, _) = get_html(app, "/web/suppliers/999/edit-form").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    async fn cost_fixture_state() -> (AppState, i64, i64) {
        use crate::models::{NewProduct, NewSupplier, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Web Cost Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: "WEB-SUP-C".into(),
                name: "prod WEB-SUP-C".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::ZERO),
                max_stock: Some(Decimal::from(100)),
                location: None,
                notes: None,
            })
            .await
            .unwrap();
        (state, supplier.id, product.id)
    }

    #[tokio::test]
    async fn web_supplier_detail_embeds_record_cost_form_with_fixed_supplier() {
        let (state, supplier_id, product_id) = cost_fixture_state().await;
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/suppliers/{supplier_id}/detail")).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        // Card B: posts to the existing backend, targets the drawer body, and
        // fixes the supplier — no supplier dropdown needed inside the drawer.
        for expected in [
            "Record Product Cost",
            "hx-post=\"/web/supplier-costs\"",
            "hx-target=\"#supplier-drawer-body\"",
            &format!("name=\"supplier_id\" value=\"{supplier_id}\""),
            &format!("value=\"{product_id}\""),
            "WEB-SUP-C",
        ] {
            assert!(html.contains(expected), "drawer must show {expected}: {html:.800}");
        }
        assert!(
            !html.contains("name=\"supplier_id\" required")
                && !html.contains("<select name=\"supplier_id\""),
            "drawer must not offer a supplier dropdown: {html:.800}"
        );
        // The purchases card is untouched.
        assert!(html.contains("Purchases ("), "drawer must keep the purchases card: {html:.400}");
    }

    #[tokio::test]
    async fn web_record_cost_from_drawer_returns_fresh_detail_and_list_trigger() {
        let (state, supplier_id, product_id) = cost_fixture_state().await;
        let app = crate::routes::router(state);
        let body = format!("product_id={product_id}&supplier_id={supplier_id}&cost=12.50&date=2024-05-01");
        let (status, headers, html) = post_form_full(
            app.clone(),
            "/web/supplier-costs",
            &body,
            &[("HX-Target", "supplier-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        // The drawer target gets the fresh detail fragment (cost form intact,
        // header re-rendered), not the list fragment.
        for expected in [
            "supplier-detail-inner",
            "Web Cost Supplier",
            "hx-post=\"/web/supplier-costs\"",
        ] {
            assert!(html.contains(expected), "drawer answer must show {expected}: {html:.600}");
        }
        assert!(
            !html.contains("supplier-list-inner"),
            "drawer answer must not be the list fragment: {html:.400}"
        );
        // The existing trigger fires so the page listener refreshes the list.
        let trigger = headers
            .get("HX-Trigger")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            trigger.contains("supplier-cost-recorded"),
            "drawer answer must fire supplier-cost-recorded, got {trigger:?}"
        );

        // Non-drawer HTMX callers keep the historical list-fragment answer.
        // A later date keeps the satellite's newer-date rule satisfied.
        let body2 = format!("product_id={product_id}&supplier_id={supplier_id}&cost=13.50&date=2024-05-02");
        let (status, headers, html) =
            post_form_full(app, "/web/supplier-costs", &body2, &[]).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("supplier-list-inner"),
            "page callers must keep getting the list fragment: {html:.400}"
        );
        let trigger = headers
            .get("HX-Trigger")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            trigger.contains("supplier-cost-recorded"),
            "list answer must keep the trigger, got {trigger:?}"
        );
    }

    #[tokio::test]
    async fn web_pay_supplier_from_drawer_reduces_balance_in_place() {
        use crate::models::{
            NewProduct, NewPurchase, NewSupplier, PaymentType, ProductKind, TransactionKind,
        };
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        let state = test_state().await;
        // A funded account with an owning Cash method: paying posts an Expense,
        // so with overdraft blocked the account needs money before the handover.
        // The name "Caja" makes `ensure_defaults_for_account` assign Cash.
        let account = state.account_service.create(audit_actor(&state).await, "Caja").await.unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(audit_actor(&state).await, account.id, "Caja")
            .await
            .unwrap();
        let cash = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("Cash is seeded")
            .id;
        state
            .transaction_service

            .create(
                audit_actor(&state).await,
                account.id,
                TransactionKind::Income,
                Decimal::from(1000),
                Some("seed".into()),
                NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();

        let supplier = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Web Pay Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: "WEB-PAY-C".into(),
                name: "prod WEB-PAY-C".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::ZERO),
                max_stock: Some(Decimal::from(100)),
                location: None,
                notes: None,
            })
            .await
            .unwrap();
        // One Confirmed Credit purchase: 3 × 25 = 75 due.
        let purchase = state
            .purchases_service
            .create_draft(NewPurchase {
                supplier_id: supplier.id,
                payment_type: PaymentType::Credit,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(
                purchase.id,
                product.id,
                Decimal::from(3),
                Some(Decimal::from(25)),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(audit_actor(&state).await, purchase.id, None)
            .await
            .unwrap();

        let app = crate::routes::router(state);
        // The drawer fragment carries the pay card wired to the collection
        // endpoint: method select, no notes field.
        let (status, html) =
            get_html(app.clone(), &format!("/web/suppliers/{}/detail", supplier.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "Pay supplier",
            "hx-post=\"/web/supplier-payments\"",
            "hx-target=\"#supplier-drawer-body\"",
            &format!("name=\"supplier_id\" value=\"{}\"", supplier.id),
            "name=\"method_id\"",
            "name=\"amount\"",
            "name=\"date\"",
        ] {
            assert!(html.contains(expected), "drawer must show {expected}: {html:.800}");
        }
        assert!(
            !html.contains("name=\"notes\""),
            "the supplier pay form must not offer a notes field: {html:.600}"
        );

        // Pay 30 of the 75 due from the drawer: the fresh fragment comes back
        // with the reduced balance and fires the list-refresh trigger.
        let body = format!(
            "supplier_id={}&method_id={cash}&amount=30&date=2024-06-20",
            supplier.id
        );
        let (status, headers, html) = post_form_full(
            app,
            "/web/supplier-payments",
            &body,
            &[("HX-Target", "supplier-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in ["supplier-detail-inner", "Web Pay Supplier", ">45 <"] {
            assert!(html.contains(expected), "drawer answer must show {expected}: {html:.600}");
        }
        assert!(
            !html.contains("supplier-list-inner"),
            "drawer answer must not be the list fragment: {html:.400}"
        );
        let trigger = headers
            .get("HX-Trigger")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            trigger.contains("supplier-paid"),
            "drawer answer must fire supplier-paid, got {trigger:?}"
        );
    }

    // -- S7 enforcement (AC10): the permission gates on the real handlers ------

    /// Like [`get_html`], but with an explicit cookie: `None` means the truly
    /// anonymous request (the shared TEST_COOKIE belongs to the
    /// full-permission principal).
    async fn get_html_as(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Like [`post_form`], but returning the full body with an explicit
    /// cookie and optional headers: an empty `HX-Request` set means the plain
    /// browser post the full-page refusal shape needs.
    async fn post_form_as(
        app: axum::Router,
        uri: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The read gates are real too: a principal WITHOUT `suppliers.read` (it
    /// holds an unrelated permission, so this is not a broken fixture) is
    /// refused the page, the list fragment and the edit form with the
    /// full-page refusal card.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let (state, supplier_id, _product_id) = cost_fixture_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        for uri in [
            "/suppliers".to_string(),
            "/web/suppliers".to_string(),
            format!("/web/suppliers/{supplier_id}/edit-form"),
            format!("/web/suppliers/{supplier_id}/detail"),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.200}");
            assert!(
                html.contains("Acción no permitida") && html.contains("suppliers.read"),
                "{uri} must refuse naming suppliers.read: {html:.300}"
            );
        }
    }

    /// The drawer carries the double gate (the S5 product drawer's shape):
    /// the per-supplier costs it renders are the SAME data the product drawer
    /// gates `purchases.costs.read`, so one dataset never answers two
    /// permissions. A suppliers-only principal is refused naming the costs
    /// gate; a costs-only principal naming `suppliers.read`; a principal
    /// holding both reads it like `deposito` does.
    #[tokio::test]
    async fn the_supplier_drawer_carries_the_double_gate_like_the_product_drawer() {
        let (state, supplier_id, _product_id) = cost_fixture_state().await;
        let app = crate::routes::router(state.clone());
        let uri = format!("/web/suppliers/{supplier_id}/detail");

        // suppliers.read only: the costs gate refuses, HTMX JSON shape.
        let probe = test_support::seed_session_with_permissions(&state.pool, &["suppliers.read"])
            .await
            .unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("HX-Request", "true")
            .header("cookie", test_support::cookie_for(&probe))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("purchases.costs.read"),
            "the drawer must refuse a suppliers-only principal naming the costs gate: {json}"
        );

        // purchases.costs.read only: the suppliers gate refuses.
        let probe =
            test_support::seed_session_with_permissions(&state.pool, &["purchases.costs.read"])
                .await
                .unwrap();
        let (status, html) = get_html_as(app.clone(), &uri, Some(&test_support::cookie_for(&probe))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.200}");
        assert!(
            html.contains("Acción no permitida") && html.contains("suppliers.read"),
            "the drawer must refuse a costs-only principal naming suppliers.read: {html:.300}"
        );

        // Both gates held: the drawer opens, like the seeded deposito.
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["suppliers.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let (status, html) = get_html_as(app, &uri, Some(&test_support::cookie_for(&probe))).await;
        assert_eq!(status, StatusCode::OK, "{html:.200}");
        assert!(
            html.contains("supplier-detail-inner"),
            "the drawer must open for a principal holding both gates: {html:.300}"
        );
    }

    /// A principal holding ONLY the read permissions opens the reads and is
    /// refused every web mutation, each naming its own code: supplier entity
    /// writes `suppliers.write`, the per-supplier cost record
    /// `purchases.costs.write` (the same code the product drawer uses), and a
    /// supplier payment `purchases.create` — the payment is a purchase-side
    /// movement, the S7 judgement call recorded in the mapping.
    #[tokio::test]
    async fn ac10_a_suppliers_read_only_principal_is_refused_the_web_mutations() {
        let (state, supplier_id, product_id) = cost_fixture_state().await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["suppliers.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The reads the probe is allowed: the page, the fragment, the drawer
        // and the edit form.
        for uri in [
            "/suppliers".to_string(),
            "/web/suppliers".to_string(),
            format!("/web/suppliers/{supplier_id}/detail"),
            format!("/web/suppliers/{supplier_id}/edit-form"),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {html:.200}");
        }

        // Creating a supplier over HTMX: JSON naming the entity gate.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/suppliers",
            "name=Denied Supplier",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("suppliers.write"),
            "the HTMX refusal must name suppliers.write: {body}"
        );

        // The same create as a plain browser post: the HTML refusal card.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/suppliers",
            "name=Denied Supplier",
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Acción no permitida") && body.contains("suppliers.write"),
            "the refusal must speak Spanish and name the gate: {body:.400}"
        );

        // Edit, activate, deactivate and delete: the entity gate, each in the
        // shape its caller reads.
        for (uri, body) in [
            ("/web/suppliers/edit".to_string(), format!("id={supplier_id}&name=hacked")),
            (
                format!("/web/suppliers/{supplier_id}/activate"),
                String::new(),
            ),
            (
                format!("/web/suppliers/{supplier_id}/deactivate"),
                String::new(),
            ),
        ] {
            let (status, body) = post_form_as(
                app.clone(),
                &uri,
                &body,
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(
                body.contains("suppliers.write"),
                "{uri} must name suppliers.write: {body}"
            );
        }
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/web/suppliers/{supplier_id}").as_str())
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("suppliers.write"),
            "the delete refusal must name suppliers.write"
        );

        // The per-supplier cost record: the costs write tier, from the drawer.
        let body = format!("product_id={product_id}&supplier_id={supplier_id}&cost=10&date=2024-05-01");
        let (status, body) = post_form_as(
            app.clone(),
            "/web/supplier-costs",
            &body,
            &[("HX-Request", "true"), ("HX-Target", "supplier-drawer-body")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("purchases.costs.write"),
            "the refusal must name purchases.costs.write: {body}"
        );

        // Paying a supplier is a purchase-side movement: purchases.create,
        // NOT suppliers.write (the S7 judgement call).
        let body = format!("supplier_id={supplier_id}&method_id=1&amount=5&date=2024-05-03");
        let (status, body) = post_form_as(
            app,
            "/web/supplier-payments",
            &body,
            &[("HX-Request", "true"), ("HX-Target", "supplier-drawer-body")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("purchases.create"),
            "the refusal must name purchases.create: {body}"
        );
    }

    /// The refusal writes nothing: the refused supplier create leaves the
    /// suppliers table where it was, the refused cost records no satellite
    /// row, and a refused delete keeps the row.
    #[tokio::test]
    async fn ac10_the_suppliers_web_refusal_writes_nothing() {
        let (state, supplier_id, product_id) = cost_fixture_state().await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["suppliers.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let suppliers_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM suppliers")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app.clone(),
            "/web/suppliers",
            "name=Denied Supplier",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.200}");
        let suppliers_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM suppliers")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(suppliers_after, suppliers_before, "a refused create must write nothing");

        let costs_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM product_supplier_costs")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let body = format!("product_id={product_id}&supplier_id={supplier_id}&cost=10&date=2024-05-01");
        let (status, body) = post_form_as(
            app.clone(),
            "/web/supplier-costs",
            &body,
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let costs_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM product_supplier_costs")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(costs_after, costs_before, "a refused cost must write nothing");

        // A refused delete keeps the supplier row.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/web/suppliers/{supplier_id}").as_str())
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let still: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM suppliers WHERE id = ?")
            .bind(supplier_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(still.0, 1, "a refused delete must keep the supplier row");
    }

    /// A principal holding the permissions gets the normal answers: supplier
    /// CRUD, the cost record through the drawer, the supplier-level payment
    /// reducing the balance in place, and the delete of an unused supplier.
    #[tokio::test]
    async fn ac10_the_suppliers_web_holding_principal_gets_the_normal_answer() {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, ProductKind, TransactionKind};
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        let state = test_state().await;
        // The funded account with its owning Cash method, so a payment can run.
        let account = state.account_service.create(audit_actor(&state).await, "Caja").await.unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(audit_actor(&state).await, account.id, "Caja")
            .await
            .unwrap();
        let cash = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("Cash is seeded")
            .id;
        state
            .transaction_service

            .create(
                audit_actor(&state).await,
                account.id,
                TransactionKind::Income,
                Decimal::from(1000),
                Some("seed".into()),
                NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Holder Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: "HOLDER-SUP-P".into(),
                name: "prod HOLDER-SUP-P".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::ZERO),
                max_stock: Some(Decimal::from(100)),
                location: None,
                notes: None,
            })
            .await
            .unwrap();
        // One Confirmed credit purchase: 3 × 25 = 75 due.
        let purchase = state
            .purchases_service
            .create_draft(NewPurchase {
                supplier_id: supplier.id,
                payment_type: crate::models::PaymentType::Credit,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(purchase.id, product.id, Decimal::from(3), Some(Decimal::from(25)))
            .await
            .unwrap();
        state.purchases_service.confirm(audit_actor(&state).await, purchase.id, None).await.unwrap();

        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &[
                "suppliers.read",
                "suppliers.write",
                "purchases.costs.read",
                "purchases.costs.write",
                "purchases.create",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let app = crate::routes::router(state.clone());

        // Create + edit + lifecycle: their normal answers.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/suppliers",
            "name=Holder Supplier 2",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.200}");
        let (status, body) = post_form_as(
            app.clone(),
            "/web/suppliers/edit",
            &format!("id={}&name=Holder Supplier Renamed", supplier.id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.200}");
        for uri in [
            format!("/web/suppliers/{}/activate", supplier.id),
            format!("/web/suppliers/{}/deactivate", supplier.id),
        ] {
            let (status, body) = post_form_as(
                app.clone(),
                &uri,
                "",
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body:.200}");
        }

        // Record a cost from the drawer: fresh detail + the trigger.
        // A later date keeps the satellite's newer-date rule satisfied (the
        // confirmed purchase's line already set a current cost at 2024-05-02).
        let body = format!(
            "product_id={}&supplier_id={}&cost=12.50&date=2024-05-05",
            product.id, supplier.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/supplier-costs")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("HX-Target", "supplier-drawer-body")
            .header("cookie", &cookie)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert_eq!(status, StatusCode::OK, "{body:.200}");
        assert!(body.contains("supplier-detail-inner"), "{body:.200}");

        // Pay 30 of the 75 due from the drawer: the balance drops in place.
        let body = format!(
            "supplier_id={}&method_id={cash}&amount=30&date=2024-06-20",
            supplier.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/supplier-payments")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("HX-Target", "supplier-drawer-body")
            .header("cookie", &cookie)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert_eq!(status, StatusCode::OK, "{body:.200}");
        assert!(body.contains(">45 <"), "the balance must drop to 45: {body:.400}");

        // Delete a supplier with no rows: its normal list answer.
        let empty = state
            .supplier_service
            .create_supplier(NewSupplier {
                name: "Empty Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/web/suppliers/{}", empty.id).as_str())
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The gate order must not change: an anonymous request gets the login
    /// redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let (state, _supplier_id, _product_id) = cost_fixture_state().await;
        let app = crate::routes::router(state);
        let (status, _) = get_html_as(app.clone(), "/suppliers", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        // An HTMX anonymous request is the 401 + HX-Redirect shape, so the
        // plain browser post is the one that answers the login redirect.
        let (status, _) = post_form_as(
            app,
            "/web/suppliers",
            "name=Anonymous",
            &[],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }
}
