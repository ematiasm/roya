use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    MovementReason, MovementType, NewMovement, NewProduct, Product, ProductKind,
    ProductStock, ProductSupplierCost, UpdateProduct,
};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, ProductSupplierCostRepository,
    StockMovementRepository,
};
use crate::routes::AppState;
use crate::security::authz::{
    InventoryRead, InventoryStockWrite, InventoryWrite, Nav, PurchasesCostsRead,
    PurchasesCostsWrite, Require,
};

// S5 enforcement mapping (products screen). The screen mixes capabilities, so
// every handler declares its own extractor — reads `inventory.read`, product
// and category mutations `inventory.write`, stock movements
// `inventory.stock.write`. The per-supplier cost surface keeps the codes of
// the module that OWNS the data even though the form lives in the product
// drawer: cost record/preferred → `purchases.costs.write`, and the drawer
// fragment itself renders those cost rows, so its read requires
// `purchases.costs.read` on top of `inventory.read` (the seeded `deposito`
// role holds both; a principal with only `inventory.read` gets the refusal
// instead of cost rows it is not allowed to see).

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "products.html")]
struct ProductsTemplate {
    products: Vec<ProductStock>,
    categories: Vec<crate::models::Category>,
    allow_negative_stock: bool,
    nav_key: &'static str,
    /// Current filter values, so the form reflects a bookmarkable `/products?q=…`.
    filter_q: String,
    filter_category: String,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/product_list.html")]
struct ProductListPartial {
    products: Vec<ProductStock>,
}

/// The server-rendered create-under-filter notice (issue #37). The box lives
/// in `templates/partials/notice.html`, whose comment carries the full
/// transport and mirroring contract; the important placement fact: the class
/// tokens must stay scanned, and Tailwind scans only `templates/`, so this
/// markup cannot live in a Rust string like `web_category_options`' option
/// list (that builder writes classless `<option>` elements, so it never
/// needed the scan). Askama escapes `{{ product_name }}` on output — the same
/// escaping guarantee the hand `html_escape` gave, plus `'` — so a name made
/// of markup characters reaches the operator as text, not HTML.
#[derive(Template)]
#[template(path = "partials/notice.html")]
struct HiddenByFilterNotice {
    product_name: String,
}

#[derive(Template)]
#[template(path = "partials/stock_list.html")]
struct StockListPartial {
    /// One row per stock-tracked product with its audit actor resolved to a
    /// display name (see `StockRow` below).
    items: Vec<StockRow>,
}

/// One row of the low/negative stock fragment with its audit actors resolved
/// to display names (M5 Phase B, slice S10): the department returns ids, the
/// wiring layer resolves them, so the interface shows a name and never an id.
struct StockRow {
    ps: ProductStock,
    /// Display name of the product's creator ("Registrado por"); `None` only
    /// when the id resolves to nothing (a concurrent deactivation).
    created_by_name: Option<String>,
    /// Display name of the last editor, when the product was edited at all.
    updated_by_name: Option<String>,
}

/// The picker results fragment. Generic on purpose: the record page supplies the
/// line action and swap target, so the purchase record page reuses it unchanged.
#[derive(Template)]
#[template(path = "partials/product_search_results.html")]
struct ProductSearchResultsPartial {
    query: String,
    matches: Vec<ProductStock>,
    line_action: String,
    line_target: String,
    /// True when the calling context buys: show the cost, not the sale price.
    show_cost: bool,
}

/// The picker island's wire row (N5). Flattened on purpose: the island renders a
/// name, a SKU, one price and a stock figure, so the wire carries exactly that
/// instead of the whole `ProductStock` with its nested product. Both prices
/// travel and the island picks by its own context, which is what keeps the
/// `price` parameter off the request entirely.
#[derive(Debug, Serialize)]
struct ProductSearchRow {
    id: i64,
    name: String,
    sku: String,
    sale_price: Decimal,
    cost_price: Decimal,
    stock: Decimal,
}

/// One satellite cost row with the supplier fields the drawer needs to render it.
#[derive(Clone)]
pub struct ProductCostView {
    pub cost: ProductSupplierCost,
    pub supplier_name: String,
}

/// Derived, never stored: the supplier reference cost against the product's
/// stored cost. `Some` only when the two genuinely disagree, because that is
/// the only state worth showing.
#[derive(Debug, Clone, Serialize)]
pub struct StaleCostView {
    pub reference: Decimal,
    pub stored: Decimal,
}

impl StaleCostView {
    /// Display form of the reference cost, same rule as `Product`'s display
    /// methods so the two numbers of the gap render consistently.
    pub fn reference_display(&self) -> String {
        crate::models::money_display(self.reference)
    }

    /// Display form of the stored cost, same rule as above.
    pub fn stored_display(&self) -> String {
        crate::models::money_display(self.stored)
    }
}

/// The product slide-over drawer body: the header with derived stock and the
/// inline edit form, the per-supplier cost satellite (record/switch preferred)
/// and the stock movement form. Field names are the template task's contract.
/// The audit actors are display names resolved in the wiring layer (M5 Phase
/// B, slice S10) — the header shows "Registrado por"/"Actualizado por" like
/// the finance detail does.
#[derive(Template)]
#[template(path = "partials/product_detail.html")]
struct ProductDetailPartial {
    product: Product,
    stock: Decimal,
    suggested: Option<Decimal>,
    /// Display name of the product's creator ("Registrado por"). Every
    /// product has one (`created_by` is NOT NULL); it renders even when the
    /// actor is the migration's sentinel.
    created_by_name: Option<String>,
    /// Display name of the last editor ("Actualizado por"), only rendered
    /// when the product has been edited at all.
    updated_by_name: Option<String>,
    categories: Vec<crate::models::Category>,
    supplier_costs: Vec<ProductCostView>,
    suppliers: Vec<crate::models::Supplier>,
    stale_cost: Option<StaleCostView>,
    today: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// The catalogue rows for a body-borne filter. The mutation forms carry
/// `hx-include="#product-filters"` — the mechanism issue #33 established for
/// the lifecycle forms — so the answer is built from the list the operator is
/// actually looking at instead of the whole catalogue. Empty strings mean "no
/// constraint", the same lenient parsing the list page uses, so a filterless
/// caller keeps the whole catalogue. One helper so the lenient parsing stays
/// written once: the create answer reuses these rows for both the fragment and
/// the create-under-filter derivation (issue #37).
async fn filtered_products(
    state: &AppState,
    q: &str,
    category_id: &str,
) -> AppResult<Vec<ProductStock>> {
    let filter = WebProductFilter {
        q: Some(q.to_string()),
        category_id: Some(category_id.to_string()),
    };
    let (query, category) = filter.parsed();
    state
        .inventory_service
        .filter_products(&query, category)
        .await
}

async fn filtered_list_html(state: &AppState, q: &str, category_id: &str) -> AppResult<String> {
    let products = filtered_products(state, q, category_id).await?;
    render_product_list(&products)
}

fn render_product_list(products: &[ProductStock]) -> AppResult<String> {
    ProductListPartial {
        products: products.to_vec(),
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))
}

/// Whether the body-borne catalogue filter actually constrains the list: a
/// non-empty search text or a category that parses. Unparseable values stay
/// inert — the lenient rule `WebProductFilter::parsed()` applies — so a stray
/// `category_id=abc` must not be treated as an active filter.
fn body_filter_is_active(q: &str, category_id: &str) -> bool {
    !q.trim().is_empty()
        || (!category_id.trim().is_empty() && category_id.trim().parse::<i64>().is_ok())
}

/// Render the create-under-filter notice for one product name. One place by
/// construction: the markup and its contract live in
/// `templates/partials/notice.html`, which cross-references this struct and
/// `base.html`'s `notice()` box it mirrors; edit the two together.
///
/// `data-notice-server="true"` on the box marks it for base.html's generic
/// `htmx:afterRequest` notice: htmx runs the swap phase (beforeSwap →
/// afterSwap) BEFORE afterRequest — measured against the vendored 1.9.12 in a
/// real browser — so the generic "Create product saved" would otherwise land
/// last and replace this box. The guard skips that generic notice when the
/// response body itself carries this marker, and both directions fail safe:
/// an ordinary create (no marker) keeps the generic notice, and if the server
/// ever stops rendering this box the generic notice comes back untouched.
fn hidden_by_filter_notice_html(product_name: &str) -> AppResult<String> {
    HiddenByFilterNotice {
        product_name: product_name.to_string(),
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))
}

fn triggered(html: String, event: &str) -> axum::response::Response {
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", event.parse().unwrap());
    resp
}

/// Adds a second trigger header to an already-built response. Plain `HX-Trigger`
/// events fire before htmx performs the swap, so a trigger that must land after
/// the swap (the drawer close) rides `HX-Trigger-After-Settle`: htmx dispatches
/// it on the body once settling completes, after any `htmx:afterSwap` open.
fn triggered_after_settle(resp: axum::response::Response, event: &str) -> axum::response::Response {
    let mut resp = resp;
    resp.headers_mut()
        .insert("HX-Trigger-After-Settle", event.parse().unwrap());
    resp
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

async fn products_page(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(q): Query<WebProductFilter>,
) -> Result<Html<String>, AppError> {
    let (query, category_id) = q.parsed();
    let products = state
        .inventory_service
        .filter_products(&query, category_id)
        .await?;
    let categories = state.inventory_service.categories.list().await?;
    let tmpl = ProductsTemplate {
        products,
        categories,
        allow_negative_stock: state.allow_negative_stock,
        nav_key: "products",
        filter_q: query,
        filter_category: q.category_id.as_deref().unwrap_or("").trim().to_string(),
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct WebProductFilter {
    #[serde(default)]
    pub category_id: Option<String>,
    /// Text search over name, SKU and barcode, matched by the inventory service.
    #[serde(default)]
    pub q: Option<String>,
}

impl WebProductFilter {
    /// The search text and the parsed category. An empty or unparseable value is
    /// treated as "no constraint", matching the lenient parsing the list already
    /// used, so a stray value never turns a bookmark into an error.
    fn parsed(&self) -> (String, Option<i64>) {
        let query = self.q.as_deref().unwrap_or("").trim().to_string();
        let category = self
            .category_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse().ok());
        (query, category)
    }
}

async fn web_product_list(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Query(q): Query<WebProductFilter>,
) -> Result<Html<String>, AppError> {
    let (query, category_id) = q.parsed();
    let products = state
        .inventory_service
        .filter_products(&query, category_id)
        .await?;
    let html = ProductListPartial { products }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_low_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.low_stock().await?;
    let html = stock_list_html(&state, items).await?;
    Ok(Html(html))
}

async fn web_negative_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.negative_stock().await?;
    let html = stock_list_html(&state, items).await?;
    Ok(Html(html))
}

/// Resolve the stock rows' audit actors in the wiring layer and render the
/// fragment: one statement covers every row, the same way the finance detail
/// resolves its names.
async fn stock_list_html(state: &AppState, items: Vec<ProductStock>) -> AppResult<String> {
    let mut actor_ids = items.iter().map(|ps| ps.product.created_by).collect::<Vec<i64>>();
    actor_ids.extend(items.iter().filter_map(|ps| ps.product.updated_by));
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let rows = items
        .into_iter()
        .map(|ps| StockRow {
            created_by_name: name_for(ps.product.created_by),
            updated_by_name: ps.product.updated_by.and_then(name_for),
            ps,
        })
        .collect();
    StockListPartial { items: rows }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// The empty option's label is caller-owned: the catalogue filter says "All
/// categories", but the product form's empty choice means "no category".
#[derive(Debug, Deserialize, Default)]
pub struct CategoryOptionsQuery {
    #[serde(default)]
    pub empty: Option<String>,
}

async fn web_category_options(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Query(q): Query<CategoryOptionsQuery>,
) -> Result<Html<String>, AppError> {
    let cats = state.inventory_service.categories.list().await?;
    let empty_label = q
        .empty
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or("All categories");
    let mut html = format!("<option value=\"\">{}</option>", html_escape(empty_label));
    for c in cats {
        html.push_str(&format!(
            "<option value=\"{}\">{}</option>",
            c.id,
            html_escape(&c.name)
        ));
    }
    Ok(Html(html))
}

async fn web_product_options(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> Result<Html<String>, AppError> {
    let products = state.inventory_service.products.list().await?;
    let mut html = String::new();
    for p in products {
        html.push_str(&format!(
            "<option value=\"{}\">{} — {}</option>",
            p.id,
            html_escape(&p.sku),
            html_escape(&p.name)
        ));
    }
    Ok(Html(html))
}

#[derive(Debug, Deserialize, Default)]
pub struct ProductSearchQuery {
    /// Documented query name.
    #[serde(default)]
    pub q: String,
    /// The picker input is named `product` because the same field feeds the line
    /// form; both names reach the same search.
    #[serde(default)]
    pub product: String,
    /// The record's line endpoint and swap target, supplied by the picker form so
    /// the fragment stays generic (sales and purchases share it).
    #[serde(default)]
    pub line_action: String,
    #[serde(default)]
    pub line_target: String,
    /// Which price the calling context works in: `cost` for a purchase line,
    /// `sale` (the default) for a sale line. Only that number is shown.
    #[serde(default)]
    pub price: String,
}

/// The picker input is named `product` because the same field feeds the line
/// form, while `q` is the documented name; both reach the same read. Shared by
/// the HTML fragment and the JSON route so the two cannot resolve differently.
fn resolve_search_query(params: &ProductSearchQuery) -> String {
    if params.q.trim().is_empty() {
        params.product.clone()
    } else {
        params.q.clone()
    }
}

/// `GET /web/product-search?q=`: the bounded picker read. Matching and stock
/// derivation live in the inventory service; the route only renders.
async fn web_product_search(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Query(params): Query<ProductSearchQuery>,
) -> Result<Html<String>, AppError> {
    let raw = resolve_search_query(&params);
    let matches = state.inventory_service.search_products(&raw).await?;
    let html = ProductSearchResultsPartial {
        query: raw.trim().to_string(),
        matches,
        line_action: params.line_action.trim().to_string(),
        line_target: params.line_target.trim().to_string(),
        show_cost: params.price.trim().eq_ignore_ascii_case("cost"),
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// `GET /web/product-search.json?q=`: the picker island's read (N5). The same
/// bounded service read as the HTML fragment, flattened to the row shape the
/// island actually renders. No `line_action`, `line_target` or `price` on this
/// route: the island owns its own context, so none of it needs to travel.
///
/// The typed-name resolution the island deliberately leaves to the server (Enter
/// posts `product` to the line route) is unaffected by this read — this route
/// only feeds the dropdown.
async fn web_product_search_json(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Query(params): Query<ProductSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = resolve_search_query(&params);
    let matches = state.inventory_service.search_products(&raw).await?;
    let products: Vec<ProductSearchRow> = matches
        .into_iter()
        .map(|ps| ProductSearchRow {
            id: ps.product.id,
            name: ps.product.name,
            sku: ps.product.sku,
            sale_price: ps.product.sale_price,
            cost_price: ps.product.cost_price,
            stock: ps.stock,
        })
        .collect();
    Ok(Json(serde_json::json!({
        "query": raw.trim(),
        "products": products,
    })))
}

/// `GET /web/products/detail/{id}`: the drawer fragment. Concrete ids sit last in
/// the path, matching the wiring guard the smoke suite enforces.
async fn web_product_detail(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    _: Require<PurchasesCostsRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    product_detail_html(&state, id).await
}

/// The drawer body with fresh derived data. The detail read and every mutating
/// drawer action answer it, so saving/costs/movements refresh the drawer in
/// place without the client rebuilding a URL. The audit actors are resolved
/// HERE, in the wiring layer, because a department may not read identity
/// tables (AC20) and the view must show a name, never an id — the same way
/// the finance detail does it.
async fn product_detail_html(state: &AppState, id: i64) -> AppResult<Html<String>> {
    let ps = state.inventory_service.product_stock(id).await?;
    let categories = state.inventory_service.categories.list().await?;
    let suppliers = state.supplier_service.list_suppliers().await?;
    let costs = state.supplier_service.costs.list_by_product(id).await?;
    let supplier_costs = costs
        .into_iter()
        .map(|cost| {
            let supplier_name = suppliers
                .iter()
                .find(|s| s.id == cost.supplier_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| format!("supplier #{}", cost.supplier_id));
            ProductCostView { cost, supplier_name }
        })
        .collect();
    let mut actor_ids = vec![ps.product.created_by];
    actor_ids.extend(ps.product.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let created_by_name = name_for(ps.product.created_by);
    let updated_by_name = ps.product.updated_by.and_then(name_for);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Derived, never stored (cost-freshness S1): the supplier reference cost
    // disagrees with the stored cost only when there IS a supplier truth to
    // compare against (no rows ⇒ the product column IS the truth), the stored
    // cost was ever recorded (0 is the NOT NULL DEFAULT, "no cost yet", not a
    // cost), and the two genuinely differ (equal ⇒ fresh).
    let stale_cost = match (state.supplier_service.reference_cost(id).await?, ps.product.cost_price != Decimal::ZERO) {
        (Some(r), true) if r != ps.product.cost_price => Some(StaleCostView {
            reference: r,
            stored: ps.product.cost_price,
        }),
        _ => None,
    };
    let html = ProductDetailPartial {
        product: ps.product,
        stock: ps.stock,
        suggested: ps.suggested,
        created_by_name,
        updated_by_name,
        categories,
        supplier_costs,
        suppliers,
        stale_cost,
        today,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Forms (HTMX, mirror dashboard patterns)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateCategoryForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateProductForm {
    #[serde(default)]
    pub sku: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    // `category_id` here is the catalogue FILTER's key, not the product's own
    // category: `hx-include="#product-filters"` merges the live filter form
    // into this body (issue #37), so the answer is the list the operator is
    // looking at.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category_id: String,
    // The rename was forced, not stylistic: the modal's own category select
    // used to ride `category_id`, but that key now belongs to the filter and
    // one body cannot carry two values under the same key.
    #[serde(default)]
    pub product_category_id: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub sale_price: String,
    #[serde(default)]
    pub cost_price: String,
    // Empty means "no markup": the price stays manual. Kept as a String so an
    // unparseable value can surface its own validation error instead of a
    // deserialization rejection.
    #[serde(default)]
    pub markup_pct: String,
    #[serde(default)]
    pub track_stock: Option<String>,
    #[serde(default)]
    pub min_stock: String,
    #[serde(default)]
    pub max_stock: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
}

/// The drawer's inline edit form always sends every field, so each field is
/// wrapped in `Some(...)` and empty optional values arrive as an explicit
/// `Some(None)` (clear), never as "leave unchanged".
#[derive(Debug, Deserialize)]
pub struct EditProductForm {
    pub id: i64,
    #[serde(default)]
    pub sku: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub category_id: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub sale_price: String,
    #[serde(default)]
    pub cost_price: String,
    // The drawer always sends the field: an empty value is an explicit CLEAR
    // back to a manual price (`Some(None)` in the update), matching the form's
    // "empty optional values are a clear" contract.
    #[serde(default)]
    pub markup_pct: String,
    #[serde(default)]
    pub track_stock: Option<String>,
    #[serde(default)]
    pub min_stock: String,
    #[serde(default)]
    pub max_stock: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordProductCostForm {
    pub product_id: i64,
    pub supplier_id: i64,
    // The catalogue filter rides the body via `hx-include="#product-filters"`
    // (issue #37): the non-drawer answer renders the list, so it must render
    // the list the operator is looking at. The drawer edit form keeps its own
    // `category_id` unchanged because it carries no `hx-include`.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category_id: String,
    #[serde(default)]
    pub cost: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct PreferredCostForm {
    pub product_id: i64,
    pub supplier_id: i64,
    // Same body-borne filter as the other product mutations (issue #37); the
    // drawer branch ignores these keys — it answers the detail fragment.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category_id: String,
}

/// Shared hidden-id form for the drawer lifecycle actions (activate/deactivate/
/// delete). Never a concrete id in the path, like the customers drawer. The
/// catalogue filter rides along via `hx-include="#product-filters"`: the
/// answer renders the list, so it must render the list the operator is looking
/// at (issue #33). The form owns only `product_id`, so the filter keys cannot
/// collide with it.
#[derive(Debug, Deserialize)]
pub struct ProductIdForm {
    pub product_id: i64,
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateMovementForm {
    pub product_id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub date: String,
    // Same body-borne filter as the other product mutations (issue #37); the
    // drawer branch ignores these keys — it answers the detail fragment.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category_id: String,
}

fn parse_opt_decimal(s: &str) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    Decimal::from_str(t)
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid decimal: {s}")))
}

fn parse_opt_i64(s: &str) -> AppResult<Option<i64>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid id: {s}")))
}

async fn web_create_category(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<CreateCategoryForm>,
) -> Result<axum::response::Response, AppError> {
    let parent_id = parse_opt_i64(&form.parent_id)?;
    state
        .inventory_service
        .create_category(principal.user_id, &form.name, parent_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = Html(String::new()).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "category-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

async fn web_create_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<CreateProductForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: ProductKind = if form.kind.trim().is_empty() {
        ProductKind::Product
    } else {
        form.kind
            .parse()
            .map_err(AppError::Validation)?
    };
    // The markup is parsed BEFORE the price gate: the gate now depends on it.
    let markup_pct = if form.markup_pct.trim().is_empty() {
        None
    } else {
        Some(
            Decimal::from_str(form.markup_pct.trim())
                .map_err(|_| AppError::Validation("invalid markup_pct".into()))?,
        )
    };
    // An empty sale_price is only an error when the price is manual. With a
    // markup the service DERIVES and validates the price and ignores the
    // incoming one, so Decimal::ZERO is a safe placeholder that can never
    // reach the database.
    let sale_price = if form.sale_price.trim().is_empty() {
        if markup_pct.is_none() {
            return Err(AppError::Validation("sale_price is required".into()));
        }
        Decimal::ZERO
    } else {
        Decimal::from_str(form.sale_price.trim())
            .map_err(|_| AppError::Validation("invalid sale_price".into()))?
    };
    let cost_price = if form.cost_price.trim().is_empty() {
        Decimal::ZERO
    } else {
        Decimal::from_str(form.cost_price.trim())
            .map_err(|_| AppError::Validation("invalid cost_price".into()))?
    };
    // Checkbox: present means checked (value "1"/"on"/"true"); absent means false.
    // A hidden default of checked in the template sends Some("1").
    let track_stock = match form.track_stock.as_deref() {
        None => false,
        Some(v) => v == "1" || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true"),
    };
    let input = NewProduct {
        sku: form.sku,
        name: form.name,
        kind,
        category_id: parse_opt_i64(&form.product_category_id)?,
        unit: if form.unit.trim().is_empty() {
            "un".to_string()
        } else {
            form.unit
        },
        sale_price,
        cost_price,
        track_stock,
        min_stock: parse_opt_decimal(&form.min_stock)?,
        max_stock: parse_opt_decimal(&form.max_stock)?,
        location: if form.location.trim().is_empty() {
            None
        } else {
            Some(form.location)
        },
        notes: if form.notes.trim().is_empty() {
            None
        } else {
            Some(form.notes)
        },
        // Parsed from the form above: `Some` derives and validates the price
        // in the service; `None` (empty field) keeps the price manual.
        markup_pct: markup_pct,
    };
    let created = state.inventory_service.create_product(principal.user_id, input).await?;
    if is_htmx(&headers) {
        // The answer is the list the caller is looking at: the filter rides the
        // body via `hx-include="#product-filters"` (issue #37), so an active
        // filter narrows this fragment and `product-created` still refreshes it.
        // ONE filtered query serves both the fragment and the under-filter
        // check: membership is read off the very rows the answer renders, so a
        // second query run can never disagree with the list actually shown.
        let products = filtered_products(&state, &form.q, &form.category_id).await?;
        // Create-under-filter: the correctly filtered list does not hold the
        // fresh product, and the operator could read that as a failed create.
        // Product decision (issue #37): keep the filter and say what happened,
        // with a one-click way out. Membership is derived in the server from
        // the service's own matching, never from a parallel comparison written
        // against the form values, and only when the filter actually filters.
        let hidden = body_filter_is_active(&form.q, &form.category_id)
            && !products.iter().any(|p| p.product.id == created.id);
        let mut html = render_product_list(&products)?;
        if hidden {
            // Prepended so the out-of-band notice opens the answer body; htmx
            // removes the wrapper from the main swap either way. The
            // transport (body, not header), the template home and the marker
            // are documented on `hidden_by_filter_notice_html`.
            html = hidden_by_filter_notice_html(&created.name)? + &html;
        }
        let mut resp = Html(html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "product-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

async fn web_create_movement(
    State(state): State<AppState>,
    _: Require<InventoryStockWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<CreateMovementForm>,
) -> Result<axum::response::Response, AppError> {
    let movement_type: MovementType = form.kind.parse().map_err(AppError::Validation)?;
    let qty = Decimal::from_str(form.qty.trim())
        .map_err(|_| AppError::Validation("invalid qty".into()))?;
    let reason: MovementReason = if form.reason.trim().is_empty() {
        MovementReason::Initial
    } else {
        form.reason.parse().map_err(AppError::Validation)?
    };
    let date = if form.date.trim().is_empty() {
        chrono::Local::now().date_naive()
    } else {
        form.date
            .trim()
            .parse()
            .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?
    };
    let input = NewMovement {
        product_id: form.product_id,
        qty,
        movement_type,
        reason,
        reference: form.reference.trim().to_string(),
        date,
    };
    state.inventory_service.record_movement(principal.user_id, input).await?;
    if is_htmx(&headers) {
        // Drawer submissions target `#product-drawer-body`: answer the fresh
        // detail fragment (stock and header reloaded) and keep the existing
        // `movement-created` trigger so the page listener refreshes the list.
        // Any other HTMX caller keeps the historical list-fragment answer.
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.product_id).await?;
            return Ok(triggered(html.0, "movement-created"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let mut resp =
            Html(filtered_list_html(&state, &form.q, &form.category_id).await?).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "movement-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

/// Drawer edit: build a full patch from the form (it always sends every
/// field), then answer per caller — the drawer target gets the fresh
/// fragment, other HTMX callers get the list the caller is looking at, a
/// plain browser the redirect.
async fn web_edit_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Query(filter): Query<WebProductFilter>,
    Form(form): Form<EditProductForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: ProductKind = if form.kind.trim().is_empty() {
        ProductKind::Product
    } else {
        form.kind.parse().map_err(AppError::Validation)?
    };
    // The markup is parsed BEFORE the price gate: the gate now depends on it.
    let markup_pct = if form.markup_pct.trim().is_empty() {
        None
    } else {
        Some(
            Decimal::from_str(form.markup_pct.trim())
                .map_err(|_| AppError::Validation("invalid markup_pct".into()))?,
        )
    };
    // An empty sale_price is only an error when the price is manual. With a
    // markup the service DERIVES and validates the price and ignores the
    // incoming one, so Decimal::ZERO is a safe placeholder that can never
    // reach the database.
    let sale_price = if form.sale_price.trim().is_empty() {
        if markup_pct.is_none() {
            return Err(AppError::Validation("sale_price is required".into()));
        }
        Decimal::ZERO
    } else {
        Decimal::from_str(form.sale_price.trim())
            .map_err(|_| AppError::Validation("invalid sale_price".into()))?
    };
    let cost_price = if form.cost_price.trim().is_empty() {
        Decimal::ZERO
    } else {
        Decimal::from_str(form.cost_price.trim())
            .map_err(|_| AppError::Validation("invalid cost_price".into()))?
    };
    // Checkbox: present means checked; absent means false, like creation.
    let track_stock = match form.track_stock.as_deref() {
        None => false,
        Some(v) => v == "1" || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true"),
    };
    let patch = UpdateProduct {
        sku: Some(form.sku),
        name: Some(form.name),
        kind: Some(kind),
        // Empty optional values are an explicit clear, not "leave unchanged".
        category_id: Some(parse_opt_i64(&form.category_id)?),
        unit: Some(if form.unit.trim().is_empty() {
            "un".to_string()
        } else {
            form.unit
        }),
        sale_price: Some(sale_price),
        cost_price: Some(cost_price),
        track_stock: Some(track_stock),
        min_stock: Some(parse_opt_decimal(&form.min_stock)?),
        max_stock: Some(parse_opt_decimal(&form.max_stock)?),
        location: Some(
            if form.location.trim().is_empty() {
                None
            } else {
                Some(form.location)
            },
        ),
        notes: Some(
            if form.notes.trim().is_empty() {
                None
            } else {
                Some(form.notes)
            },
        ),
        // The drawer always sends the field: an empty markup is an explicit
        // clear back to a manual price (`Some(None)`); a value re-derives the
        // price from the cost.
        markup_pct: Some(markup_pct),
    };
    state
        .inventory_service
        .update_product(principal.user_id, form.id, patch)
        .await?;
    if is_htmx(&headers) {
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.id).await?;
            // Two events with distinct jobs: `product-changed` (pre-swap)
            // refreshes the lists, `product-saved` closes the drawer. The close
            // must fire after the swap so the `htmx:afterSwap` open cannot
            // resurrect the panel, hence the after-settle header.
            return Ok(triggered_after_settle(
                triggered(html.0, "product-changed"),
                "product-saved",
            ));
        }
        // The non-drawer branch takes the catalogue filter from the query
        // string, not the body: the form body already carries the product's
        // own `category_id`, so a body-borne filter would collide with that
        // key, and this branch has no in-repo caller today (issue #33 keeps it
        // filter-honest anyway).
        let (query, category_id) = filter.parsed();
        let products = state
            .inventory_service
            .filter_products(&query, category_id)
            .await?;
        let html = ProductListPartial { products }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(triggered(html, "product-changed"));
    }
    Ok(Redirect::to("/products").into_response())
}

/// Record (or shift) a per-supplier cost from the drawer, with the same parsing
/// rules as the supplier-side form: a required cost and a date that defaults to
/// today; the satellite's newer-date rule rejects older ones with 400.
async fn web_record_product_cost(
    State(state): State<AppState>,
    _: Require<PurchasesCostsWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<RecordProductCostForm>,
) -> Result<axum::response::Response, AppError> {
    let cost = Decimal::from_str(form.cost.trim())
        .map_err(|_| AppError::Validation("invalid cost".into()))?;
    let date = if form.date.trim().is_empty() {
        chrono::Local::now().date_naive()
    } else {
        form.date
            .trim()
            .parse()
            .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?
    };
    state
        .supplier_service
        .record_cost(principal.user_id, form.product_id, form.supplier_id, cost, date)
        .await?;
    if is_htmx(&headers) {
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.product_id).await?;
            return Ok(triggered(html.0, "product-cost-recorded"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let html = filtered_list_html(&state, &form.q, &form.category_id).await?;
        return Ok(triggered(html, "product-cost-recorded"));
    }
    Ok(Redirect::to("/products").into_response())
}

/// Move the preferred marker to one supplier for this product; the service
/// clears the previous preferred row and rejects unknown cost pairs (404).
async fn web_set_preferred_cost(
    State(state): State<AppState>,
    _: Require<PurchasesCostsWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<PreferredCostForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .supplier_service
        .set_preferred(principal.user_id, form.product_id, form.supplier_id)
        .await?;
    if is_htmx(&headers) {
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.product_id).await?;
            return Ok(triggered(html.0, "product-cost-recorded"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let html = filtered_list_html(&state, &form.q, &form.category_id).await?;
        return Ok(triggered(html, "product-cost-recorded"));
    }
    Ok(Redirect::to("/products").into_response())
}

/// Lifecycle actions share one three-way answer: HTMX gets the fresh list
/// fragment with `product-changed` (the drawer closes itself on success via
/// its `hx-on::after-request`), a plain browser gets the redirect.
async fn web_activate_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .inventory_service
        .set_product_active(principal.user_id, form.product_id, true)
        .await?;
    product_lifecycle_response(&state, &headers, &form).await
}

async fn web_deactivate_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .inventory_service
        .set_product_active(principal.user_id, form.product_id, false)
        .await?;
    product_lifecycle_response(&state, &headers, &form).await
}

async fn web_delete_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state.inventory_service.delete_product(form.product_id).await?;
    product_lifecycle_response(&state, &headers, &form).await
}

/// The shared answer for the lifecycle actions: list fragment + trigger for
/// HTMX callers, redirect for plain browsers. The fragment honours the
/// catalogue filter the caller is looking at — the forms carry
/// `#product-filters`, and an unfiltered answer here is one dropped trigger
/// away from a list that disagrees with its own filter controls (issue #33).
async fn product_lifecycle_response(
    state: &AppState,
    headers: &HeaderMap,
    form: &ProductIdForm,
) -> Result<axum::response::Response, AppError> {
    if is_htmx(headers) {
        // The same lenient parsing as the page: empty strings mean "no
        // constraint", so a filterless caller keeps the whole catalogue.
        let filter = WebProductFilter {
            q: Some(form.q.clone()),
            category_id: Some(form.category_id.clone()),
        };
        let (query, category_id) = filter.parsed();
        let products = state
            .inventory_service
            .filter_products(&query, category_id)
            .await?;
        let html = ProductListPartial { products }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(triggered(html, "product-changed"));
    }
    Ok(Redirect::to("/products").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/products", get(products_page))
        .route(
            "/web/products",
            get(web_product_list).post(web_create_product),
        )
        .route("/web/categories", post(web_create_category))
        .route("/web/category-options", get(web_category_options))
        .route("/web/product-options", get(web_product_options))
        .route("/web/product-search", get(web_product_search))
        .route("/web/product-search.json", get(web_product_search_json))
        .route("/web/products/detail/{id}", get(web_product_detail))
        .route("/web/products/edit", post(web_edit_product))
        .route("/web/products/activate", post(web_activate_product))
        .route("/web/products/deactivate", post(web_deactivate_product))
        .route("/web/products/delete", post(web_delete_product))
        .route("/web/product-costs", post(web_record_product_cost))
        .route("/web/product-costs/preferred", post(web_set_preferred_cost))
        .route("/web/stock-movements", post(web_create_movement))
        .route("/web/low-stock", get(web_low_stock))
        .route("/web/negative-stock", get(web_negative_stock))
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
    use crate::security::test_support;

    /// A valid acting user for fixture writes through the service: the
    /// migration's sentinel. Route-level tests authenticate through the
    /// seeded session, so the route's own actor is the session's user.
    async fn audit_actor_id(state: &AppState) -> i64 {
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

    /// The picker island reads JSON, so a body that is not JSON is a failure of
    /// the route, not of the test: report what actually came back.
    async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let (status, body) = get_html(app, uri).await;
        let json = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("expected JSON from {uri}, got: {body} ({e})"));
        (status, json)
    }

    // -- S5 enforcement (AC10): the permission gate on the real handlers ------

    /// A principal holding ONLY `inventory.read` is refused the product
    /// mutations in the shape each caller reads: an HTMX form gets the JSON
    /// the global notice box renders, a plain browser post gets the full-page
    /// refusal. The probe is a second session built for exactly this set.
    #[tokio::test]
    async fn ac10_an_inventory_read_only_principal_is_refused_the_htmx_mutations() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // Create product over HTMX: JSON refusal naming the gate.
        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products",
            "sku=HTMX-DENIED&name=x&kind=Product&unit=un&sale_price=10",
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the HTMX refusal must name inventory.write: {json}"
        );

        // Stock movement over HTMX: its own gate, named as such.
        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/stock-movements",
            "product_id=1&type=In&qty=2",
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.stock.write"),
            "the HTMX refusal must name inventory.stock.write: {json}"
        );

        // Editing a per-supplier cost is gated by the module that owns the
        // data: purchases.costs.write, even though the form lives in the
        // product drawer.
        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/product-costs",
            "product_id=1&supplier_id=1&cost=5",
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("purchases.costs.write"),
            "the HTMX refusal must name purchases.costs.write: {json}"
        );
    }

    /// A plain browser post (no HX-Request) gets the full-page refusal card,
    /// extending the shell, in Spanish, naming the missing permission.
    #[tokio::test]
    async fn ac10_a_plain_browser_mutation_refusal_is_the_html_page() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products",
            "sku=PAGE-DENIED&name=x&kind=Product&unit=un&sale_price=10",
            &[],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.400}");
        assert!(
            html.contains("Acción no permitida"),
            "the refusal must speak Spanish: {html:.400}"
        );
        assert!(
            html.contains("inventory.write"),
            "the refusal must name the missing permission: {html:.400}"
        );
        assert!(
            html.contains("data-nav=\"products\""),
            "the refusal page must keep the navigation shell, and the nav must \n             show what this principal may read (inventory.read, not dashboard): {html:.400}"
        );
    }

    /// The drawer fragment renders per-supplier cost rows, so its read needs
    /// `purchases.costs.read` ON TOP of `inventory.read`: an inventory-only
    /// principal gets the refusal, and the read-only principal that also holds
    /// the cost read sees the drawer normally.
    #[tokio::test]
    async fn the_drawer_read_requires_the_cost_read_permission_beyond_inventory_read() {
        let state = test_state().await;
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "DRAWER-GATE".into(),
                name: "drawer gate prod".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(10),
                cost_price: Decimal::from(5),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let uri = format!("/web/products/detail/{}", product.id);

        // inventory.read alone: refused — the fragment would have rendered
        // cost rows the principal may not see.
        let inventory_only =
            test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
                .await
                .unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("cookie", test_support::cookie_for(&inventory_only))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Both reads held: the drawer answers.
        let reader = test_support::seed_session_with_permissions(
            &state.pool,
            &["inventory.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("cookie", test_support::cookie_for(&reader))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A principal holding the permission gets its normal status: create and
    /// move stock over HTMX with the exact set the actions need.
    #[tokio::test]
    async fn ac10_the_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let token = test_support::seed_session_with_permissions(
            &state.pool,
            &["inventory.read", "inventory.write", "inventory.stock.write"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&token);
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products",
            "sku=WEB-HOLDER&name=Holder+Prod&kind=Product&unit=un&sale_price=10&track_stock=1&min_stock=2&max_stock=50",
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("WEB-HOLDER"),
            "the create must answer the refreshed list: {html:.400}"
        );
    }

    /// The gate runs FIRST: an anonymous request keeps the deny-by-default
    /// login redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let app = crate::routes::router(test_state().await);
        let req = Request::builder()
            .method("GET")
            .uri("/products")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            location == "/login" || location.starts_with("/login?next="),
            "anonymous navigation must hit the login gate first, got {location}"
        );
    }

    // -- S5 correction round (T19): the gates the first round's tests did not
    //    reach. Each product/category mutation handler is pinned by its own
    //    refusal test against an inventory.read-only principal, and each gate
    //    is mutation-validated: removing its annotation makes its test fail
    //    (the round's table in tasks.md). The shared principal keeps proving
    //    the happy paths; the probe is a second session built for this set.

    async fn inventory_read_only_probe(state: &AppState) -> String {
        let token =
            test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
                .await
                .unwrap();
        test_support::cookie_for(&token)
    }

    /// The drawer's inline edit (the first refusal round did not reach this
    /// handler): an inventory.read-only principal is refused in the HTMX JSON
    /// shape, and nothing about the product changes.
    #[tokio::test]
    async fn the_product_edit_gate_refuses_an_inventory_read_only_principal() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "EDIT-GATE").await;
        let cookie = inventory_read_only_probe(&state).await;
        let app = crate::routes::router(state.clone());

        let body = format!(
            "id={}&sku=EDIT-GATE-HACKED&name=Renamed+by+an+under-permissioned+principal&kind=Product&unit=un&sale_price=10&cost_price=5",
            product.id
        );
        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products/edit",
            &body,
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the HTMX refusal must name inventory.write: {json}"
        );

        // The refusal writes nothing: the product keeps its stored name.
        let after = state.inventory_service.get_product(product.id).await.unwrap();
        assert_eq!(after.name, "prod EDIT-GATE");
        assert_eq!(after.sku, "EDIT-GATE");
    }

    /// Activate is a product mutation: refused, HTMX JSON shape.
    #[tokio::test]
    async fn the_product_activate_gate_refuses_an_inventory_read_only_principal() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "ACT-GATE").await;
        let cookie = inventory_read_only_probe(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products/activate",
            &format!("product_id={}", product.id),
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the HTMX refusal must name inventory.write: {json}"
        );
    }

    /// Deactivate is the same lifecycle gate, pinned separately so a swapped
    /// extractor on either endpoint fails exactly one test.
    #[tokio::test]
    async fn the_product_deactivate_gate_refuses_an_inventory_read_only_principal() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "DEACT-GATE").await;
        let cookie = inventory_read_only_probe(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products/deactivate",
            &format!("product_id={}", product.id),
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the HTMX refusal must name inventory.write: {json}"
        );
        let after = state.inventory_service.get_product(product.id).await.unwrap();
        assert!(after.is_active, "a refused deactivate must not flip the flag");
    }

    /// Delete as a plain browser post: the full-page refusal card, and the
    /// row-count proof that the refusal writes nothing.
    #[tokio::test]
    async fn the_product_delete_gate_refuses_an_inventory_read_only_principal_and_writes_nothing() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "DEL-GATE").await;
        let cookie = inventory_read_only_probe(&state).await;
        let app = crate::routes::router(state.clone());
        let products_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM products")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/products/delete",
            &format!("product_id={}", product.id),
            &[],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.400}");
        assert!(
            html.contains("Acción no permitida"),
            "the refusal must speak Spanish: {html:.400}"
        );
        assert!(
            html.contains("inventory.write"),
            "the refusal must name the missing permission: {html:.400}"
        );

        let products_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM products")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            products_after, products_before,
            "a refused delete must write nothing"
        );
    }

    /// Category creation is an inventory.write mutation, not a read.
    #[tokio::test]
    async fn the_category_gate_refuses_an_inventory_read_only_principal() {
        let state = test_state().await;
        let cookie = inventory_read_only_probe(&state).await;
        let app = crate::routes::router(state.clone());
        let categories_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM categories")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/categories",
            "name=Denied+Cat",
            &[("HX-Request", "true")],
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
        let json: serde_json::Value = serde_json::from_str(&html).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the HTMX refusal must name inventory.write: {json}"
        );

        let categories_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM categories")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            categories_after, categories_before,
            "a refused category create must write nothing"
        );
    }

    async fn post_form_with_cookie(
        app: axum::Router,
        uri: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
        cookie: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", cookie);
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn web_products_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/products").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Products") || html.contains("products"),
            "page should mention products"
        );
        assert!(html.contains("Low Stock"), "page should have low-stock section");
    }

    #[tokio::test]
    async fn web_product_list_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/products").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn web_low_stock_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/web/low-stock").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Stock OK") || html.contains("stock-") || html.contains("low"));
    }

    #[tokio::test]
    async fn web_create_product_then_list_shows_it() {
        use axum::body::Body;
        let state = test_state().await;
        let app = crate::routes::router(state);
        let body = "sku=WEB-1&name=Web+Prod&kind=Product&unit=un&sale_price=10&cost_price=5&track_stock=1&min_stock=5&max_stock=50";
        let req = Request::builder()
            .method("POST")
            .uri("/web/products")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (status, html) = get_html(app, "/web/products").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("WEB-1"), "fragment should contain new sku: {html:.300}");
    }

    // -- N4: the picker search fragment ---------------------------------------

    async fn seed_search_product(state: &AppState) -> crate::models::Product {
        use crate::models::{NewProduct, ProductKind};
        use rust_decimal::Decimal;

        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "PICK-1".into(),
                name: "Yerba Picker".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(10),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        state
            .inventory_service
            .add_barcode(product.id, "7791234567890")
            .await
            .unwrap();
        product
    }

    /// AC8: name, SKU and barcode all find the product in one fragment, and the
    /// fragment carries price and current stock.
    #[tokio::test]
    async fn n4_product_search_matches_name_sku_and_barcode() {
        let state = test_state().await;
        seed_search_product(&state).await;
        let app = crate::routes::router(state);

        for needle in ["picker", "PICK-1", "7791234567890"] {
            let (status, html) =
                get_html(app.clone(), &format!("/web/product-search?q={needle}")).await;
            assert_eq!(status, StatusCode::OK, "{needle}: {html}");
            assert!(html.contains("Yerba Picker"), "{needle}: {html}");
            assert!(html.contains("PICK-1"), "{needle}: {html}");
            assert!(html.contains("$25"), "price rides along: {html}");
            assert!(html.contains("stock 0"), "stock rides along: {html}");
        }
    }

    /// AC8 (negative): an empty query returns no results, not the catalogue.
    #[tokio::test]
    async fn n4_product_search_empty_query_returns_no_results() {
        let state = test_state().await;
        seed_search_product(&state).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/web/product-search?q=").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("Yerba Picker"),
            "empty query must not dump the catalogue: {html}"
        );
    }

    /// N5: the picker island reads the search as JSON, so the same name, SKU and
    /// barcode matching the HTML fragment proves must hold on the wire. Both
    /// prices travel — the island picks by its own context, which is what keeps
    /// the `price` parameter off the request entirely.
    #[tokio::test]
    async fn n5_product_search_json_matches_name_sku_and_barcode() {
        let state = test_state().await;
        let product = seed_search_product(&state).await;
        let app = crate::routes::router(state);

        for needle in ["picker", "PICK-1", "7791234567890"] {
            let (status, json) =
                get_json(app.clone(), &format!("/web/product-search.json?q={needle}")).await;
            assert_eq!(status, StatusCode::OK, "{needle}: {json}");
            assert_eq!(json["query"], needle, "{needle}: {json}");

            let products = json["products"].as_array().expect("products array");
            assert_eq!(products.len(), 1, "{needle}: {json}");
            let row = &products[0];
            assert_eq!(row["id"], product.id, "{needle}: {json}");
            assert_eq!(row["name"], "Yerba Picker", "{needle}: {json}");
            assert_eq!(row["sku"], "PICK-1", "{needle}: {json}");

            let sale = row["sale_price"].as_str().expect("sale_price as string");
            assert_eq!(sale.parse::<f64>().unwrap(), 25.0, "{needle}: {json}");
            let cost = row["cost_price"].as_str().expect("cost_price as string");
            assert_eq!(cost.parse::<f64>().unwrap(), 10.0, "{needle}: {json}");

            let stock = row["stock"].as_str().expect("stock as string");
            assert_eq!(stock.parse::<f64>().unwrap(), 0.0, "{needle}: {json}");
        }
    }

    /// N5 negative: an empty query returns no products, not the catalogue. The
    /// island renders the empty state from this, so a catalogue dump here would
    /// be a silent data leak on a keystroke.
    #[tokio::test]
    async fn n5_product_search_json_empty_query_returns_no_products() {
        let state = test_state().await;
        seed_search_product(&state).await;
        let app = crate::routes::router(state);

        let (status, json) = get_json(app, "/web/product-search.json?q=").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(
            json["products"].as_array().expect("products array").len(),
            0,
            "empty query must not dump the catalogue: {json}"
        );
    }

    /// N5: the island's search field is named `product` on the wire because the
    /// same input feeds the line form, so both names must reach the same read.
    /// The HTML route resolves them in one place; this pins that the JSON route
    /// shares it instead of drifting.
    #[tokio::test]
    async fn n5_product_search_json_resolves_both_query_names() {
        let state = test_state().await;
        seed_search_product(&state).await;
        let app = crate::routes::router(state);

        let (status, json) = get_json(app, "/web/product-search.json?product=PICK-1").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(
            json["products"].as_array().expect("products array").len(),
            1,
            "the `product` alias must reach the same read: {json}"
        );
    }

    /// The fragment is generic: when the caller names the record's line action,
    /// every match becomes its own add form that includes the picker form and
    /// supplies its own product id. Without an action there are no dead controls.
    #[tokio::test]
    async fn n4_product_search_fragment_renders_one_add_action_per_match() {
        let state = test_state().await;
        let product = seed_search_product(&state).await;
        let app = crate::routes::router(state);

        let (status, plain) = get_html(app.clone(), "/web/product-search?q=picker").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !plain.contains("hx-post"),
            "without an action the fragment must not render dead controls: {plain}"
        );

        let (status, html) = get_html(
            app,
            "/web/product-search?q=picker&line_action=/web/sales/7/lines&line_target=%23sale-record-money",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(html.matches("<form").count(), 1, "{html}");
        assert!(html.contains("hx-post=\"/web/sales/7/lines\""), "{html}");
        assert!(html.contains("hx-include=\"#line-picker\""), "{html}");
        assert!(html.contains("hx-target=\"#sale-record-money\""), "{html}");
        let vals = format!("hx-vals='{{\"product_id\": {}}}'", product.id);
        assert!(
            html.contains(&vals),
            "result must supply its own product id: {html}"
        );
    }

    // -- T2 redesign-products: the product drawer routes -----------------------

    use crate::models::{NewProduct, NewSupplier, ProductKind};
    use rust_decimal::Decimal;

    async fn post_form_full(
        app: axum::Router,
        uri: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, String) {
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

    async fn seed_tracked_product(state: &AppState, sku: &str) -> crate::models::Product {
        state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: sku.into(),
                name: format!("prod {sku}"),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::from(2)),
                max_stock: Some(Decimal::from(50)),
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap()
    }

    fn hx_trigger(headers: &axum::http::HeaderMap) -> String {
        header_value(headers, "HX-Trigger")
    }

    fn header_value(headers: &axum::http::HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    /// The drawer fragment renders the header with the derived stock and one row
    /// per supplier cost (supplier name, current cost, preferred marker); an
    /// unknown id is a 404 like the suppliers drawer.
    #[tokio::test]
    async fn web_product_detail_renders_costs_and_unknown_is_404() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "DETAIL-1").await;
        state
            .inventory_service
            .record_movement(
                audit_actor_id(&state).await,
                crate::models::NewMovement {
                product_id: product.id,
                qty: Decimal::from(5),
                movement_type: crate::models::MovementType::In,
                reason: crate::models::MovementReason::Initial,
                reference: String::new(),
                date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            })
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(audit_actor_id(&state).await, NewSupplier {
                name: "Detail Sup".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .supplier_service
            .record_cost(audit_actor_id(&state).await, 
                product.id,
                supplier.id,
                Decimal::from_str("12.50").unwrap(),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        state
            .supplier_service
            .set_preferred(audit_actor_id(&state).await, product.id, supplier.id)
            .await
            .unwrap();

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for expected in [
            "product-detail-inner",
            "prod DETAIL-1",
            "stock 5",
            "Detail Sup",
            "12.50",
            "preferred",
        ] {
            assert!(html.contains(expected), "drawer must show {expected}: {html:.900}");
        }

        // The global notice region labels its success/error messages with the
        // submitting form's `data-action`, so renaming one of these labels is a
        // silent UX regression: every HTTP-status test still sees 200 and only
        // the toast text degrades. Pin the exact three labels and forbid any
        // other `data-action` in the fragment.
        for label in ["Save product", "Record product cost", "Record movement"] {
            let attr = format!("data-action=\"{label}\"");
            assert_eq!(
                html.matches(&attr).count(),
                1,
                "exactly one data-action '{label}' in the drawer fragment: {html:.900}"
            );
        }
        assert_eq!(
            html.matches("data-action=").count(),
            3,
            "no other data-action labels may appear in the drawer fragment: {html:.900}"
        );

        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/products/detail/99999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // -- Stale-cost badge: the derived disagreement between the supplier
    //    reference cost (`SupplierService::reference_cost`, reused as-is) and
    //    the product's stored `cost_price`. `Some` only when the two genuinely
    //    disagree: no supplier rows means the product column IS the truth, and
    //    `cost_price` 0 means "no cost recorded yet", not a comparable cost.

    /// Same seeding as `seed_tracked_product` but with the cost price under
    /// test: the badge's whole job is comparing it to the supplier rows.
    async fn seed_product_with_cost(
        state: &AppState,
        sku: &str,
        cost_price: Decimal,
    ) -> crate::models::Product {
        state
            .inventory_service
            .create_product(
                audit_actor_id(state).await,
                NewProduct {
                    sku: sku.into(),
                    name: format!("prod {sku}"),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price,
                    track_stock: true,
                    min_stock: Some(Decimal::from(2)),
                    max_stock: Some(Decimal::from(50)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
    }

    async fn seed_supplier_cost(
        state: &AppState,
        product_id: i64,
        supplier_name: &str,
        cost: Decimal,
        preferred: bool,
    ) {
        let actor = audit_actor_id(state).await;
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                NewSupplier {
                    name: supplier_name.into(),
                    phone: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .supplier_service
            .record_cost(
                actor,
                product_id,
                supplier.id,
                cost,
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        if preferred {
            state
                .supplier_service
                .set_preferred(actor, product_id, supplier.id)
                .await
                .unwrap();
        }
    }

    /// The badge appears when the preferred supplier's reference cost differs
    /// from the stored cost, and shows BOTH values so the operator sees the
    /// gap without arithmetic.
    #[tokio::test]
    async fn web_product_detail_shows_stale_cost_badge_when_reference_differs_stored() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-1", Decimal::from(5)).await;
        seed_supplier_cost(&state, product.id, "Stale Sup", Decimal::from_str("12.50").unwrap(), true)
            .await;

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for expected in ["stale cost", "reference $12.50", "stored $5.00"] {
            assert!(
                html.contains(expected),
                "stale badge must show {expected}: {html:.900}"
            );
        }
    }

    /// Equal costs mean fresh: no badge, even though a supplier row exists.
    #[tokio::test]
    async fn web_product_detail_hides_stale_cost_badge_when_costs_are_equal() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-2", Decimal::from(5)).await;
        seed_supplier_cost(&state, product.id, "Fresh Sup", Decimal::from(5), true).await;

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("stale cost"),
            "equal costs must not render the badge: {html:.900}"
        );
    }

    /// No supplier rows means `reference_cost` is `None`: the product column
    /// IS the truth, so there is nothing to compare and no badge.
    #[tokio::test]
    async fn web_product_detail_hides_stale_cost_badge_without_supplier_rows() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-3", Decimal::from(5)).await;

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("stale cost"),
            "no supplier rows must not render the badge: {html:.900}"
        );
    }

    /// `cost_price` 0 is `NOT NULL DEFAULT '0'` = "no cost recorded yet", not
    /// a cost to compare against: the badge must stay hidden even when a
    /// supplier cost exists.
    #[tokio::test]
    async fn web_product_detail_hides_stale_cost_badge_when_stored_cost_is_zero() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-4", Decimal::ZERO).await;
        seed_supplier_cost(&state, product.id, "Zero-Cost Sup", Decimal::from_str("12.50").unwrap(), true)
            .await;

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("stale cost"),
            "stored cost 0 must not render the badge: {html:.900}"
        );
    }

    /// The reference is the PREFERRED supplier's cost, not the cheapest: the
    /// badge must reuse `reference_cost`'s rule, never reimplement a min().
    #[tokio::test]
    async fn web_product_detail_stale_cost_badge_uses_preferred_supplier_cost_not_cheapest() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-5", Decimal::from(5)).await;
        seed_supplier_cost(&state, product.id, "Cheap Sup", Decimal::from(8), false).await;
        seed_supplier_cost(&state, product.id, "Preferred Sup", Decimal::from(20), true).await;

        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("reference $20.00"),
            "badge must show the preferred supplier's cost: {html:.900}"
        );
        assert!(
            !html.contains("reference $8.00"),
            "badge must not fall back to the cheapest row: {html:.900}"
        );
    }

    /// Drawer submissions get the fresh detail fragment plus two triggers:
    /// pre-swap `HX-Trigger` `product-changed` refreshes the lists, and
    /// after-settle `HX-Trigger-After-Settle` `product-saved` closes the
    /// drawer; a plain browser gets the redirect.
    #[tokio::test]
    async fn web_edit_product_from_drawer_answers_fragment_and_trigger() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "EDIT-WEB").await;
        let app = crate::routes::router(state.clone());

        let body = format!(
            "id={}&sku=EDIT-WEB-2&name=Edited+via+drawer&kind=Product&unit=kg&sale_price=20.50&cost_price=8&category_id=&track_stock=1&min_stock=2&max_stock=80&location=shelf+3&notes=edited",
            product.id
        );
        let (status, headers, html) = post_form_full(
            app.clone(),
            "/web/products/edit",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for expected in ["product-detail-inner", "Edited via drawer", "EDIT-WEB-2"] {
            assert!(
                html.contains(expected),
                "drawer answer must show {expected}: {html:.600}"
            );
        }
        // Two trigger headers, one distinct job each: `product-changed`
        // (pre-swap `HX-Trigger`) refreshes the product/low-stock lists;
        // `product-saved` (after-settle) is the drawer's own close signal for a
        // successful save. The close must ride the after-settle header because
        // the drawer's `htmx:afterSwap` open fires after the swap and would
        // otherwise reopen the panel the plain header just closed. A failed
        // save swaps nothing and fires neither.
        assert_eq!(
            hx_trigger(&headers),
            "product-changed",
            "drawer answer must fire product-changed pre-swap"
        );
        assert_eq!(
            header_value(&headers, "HX-Trigger-After-Settle"),
            "product-saved",
            "drawer answer must fire product-saved after settle (closes the drawer)"
        );

        // The non-drawer HTMX branch (e.g. the standalone list edit form) keeps
        // the single-event answer: `product-changed` only, never `product-saved`,
        // so no save outside the drawer can close the panel.
        let (status, headers, _) = post_form_full(
            app.clone(),
            "/web/products/edit",
            &body,
            &[("HX-Target", "product-list")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let trigger = hx_trigger(&headers);
        assert!(trigger.contains("product-changed"));
        assert!(
            !trigger.contains("product-saved"),
            "non-drawer save must not fire product-saved, got {trigger:?}"
        );

        // Issue #33: the non-drawer branch must honour the catalogue filter too.
        // The filter rides the query string, not the body: the form body already
        // carries the product's own `category_id`, so the keys would collide.
        let cat_a = state
            .inventory_service
            .create_category(
                audit_actor_id(&state).await,
                "Edit Cat A", None)
            .await
            .unwrap();
        let cat_b = state
            .inventory_service
            .create_category(
                audit_actor_id(&state).await,
                "Edit Cat B", None)
            .await
            .unwrap();
        let _other = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "EDIT-OTHER".into(),
                name: "other EDIT-OTHER".into(),
                kind: ProductKind::Product,
                category_id: Some(cat_b.id),
                unit: "un".into(),
                sale_price: Decimal::from(10),
                cost_price: Decimal::from(2),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        // The body keeps the product in cat_a so the query filter can select it.
        let filtered_body = format!(
            "id={}&sku=EDIT-WEB-2&name=Edited+via+drawer&kind=Product&unit=kg&sale_price=20.50&cost_price=8&category_id={}&track_stock=1&min_stock=2&max_stock=80&location=shelf+3&notes=edited",
            product.id, cat_a.id
        );
        let (status, headers, html) = post_form_full(
            app.clone(),
            &format!("/web/products/edit?category_id={}", cat_a.id),
            &filtered_body,
            &[("HX-Target", "product-list")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("EDIT-WEB-2"),
            "filtered non-drawer answer must hold the edited row: {html:.400}"
        );
        assert!(
            !html.contains("EDIT-OTHER"),
            "filtered non-drawer answer must not hold the other category's row: {html:.400}"
        );
        assert!(
            hx_trigger(&headers).contains("product-changed"),
            "filtered non-drawer answer must fire product-changed, got {:?}",
            hx_trigger(&headers)
        );

        // Without the query the non-drawer answer is still the full catalogue.
        let (status, _, html) = post_form_full(
            app.clone(),
            "/web/products/edit",
            &filtered_body,
            &[("HX-Target", "product-list")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("EDIT-WEB-2") && html.contains("EDIT-OTHER"),
            "unfiltered non-drawer answer must cover the whole catalogue: {html:.400}"
        );

        // A plain browser post is a redirect to the page.
        let req = Request::builder()
            .method("POST")
            .uri("/web/products/edit")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get("Location").and_then(|v| v.to_str().ok()),
            Some("/products")
        );
    }

    /// Emptying an optional field through the drawer form is an explicit
    /// clear, not "leave unchanged": the create and update paths share
    /// validation, so a regression that silently kept the stale optional
    /// value would corrupt the product on save. The unticked `track_stock`
    /// checkbox sends nothing, which must also mean `false`.
    #[tokio::test]
    async fn web_edit_product_clears_emptied_optional_fields() {
        let state = test_state().await;
        let category = state
            .inventory_service
            .create_category(
                audit_actor_id(&state).await,
                "Clear Cat", None)
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "CLEAR-1".into(),
                name: "clearable prod".into(),
                kind: ProductKind::Product,
                category_id: Some(category.id),
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::from(2)),
                max_stock: Some(Decimal::from(50)),
                location: Some("shelf 3".into()),
                notes: Some("fragile".into()),
                markup_pct: None,
            })
            .await
            .unwrap();

        // Every optional field is sent empty and the `track_stock` key is
        // absent (the unticked checkbox posts nothing at all).
        let body = format!(
            "id={}&sku=CLEAR-1&name=clearable+prod&kind=Product&unit=un&sale_price=25&cost_price=5&category_id=&min_stock=&max_stock=&location=&notes=",
            product.id
        );
        let app = crate::routes::router(state.clone());
        let (status, _, html) = post_form_full(
            app,
            "/web/products/edit",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("product-detail-inner"),
            "drawer edit must answer the detail fragment: {html:.600}"
        );

        let after = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
        assert!(!after.track_stock, "absent checkbox must mean false");
        assert_eq!(after.min_stock, None, "empty min_stock must clear");
        assert_eq!(after.max_stock, None, "empty max_stock must clear");
        assert_eq!(after.location, None, "empty location must clear");
        assert_eq!(after.notes, None, "empty notes must clear");
        assert_eq!(after.category_id, None, "empty category_id must clear");
        // Fields the form did send survive untouched.
        assert_eq!(after.sku, "CLEAR-1");
        assert_eq!(after.name, "clearable prod");
        assert_eq!(after.sale_price, Decimal::from(25));
        assert_eq!(after.cost_price, Decimal::from(5));
    }

    // -- markup on the web forms (product-markup T6) --------------------------

    /// The stored product for a web SKU: the catalogue filter is the same read
    /// the list uses, so an exact SKU match fetches the row without touching a
    /// repository directly from the tests.
    async fn web_product_by_sku(state: &AppState, sku: &str) -> crate::models::Product {
        state
            .inventory_service
            .filter_products(sku, None)
            .await
            .unwrap()
            .into_iter()
            .find(|ps| ps.product.sku == sku)
            .map(|ps| ps.product)
            .unwrap()
    }

    /// A markup and an EMPTY sale_price is the derived-price path: the create
    /// form lets the operator type only cost and markup, and the service both
    /// derives and validates the price, so the empty field must not be a 400.
    #[tokio::test]
    async fn web_create_with_markup_and_empty_sale_price_derives_the_price() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let body = "sku=MKWEB-1&name=Markup+Web&kind=Product&unit=un&sale_price=&cost_price=5&markup_pct=100&track_stock=1&min_stock=5&max_stock=50";
        let (status, _, html) = post_form_full(app, "/web/products", body, &[]).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");

        let stored = web_product_by_sku(&state, "MKWEB-1").await;
        // cost 5 with a 100% markup derives 5 * (1 + 100/100) = 10.
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap());
        assert_eq!(stored.markup_pct, Some(Decimal::from(100)));
    }

    /// Without a markup the price is manual, so the empty-sale_price gate keeps
    /// firing exactly as before; the markup field must not weaken it.
    #[tokio::test]
    async fn web_create_without_markup_still_requires_sale_price() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let body = "sku=MKWEB-2&name=No+Markup&kind=Product&unit=un&sale_price=&cost_price=5";
        let (status, _, html) = post_form_full(app, "/web/products", body, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");
        assert!(html.contains("sale_price is required"), "{html}");
    }

    /// The submitted price is IGNORED whenever a markup is present: the stored
    /// price is the derived one, never the form's value.
    #[tokio::test]
    async fn web_create_with_markup_ignores_the_submitted_price() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        // The 999 is deliberate: if the stored price is 10, the form's price
        // was overridden, not coincidentally equal.
        let body = "sku=MKWEB-3&name=Override+Web&kind=Product&unit=un&sale_price=999&cost_price=5&markup_pct=100&track_stock=1&min_stock=5&max_stock=50";
        let (status, _, html) = post_form_full(app, "/web/products", body, &[]).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");

        let stored = web_product_by_sku(&state, "MKWEB-3").await;
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap());
    }

    /// Editing with a markup set derives the price on the web path too, and the
    /// submitted sale_price is ignored exactly like creation.
    #[tokio::test]
    async fn web_edit_with_markup_derives_the_price() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let product = seed_tracked_product(&state, "EDIT-MK").await;
        let app = crate::routes::router(state.clone());

        let body = format!(
            "id={}&sku=EDIT-MK&name=Edited+markup&kind=Product&unit=un&sale_price=999&cost_price=5&markup_pct=100&category_id=&track_stock=1&min_stock=2&max_stock=80",
            product.id
        );
        let (status, _, html) = post_form_full(
            app,
            "/web/products/edit",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");

        let stored = state.inventory_service.get_product(product.id).await.unwrap();
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap());
        assert_eq!(stored.markup_pct, Some(Decimal::from(100)));
    }

    /// Clearing the markup (empty field — the drawer always sends every field)
    /// keeps the last derived price and returns the product to a manual price
    /// (markup NULL). The fresh fragment must render the manual gate again:
    /// no readonly price input while there is no markup.
    #[tokio::test]
    async fn web_edit_clearing_the_markup_keeps_the_price_and_goes_manual() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "EDIT-MKCLR".into(),
                name: "markup to clear".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                // Deliberately absurd: with a markup the request price must be
                // ignored, so a stored 10 proves the derivation happened.
                sale_price: Decimal::from(999),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::from(2)),
                max_stock: Some(Decimal::from(50)),
                location: None,
                notes: None,
                markup_pct: Some(Decimal::from(100)),
            })
            .await
            .unwrap();
        assert_eq!(product.sale_price, Decimal::from_str("10").unwrap());
        let app = crate::routes::router(state.clone());

        // `sale_price=10` is what the readonly price input submits — the
        // operator sees the price the server last derived.
        let body = format!(
            "id={}&sku=EDIT-MKCLR&name=markup+cleared&kind=Product&unit=un&sale_price=10&cost_price=5&markup_pct=&category_id=&track_stock=1&min_stock=2&max_stock=80",
            product.id
        );
        let (status, _, html) = post_form_full(
            app,
            "/web/products/edit",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");

        let stored = state.inventory_service.get_product(product.id).await.unwrap();
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap(), "the last price survives the clear");
        assert_eq!(stored.markup_pct, None, "an empty markup field clears back to manual");
        assert!(
            !html.contains("readonly"),
            "the manual price must not render readonly again: {html:.600}"
        );
    }

    /// The edit gate mirrors creation's in BOTH directions: with a markup an
    /// empty sale_price is legal because the server derives it, and without a
    /// markup an empty price is still refused. The create path had both
    /// directions tested; the edit path had neither, so its gate was dead code
    /// as far as the suite could tell.
    #[tokio::test]
    async fn web_edit_gate_depends_on_the_markup_like_creation() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let product = seed_tracked_product(&state, "EDIT-MKGATE").await;
        let app = crate::routes::router(state.clone());
        let body = |markup: &str, price: &str| {
            format!(
                "id={}&sku=EDIT-MKGATE&name=Edited+gate&kind=Product&unit=un&sale_price={price}&cost_price=5&markup_pct={markup}&category_id=&track_stock=1&min_stock=2&max_stock=80",
                product.id
            )
        };

        // A markup makes an empty price legal: the server derives and stores it.
        let (status, _, html) = post_form_full(
            app.clone(),
            "/web/products/edit",
            &body("100", ""),
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        let stored = state.inventory_service.get_product(product.id).await.unwrap();
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap());
        assert_eq!(stored.markup_pct, Some(Decimal::from(100)));

        // Without a markup the price is manual, so empty stays an error and
        // the refusal must write nothing.
        let (status, _, html) = post_form_full(
            app,
            "/web/products/edit",
            &body("", ""),
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{html:.600}");
        assert!(html.contains("sale_price is required"), "{html:.400}");
        let stored = state.inventory_service.get_product(product.id).await.unwrap();
        assert_eq!(stored.sale_price, Decimal::from_str("10").unwrap());
        assert_eq!(stored.markup_pct, Some(Decimal::from(100)));
    }

    /// The drawer fragment renders the stored markup value and pins the price
    /// input readonly while a markup is present: the server-rendered state is
    /// correct before any script runs.
    #[tokio::test]
    async fn web_drawer_fragment_renders_the_stored_markup_and_readonly_price() {
        use rust_decimal::Decimal;
        let state = test_state().await;
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "DRAW-MK".into(),
                name: "drawer markup".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(999),
                cost_price: Decimal::from(8),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: Some(Decimal::from(25)),
            })
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) =
            get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("name=\"markup_pct\"") && html.contains("value=\"25\""),
            "the drawer must render the stored markup value: {html:.900}"
        );
        // cost 8 with a 25% markup derives 10, stored at two decimals.
        assert!(
            html.contains("readonly") && html.contains("value=\"10.00\""),
            "the derived price must render readonly while a markup is set: {html:.900}"
        );
    }

    /// Recording a cost creates/updates the satellite row and fires
    /// `product-cost-recorded`; a date older than the current one is 400.
    #[tokio::test]
    async fn web_record_product_cost_creates_row_and_fires_trigger() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "COST-WEB").await;
        let supplier = state
            .supplier_service
            .create_supplier(audit_actor_id(&state).await, NewSupplier {
                name: "Cost Sup".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let body = format!(
            "product_id={}&supplier_id={}&cost=12.50&date=2024-05-01",
            product.id, supplier.id
        );
        let (status, headers, html) = post_form_full(
            app.clone(),
            "/web/product-costs",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("product-detail-inner") && html.contains("Cost Sup"),
            "drawer answer must be the fresh detail: {html:.600}"
        );
        assert!(
            hx_trigger(&headers).contains("product-cost-recorded"),
            "must fire product-cost-recorded, got {:?}",
            hx_trigger(&headers)
        );

        let costs = state
            .supplier_service
            .list_costs_for_product(product.id)
            .await
            .unwrap();
        assert_eq!(costs.len(), 1, "one satellite row after the web post");
        assert_eq!(costs[0].current_cost, Decimal::from_str("12.50").unwrap());

        // A date older than the current cost date is rejected by the service.
        let body = format!(
            "product_id={}&supplier_id={}&cost=13&date=2024-04-01",
            product.id, supplier.id
        );
        let (status, _, _) = post_form_full(
            app,
            "/web/product-costs",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Marking a preferred supplier moves the marker: after switching, exactly
    /// one row for the product is preferred and it is the newest choice.
    #[tokio::test]
    async fn web_set_preferred_cost_switches_preferred() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "PREF-WEB").await;
        let a = state
            .supplier_service
            .create_supplier(audit_actor_id(&state).await, NewSupplier {
                name: "Pref A".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let b = state
            .supplier_service
            .create_supplier(audit_actor_id(&state).await, NewSupplier {
                name: "Pref B".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        for (sid, cost) in [(a.id, "9"), (b.id, "8")] {
            state
                .supplier_service
                .record_cost(audit_actor_id(&state).await, 
                    product.id,
                    sid,
                    Decimal::from_str(cost).unwrap(),
                    chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                )
                .await
                .unwrap();
        }
        let app = crate::routes::router(state.clone());

        for (target, expected_preferred) in [(a.id, a.id), (b.id, b.id)] {
            let (status, _, _) = post_form_full(
                app.clone(),
                "/web/product-costs/preferred",
                &format!("product_id={}&supplier_id={}", product.id, target),
                &[("HX-Target", "product-drawer-body")],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "set preferred {target}");

            let costs = state
                .supplier_service
                .list_costs_for_product(product.id)
                .await
                .unwrap();
            let preferred: Vec<_> = costs.iter().filter(|c| c.is_preferred).collect();
            assert_eq!(
                preferred.len(),
                1,
                "exactly one preferred row after choosing supplier {target}"
            );
            assert_eq!(preferred[0].supplier_id, expected_preferred);
        }
    }

    /// The movement form lives in the drawer: a drawer submission answers the
    /// fresh fragment and keeps firing `movement-created`; a movement for a
    /// service still surfaces the service error (400).
    #[tokio::test]
    async fn web_stock_movement_from_drawer_answers_fragment_and_keeps_trigger() {
        let state = test_state().await;
        let product = seed_tracked_product(&state, "MOVE-WEB").await;
        let service = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "SRV-WEB".into(),
                name: "service SRV-WEB".into(),
                kind: ProductKind::Service,
                category_id: None,
                unit: "hr".into(),
                sale_price: Decimal::from(30),
                cost_price: Decimal::ZERO,
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let body = format!(
            "product_id={}&type=In&qty=5&reason=Initial&date=2024-05-01",
            product.id
        );
        let (status, headers, html) = post_form_full(
            app.clone(),
            "/web/stock-movements",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("product-detail-inner") && html.contains("stock 5"),
            "drawer answer must be the fresh detail with derived stock: {html:.600}"
        );
        assert!(
            hx_trigger(&headers).contains("movement-created"),
            "drawer answer must keep movement-created, got {:?}",
            hx_trigger(&headers)
        );

        // Service product => the service error (400), not a fragment.
        let body = format!("product_id={}&type=In&qty=1&reason=Purchase", service.id);
        let (status, _, _) = post_form_full(
            app,
            "/web/stock-movements",
            &body,
            &[("HX-Target", "product-drawer-body")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Activate/deactivate/delete post with a hidden product_id and answer the
    /// list fragment with `product-changed`; delete of a product with movements
    /// stays a 400 and the row survives. The fragment must be the list the
    /// caller is looking at (issue #33): a filter riding the body narrows the
    /// answer to that category, and without one the whole catalogue comes back.
    #[tokio::test]
    async fn web_product_lifecycle_actions() {
        let state = test_state().await;
        let cat_a = state
            .inventory_service
            .create_category(
                audit_actor_id(&state).await,
                "Life Cat A", None)
            .await
            .unwrap();
        let cat_b = state
            .inventory_service
            .create_category(
                audit_actor_id(&state).await,
                "Life Cat B", None)
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "LIFE-WEB".into(),
                name: "prod LIFE-WEB".into(),
                kind: ProductKind::Product,
                category_id: Some(cat_a.id),
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::from(2)),
                max_stock: Some(Decimal::from(50)),
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        let other = state
            .inventory_service
            .create_product(
                audit_actor_id(&state).await,
                NewProduct {
                sku: "LIFE-OTHER".into(),
                name: "prod LIFE-OTHER".into(),
                kind: ProductKind::Product,
                category_id: Some(cat_b.id),
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(5),
                track_stock: true,
                min_stock: Some(Decimal::from(2)),
                max_stock: Some(Decimal::from(50)),
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Deactivate with the catalogue filter in the body: the flag flips and
        // the answer is the filtered list fragment, still firing the trigger.
        let body = format!("product_id={}&category_id={}", product.id, cat_a.id);
        let (status, headers, html) = post_form_full(
            app.clone(),
            "/web/products/deactivate",
            &body,
            &[("HX-Target", "product-list")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("product-list-inner"),
            "must answer the list fragment: {html:.400}"
        );
        assert!(
            html.contains("LIFE-WEB"),
            "filtered answer must hold the product's row: {html:.400}"
        );
        assert!(
            !html.contains("LIFE-OTHER"),
            "filtered answer must not hold the other category's row: {html:.400}"
        );
        assert!(
            hx_trigger(&headers).contains("product-changed"),
            "must fire product-changed, got {:?}",
            hx_trigger(&headers)
        );
        let after = state.inventory_service.get_product(product.id).await.unwrap();
        assert!(!after.is_active, "deactivate must flip is_active");

        // Activate without a filter: the answer is still the whole catalogue.
        let (status, _, html) = post_form_full(
            app.clone(),
            "/web/products/activate",
            &format!("product_id={}", product.id),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("LIFE-WEB") && html.contains("LIFE-OTHER"),
            "unfiltered answer must cover the whole catalogue: {html:.400}"
        );
        let after = state.inventory_service.get_product(product.id).await.unwrap();
        assert!(after.is_active, "activate must flip is_active back");

        // Delete without movements: gone.
        let plain_id = other.id;
        let (status, _, _) = post_form_full(
            app.clone(),
            "/web/products/delete",
            &format!("product_id={}", plain_id),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get_html(app.clone(), &format!("/web/products/detail/{}", plain_id)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Delete with movements: 400, row survives.
        state
            .inventory_service
            .record_movement(
                audit_actor_id(&state).await,
                crate::models::NewMovement {
                product_id: product.id,
                qty: Decimal::from(3),
                movement_type: crate::models::MovementType::In,
                reason: crate::models::MovementReason::Initial,
                reference: String::new(),
                date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            })
            .await
            .unwrap();
        let (status, _, _) = post_form_full(
            app.clone(),
            "/web/products/delete",
            &format!("product_id={}", product.id),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "a product with movements survives delete");
    }

    // -- audit attribution (M5 Phase B, slice S10, AC18): what the view shows --

    /// The product detail shows the actor as a DISPLAY NAME, never an id:
    /// "Registrado por" names the creator and an edit adds "Actualizado por"
    /// without erasing the creator. Two principals drive the flow so the two
    /// names are distinct — the shared session user creates the product, a
    /// second probe session edits it. The low-stock fragment shows the same
    /// name on its rows.
    #[tokio::test]
    async fn audit_inventory_detail_view_shows_the_actor_display_name() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        // The shared session user ("Test Admin") creates the product.
        let (status, resp) = post_form_with_cookie(
            app.clone(),
            "/web/products",
            "sku=AUDIT-VIEW&name=Audited+product&kind=&product_category_id=&unit=un&sale_price=10&cost_price=&min_stock=&max_stock=&location=&notes=",
            &[("hx-request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let product_id: i64 =
            sqlx::query_scalar("SELECT id FROM products WHERE sku = 'AUDIT-VIEW'")
                .fetch_one(&state.pool)
                .await
                .unwrap();

        // A second principal (display name "Test Probe") edits the product.
        let probe_token =
            test_support::seed_session_with_permissions(&state.pool, &["inventory.write"])
                .await
                .unwrap();
        let (status, resp) = post_form_with_cookie(
            app.clone(),
            "/web/products/edit",
            &format!(
                "id={product_id}&sku=AUDIT-VIEW&name=Audited+product&kind=&category_id=&unit=un&sale_price=12&cost_price=&min_stock=&max_stock=&location=&notes=",
            ),
            &[("hx-request", "true")],
            &test_support::cookie_for(&probe_token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");

        let (status, page) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(
            page.matches("Registrado por Test Admin").count(),
            1,
            "the detail names the creator's display name: {page}"
        );
        assert_eq!(
            page.matches("Actualizado por Test Probe").count(),
            1,
            "the edit names its editor: {page}"
        );
        assert!(
            !page.contains("Registrado por 1"),
            "the interface never renders a raw user id: {page}"
        );
    }

    /// The low-stock fragment shows the same attribution: every row names the
    /// product's creator as a display name.
    #[tokio::test]
    async fn audit_the_stock_list_rows_show_the_actor_display_name() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        // A tracked product below its minimum stock.
        let (status, resp) = post_form_with_cookie(
            app.clone(),
            "/web/products",
            "sku=LOW-VIEW&name=Low+product&kind=&product_category_id=&unit=un&sale_price=10&cost_price=5&track_stock=1&min_stock=5&max_stock=50&location=&notes=",
            &[("hx-request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let product_id: i64 =
            sqlx::query_scalar("SELECT id FROM products WHERE sku = 'LOW-VIEW'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        state
            .inventory_service
            .record_movement(
                audit_actor_id(&state).await,
                crate::models::NewMovement {
                    product_id,
                    qty: Decimal::from(1),
                    movement_type: crate::models::MovementType::In,
                    reason: crate::models::MovementReason::Initial,
                    reference: String::new(),
                    date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                },
            )
            .await
            .unwrap();

        let (status, html) = get_html(app, "/web/low-stock").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("Producto registrado por Test Admin").count(),
            1,
            "the stock row names the product's creator, and says that is what it names: {html}"
        );
        assert!(
            !html.contains("Producto registrado por 1"),
            "the fragment never renders a raw user id: {html}"
        );
    }
}
