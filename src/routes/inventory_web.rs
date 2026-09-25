use askama::Template;
use axum::{
    extract::{Extension, Form, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{
    LadderInput, MovementReason, MovementType, NewMovement, NewProduct, PriceField, PriceFields,
    PriceRefusal, Product, ProductKind, ProductPriceLadder, ProductStock, ProductSupplierCost,
    ProductTaxView, Tax, UpdateProduct,
};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, ProductSupplierCostRepository,
    StockMovementRepository,
};
use crate::routes::{localized_refusal_error, price_refusal_message, AppState};
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
//
// The screen owns no tax DEFINITION. This module used to expose a second,
// `inventory.write`-gated address space for tax administration
// (`/web/taxes`, `/web/taxes/edit`, `/web/taxes/deactivate`) and render its
// catalogue on the page. Both are gone; see the note above
// `web_link_product_tax` and `settings_web.rs`'s URL decision. The product-tax
// ASSOCIATION is the only tax surface here, and it stays on this gate. The JSON
// API agrees: its three definition routes are `settings.manage` too.

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "products.html")]
struct ProductsTemplate {
    products: Vec<ProductStock>,
    categories: Vec<crate::models::Category>,
    localization: LocalizationContext,
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
    localization: LocalizationContext,
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
    message: String,
    clear_filter_label: String,
    dismiss_label: String,
}

#[derive(Template)]
#[template(path = "partials/stock_list.html")]
struct StockListPartial {
    /// One row per stock-tracked product with its audit actor resolved to a
    /// display name (see `StockRow` below).
    items: Vec<StockRow>,
    localization: LocalizationContext,
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

/// The picker island's wire row (N5). Flattened on purpose: the island renders a
/// name, a SKU, one price and a stock figure, so the wire carries exactly that
/// instead of the whole `ProductStock` with its nested product.
///
/// The money fields are localized display strings produced by the request
/// context. The island renders them verbatim, so the server keeps the single
/// formatting rule instead of the island reimplementing it in JS and drifting.
/// `stock` is formatted with the same request context as the prices, so every
/// quantity shown by the island follows the business locale without JavaScript
/// reimplementing number formatting.
///
/// Both prices travel and the island picks by its own context, which is what
/// keeps the `price` parameter off the request entirely.
#[derive(Debug, Serialize)]
struct ProductSearchRow {
    id: i64,
    name: String,
    sku: String,
    sale_price: String,
    cost_price: String,
    stock: String,
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

/// The product price ladder, the ONE place the drawer shows money. A
/// server-rendered fragment with two shapes — the drawer embeds it through
/// `product_detail.html`, and the preview endpoint answers it on its own.
#[derive(Template)]
#[template(path = "partials/product_price_ladder.html")]
struct ProductPriceLadderPartial {
    localization: LocalizationContext,
    ladder: ProductPriceLadder,
    /// The refusal the ladder states, already in the active locale. Resolved
    /// HERE, by the one shared mapping, so the fragment never renders a refusal
    /// in a language of its own.
    net_refusal_message: Option<String>,
}

/// The refusal a ladder publishes, as the active locale words it — or `None`,
/// because a ladder with a price has no refusal to state. The single place the
/// fragment's text is produced, for the drawer's own first render and for the
/// preview endpoint alike.
fn ladder_refusal_message(
    ladder: &ProductPriceLadder,
    localization: &LocalizationContext,
) -> Option<String> {
    ladder
        .net_refusal
        .map(|refusal| price_refusal_message(&refusal, localization))
}

fn product_price_ladder_html(
    localization: &LocalizationContext,
    ladder: ProductPriceLadder,
) -> AppResult<Html<String>> {
    let net_refusal_message = ladder_refusal_message(&ladder, localization);
    ProductPriceLadderPartial {
        localization: localization.clone(),
        ladder,
        net_refusal_message,
    }
    .render()
    .map(Html)
    .map_err(|e| AppError::Internal(e.to_string()))
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
    localization: LocalizationContext,
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
    product_taxes: Vec<ProductTaxView>,
    available_taxes: Vec<Tax>,
    /// The price ladder, computed from the product's STORED state: the drawer
    /// has no form values yet when it first renders, and every one of its
    /// money figures lives here (U2). The stored net price is what a save
    /// persists, untouched by the taxes.
    ladder: ProductPriceLadder,
    /// The same field the standalone ladder partial carries: the drawer embeds
    /// that fragment, so the sentence it states is resolved by the same shared
    /// mapping rather than by a rendering rule of its own.
    net_refusal_message: Option<String>,
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

async fn filtered_list_html(
    state: &AppState,
    q: &str,
    category_id: &str,
    localization: &LocalizationContext,
) -> AppResult<String> {
    let products = filtered_products(state, q, category_id).await?;
    render_product_list(&products, localization)
}

fn render_product_list(
    products: &[ProductStock],
    localization: &LocalizationContext,
) -> AppResult<String> {
    ProductListPartial {
        products: products.to_vec(),
        localization: localization.clone(),
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
fn hidden_by_filter_notice_html(
    product_name: &str,
    localization: &LocalizationContext,
) -> AppResult<String> {
    HiddenByFilterNotice {
        message: localization.tr_with(
            crate::localization::MessageKey::NoticeCreated,
            &[("product_name", product_name)],
        ),
        clear_filter_label: localization
            .tr(crate::localization::MessageKey::NoticeCreatedFilterClear)
            .to_string(),
        dismiss_label: localization
            .tr(crate::localization::MessageKey::AccessibilityDismiss)
            .to_string(),
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
    Extension(localization): Extension<LocalizationContext>,
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
        localization,
        allow_negative_stock: state.allow_negative_stock,
        nav_key: "products",
        filter_q: query,
        filter_category: q.category_id.as_deref().unwrap_or("").trim().to_string(),
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
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
    Extension(localization): Extension<LocalizationContext>,
    Query(q): Query<WebProductFilter>,
) -> Result<Html<String>, AppError> {
    let (query, category_id) = q.parsed();
    let products = state
        .inventory_service
        .filter_products(&query, category_id)
        .await?;
    let html = ProductListPartial {
        products,
        localization: localization.clone(),
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_low_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.low_stock().await?;
    let html = stock_list_html(&state, items, localization).await?;
    Ok(Html(html))
}

async fn web_negative_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.negative_stock().await?;
    let html = stock_list_html(&state, items, localization).await?;
    Ok(Html(html))
}

/// Resolve the stock rows' audit actors in the wiring layer and render the
/// fragment: one statement covers every row, the same way the finance detail
/// resolves its names.
async fn stock_list_html(
    state: &AppState,
    items: Vec<ProductStock>,
    localization: LocalizationContext,
) -> AppResult<String> {
    let mut actor_ids = items
        .iter()
        .map(|ps| ps.product.created_by)
        .collect::<Vec<i64>>();
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
    StockListPartial {
        items: rows,
        localization,
    }
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
    Extension(localization): Extension<LocalizationContext>,
    Query(q): Query<CategoryOptionsQuery>,
) -> Result<Html<String>, AppError> {
    let cats = state.inventory_service.categories.list().await?;
    let empty_label = match q.empty.as_deref().map(str::trim) {
        Some("none") => localization
            .tr(crate::localization::MessageKey::CommonNoCategory)
            .to_string(),
        Some(label) if !label.is_empty() => label.to_string(),
        _ => localization
            .tr(crate::localization::MessageKey::CommonAllCategories)
            .to_string(),
    };
    let mut html = format!("<option value=\"\">{}</option>", html_escape(&empty_label));
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
}

/// The picker input is named `product` because the same field feeds the line
/// form, while `q` is the documented name; both reach the same read. Shared by
/// the JSON route so its two names cannot resolve differently.
fn resolve_search_query(params: &ProductSearchQuery) -> String {
    if params.q.trim().is_empty() {
        params.product.clone()
    } else {
        params.q.clone()
    }
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
    Extension(localization): Extension<LocalizationContext>,
    Query(params): Query<ProductSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = resolve_search_query(&params);
    let matches = state.inventory_service.search_products(&raw).await?;
    let products: Vec<ProductSearchRow> = matches
        .into_iter()
        .map(|ps| {
            // This is the static island's presentation boundary, not the JSON
            // API. Keep its existing string fields, but format them from the
            // same request context as HTML and HTMX.
            let sale_price = localization.format_currency(ps.product.sale_price);
            let cost_price = localization.format_currency(ps.product.cost_price);
            ProductSearchRow {
                id: ps.product.id,
                name: ps.product.name,
                sku: ps.product.sku,
                sale_price,
                cost_price,
                stock: localization.format_quantity(ps.stock),
            }
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
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    product_detail_html(&state, id, &localization).await
}

/// The drawer's price fields as the `change` listener sends them.
///
/// `id` is the drawer's OWN key: the edit form posts to `/web/products/edit`
/// under that name, and the three ladder inputs carry `hx-include="closest
/// form"`, so the browser sends the whole form — the product under `id` and
/// never under a second name. Naming the field differently here is what made
/// this endpoint answer 400 to every real browser request while a suite that
/// hand-built `?product_id=…` stayed green: the endpoint has to speak the
/// form's language, not a private one.
///
/// The three price fields are `Option` on purpose: an ABSENT key means "this
/// request carries no form at all" (a direct read), which is different from a
/// key the operator emptied — the first reports the stored state, the second
/// reports what a save would do with an empty field. Every other field the form
/// sends is ignored, so the ladder never has to be taught the form's shape
/// twice.
#[derive(Debug, Deserialize)]
struct PriceLadderQuery {
    id: i64,
    /// The kind select rides the same form, and the price rule branches on it,
    /// so a `None` here means "no readable kind in the form" and the ladder
    /// falls back to the product's stored kind.
    kind: Option<String>,
    sale_price: Option<String>,
    cost_price: Option<String>,
    markup_pct: Option<String>,
}

/// `GET /web/product-price-ladder`: the read-only price ladder, for the CURRENT
/// form values, before anything is saved.
///
/// THE ARCHITECTURAL RULE lives here: the ladder is computed on the server and
/// returned already formatted in the active locale. The browser never applies
/// the markup formula and never rounds money, so a drawer and a document cannot
/// end up a cent apart.
///
/// It is a GET behind `inventory.read`, which is what makes "it persists
/// nothing" structural rather than promised: there is no write verb on this
/// path, and a POST is answered 405. The id travels in the query, like every
/// other body-borne filter on this screen, so the wiring guard's "no concrete
/// id in a path" rule stays satisfied.
///
/// A field that is not a number is not a refusal here: the operator is still
/// typing. The ladder reports the last state the save path accepted and says
/// so, which is more useful than an error box over a half-typed number.
async fn web_product_price_ladder(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<PriceLadderQuery>,
) -> AppResult<Html<String>> {
    let carries_a_form = query.sale_price.is_some()
        || query.cost_price.is_some()
        || query.markup_pct.is_some()
        || query.kind.is_some();
    // The kind is parsed with the SAME `ProductKind` parser the save handler
    // uses, and an unreadable one is not guessed: the ladder falls back to the
    // stored kind rather than picking a threshold out of thin air.
    let kind = query
        .kind
        .as_deref()
        .and_then(|raw| raw.trim().parse::<ProductKind>().ok());
    let fields = form_price_fields(
        query.sale_price.as_deref().unwrap_or_default(),
        query.cost_price.as_deref().unwrap_or_default(),
        query.markup_pct.as_deref().unwrap_or_default(),
        &localization,
    );
    let input = if !carries_a_form {
        // No form: the stored row is the ladder, and the drawer already has the
        // stored values in its own fields.
        None
    } else if [fields.sale_price, fields.cost_price, fields.markup_pct]
        .contains(&PriceField::Unreadable)
    {
        // THE LADDER'S OWN PRECEDENCE, and it is documented on
        // `LadderInput::Unreadable`: an unreadable field stops the preview
        // before any form-shape gate is considered. The save path checks the
        // sale price before the cost, so the same request would be answered for
        // the missing price and never mention the field the operator is
        // actually typing into. The ladder answers what the operator needs
        // instead — WHICH field cannot be read — because a preview has nothing
        // to say once a field is not a number.
        Some(LadderInput::Unreadable)
    } else {
        match resolve_price_fields(fields) {
            Ok(prices) => Some(ladder_input_from(prices, kind)),
            // A refusal the SAVE would also answer (an empty manual price) is a
            // state the ladder reports, not an error of its own. The cost the
            // form does hold still belongs on the ladder.
            Err(PriceFieldError::Refusal(refusal)) => Some(LadderInput::Refused {
                refusal,
                cost_price: fields.cost_price.as_value().unwrap_or(Decimal::ZERO),
            }),
            // Unreachable by the check above; kept total so a future field
            // cannot silently fall through into a fabricated preview.
            Err(PriceFieldError::Unreadable(_)) => Some(LadderInput::Unreadable),
        }
    };
    let ladder = state
        .tax_service
        .product_price_ladder(query.id, input)
        .await?;
    product_price_ladder_html(&localization, ladder)
}

/// The drawer body with fresh derived data. The detail read and every mutating
/// drawer action answer it, so saving/costs/movements refresh the drawer in
/// place without the client rebuilding a URL. The audit actors are resolved
/// HERE, in the wiring layer, because a department may not read identity
/// tables (AC20) and the view must show a name, never an id — the same way
/// the finance detail does it.
async fn product_detail_html(
    state: &AppState,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<Html<String>> {
    let ps = state.inventory_service.product_stock(id).await?;
    let categories = state.inventory_service.categories.list().await?;
    let product_taxes = state.tax_service.list_product_taxes(id).await?;
    let available_taxes = state.tax_service.list_active_taxes_excluding(id).await?;
    let suppliers = state.supplier_service.list_suppliers().await?;
    let costs = state.supplier_service.costs.list_by_product(id).await?;
    let supplier_costs = costs
        .into_iter()
        .map(|cost| {
            let supplier_name = suppliers
                .iter()
                .find(|s| s.id == cost.supplier_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{} #{}",
                        localization.tr(crate::localization::MessageKey::ProductSupplier),
                        cost.supplier_id
                    )
                });
            ProductCostView {
                cost,
                supplier_name,
            }
        })
        .collect();
    let mut actor_ids = vec![ps.product.created_by];
    actor_ids.extend(ps.product.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let created_by_name = name_for(ps.product.created_by);
    let updated_by_name = ps.product.updated_by.and_then(name_for);
    let today = localization.today_iso();
    // Derived, never stored (cost-freshness S1): the supplier reference cost
    // disagrees with the stored cost only when there IS a supplier truth to
    // compare against (no rows ⇒ the product column IS the truth), the stored
    // cost was ever recorded (0 is the NOT NULL DEFAULT, "no cost yet", not a
    // cost), and the two genuinely differ (equal ⇒ fresh).
    let stale_cost = match (
        state.supplier_service.reference_cost(id).await?,
        ps.product.cost_price != Decimal::ZERO,
    ) {
        (Some(r), true) if r != ps.product.cost_price => Some(StaleCostView {
            reference: r,
            stored: ps.product.cost_price,
        }),
        _ => None,
    };
    // The drawer has no form values on its first render, so the ladder reports
    // the stored state — the very figures the form is prefilled with, so the
    // two cannot disagree on arrival. Read ONCE: the refusal sentence the
    // embedded fragment prints is derived from this same value, through the one
    // shared mapping the product save form also goes through.
    let ladder = state.tax_service.product_price_ladder(id, None).await?;
    let net_refusal_message = ladder_refusal_message(&ladder, localization);
    let html = ProductDetailPartial {
        localization: localization.clone(),
        product: ps.product,
        stock: ps.stock,
        suggested: ps.suggested,
        created_by_name,
        updated_by_name,
        categories,
        product_taxes,
        available_taxes,
        // The drawer has no form values on its first render, so the ladder
        // reports the stored state — the very figures the form is prefilled
        // with, so the two cannot disagree on arrival.
        ladder,
        // The embedded ladder fragment reads this.
        net_refusal_message,
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
pub struct ProductTaxForm {
    pub product_id: i64,
    pub tax_id: i64,
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

fn parse_opt_decimal(s: &str, localization: &LocalizationContext) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    localization
        .parse_decimal(t)
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

// ---------------------------------------------------------------------------
// The product price ladder's inputs (U2)
//
// ONE reading of the three price fields, shared by the save path
// (`/web/products`, `/web/products/edit`) and the read-only ladder preview
// (`/web/product-price-ladder`). The rule each one follows:
//
// * EMPTY is a value, not an absence. An empty markup means "no markup", the
//   manual-price mode; an empty cost is the column's "no cost recorded" zero.
//   A save accepts both, so the ladder reports both.
// * UNREADABLE is a typo. A save answers 400; the ladder refuses to guess and
//   reports the last state the save path accepted, because a preview that
//   invented a number from a typo is worse than no preview.
// ---------------------------------------------------------------------------

/// How one form value reads, in the request's own locale.
fn read_price_field(raw: &str, localization: &LocalizationContext) -> PriceField {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return PriceField::Empty;
    }
    match localization.parse_decimal(trimmed) {
        Ok(value) => PriceField::Value(value),
        Err(_) => PriceField::Unreadable,
    }
}

/// The three form price fields, read the one way, and — if they refuse — answered
/// in the operator's language.
///
/// Both save routes go through here, so a refusal produced by the FORM's shape
/// (an emptied manual price) reaches the operator through the same mapping a
/// refusal produced by a price RULE does. A route that resolved its own fields
/// would be free to answer one of the two in English, and the ladder would then
/// disagree with it in the one case the two are most likely to hit.
fn resolved_form_prices(
    sale_price: &str,
    cost_price: &str,
    markup_pct: &str,
    localization: &LocalizationContext,
) -> AppResult<ResolvedPrices> {
    resolve_price_fields(form_price_fields(
        sale_price,
        cost_price,
        markup_pct,
        localization,
    ))
    .map_err(PriceFieldError::into_app_error)
    .map_err(|error| localized_refusal_error(error, localization))
}

/// Why a form's price fields could not become a product's prices.
#[derive(Debug)]
enum PriceFieldError {
    /// A field was not a number in this locale. The payload is the save path's
    /// own refusal for that exact field, so answering 400 changes nothing an
    /// existing caller can observe. It is NOT a price RULE — nothing was refused
    /// for being out of range, the text simply is not a number here — so it
    /// stays a plain message and is out of this module's localization scope.
    Unreadable(&'static str),
    /// The save path's own form-shape refusal, as the rule it is.
    Refusal(PriceRefusal),
}

impl PriceFieldError {
    fn into_app_error(self) -> AppError {
        match self {
            Self::Unreadable(message) => AppError::Validation(message.into()),
            Self::Refusal(refusal) => AppError::PriceRefused(refusal),
        }
    }
}

/// The three prices a product form submits, read exactly once for every caller.
#[derive(Debug, Clone, Copy)]
struct ResolvedPrices {
    sale_price: Decimal,
    cost_price: Decimal,
    markup_pct: Option<Decimal>,
}

/// The reading, in the order the handlers used to apply it — and the order is
/// load-bearing for the messages: the markup decides whether an empty sale
/// price is a refusal or a placeholder, so it is read first.
fn resolve_price_fields(fields: PriceFields) -> Result<ResolvedPrices, PriceFieldError> {
    let markup_pct = match fields.markup_pct {
        PriceField::Value(value) => Some(value),
        PriceField::Empty => None,
        PriceField::Unreadable => return Err(PriceFieldError::Unreadable("invalid markup_pct")),
    };
    // An empty sale_price is only an error when the price is manual. With a
    // markup the service DERIVES and validates the price and ignores the
    // incoming one, so Decimal::ZERO is a safe placeholder that can never
    // reach the database.
    let sale_price = match fields.sale_price {
        PriceField::Value(value) => value,
        PriceField::Empty if markup_pct.is_some() => Decimal::ZERO,
        PriceField::Empty => return Err(PriceFieldError::Refusal(PriceRefusal::SalePriceRequired)),
        PriceField::Unreadable => return Err(PriceFieldError::Unreadable("invalid sale_price")),
    };
    let cost_price = match fields.cost_price {
        PriceField::Value(value) => value,
        PriceField::Empty => Decimal::ZERO,
        PriceField::Unreadable => return Err(PriceFieldError::Unreadable("invalid cost_price")),
    };
    Ok(ResolvedPrices {
        sale_price,
        cost_price,
        markup_pct,
    })
}

/// The ladder's input from an already-resolved form: the very values a save
/// would submit, so the preview cannot disagree with the save. `kind` is
/// `None` when the form named none that could be read.
fn ladder_input_from(prices: ResolvedPrices, kind: Option<ProductKind>) -> LadderInput {
    LadderInput::Form {
        kind,
        sale_price: prices.sale_price,
        cost_price: prices.cost_price,
        markup_pct: prices.markup_pct,
    }
}

/// The three raw form values, read the one way every caller reads them.
fn form_price_fields(
    sale_price: &str,
    cost_price: &str,
    markup_pct: &str,
    localization: &LocalizationContext,
) -> PriceFields {
    PriceFields {
        sale_price: read_price_field(sale_price, localization),
        cost_price: read_price_field(cost_price, localization),
        markup_pct: read_price_field(markup_pct, localization),
    }
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
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateProductForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: ProductKind = if form.kind.trim().is_empty() {
        ProductKind::Product
    } else {
        form.kind.parse().map_err(AppError::Validation)?
    };
    let prices = resolved_form_prices(
        &form.sale_price,
        &form.cost_price,
        &form.markup_pct,
        &localization,
    )?;
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
        // Read above by the one shared reader: `Some` derives and validates the
        // price in the service; `None` (empty field) keeps the price manual.
        sale_price: prices.sale_price,
        cost_price: prices.cost_price,
        track_stock,
        min_stock: parse_opt_decimal(&form.min_stock, &localization)?,
        max_stock: parse_opt_decimal(&form.max_stock, &localization)?,
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
        markup_pct: prices.markup_pct,
    };
    let created = state
        .inventory_service
        .create_product(principal.user_id, input)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
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
        let mut html = render_product_list(&products, &localization)?;
        if hidden {
            // Prepended so the out-of-band notice opens the answer body; htmx
            // removes the wrapper from the main swap either way. The
            // transport (body, not header), the template home and the marker
            // are documented on `hidden_by_filter_notice_html`.
            html = hidden_by_filter_notice_html(&created.name, &localization)? + &html;
        }
        let mut resp = Html(html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "product-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

// The product-tax ASSOCIATION is the only tax surface left on this screen: the
// drawer links and unlinks an existing definition. A tax DEFINITION (code, name,
// rate, active) is owned by Settings, addressed under `/web/settings/taxes…`
// behind `settings.manage`, so no `inventory.write` WEB route in this module can
// create, rename, re-rate or activate/deactivate one.
//
// That is not a web-only rule either: the JSON API's three definition routes
// (`POST /api/taxes`, `PUT /api/taxes/{id}`, `POST /api/taxes/{id}/deactivate`)
// are `Require<SettingsManage>` as well, while the association below is
// `inventory.write` on both surfaces. See the URL decision in `settings_web.rs`.

async fn web_link_product_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<ProductTaxForm>,
) -> AppResult<axum::response::Response> {
    state
        .tax_service
        .link_product_tax(principal.user_id, form.product_id, form.tax_id)
        .await?;
    product_tax_mutation_response(&state, &headers, form.product_id, &localization).await
}

async fn web_unlink_product_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<ProductTaxForm>,
) -> AppResult<axum::response::Response> {
    state
        .tax_service
        .unlink_product_tax(form.product_id, form.tax_id)
        .await?;
    product_tax_mutation_response(&state, &headers, form.product_id, &localization).await
}

async fn product_tax_mutation_response(
    state: &AppState,
    headers: &HeaderMap,
    product_id: i64,
    localization: &LocalizationContext,
) -> AppResult<axum::response::Response> {
    if is_htmx(headers) {
        let html = product_detail_html(state, product_id, localization).await?;
        return Ok(triggered(html.0, "product-taxes-changed"));
    }
    Ok(Redirect::to("/products").into_response())
}

async fn web_create_movement(
    State(state): State<AppState>,
    _: Require<InventoryStockWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateMovementForm>,
) -> Result<axum::response::Response, AppError> {
    let movement_type: MovementType = form.kind.parse().map_err(AppError::Validation)?;
    let qty = localization
        .parse_decimal(form.qty.trim())
        .map_err(|_| AppError::Validation("invalid qty".into()))?;
    let reason: MovementReason = if form.reason.trim().is_empty() {
        MovementReason::Initial
    } else {
        form.reason.parse().map_err(AppError::Validation)?
    };
    let date = if form.date.trim().is_empty() {
        localization
            .today_iso()
            .parse()
            .map_err(|_| AppError::Internal("invalid localized date".into()))?
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
    state
        .inventory_service
        .record_movement(principal.user_id, input)
        .await?;
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
            let html = product_detail_html(&state, form.product_id, &localization).await?;
            return Ok(triggered(html.0, "movement-created"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let mut resp =
            Html(filtered_list_html(&state, &form.q, &form.category_id, &localization).await?)
                .into_response();
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
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Query(filter): Query<WebProductFilter>,
    Form(form): Form<EditProductForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: ProductKind = if form.kind.trim().is_empty() {
        ProductKind::Product
    } else {
        form.kind.parse().map_err(AppError::Validation)?
    };
    let prices = resolved_form_prices(
        &form.sale_price,
        &form.cost_price,
        &form.markup_pct,
        &localization,
    )?;
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
        sale_price: Some(prices.sale_price),
        cost_price: Some(prices.cost_price),
        track_stock: Some(track_stock),
        min_stock: Some(parse_opt_decimal(&form.min_stock, &localization)?),
        max_stock: Some(parse_opt_decimal(&form.max_stock, &localization)?),
        location: Some(if form.location.trim().is_empty() {
            None
        } else {
            Some(form.location)
        }),
        notes: Some(if form.notes.trim().is_empty() {
            None
        } else {
            Some(form.notes)
        }),
        // The drawer always sends the field: an empty markup is an explicit
        // clear back to a manual price (`Some(None)`); a value re-derives the
        // price from the cost.
        markup_pct: Some(prices.markup_pct),
    };
    state
        .inventory_service
        .update_product(principal.user_id, form.id, patch)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.id, &localization).await?;
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
        let html = ProductListPartial {
            products,
            localization: localization.clone(),
        }
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
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<RecordProductCostForm>,
) -> Result<axum::response::Response, AppError> {
    let cost = localization
        .parse_decimal(form.cost.trim())
        .map_err(|_| AppError::Validation("invalid cost".into()))?;
    let date = if form.date.trim().is_empty() {
        localization
            .today_iso()
            .parse()
            .map_err(|_| AppError::Internal("invalid localized date".into()))?
    } else {
        form.date
            .trim()
            .parse()
            .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?
    };
    state
        .supplier_service
        .record_cost(
            principal.user_id,
            form.product_id,
            form.supplier_id,
            cost,
            date,
        )
        .await?;
    if is_htmx(&headers) {
        let from_drawer = headers
            .get("HX-Target")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("product-drawer-body"))
            .unwrap_or(false);
        if from_drawer {
            let html = product_detail_html(&state, form.product_id, &localization).await?;
            return Ok(triggered(html.0, "product-cost-recorded"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let html = filtered_list_html(&state, &form.q, &form.category_id, &localization).await?;
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
    Extension(localization): Extension<LocalizationContext>,
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
            let html = product_detail_html(&state, form.product_id, &localization).await?;
            return Ok(triggered(html.0, "product-cost-recorded"));
        }
        // Non-drawer callers get the list they are looking at (issue #37): the
        // filter rides the body via `hx-include="#product-filters"`.
        let html = filtered_list_html(&state, &form.q, &form.category_id, &localization).await?;
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
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .inventory_service
        .set_product_active(principal.user_id, form.product_id, true)
        .await?;
    product_lifecycle_response(&state, &headers, &form, &localization).await
}

async fn web_deactivate_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .inventory_service
        .set_product_active(principal.user_id, form.product_id, false)
        .await?;
    product_lifecycle_response(&state, &headers, &form, &localization).await
}

async fn web_delete_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<ProductIdForm>,
) -> Result<axum::response::Response, AppError> {
    state
        .inventory_service
        .delete_product(form.product_id)
        .await?;
    product_lifecycle_response(&state, &headers, &form, &localization).await
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
    localization: &LocalizationContext,
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
        let html = ProductListPartial {
            products,
            localization: localization.clone(),
        }
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
        .route("/web/product-taxes", post(web_link_product_tax))
        .route("/web/product-taxes/unlink", post(web_unlink_product_tax))
        .route("/web/category-options", get(web_category_options))
        .route("/web/product-options", get(web_product_options))
        .route("/web/product-search.json", get(web_product_search_json))
        .route("/web/products/detail/{id}", get(web_product_detail))
        .route("/web/product-price-ladder", get(web_product_price_ladder))
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
            html.contains("Action not permitted"),
            "the refusal must use the English fallback: {html:.400}"
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
                },
            )
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
        let token = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
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
        let after = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
        let after = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
        assert!(
            after.is_active,
            "a refused deactivate must not flip the flag"
        );
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
            html.contains("Action not permitted"),
            "the refusal must use the English fallback: {html:.400}"
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
        let state = test_state().await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, "/products").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Productos") || html.contains("Products"),
            "page should mention products"
        );
        assert!(
            html.contains(localization.tr(crate::localization::MessageKey::ProductLowStock)),
            "page should have low-stock section"
        );
    }

    #[tokio::test]
    async fn web_product_list_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/products").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn web_low_stock_fragment_renders() {
        let state = test_state().await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, "/web/low-stock").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(localization.tr(crate::localization::MessageKey::ProductStockOk)));
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
        assert!(
            html.contains("WEB-1"),
            "fragment should contain new sku: {html:.300}"
        );
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
                },
            )
            .await
            .unwrap();
        state
            .inventory_service
            .add_barcode(product.id, "7791234567890")
            .await
            .unwrap();
        product
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

            // The island renders these strings verbatim, so they must be the
            // request's display form rather than canonical API decimals. The
            // pre-setup fallback is USD and the stored scale is preserved.
            assert_eq!(row["sale_price"], "25 USD", "{needle}: {json}");
            assert_eq!(row["cost_price"], "10 USD", "{needle}: {json}");

            let stock = row["stock"].as_str().expect("stock as string");
            assert_eq!(stock.parse::<f64>().unwrap(), 0.0, "{needle}: {json}");
        }
    }

    // Deleted with the HTML route (T4c): `n5_product_search_json_money_is_the_fragments_display_form`
    // pinned the wire money against that fragment. The request-localized display
    // form is now pinned literally by
    // `n5_product_search_json_matches_name_sku_and_barcode`.

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

    /// N5: matching folds accents and case on both sides — the same behaviour
    /// the party searches share (N6) — now pinned on the route the island
    /// reads. The gate is paired so it cannot pass vacuously, in both of the
    /// ways a match test can be vacuous:
    ///
    /// - The SKUs are opaque (`P-001`, `P-002`): `match_catalogue` matches on
    ///   name OR sku, so a SKU that echoed the query text (e.g. `CAFE-1`) would
    ///   let every case pass through the SKU branch with the folding deleted.
    ///   With opaque SKUs the name branch is the only path to a match, and
    ///   neither `CAFE` nor `cafÉ` can match the unfolded `Café`/`cafÉ` — so a
    ///   broken fold fails on the query side AND on the stored side.
    /// - The exact-one-match check plus the other-name exclusion fails if the
    ///   read stopped discriminating and matched everything.
    #[tokio::test]
    async fn n5_product_search_json_folds_accents_and_case() {
        use crate::models::{NewProduct, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state().await;
        for (sku, name) in [("P-001", "Café"), ("P-002", "Ñandú")] {
            state
                .inventory_service
                .create_product(
                    audit_actor_id(&state).await,
                    NewProduct {
                        sku: sku.into(),
                        name: name.into(),
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
                    },
                )
                .await
                .unwrap();
        }
        let app = crate::routes::router(state);

        for (needle, name, other) in [
            ("CAFE", "Café", "Ñandú"),
            ("cafÉ", "Café", "Ñandú"),
            ("nandu", "Ñandú", "Café"),
            ("ÑANDÚ", "Ñandú", "Café"),
        ] {
            let (status, json) =
                get_json(app.clone(), &format!("/web/product-search.json?q={needle}")).await;
            assert_eq!(status, StatusCode::OK, "{needle}: {json}");
            let products = json["products"].as_array().expect("products array");
            assert_eq!(products.len(), 1, "{needle}: exactly one match: {json}");
            assert_eq!(products[0]["name"], name, "{needle}: {json}");
            assert!(
                !json.to_string().contains(other),
                "{needle}: {other} must not match: {json}"
            );
        }
    }

    // -- T2 redesign-products: the product drawer routes -----------------------

    use crate::models::{NewProduct, NewSupplier, PriceRefusal, ProductKind};
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
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
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
                },
            )
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
                },
            )
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(
                audit_actor_id(&state).await,
                NewSupplier {
                    name: "Detail Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        state
            .supplier_service
            .record_cost(
                audit_actor_id(&state).await,
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

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let stock_summary = format!(
            "{} 5",
            localization.tr(crate::localization::MessageKey::ProductStock)
        );
        for expected in [
            "product-detail-inner",
            "prod DETAIL-1",
            stock_summary.as_str(),
            "Detail Sup",
            "12.50",
            localization.tr(crate::localization::MessageKey::ProductPreferred),
        ] {
            assert!(
                html.contains(expected),
                "drawer must show {expected}: {html:.900}"
            );
        }

        // The global notice region labels its success/error messages with the
        // submitting form's `data-action`, so renaming one of these labels is a
        // silent UX regression: every HTTP-status test still sees 200 and only
        // the toast text degrades. Pin the exact three labels and forbid any
        // other `data-action` in the fragment.
        for label in [
            localization.tr(crate::localization::MessageKey::ProductSave),
            localization.tr(crate::localization::MessageKey::ProductLinkTax),
            localization.tr(crate::localization::MessageKey::ProductRecordCost),
            localization.tr(crate::localization::MessageKey::ProductRecordMovement),
        ] {
            let attr = format!("data-action=\"{label}\"");
            assert_eq!(
                html.matches(&attr).count(),
                1,
                "exactly one data-action '{label}' in the drawer fragment: {html:.900}"
            );
        }
        assert_eq!(
            html.matches("data-action=").count(),
            4,
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
                    due_days: None,
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
        seed_supplier_cost(
            &state,
            product.id,
            "Stale Sup",
            Decimal::from_str("12.50").unwrap(),
            true,
        )
        .await;

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let expected = vec![
            localization
                .tr(crate::localization::MessageKey::ProductStaleCost)
                .to_string(),
            format!(
                "{} 12.50 USD",
                localization.tr(crate::localization::MessageKey::ProductReferenceCost)
            ),
            format!(
                "{} 5 USD",
                localization.tr(crate::localization::MessageKey::ProductStoredCost)
            ),
        ];
        for expected in &expected {
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

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains(localization.tr(crate::localization::MessageKey::ProductStaleCost)),
            "equal costs must not render the badge: {html:.900}"
        );
    }

    /// No supplier rows means `reference_cost` is `None`: the product column
    /// IS the truth, so there is nothing to compare and no badge.
    #[tokio::test]
    async fn web_product_detail_hides_stale_cost_badge_without_supplier_rows() {
        let state = test_state().await;
        let product = seed_product_with_cost(&state, "STALE-3", Decimal::from(5)).await;

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains(localization.tr(crate::localization::MessageKey::ProductStaleCost)),
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
        seed_supplier_cost(
            &state,
            product.id,
            "Zero-Cost Sup",
            Decimal::from_str("12.50").unwrap(),
            true,
        )
        .await;

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains(localization.tr(crate::localization::MessageKey::ProductStaleCost)),
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

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let reference = localization.tr(crate::localization::MessageKey::ProductReferenceCost);
        assert!(
            html.contains(&format!("{reference} 20 USD")),
            "badge must show the preferred supplier's cost: {html:.900}"
        );
        assert!(
            !html.contains(&format!("{reference} 8 USD")),
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
            .create_category(audit_actor_id(&state).await, "Edit Cat A", None)
            .await
            .unwrap();
        let cat_b = state
            .inventory_service
            .create_category(audit_actor_id(&state).await, "Edit Cat B", None)
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
                },
            )
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
            .create_category(audit_actor_id(&state).await, "Clear Cat", None)
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
                },
            )
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

        let stored = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
                },
            )
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

        let stored = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
        assert_eq!(
            stored.sale_price,
            Decimal::from_str("10").unwrap(),
            "the last price survives the clear"
        );
        assert_eq!(
            stored.markup_pct, None,
            "an empty markup field clears back to manual"
        );
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
        let stored = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
        let stored = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html(app, &format!("/web/products/detail/{}", product.id)).await;
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
            .create_supplier(
                audit_actor_id(&state).await,
                NewSupplier {
                    name: "Cost Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
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
            .create_supplier(
                audit_actor_id(&state).await,
                NewSupplier {
                    name: "Pref A".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let b = state
            .supplier_service
            .create_supplier(
                audit_actor_id(&state).await,
                NewSupplier {
                    name: "Pref B".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        for (sid, cost) in [(a.id, "9"), (b.id, "8")] {
            state
                .supplier_service
                .record_cost(
                    audit_actor_id(&state).await,
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
                },
            )
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
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
            html.contains("product-detail-inner")
                && html.contains(&format!(
                    "{} 5",
                    localization.tr(crate::localization::MessageKey::ProductStock)
                )),
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
            .create_category(audit_actor_id(&state).await, "Life Cat A", None)
            .await
            .unwrap();
        let cat_b = state
            .inventory_service
            .create_category(audit_actor_id(&state).await, "Life Cat B", None)
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
                },
            )
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
                },
            )
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
        let after = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
        let after = state
            .inventory_service
            .get_product(product.id)
            .await
            .unwrap();
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
        let (status, _) =
            get_html(app.clone(), &format!("/web/products/detail/{}", plain_id)).await;
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
                },
            )
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
        assert_eq!(
            status,
            StatusCode::OK,
            "a product with movements survives delete"
        );
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

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let (status, page) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let registered_by = format!(
            "{} Test Admin",
            localization.tr(crate::localization::MessageKey::AuditRegisteredBy)
        );
        let updated_by = format!(
            "{} Test Probe",
            localization.tr(crate::localization::MessageKey::AuditUpdatedBy)
        );
        assert_eq!(
            page.matches(&registered_by).count(),
            1,
            "the detail names the creator's display name: {page}"
        );
        assert_eq!(
            page.matches(&updated_by).count(),
            1,
            "the edit names its editor: {page}"
        );
        assert!(
            !page.contains(&format!("{registered_by}").replace("Test Admin", "1")),
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
        let product_id: i64 = sqlx::query_scalar("SELECT id FROM products WHERE sku = 'LOW-VIEW'")
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

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let (status, html) = get_html(app, "/web/low-stock").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let registered_by = format!(
            "{} Test Admin",
            localization.tr(crate::localization::MessageKey::AuditRegisteredBy)
        );
        assert_eq!(
            html.matches(&registered_by).count(),
            1,
            "the stock row names the product's creator, and says that is what it names: {html}"
        );
        assert!(
            !html.contains(&registered_by.replace("Test Admin", "1")),
            "the fragment never renders a raw user id: {html}"
        );
    }

    // -----------------------------------------------------------------------
    // T2: the product drawer shows the net price, the linked-tax breakdown and
    // the derived tax-inclusive price, without ever writing the stored price.
    // -----------------------------------------------------------------------

    async fn product_with_taxes(state: &AppState, sku: &str, price: &str) -> i64 {
        let product = state
            .inventory_service
            .create_product(
                audit_actor_id(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Product {sku}"),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from_str(price).unwrap(),
                    cost_price: Decimal::from_str("5").unwrap(),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        product.id
    }

    /// The drawer must show all three figures an operator needs to price with,
    /// localized, and the stored net sale price must be exactly what it was.
    #[tokio::test]
    async fn tax_preview_shows_net_breakdown_and_tax_inclusive_price_in_the_drawer() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "TAX-DRAWER", "100").await;
        let actor = audit_actor_id(&state).await;
        let iva = state
            .tax_service
            .create_tax(
                actor,
                crate::models::NewTax {
                    code: "IVA21".into(),
                    name: "IVA 21%".into(),
                    rate: Decimal::from_str("21").unwrap(),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        let iibb = state
            .tax_service
            .create_tax(
                actor,
                crate::models::NewTax {
                    code: "IIBB10".into(),
                    name: "IIBB 10%".into(),
                    rate: Decimal::from_str("10").unwrap(),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        state
            .tax_service
            .link_product_tax(actor, product_id, iva.id)
            .await
            .unwrap();
        state
            .tax_service
            .link_product_tax(actor, product_id, iibb.id)
            .await
            .unwrap();

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");

        // Every figure is rendered through the localized helpers, so the same
        // stored value can be asserted without hard-coding a convention.
        let net = localization.format_currency(Decimal::from_str("100").unwrap());
        let tax = localization.format_currency(Decimal::from_str("31").unwrap());
        let gross = localization.format_currency(Decimal::from_str("131").unwrap());
        assert!(html.contains(&net), "the net price must be shown: {html}");
        assert!(html.contains(&tax), "the tax total must be shown: {html}");
        assert!(
            html.contains(&gross),
            "the tax-inclusive price must be shown: {html}"
        );
        assert!(
            html.contains("IVA21") && html.contains("IIBB10"),
            "the linked taxes must be named: {html}"
        );
        for key in [
            crate::localization::MessageKey::TaxNetPrice,
            crate::localization::MessageKey::TaxInclusivePrice,
            crate::localization::MessageKey::TaxRate,
        ] {
            assert!(
                html.contains(localization.tr(key)),
                "{key:?} must label the preview: {html}"
            );
        }

        // The stored net price is untouched by the preview.
        let stored: String = sqlx::query_scalar("SELECT sale_price FROM products WHERE id = ?")
            .bind(product_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(stored, "100", "the preview must never write the net price");
    }

    /// A product with no linked tax still shows the two prices — equal — and
    /// says plainly that nothing is linked, instead of rendering an empty table.
    #[tokio::test]
    async fn tax_preview_of_an_untaxed_product_says_so_and_keeps_both_prices() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "TAX-DRAWER-0", "42").await;

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let price = Decimal::from_str("42").unwrap();
        assert_eq!(
            html.matches(&localization.format_currency(price)).count() >= 2,
            true,
            "both the net and the tax-inclusive price render the same value: {html}"
        );
        let notice = localization.tr(crate::localization::MessageKey::TaxNoBreakdown);
        assert!(
            html.contains(notice),
            "an untaxed product must say so: {html}"
        );
        // Exactly ONCE. The header explains why the two prices are equal and the
        // tax card below already says nothing is linked; saying it a second time
        // is duplicate copy in one card, and the visual baseline caught it.
        assert_eq!(
            html.matches(notice).count(),
            1,
            "the no-tax notice must not be repeated: {html}"
        );
    }

    /// The drawer already edits associations, and that must keep working: the
    /// link answer re-renders the drawer with the new breakdown included.
    #[tokio::test]
    async fn tax_preview_survives_linking_a_tax_from_the_drawer() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "TAX-LINK", "200").await;
        let actor = audit_actor_id(&state).await;
        let iva = state
            .tax_service
            .create_tax(
                actor,
                crate::models::NewTax {
                    code: "IVA21".into(),
                    name: "IVA 21%".into(),
                    rate: Decimal::from_str("21").unwrap(),
                    is_active: true,
                },
            )
            .await
            .unwrap();

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let (status, html) = post_form_with_cookie(
            app.clone(),
            "/web/product-taxes",
            &format!("product_id={product_id}&tax_id={}", iva.id),
            &[("HX-Request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("IVA21"),
            "the linked tax is now shown: {html}"
        );
        assert!(
            html.contains(&localization.format_currency(Decimal::from_str("42").unwrap())),
            "21% of 200 is a 42 tax total: {html}"
        );
        assert!(
            html.contains(&localization.format_currency(Decimal::from_str("242").unwrap())),
            "the tax-inclusive price follows the link: {html}"
        );

        // The association really was written, not only rendered.
        let linked: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM product_taxes WHERE product_id = ?")
                .bind(product_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(linked, 1);
    }

    // -----------------------------------------------------------------------
    // U2 (product price ladder): ONE ordered, server-computed ladder — cost,
    // markup, net sale price, each tax with its amount, total taxes and the
    // tax-inclusive price — previewed from the drawer's own form values and
    // never written anywhere.
    // -----------------------------------------------------------------------

    const LADDER_PATH: &str = "/web/product-price-ladder";

    /// Money as the ACTIVE locale formats it, so a test asserts the value, not
    /// a convention.
    fn money(localization: &crate::localization::LocalizationContext, value: &str) -> String {
        localization.format_currency(Decimal::from_str(value).unwrap())
    }

    /// A product in either derivation mode: `markup: None` keeps a manual net
    /// price, `Some` derives it from the cost exactly as the save path does.
    async fn product_with_markup(
        state: &AppState,
        sku: &str,
        price: &str,
        cost: &str,
        markup: Option<&str>,
    ) -> i64 {
        state
            .inventory_service
            .create_product(
                audit_actor_id(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Product {sku}"),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    // A markup makes the server ignore this field, so the
                    // absurd value proves the derivation rather than a
                    // coincidence.
                    sale_price: Decimal::from_str(price).unwrap(),
                    cost_price: Decimal::from_str(cost).unwrap(),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: markup.map(|m| Decimal::from_str(m).unwrap()),
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn link_tax(state: &AppState, code: &str, name: &str, rate: &str) -> i64 {
        state
            .tax_service
            .create_tax(
                audit_actor_id(state).await,
                crate::models::NewTax {
                    code: code.into(),
                    name: name.into(),
                    rate: Decimal::from_str(rate).unwrap(),
                    is_active: true,
                },
            )
            .await
            .unwrap()
            .id
    }

    /// The read-only preview, sent EXACTLY as the drawer sends it: the whole
    /// `hx-include="closest form"` body, field for field, under the keys the
    /// drawer's own inputs carry — the product id included, which is `id`.
    ///
    /// Nothing here is invented for the test. A hand-built `?product_id=…` that
    /// the browser never sends is precisely how a dead endpoint can sit behind a
    /// green suite, so every test in this block goes through this one builder.
    /// `prices` is the tail of the price fields the test is about, e.g.
    /// `cost_price=0&markup_pct=50`; `kind` is passed here by the tests that
    /// exercise it, so a query can never carry the same key twice. An empty
    /// `prices` is a bare read with no price fields at all, which is the one
    /// shape the browser never sends and which the stored-state tests need.
    async fn preview(app: axum::Router, product_id: i64, prices: &str) -> (StatusCode, String) {
        // One literal on one line: `cargo fmt` rewraps a `\` continuation and
        // the indentation it leaves behind is not a valid URI character.
        let body = format!("id={product_id}&sku=LADDER-BODY&name=Ladder+body&category_id=&unit=un&track_stock=&min_stock=&max_stock=&location=&notes=&{prices}");
        get_html(app, &format!("{LADDER_PATH}?{body}")).await
    }

    // -----------------------------------------------------------------------
    // Independent verification round: the ladder must work in a REAL browser
    // and must refuse everything the save path refuses.
    // -----------------------------------------------------------------------

    /// BLOCKER 1, proven at the HTTP boundary: the request the BROWSER sends
    /// must answer 200. The drawer's three price fields carry
    /// `hx-include="closest form"`, so the body is the whole edit form and the
    /// product id arrives under the form's own key, `id`. Every other test in
    /// this block already goes through `preview`, which sends that exact body;
    /// this one states the contract in one place so the field-name contract
    /// cannot drift again.
    #[tokio::test]
    async fn product_price_ladder_answers_the_drawers_own_request_shape() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-BODY", "42").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        // Field for field, exactly as `templates/partials/product_detail.html`
        // renders the edit form, with the three price fields left out so the
        // request is the bare form a direct read makes. One literal on one
        // line: a `\` continuation is rewrapped by `cargo fmt`, and the spaces
        // it leaves behind are not valid in a URI.
        let body = format!("id={product_id}&sku=LADDER-BODY&name=Ladder+body&kind=Product&category_id=&unit=un&sale_price=42&cost_price=5&markup_pct=&track_stock=&min_stock=&max_stock=&location=&notes=");
        let (status, html) = get_html(app, &format!("{LADDER_PATH}?{body}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("id=\"product-price-ladder\""),
            "the browser's request must be answered with the ladder: {html}"
        );
        assert!(
            html.contains(&money(&localization, "42")),
            "and with the figures it asked about: {html}"
        );
    }

    /// BLOCKER 2: a markup whose derived price rounds to zero is a state the
    /// save path refuses ("sale_price must be > 0 for products"). The ladder
    /// must carry that refusal and publish no money at all — a net of 0.00 with
    /// a tax total beside it is exactly the fabricated figure this ladder
    /// exists not to show.
    #[tokio::test]
    async fn product_price_ladder_refuses_a_derived_price_that_rounds_to_zero() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-ZERO", "42").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        // 0.05 * (1 - 99/100) = 0.0005, which pins to 0.00 at cents.
        let (status, html) = preview(
            app,
            product_id,
            "cost_price=0.05&markup_pct=-99&sale_price=1",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("sale_price must be &gt; 0 for products"),
            "the ladder must carry the save path's own refusal: {html}"
        );
        assert_publishes_no_tax_money(&localization, &html);
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("42"),
            "and a refusal writes nothing"
        );
    }

    /// BLOCKER 2: a negative cost is refused by the save path too, and the
    /// ladder must not publish a price built on one. It has no markup here, so
    /// the derivation rule stays silent and the COST rule is what refuses —
    /// which is exactly the state a hand-written mirror would miss.
    #[tokio::test]
    async fn product_price_ladder_refuses_a_negative_cost() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-NEGCOST", "42").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) =
            preview(app, product_id, "cost_price=-5&markup_pct=&sale_price=42").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("cost_price cannot be negative"),
            "the ladder must carry the save path's own refusal: {html}"
        );
        assert_publishes_no_tax_money(&localization, &html);
    }

    /// LOW: the emptied-manual-price branch, at the HTTP boundary. A manual
    /// product whose sale price the operator clears is a save refusal, and the
    /// ladder reports the same one instead of inventing a price.
    #[tokio::test]
    async fn product_price_ladder_reports_the_save_paths_refusal_for_an_emptied_manual_price() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-EMPTIED", "42").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = preview(app, product_id, "cost_price=5&markup_pct=&sale_price=").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("sale_price is required"),
            "the ladder must carry the save path's own refusal: {html}"
        );
        assert_publishes_no_tax_money(&localization, &html);
        assert_eq!(stored_net(&state, product_id).await, dec("42"));
    }

    /// MEDIUM: provenance must be honest. A figure the operator has typed but
    /// not saved is NOT stored, and labelling it "Stored" is a lie the footer
    /// then contradicts. Each of the three stored columns is marked per row.
    #[tokio::test]
    async fn product_price_ladder_never_presents_an_unsaved_value_as_stored() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_markup(&state, "LADDER-PROV", "999", "10", Some("100")).await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let stored = localization.tr(crate::localization::MessageKey::ProductLadderStored);
        let unsaved = "Unsaved";

        // The drawer's own first render: the stored row, honestly labelled.
        let (status, html) = preview(app.clone(), product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("data-product-ladder-source=\"stored\"")
                .count(),
            3,
            "the stored state labels all three of its own columns as stored: {html}"
        );
        assert_eq!(
            html.matches(&format!(">{unsaved}<")).count(),
            0,
            "nothing is unsaved in the stored state: {html}"
        );
        assert!(html.contains(&format!(">{stored}<")), "{html}");

        // The form's own values, unsaved: none of the three may claim to be
        // stored, and the net row must not contradict its own footer.
        let (status, html) = preview(
            app,
            product_id,
            "cost_price=25&markup_pct=100&sale_price=15",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("data-product-ladder-source=\"unsaved\"")
                .count(),
            3,
            "every figure taken from the form is unsaved: {html}"
        );
        assert_eq!(
            html.matches("data-product-ladder-source=\"stored\"")
                .count(),
            0,
            "an unsaved value must never be presented as stored: {html}"
        );
        assert_eq!(
            html.matches(&format!(">{unsaved}<")).count(),
            3,
            "and each one says so in words: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("20"),
            "labelling a value unsaved is not a write"
        );
    }

    /// MEDIUM: the new endpoint is a real surface, so it carries the real gates.
    /// A principal without `inventory.read` is refused in the shape every other
    /// refusal on this screen uses, and an unknown product is a 404 — never a
    /// ladder built from a product that does not exist.
    #[tokio::test]
    async fn product_price_ladder_enforces_its_gate_and_its_product() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-GATE", "42").await;
        let body = format!("id={product_id}&cost_price=5&markup_pct=&sale_price=42");

        // No inventory access at all: FORBIDDEN, naming the gate.
        let probe = test_support::seed_session_with_permissions(&state.pool, &["dashboard.read"])
            .await
            .unwrap();
        let (status, refused) = get_html_with_cookie(
            app.clone(),
            &format!("{LADDER_PATH}?{body}"),
            &test_support::cookie_for(&probe),
        )
        .await;
        // This module's convention: a plain browser navigation gets the
        // full-page refusal, not a JSON body.
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert!(
            refused.contains("inventory.read"),
            "the refusal must name the gate: {refused:.400}"
        );
        assert!(
            refused.contains("data-nav=\"products\"") || refused.contains("data-nav=\"dashboard\""),
            "and must keep the navigation shell: {refused:.400}"
        );

        // A product that does not exist: 404, not a zero ladder.
        let (status, missing) = get_html(
            app,
            &format!("{LADDER_PATH}?id=999999&cost_price=5&markup_pct=&sale_price=42"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    }

    // -----------------------------------------------------------------------
    // The ladder's kind is the FORM's kind, not the stored one.
    // -----------------------------------------------------------------------

    /// A Service priced at exactly 0.00 is legal (the Service rule refuses only
    /// BELOW zero), so switching it to Product in the form must move the ladder
    /// onto the Product threshold — which refuses 0.00 — and publish no money.
    /// Binding the stored kind would have kept publishing 0.00 as a price a
    /// save would reject.
    #[tokio::test]
    async fn product_price_ladder_uses_the_kinds_in_the_form_not_the_stored_one() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let service_id =
            product_of_kind(&state, "LADDER-KIND-SVC", ProductKind::Service, "0").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, service_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        // As stored: a Service may cost nothing, so the ladder publishes 0.00.
        let (status, html) = preview(app.clone(), service_id, "kind=Service&sale_price=0").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("data-product-ladder-net-refused"),
            "a zero-priced service is a price the save path accepts: {html}"
        );

        // Switched to Product in the form: the Product rule refuses 0.00, and
        // the ladder must say so instead of publishing a price a save rejects.
        let (status, html) = preview(app, service_id, "kind=Product&sale_price=0").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("sale_price must be &gt; 0 for products"),
            "the ladder must apply the kind IN THE FORM: {html}"
        );
        assert_publishes_no_tax_money(&localization, &html);
        assert_eq!(
            stored_kind(&state, service_id).await,
            ProductKind::Service,
            "a preview must never write the kind either"
        );
    }

    /// The reverse direction: a stored Product priced at 0.00 is not a legal
    /// state to begin with, so this pins the half that matters — a Service the
    /// form keeps as a Service must still publish, whatever the row is.
    #[tokio::test]
    async fn product_price_ladder_publishes_a_zero_priced_service_the_form_keeps() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let service_id = product_of_kind(&state, "LADDER-KIND-OK", ProductKind::Service, "0").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) =
            preview(app, service_id, "kind=Service&sale_price=0&cost_price=5").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("data-product-ladder-net-refused"),
            "the service rule refuses only below zero: {html}"
        );
        assert!(
            html.contains(&money(&localization, "0")),
            "and the ladder publishes the figure rather than refusing: {html}"
        );
    }

    /// An absent or unreadable kind is not a licence to guess: the ladder falls
    /// back to the kind the product is stored as, and says nothing about it.
    #[tokio::test]
    async fn product_price_ladder_falls_back_to_the_stored_kind_when_the_form_sends_none() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let service_id =
            product_of_kind(&state, "LADDER-KIND-FALLBACK", ProductKind::Service, "0").await;

        for prices in [
            "sale_price=0",
            "kind=&sale_price=0",
            "kind=Widget&sale_price=0",
        ] {
            let (status, html) = preview(app.clone(), service_id, prices).await;
            assert_eq!(status, StatusCode::OK, "{prices} -> {html}");
            assert!(
                !html.contains("data-product-ladder-net-refused"),
                "{prices}: with no readable kind the stored one applies, and a \
                 zero-priced service is legal: {html}"
            );
        }
    }

    /// The kind select is a ladder input like the three price fields: switching
    /// a free service to a product has to move the figures before any save.
    #[tokio::test]
    async fn product_price_ladder_refreshes_on_a_kind_change_too() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-KIND-WIRE", "42").await;

        let (status, html) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        let select = html
            .split("name=\"kind\"")
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .unwrap_or_default();
        assert!(
            select.contains(&format!("hx-get=\"{LADDER_PATH}\"")),
            "the kind select must ask the server for the ladder: {select}"
        );
        assert!(
            select.contains("hx-trigger=\"change\""),
            "the kind select must ask on change: {select}"
        );
        assert!(
            select.contains("hx-target=\"#product-price-ladder\""),
            "the kind select must replace the ladder island: {select}"
        );
    }

    /// A Service in either derivation mode, for the kind-bound rules above.
    async fn product_of_kind(state: &AppState, sku: &str, kind: ProductKind, price: &str) -> i64 {
        state
            .inventory_service
            .create_product(
                audit_actor_id(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Product {sku}"),
                    kind,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec(price),
                    cost_price: dec("5"),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn stored_kind(state: &AppState, product_id: i64) -> ProductKind {
        let raw: String = sqlx::query_scalar("SELECT kind FROM products WHERE id = ?")
            .bind(product_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        raw.parse().unwrap()
    }

    /// A GET with an explicit cookie, for the authorization probes: the seeded
    /// test session is not the principal under test.
    async fn get_html_with_cookie(
        app: axum::Router,
        uri: &str,
        cookie: &str,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The refusal the ladder shows instead of a price, proven to be the only
    /// thing it published: no tax total and no tax-inclusive price at all.
    fn assert_publishes_no_tax_money(
        localization: &crate::localization::LocalizationContext,
        html: &str,
    ) {
        assert!(
            html.contains("data-product-ladder-net-refused"),
            "the ladder must state a refusal, not a figure: {html}"
        );
        for key in [
            crate::localization::MessageKey::TaxTotal,
            crate::localization::MessageKey::TaxInclusivePrice,
        ] {
            assert!(
                !html.contains(localization.tr(key)),
                "{key:?} is tax money derived from a price that does not exist, \
                 so it must not be published: {html}"
            );
        }
    }

    /// The drawer's stored net price, read straight from the row so a test can
    /// prove a derived figure never became a stored one. Compared as a
    /// `Decimal`, because the stored TEXT scale is the repository's business and
    /// not what these tests are about.
    async fn stored_net(state: &AppState, product_id: i64) -> Decimal {
        let raw: String = sqlx::query_scalar("SELECT sale_price FROM products WHERE id = ?")
            .bind(product_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        Decimal::from_str(&raw).unwrap()
    }

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    /// An untaxed product: the ladder states the net price once and the
    /// tax-inclusive price once, they are equal, and it says why.
    #[tokio::test]
    async fn product_price_ladder_shows_an_untaxed_product_with_equal_prices() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-0", "42").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, drawer) =
            get_html(app.clone(), &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{drawer}");
        assert!(
            drawer.contains("id=\"product-price-ladder\""),
            "the drawer must carry the ladder: {drawer}"
        );

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches(&format!(">{}<", money(&localization, "42")))
                .count(),
            2,
            "an untaxed ladder states the net and the tax-inclusive price once each: {html}"
        );
        assert_eq!(
            html.matches(localization.tr(crate::localization::MessageKey::TaxNoBreakdown))
                .count(),
            1,
            "an untaxed ladder says why the two figures are equal: {html}"
        );
    }

    /// One tax: the ladder names it, shows its localized rate and the amount
    /// that rate adds, then totals and the tax-inclusive price.
    #[tokio::test]
    async fn product_price_ladder_shows_one_tax_with_its_rate_and_amount() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-1", "100").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains("IVA21") && html.contains("IVA 21%"),
            "the tax identity: {html}"
        );
        assert!(
            html.contains(&localization.format_percentage(Decimal::from_str("21").unwrap())),
            "the localized rate: {html}"
        );
        for value in ["100", "21", "121"] {
            assert!(
                html.contains(&money(&localization, value)),
                "the ladder must state {value}: {html}"
            );
        }
        for key in [
            crate::localization::MessageKey::TaxTotal,
            crate::localization::MessageKey::TaxInclusivePrice,
        ] {
            assert!(
                html.contains(localization.tr(key)),
                "{key:?} must label the ladder: {html}"
            );
        }
    }

    /// Several taxes are additive, never compounded, and the per-row amounts
    /// reconcile with the total the ladder states.
    #[tokio::test]
    async fn product_price_ladder_sums_several_linked_taxes_additively() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-2", "100").await;
        let actor = audit_actor_id(&state).await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        let iibb = link_tax(&state, "IIBB10", "IIBB 10%", "10").await;
        state
            .tax_service
            .link_product_tax(actor, product_id, iva)
            .await
            .unwrap();
        state
            .tax_service
            .link_product_tax(actor, product_id, iibb)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for value in ["10", "21", "31", "131"] {
            assert!(
                html.contains(&money(&localization, value)),
                "additive, not compounded: the ladder must state {value}: {html}"
            );
        }
    }

    /// The per-tax amount is pinned to cents half-up, exactly like a document
    /// line of the same product: 10.005 at 21% is 2.10, never 2.11 or 2.09.
    #[tokio::test]
    async fn product_price_ladder_rounds_each_tax_amount_half_up_to_cents() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-3", "10.005").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "2.10")),
            "21% of 10.005 is 2.10 half-up: {html}"
        );
        assert!(
            html.contains(&money(&localization, "12.11")),
            "and the tax-inclusive price is the same one a line would carry: {html}"
        );
    }

    /// A manual-price product keeps its own net price and still gets the tax
    /// rows, the total and the tax-inclusive price.
    #[tokio::test]
    async fn product_price_ladder_keeps_a_manual_price_product_manual() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-4", "80").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        // The form changes the cost only: with no markup the net price stays the
        // manual one, so the ladder must not derive from the new cost.
        let (status, html) =
            preview(app, product_id, "cost_price=5&markup_pct=&sale_price=80").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "80")),
            "the manual net price is the truth: {html}"
        );
        assert!(
            html.contains(&money(&localization, "16.80")),
            "21% of 80 is 16.80: {html}"
        );
        assert!(
            html.contains(&money(&localization, "96.80")),
            "and the tax-inclusive price follows it: {html}"
        );
    }

    /// A markup product: the ladder's net price IS the cost times the markup,
    /// stated with the very figure the save path would store.
    #[tokio::test]
    async fn product_price_ladder_derives_a_markup_product_net_from_its_cost() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_markup(&state, "LADDER-5", "999", "10", Some("100")).await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("20"),
            "10 * (1 + 100/100) is what the save path stored"
        );

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "20.00")),
            "the ladder derives the net price from the cost and the markup: {html}"
        );
        assert!(
            html.contains(&money(&localization, "4.20"))
                && html.contains(&money(&localization, "24.20")),
            "the taxes apply to the derived net price: {html}"
        );
    }

    /// Changing the cost in the form moves the ladder, before anything is
    /// saved, and the stored row is untouched.
    #[tokio::test]
    async fn product_price_ladder_follows_a_cost_change_in_the_form() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_markup(&state, "LADDER-6", "999", "10", Some("100")).await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        // A DERIVED price always carries cents: the markup factor is a
        // two-decimal scale shift, so 25 * (1 + 100/100) is 50.00 and not 50.
        let (status, html) = preview(
            app,
            product_id,
            "cost_price=25&markup_pct=100&sale_price=15",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "50.00")),
            "25 * (1 + 100/100) = 50.00: {html}"
        );
        assert!(
            !html.contains(&money(&localization, "20.00")),
            "the previous derived price must not linger: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("20"),
            "a preview must never write the product"
        );
    }

    /// Changing the markup in the form moves the ladder too, on the same
    /// derivation the save enforces.
    #[tokio::test]
    async fn product_price_ladder_follows_a_markup_change_in_the_form() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_markup(&state, "LADDER-7", "999", "10", Some("100")).await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) =
            preview(app, product_id, "cost_price=10&markup_pct=50&sale_price=15").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "15.00")),
            "10 * (1 + 50/100) = 15.00: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("20"),
            "the preview persists nothing"
        );
    }

    /// The preview derives through the SAME rule the save path enforces: the
    /// numbers the form shows before a save are the numbers the save stores.
    #[tokio::test]
    async fn product_price_ladder_preview_matches_what_saving_the_same_values_stores() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-8", "42").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        // The drawer always re-submits its readonly price field, so the save
        // below sends the stale 42.00 exactly as the browser would.
        let edit = format!(
            "id={product_id}&sku=LADDER-8&name=Product+LADDER-8&kind=Product&unit=un\
             &sale_price=42&cost_price=10&category_id=&track_stock=&min_stock=&max_stock=\
             &location=&notes=&markup_pct=100"
        );
        let (status, saved) = post_form_with_cookie(
            app.clone(),
            "/web/products/edit",
            &edit,
            &[
                ("HX-Request", "true"),
                ("HX-Target", "#product-drawer-body"),
            ],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");

        let (status, html) = preview(
            app,
            product_id,
            "cost_price=10&markup_pct=100&sale_price=42",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("20"),
            "the save re-derives the stored net price"
        );
        assert!(
            html.contains(&money(&localization, "20.00")),
            "the preview of the same form values must be the stored figure: {html}"
        );
    }

    /// Linking a tax moves the tax-inclusive price; unlinking it moves the price
    /// back. Neither rewrites the stored net price.
    #[tokio::test]
    async fn product_price_ladder_follows_linking_and_unlinking_a_tax() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-9", "100").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = preview(app.clone(), product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains(&money(&localization, "121")),
            "no tax is linked yet: {html}"
        );

        let (status, linked) = post_form_with_cookie(
            app.clone(),
            "/web/product-taxes",
            &format!("product_id={product_id}&tax_id={iva}"),
            &[("HX-Request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{linked}");

        let (status, html) = preview(app.clone(), product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            html.contains(&money(&localization, "121")),
            "linking the tax moves the tax-inclusive price: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("100"),
            "a tax change never rewrites the stored net price"
        );

        let (status, unlinked) = post_form_with_cookie(
            app.clone(),
            "/web/product-taxes/unlink",
            &format!("product_id={product_id}&tax_id={iva}"),
            &[("HX-Request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{unlinked}");

        let (status, html) = preview(app, product_id, "").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains(&money(&localization, "121")),
            "unlinking the tax moves it back: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("100"),
            "the stored net price is still untouched"
        );
    }

    /// A markup with no cost cannot produce a price — the save path rejects it
    /// — so the ladder states that refusal instead of a zero or a stale
    /// figure, and publishes no tax money derived from nothing.
    #[tokio::test]
    async fn product_price_ladder_shows_the_save_paths_refusal_for_a_markup_without_a_cost() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-10", "42").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        state
            .tax_service
            .link_product_tax(audit_actor_id(&state).await, product_id, iva)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) =
            preview(app, product_id, "cost_price=0&markup_pct=50&sale_price=").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        // The save path's own refusal, verbatim. `&gt;` is the wire form of
        // `>`: the ladder escapes it like any other rendered text and the
        // browser decodes it back, so the operator reads the server's message.
        assert!(
            html.contains("cost_price must be &gt; 0 when markup_pct is set"),
            "the ladder states the save path's own refusal: {html}"
        );
        assert!(
            html.contains("data-product-ladder-net-refused"),
            "and says which figures it therefore cannot state: {html}"
        );
        assert!(
            !html.contains(&money(&localization, "46.20"))
                && !html.contains(&money(&localization, "4.20")),
            "no tax money may be derived from a price that does not exist: {html}"
        );
        assert_eq!(
            stored_net(&state, product_id).await,
            dec("42"),
            "and the refusal writes nothing"
        );
    }

    /// A field that is not a number is not a zero: the ladder falls back to the
    /// last state the save path accepted, and says that is what it is showing.
    #[tokio::test]
    async fn product_price_ladder_shows_the_last_saved_state_for_an_unreadable_field() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-11", "42").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        for query in [
            "cost_price=abc&markup_pct=&sale_price=",
            "cost_price=&markup_pct=twenty&sale_price=",
            "cost_price=&markup_pct=&sale_price=4o0",
        ] {
            let (status, html) = preview(app.clone(), product_id, query).await;
            assert_eq!(status, StatusCode::OK, "{query} -> {html}");
            assert!(
                html.contains("data-product-ladder-unreadable"),
                "{query}: the ladder must say it fell back to the saved values: {html}"
            );
            assert!(
                html.contains("not a number"),
                "{query}: in the operator's own words: {html}"
            );
            assert!(
                html.contains(&money(&localization, "42")),
                "{query}: the last coherent state is the stored net price: {html}"
            );
            assert!(
                html.contains(&money(&localization, "5")),
                "{query}: and the stored cost, not a misread one: {html}"
            );
        }
    }

    /// An empty cost is NOT an unreadable field: the save path stores it as the
    /// "no cost recorded" zero, so the ladder states that, and a manual net
    /// price still stands.
    #[tokio::test]
    async fn product_price_ladder_treats_an_empty_cost_as_no_cost_recorded() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-12", "42").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) =
            preview(app, product_id, "cost_price=&markup_pct=&sale_price=42").await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(
            !html.contains("data-product-ladder-unreadable"),
            "an empty cost is a value the save path accepts: {html}"
        );
        assert!(
            html.contains(&money(&localization, "42")),
            "the manual net price stands: {html}"
        );
    }

    /// The ladder is readable in every enabled locale, through the existing
    /// currency and percentage helpers.
    #[tokio::test]
    async fn product_price_ladder_renders_in_both_enabled_locales() {
        for (locale_code, decimal_sep, ladder, net, gross) in [
            ("en-US", '.', "Price ladder", "Net price", "Price with tax"),
            (
                "es-AR",
                ',',
                "Escalera de precios",
                "Precio neto",
                "Precio con impuestos",
            ),
        ] {
            let state = test_state().await;
            sqlx::query(
                "INSERT INTO business_settings \
                 (id, business_name, default_locale_code, currency_code, timezone) \
                 VALUES (1, 'Acme', ?, 'ARS', 'UTC')",
            )
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
            for (code, language) in [("en-US", "en"), ("es-AR", "es")] {
                sqlx::query(
                    "INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) \
                     VALUES (?, ?, ?, 1)",
                )
                .bind(code)
                .bind(language)
                .bind(code)
                .execute(&state.pool)
                .await
                .unwrap();
            }
            let app = crate::routes::router(state.clone());
            let product_id = product_with_taxes(&state, "LADDER-13", "100").await;
            // A rate with a fraction, so the locale's decimal separator is
            // actually load-bearing: a whole number would render identically in
            // both locales and the test would pass vacuously.
            let iva = link_tax(&state, "IVA21", "IVA 21.5%", "21.5").await;
            state
                .tax_service
                .link_product_tax(audit_actor_id(&state).await, product_id, iva)
                .await
                .unwrap();
            let localization = crate::localization::load_context(&state.pool)
                .await
                .unwrap();
            assert_eq!(localization.locale_code, locale_code);

            let (status, html) = preview(app, product_id, "").await;
            assert_eq!(status, StatusCode::OK, "{locale_code}: {html}");
            assert!(
                html.contains(&format!("21{decimal_sep}5 ARS")),
                "{locale_code}: the tax amount must follow the locale's own conventions: {html}"
            );
            assert!(
                html.contains(&format!("121{decimal_sep}5 ARS")),
                "{locale_code}: and so must the tax-inclusive price: {html}"
            );
            assert!(
                html.contains(&localization.format_currency(Decimal::from_str("100").unwrap())),
                "{locale_code}: the net price must be localized too: {html}"
            );
            assert!(
                html.contains(&format!("21{decimal_sep}5 %")),
                "{locale_code}: the rate must be localized: {html}"
            );
            for label in [ladder, net, gross] {
                assert!(
                    html.contains(label),
                    "{locale_code}: the ladder label {label:?} must be translated: {html}"
                );
            }
        }
    }

    /// The preview is a read: it answers a GET, refuses any other verb, and
    /// leaves every product, association and audit column byte-for-byte as it
    /// found them.
    #[tokio::test]
    async fn product_price_ladder_endpoint_persists_nothing() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-14", "42").await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        let actor = audit_actor_id(&state).await;
        state
            .tax_service
            .link_product_tax(actor, product_id, iva)
            .await
            .unwrap();

        /// EVERY column of `products` and `product_taxes`, not a chosen subset:
        /// "the preview writes nothing" is a claim about the whole row, and a
        /// snapshot that left a column out could not falsify it.
        ///
        /// The statements are literal, and the completeness of the column list
        /// is MACHINE-CHECKED against `pragma_table_info`: a migration that adds
        /// a column fails this test until the snapshot names it, so the claim
        /// cannot rot into "every column I remembered".
        async fn snapshot(pool: &sqlx::SqlitePool) -> Vec<String> {
            /// One table's rows, with its column list checked against the
            /// table's own `pragma_table_info` before the rows are read.
            async fn rows_of(
                pool: &sqlx::SqlitePool,
                // sqlx 0.9 only accepts a query string that is 'static or
                // explicitly audited, so these are literals by construction.
                pragma: &'static str,
                table: &str,
                projection: &'static str,
                expected: &[&str],
            ) -> Vec<String> {
                let actual: Vec<String> = sqlx::query_scalar::<_, String>(pragma)
                    .fetch_all(pool)
                    .await
                    .unwrap();
                let expected: Vec<String> = expected.iter().map(|c| (*c).to_string()).collect();
                assert_eq!(
                    actual, expected,
                    "the {table} snapshot must name EVERY column the table has; \
                     a migration that adds one must add it here too"
                );
                sqlx::query_scalar::<_, String>(projection)
                    .fetch_all(pool)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|row| format!("{table}|{row}"))
                    .collect()
            }

            let mut out = rows_of(
                pool,
                "SELECT name FROM pragma_table_info('products') ORDER BY cid",
                "products",
                "SELECT id || '|' || sku || '|' || name || '|' || kind || '|' || unit || '|' \
                 || sale_price || '|' || cost_price || '|' || COALESCE(markup_pct, '-') || '|' \
                 || is_active || '|' || COALESCE(category_id, '-') || '|' || track_stock || '|' \
                 || COALESCE(min_stock, '-') || '|' || COALESCE(max_stock, '-') || '|' \
                 || COALESCE(location, '-') || '|' || COALESCE(notes, '-') || '|' \
                 || created_by || '|' || COALESCE(updated_by, '-') || '|' || created_at || '|' \
                 || COALESCE(updated_at, '-') FROM products ORDER BY id",
                // In the table's own column order, which the equality above
                // pins: the projection below may concatenate in any order it
                // likes, but this list may not drift from the schema.
                &[
                    "id",
                    "sku",
                    "name",
                    "kind",
                    "category_id",
                    "unit",
                    "sale_price",
                    "cost_price",
                    "track_stock",
                    "min_stock",
                    "max_stock",
                    "location",
                    "notes",
                    "is_active",
                    "created_by",
                    "updated_by",
                    "created_at",
                    "updated_at",
                    "markup_pct",
                ],
            )
            .await;
            out.extend(
                rows_of(
                    pool,
                    "SELECT name FROM pragma_table_info('product_taxes') ORDER BY cid",
                    "product_taxes",
                    "SELECT id || '|' || product_id || '|' || tax_id || '|' || created_by || '|' \
                     || created_at FROM product_taxes ORDER BY id",
                    &["id", "product_id", "tax_id", "created_by", "created_at"],
                )
                .await,
            );
            assert!(
                out.iter().any(|row| row.starts_with("products|")),
                "the snapshot must actually contain the product it is protecting"
            );
            out
        }

        let before = snapshot(&state.pool).await;
        let (status, html) = preview(
            app.clone(),
            product_id,
            "cost_price=999&markup_pct=500&sale_price=1",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            snapshot(&state.pool).await,
            before,
            "the ladder preview must persist nothing at all"
        );

        // Only a read verb is served: a POST cannot be the same endpoint.
        let (status, refused) = post_form_with_cookie(
            app,
            LADDER_PATH,
            "product_id=1&cost_price=1",
            &[("HX-Request", "true")],
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "the preview must be a GET-only read: {refused}"
        );
        assert_eq!(
            snapshot(&state.pool).await,
            before,
            "the refused verb must persist nothing either"
        );
    }

    /// Consolidation: the ladder is the ONE place the drawer shows money. The
    /// scattered header lines are gone, and the association card keeps the
    /// controls and the tax identity without repeating an amount.
    #[tokio::test]
    async fn product_price_ladder_is_the_only_money_the_drawer_shows() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-15", "100").await;
        // Two taxes, so the ladder's total (31) is a figure no single row
        // repeats: with one tax the total and the row would be the same number
        // twice, which is the ladder's own arithmetic and not a duplication.
        let actor = audit_actor_id(&state).await;
        let iva = link_tax(&state, "IVA21", "IVA 21%", "21").await;
        let iibb = link_tax(&state, "IIBB10", "IIBB 10%", "10").await;
        state
            .tax_service
            .link_product_tax(actor, product_id, iva)
            .await
            .unwrap();
        state
            .tax_service
            .link_product_tax(actor, product_id, iibb)
            .await
            .unwrap();
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, html) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(
            html.matches("data-product-price-ladder").count(),
            1,
            "the ladder appears exactly once: {html}"
        );
        assert!(
            !html.contains("data-product-tax-preview"),
            "the scattered tax-inclusive preview line is gone: {html}"
        );
        // Each money figure is counted as the whole cell it renders in, so a
        // value that happens to be a substring of another ("10" inside "100")
        // cannot pass or fail by accident.
        for value in ["100", "21", "10", "31", "131"] {
            let cell = format!(">{}<", money(&localization, value));
            assert_eq!(
                html.matches(&cell).count(),
                1,
                "{value} is displayed in exactly one place, the ladder: {html}"
            );
        }
        // The association card keeps the identity and the controls, and no
        // amount: the ladder owns every figure.
        let associations = html
            .split("data-product-tax-associations")
            .nth(1)
            .expect("the association list must be marked");
        let associations = associations
            .split("</section>")
            .next()
            .unwrap_or(associations);
        for value in ["100", "21", "10", "31", "131"] {
            assert!(
                !associations.contains(&money(&localization, value)),
                "the association card must not repeat {value}: {associations}"
            );
        }
        assert!(
            associations.contains("IVA21")
                && associations.contains("/web/product-taxes/unlink")
                && associations.contains("/web/product-taxes"),
            "but it must keep the identity and both association controls: {associations}"
        );
    }

    /// The ladder refreshes because the price fields ask the server, and the
    /// browser is never asked to compute money: no markup formula and no
    /// rounding anywhere in the project's own JavaScript.
    #[tokio::test]
    async fn product_price_ladder_refreshes_from_the_server_without_computing_money_in_javascript()
    {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let product_id = product_with_taxes(&state, "LADDER-16", "42").await;

        let (status, html) = get_html(app, &format!("/web/products/detail/{product_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for field in ["sale_price", "cost_price", "markup_pct"] {
            let input = html
                .split(&format!("name=\"{field}\""))
                .nth(1)
                .and_then(|rest| rest.split('>').next())
                .unwrap_or_default();
            assert!(
                input.contains(&format!("hx-get=\"{LADDER_PATH}\"")),
                "the {field} field must ask the server for the ladder: {input}"
            );
            assert!(
                input.contains("hx-trigger=\"change\""),
                "the {field} field must ask on change: {input}"
            );
            assert!(
                input.contains("hx-target=\"#product-price-ladder\""),
                "the {field} field must replace the ladder island: {input}"
            );
        }

        // The architectural rule, as a structural check over every piece of
        // JavaScript this project ships. The set is ENUMERATED at run time —
        // every `static/*.js` and every `<script>` in every template — so a
        // future island is covered by construction instead of by remembering to
        // add it here. The vendored htmx bundle is excluded BY NAME, and only
        // because it is a third-party library this project does not author.
        const VENDORED: [&str; 1] = ["htmx.min.js"];
        // `cargo test` runs with the package root as the working directory, so
        // these are the real directories, not a guess about the layout.
        let mut sources: Vec<(String, String)> = Vec::new();

        let mut static_files: Vec<_> = std::fs::read_dir("static")
            .expect("the static directory must exist")
            .map(|entry| entry.expect("a readable directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "js"))
            .collect();
        static_files.sort();
        assert!(
            static_files
                .iter()
                .any(|path| { path.file_name().is_some_and(|name| name == "picker.js") }),
            "the static JavaScript set must really contain the picker island: \
             {static_files:?}"
        );
        for path in static_files {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if VENDORED.contains(&name.as_str()) {
                continue;
            }
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{path:?} must be readable: {error}"));
            assert!(
                !body.is_empty(),
                "{path:?} is empty: an empty scan target proves nothing"
            );
            sources.push((format!("static/{name}"), body));
        }

        fn collect_templates(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap_or_else(|error| panic!("{dir:?} must be readable: {error}"))
                .map(|entry| entry.expect("a readable directory entry").path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    collect_templates(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "html") {
                    out.push(path);
                }
            }
        }
        let mut templates = Vec::new();
        collect_templates(std::path::Path::new("templates"), &mut templates);
        assert!(
            !templates.is_empty(),
            "the template tree must not be empty, or this scan is vacuous"
        );
        for path in templates {
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{path:?} must be readable: {error}"));
            let mut rest = body.as_str();
            while let Some(start) = rest.find("<script") {
                let after = &rest[start..];
                let Some(end) = after.find("</script>") else {
                    break;
                };
                let index = body[..body.len() - rest.len() + start]
                    .matches("<script")
                    .count();
                sources.push((
                    format!("{} inline script #{index}", path.display()),
                    after[..end].to_string(),
                ));
                rest = &after[end..];
            }
        }
        // The reach is ASSERTED, not assumed: a scan that silently found one
        // file would pass the FORBIDDEN loop below for the wrong reason.
        assert!(
            sources.len() >= 3,
            "the scan must reach the static islands and the inline scripts: {sources:#?}"
        );
        println!(
            "JS-SCAN-REACH: {} sources: {:?}",
            sources.len(),
            sources
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
        );

        // Money primitives. A single match is a defect: the ladder is computed
        // on the server or not at all.
        const FORBIDDEN: [&str; 8] = [
            "Math.round",
            "toFixed",
            "parseFloat",
            "/ 100",
            "* (1 +",
            "currency",
            "sale_price *",
            "markup_pct *",
        ];
        let violations: Vec<String> = sources
            .iter()
            .flat_map(|(name, body)| {
                FORBIDDEN
                    .iter()
                    .filter(|needle| body.contains(**needle))
                    .map(move |needle| format!("{name}: {needle}"))
            })
            .collect();
        assert!(
            violations.is_empty(),
            "money must not be computed in the browser: {violations:#?}"
        );
        // The exclusion must be a decision about a file that really exists,
        // not a filter that matches nothing. Asserting "no scanned source is
        // named htmx.min.js" would be true even if the vendored file were
        // deleted, which is the tautology this assertion exists to avoid.
        for name in VENDORED {
            let vendored = std::path::Path::new("static").join(name);
            assert!(
                vendored.exists(),
                "{name} is declared vendored, so it must exist to be excluded"
            );
            assert!(
                !sources.iter().any(|(scanned, _)| scanned.ends_with(name)),
                "{name} is vendored and must stay out of the authored scan"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Localized price refusals: the ladder preview and the product save tell the
    // operator the same thing, in the active locale.
    // -----------------------------------------------------------------------

    /// A refusal the operator READS needs a real locale, and `test_state` seeds
    /// no business configuration, so every answer would come back in the
    /// pre-setup English fallback. This seeds the two rows `load_context`
    /// resolves from — the default locale and both enabled profiles — the same
    /// way the ladder's own bilingual test does, and asserts the resolution
    /// rather than assuming it.
    async fn localized_state(locale_code: &str, language_code: &str) -> AppState {
        let state = test_state().await;
        sqlx::query(
            "INSERT INTO business_settings \
             (id, business_name, default_locale_code, currency_code, timezone) \
             VALUES (1, 'Acme', ?, 'ARS', 'UTC')",
        )
        .bind(locale_code)
        .execute(&state.pool)
        .await
        .unwrap();
        for (code, language) in [("en-US", "en"), ("es-AR", "es")] {
            sqlx::query(
                "INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) \
                 VALUES (?, ?, ?, 1)",
            )
            .bind(code)
            .bind(language)
            .bind(code)
            .execute(&state.pool)
            .await
            .unwrap();
        }
        let resolved = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        assert_eq!(resolved.locale_code, locale_code);
        assert_eq!(resolved.language_code, language_code);
        state
    }

    /// A zero-priced PRODUCT, submitted as the create form sends it: no markup,
    /// so the manual price is the effective one and the product price rule is
    /// what refuses. The drawer's ladder is asked about the SAME three fields.
    const ZERO_PRICED_PRODUCT_SAVE: &str =
        "sku=LOC-PRICE&name=Localized+price&kind=Product&unit=un&sale_price=0&cost_price=5";

    /// The one sentence both surfaces answer for a zero-priced product, in each
    /// enabled locale. Both strings are written out on purpose: a test that
    /// asked the code under test for the expected wording would agree with any
    /// wording, and the wording is the whole point of this work unit.
    ///
    /// The English entry has no final period because it is the exact string the
    /// refusal has always answered with — the ladder's existing tests and the
    /// browser suite both assert it verbatim. The Spanish one is a real
    /// translation and reads as a sentence.
    const ZERO_PRICE_REFUSAL: [(&str, &str, &str); 2] = [
        ("en-US", "en", "sale_price must be > 0 for products"),
        (
            "es-AR",
            "es",
            "El precio de venta debe ser mayor que 0 en los productos.",
        ),
    ];

    /// The emptied MANUAL price, the one price refusal the form's shape produces
    /// before the service is ever asked. It is a price refusal too, so it is
    /// translated by the same rule and not left in English on a Spanish ladder.
    const REQUIRED_PRICE_REFUSAL: [(&str, &str, &str); 2] = [
        ("en-US", "en", "sale_price is required"),
        ("es-AR", "es", "El precio de venta es obligatorio."),
    ];

    /// The fragment renders its text through Askama, which escapes `>` like any
    /// other rendered text; the JSON refusal body is not escaped. Asserting both
    /// surfaces against one sentence therefore needs its wire form for the HTML.
    fn wire(sentence: &str) -> String {
        sentence.replace('>', "&gt;")
    }

    /// THE RED, stated as behavior: a Spanish operator saving a product with a
    /// price the save refuses reads that refusal in Spanish. The English locale
    /// is in the same table because "it changed" is only a defect if the
    /// language that already worked kept working, byte for byte.
    #[tokio::test]
    async fn a_product_save_refused_by_a_price_rule_answers_in_the_active_locale() {
        for (locale_code, language_code, sentence) in ZERO_PRICE_REFUSAL {
            let state = localized_state(locale_code, language_code).await;
            let app = crate::routes::router(state);
            let (status, _, body) =
                post_form_full(app, "/web/products", ZERO_PRICED_PRODUCT_SAVE, &[]).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{locale_code}: a refused price is still a 400: {body}"
            );
            assert!(
                body.contains(sentence),
                "{locale_code}: the operator must read the refusal in their own language: {body}"
            );
            if language_code == "es" {
                assert!(
                    !body.contains("sale_price must be"),
                    "a Spanish operator must never read the English refusal: {body}"
                );
            }
        }
    }

    /// The htmx answer and the plain-browser answer must be the SAME answer.
    ///
    /// Every save test above goes through `post_form_full`, which sets
    /// `HX-Request: true` unconditionally, so the other half of both save routes
    /// — the branch that redirects a plain browser to `/products` — has never
    /// been asserted through the wire. The refusal is mapped BEFORE that branch,
    /// so the localized sentence cannot depend on it, and this test is what makes
    /// that structural claim an observed one instead.
    ///
    /// The refusal is a 400 with a body either way: `AppError`'s single
    /// `IntoResponse` is the only way a handler's error becomes a response, and
    /// it does not know whether the request was htmx.
    #[tokio::test]
    async fn a_plain_browser_save_refused_by_a_price_rule_answers_in_the_active_locale() {
        for (locale_code, language_code, sentence) in ZERO_PRICE_REFUSAL {
            let state = localized_state(locale_code, language_code).await;
            let app = crate::routes::router(state.clone());

            // THE CONTROL, and it is what makes the refusal below mean something:
            // an ACCEPTED plain-browser post answers 303 to `/products`, which is
            // the branch `is_htmx` sends a browser to — the htmx branch answers
            // 200 with the list fragment instead. If this ever stopped being a
            // redirect, the helper would no longer be exercising the non-htmx
            // path and the refusal assertion would be proving nothing about it.
            let (accepted, _) = post_form_with_cookie(
                app.clone(),
                "/web/products",
                "sku=LOC-PLAIN-OK&name=Plain+browser&kind=Product&unit=un&sale_price=10&cost_price=5",
                &[],
                test_support::TEST_COOKIE,
            )
            .await;
            assert_eq!(
                accepted,
                StatusCode::SEE_OTHER,
                "{locale_code}: this helper must be the plain-browser path, or the refusal \
                 below says nothing about it"
            );

            // `post_form_with_cookie` sends NO `HX-Request` header: this is the
            // full-page form post, the branch `post_form_full` never reaches.
            let (status, body) = post_form_with_cookie(
                app,
                "/web/products",
                ZERO_PRICED_PRODUCT_SAVE,
                &[],
                test_support::TEST_COOKIE,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{locale_code}: a plain browser post is refused exactly as an htmx one is: {body}"
            );
            assert!(
                body.contains(sentence),
                "{locale_code}: the full-page post must answer the same sentence the htmx post \
                 does, or the language of a refusal would depend on how the form was submitted: \
                 {body}"
            );
            if language_code == "es" {
                assert!(
                    !body.contains("sale_price must be"),
                    "a Spanish operator must never read the English refusal: {body}"
                );
            }
        }
    }

    /// The ladder's whole value is that it never lies about the save, and a
    /// preview that answered in a different language would be that lie in a new
    /// form. ONE sentence, in the identical locale, from both surfaces.
    #[tokio::test]
    async fn the_ladder_and_the_save_answer_the_identical_sentence_in_the_identical_locale() {
        for (locale_code, language_code, sentence) in ZERO_PRICE_REFUSAL {
            let state = localized_state(locale_code, language_code).await;
            let app = crate::routes::router(state.clone());
            let product_id = product_with_taxes(&state, "LOC-LADDER", "42").await;

            let (status, _, save) =
                post_form_full(app.clone(), "/web/products", ZERO_PRICED_PRODUCT_SAVE, &[]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{locale_code}: {save}");

            let (status, ladder) = preview(
                app,
                product_id,
                "kind=Product&cost_price=5&markup_pct=&sale_price=0",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{locale_code}: {ladder}");
            assert!(
                ladder.contains("data-product-ladder-net-refused"),
                "{locale_code}: the ladder must still state a refusal, not a figure: {ladder}"
            );
            assert!(
                ladder.contains(&wire(sentence)),
                "{locale_code}: the ladder must say what the save says, in the same language: \
                 the save answered {save}, the ladder answered {ladder}"
            );
            assert!(
                save.contains(sentence),
                "{locale_code}: the save must say what the ladder says: {save}"
            );
        }
    }

    /// The emptied manual price is the same rule on both surfaces, in the same
    /// language: a Spanish ladder that answered it in English would be the
    /// disagreement this work unit exists to remove, one row further down.
    #[tokio::test]
    async fn the_emptied_manual_price_refusal_is_the_same_sentence_on_both_surfaces() {
        for (locale_code, language_code, sentence) in REQUIRED_PRICE_REFUSAL {
            let state = localized_state(locale_code, language_code).await;
            let app = crate::routes::router(state.clone());
            let product_id = product_with_taxes(&state, "LOC-REQUIRED", "42").await;

            let (status, _, save) = post_form_full(
                app.clone(),
                "/web/products",
                "sku=LOC-REQUIRED-SAVE&name=Localized+price&kind=Product&unit=un&sale_price=&cost_price=5",
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{locale_code}: {save}");
            assert!(
                save.contains(sentence),
                "{locale_code}: the save answers its own form-shape refusal: {save}"
            );

            let (status, ladder) = preview(
                app,
                product_id,
                "kind=Product&cost_price=5&markup_pct=&sale_price=",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{locale_code}: {ladder}");
            assert!(
                ladder.contains(&wire(sentence)),
                "{locale_code}: the ladder must answer the same refusal, in the same language: {ladder}"
            );
        }
    }

    /// The JSON API is not localized, and this task must not change that: a
    /// consumer reading the error body gets TODAY's exact bytes, English, even
    /// when the business itself is configured in Spanish. Asserted as a whole
    /// body, because "the message is right" is weaker than "the response is
    /// unchanged".
    #[tokio::test]
    async fn the_json_api_still_answers_the_exact_english_price_refusal_body() {
        let state = localized_state("es-AR", "es").await;
        let app = crate::routes::router(state);
        let (status, body) = post_json(
            app,
            "/api/products",
            r#"{"sku":"LOC-API","name":"API refusal","kind":"Product","unit":"un","sale_price":"0","cost_price":"5","track_stock":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body, r#"{"error":"sale_price must be > 0 for products"}"#,
            "the API body is the exact English sentence this task must not change"
        );
    }

    /// EVERY price refusal, with the form the SAVE submits, the form the ladder
    /// is asked about, and the sentence each must answer in each locale. One row
    /// per `PriceRefusal`, so "the two surfaces agree" is a claim about the whole
    /// rule set and not about the one rule that happened to be picked first.
    ///
    /// The two columns are the SAME request in the two shapes it travels in: the
    /// create form's body, and the drawer's `hx-include="closest form"` query. A
    /// row where those two shapes could not express one input would be a row
    /// about a different rule, so both are written from the form's own fields.
    const REFUSAL_PARITY: [(&str, &str, &str, &str, &str); 7] = [
        (
            "a zero-priced product",
            "sku=LOC-P1&name=Parity+1&kind=Product&unit=un&sale_price=0&cost_price=5",
            "kind=Product&cost_price=5&markup_pct=&sale_price=0",
            "sale_price must be > 0 for products",
            "El precio de venta debe ser mayor que 0 en los productos.",
        ),
        (
            "an emptied manual price",
            "sku=LOC-P2&name=Parity+2&kind=Product&unit=un&sale_price=&cost_price=5",
            "kind=Product&cost_price=5&markup_pct=&sale_price=",
            "sale_price is required",
            "El precio de venta es obligatorio.",
        ),
        (
            "a markup with no cost",
            "sku=LOC-P3&name=Parity+3&kind=Product&unit=un&sale_price=&cost_price=0&markup_pct=50",
            "kind=Product&cost_price=0&markup_pct=50&sale_price=0",
            "cost_price must be > 0 when markup_pct is set",
            "El costo debe ser mayor que 0 cuando se indica un margen.",
        ),
        (
            "a markup at the -100 boundary",
            "sku=LOC-P4&name=Parity+4&kind=Product&unit=un&sale_price=&cost_price=5&markup_pct=-100",
            "kind=Product&cost_price=5&markup_pct=-100&sale_price=0",
            "markup_pct must be > -100",
            "El margen debe ser mayor que -100.",
        ),
        (
            "a markup that overflows",
            "sku=LOC-P5&name=Parity+5&kind=Product&unit=un&sale_price=&cost_price=1000&markup_pct=79228162514264337593543950335",
            "kind=Product&cost_price=1000&markup_pct=79228162514264337593543950335&sale_price=0",
            "markup_pct or cost_price is too large to derive a sale_price",
            "El margen o el costo son demasiado grandes para derivar el precio de venta.",
        ),
        (
            "a negative cost",
            "sku=LOC-P6&name=Parity+6&kind=Service&unit=un&sale_price=10&cost_price=-5",
            "kind=Service&cost_price=-5&markup_pct=&sale_price=10",
            "cost_price cannot be negative",
            "El costo no puede ser negativo.",
        ),
        (
            "a negative service price",
            "sku=LOC-P7&name=Parity+7&kind=Service&unit=un&sale_price=-1&cost_price=5",
            "kind=Service&cost_price=5&markup_pct=&sale_price=-1",
            "sale_price cannot be negative",
            "El precio de venta no puede ser negativo.",
        ),
    ];

    /// The sentence a refusal body carries, read out of the JSON rather than
    /// matched inside it: `{"error": "…"}` is the whole body, so the value IS
    /// the message the notice box will paint.
    fn save_refusal_sentence(body: &str) -> String {
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|error| panic!("a refused save answers JSON, got {body} ({error})"));
        json["error"]
            .as_str()
            .unwrap_or_else(|| panic!("the refusal body has no error message: {body}"))
            .to_string()
    }

    /// The sentence the ladder's refusal cell states, read out of the fragment:
    /// the cell is the one carrying the refusal marker, and the sentence is the
    /// only thing inside its own emphasis, so the surrounding lead and figures
    /// cannot be mistaken for it. The wire form is decoded, because the fragment
    /// escapes its text the way it escapes any other.
    fn ladder_refusal_sentence(html: &str) -> Option<String> {
        let cell = html
            .split("data-product-ladder-net-refused")
            .nth(1)?
            .split("</td>")
            .next()?;
        let sentence = cell.split(r#"<span class="font-semibold">"#).nth(1)?;
        Some(
            sentence
                .split("</span>")
                .next()?
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
                .trim()
                .to_string(),
        )
    }

    /// The claim, over the WHOLE rule set: for every price refusal, the ladder
    /// and the save answer the identical sentence in the identical locale, and
    /// that sentence is the locale's own — never the English one handed to a
    /// Spanish operator.
    ///
    /// Both surfaces are driven through HTTP, in the request shapes the browser
    /// sends, because a shared helper that only one of them calls would still
    /// pass a test that exercised the helper directly.
    #[tokio::test]
    async fn the_ladder_and_the_save_never_differ_for_any_price_refusal() {
        for (locale_code, language_code) in [("en-US", "en"), ("es-AR", "es")] {
            let state = localized_state(locale_code, language_code).await;
            let app = crate::routes::router(state.clone());
            let product_id = product_with_taxes(&state, "LOC-PARITY", "42").await;

            for (label, save_form, ladder_query, english, spanish) in REFUSAL_PARITY {
                let expected = if language_code == "es" {
                    spanish
                } else {
                    english
                };

                let (status, _, body) =
                    post_form_full(app.clone(), "/web/products", save_form, &[]).await;
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "{locale_code} {label}: {body}"
                );
                let save_sentence = save_refusal_sentence(&body);
                assert_eq!(
                    save_sentence, expected,
                    "{locale_code} {label}: the save answers this refusal in the active language"
                );

                let (status, html) = preview(app.clone(), product_id, ladder_query).await;
                assert_eq!(status, StatusCode::OK, "{locale_code} {label}: {html}");
                let ladder_sentence = ladder_refusal_sentence(&html).unwrap_or_else(|| {
                    panic!("{locale_code} {label}: the ladder must state a refusal: {html}")
                });
                assert_eq!(
                    ladder_sentence, save_sentence,
                    "{locale_code} {label}: the preview must say what the save says, in the same \
                     language, or it is lying about the save"
                );
            }
        }
    }

    /// The two catalogs as an operator's request resolves them: the closed key
    /// set, the English row and the Spanish row, with no database behind it.
    fn catalog_contexts() -> (
        crate::localization::LocalizationContext,
        crate::localization::LocalizationContext,
    ) {
        let english = crate::localization::LocalizationContext {
            locale_code: "en-US".into(),
            language_code: "en".into(),
            currency_code: "USD".into(),
            timezone: "UTC".into(),
        };
        let spanish = crate::localization::LocalizationContext {
            language_code: "es".into(),
            ..english.clone()
        };
        (english, spanish)
    }

    /// EVERY price refusal is translated in BOTH catalogs, and the English row
    /// is the exact text the JSON API still answers with. Three claims in one
    /// test because they are three ways this coupling could break:
    ///
    /// * a `PriceRefusal` variant with no catalog row renders as
    ///   "Translation unavailable" — a key leak into the operator's notice;
    /// * a row with only an English entry leaves a Spanish operator reading
    ///   English, which is the exact defect this work unit removes;
    /// * an English row re-worded away from `PriceRefusal::as_str` would make
    ///   the API's body and the web's sentence two different sentences for one
    ///   rule.
    ///
    /// The loop is over `PriceRefusal::ALL`, so a NEW variant fails here until
    /// it is mapped and translated — a variant cannot be added silently.
    #[test]
    fn every_price_refusal_is_translated_in_both_catalogs_and_keeps_its_api_text() {
        let (english, spanish) = catalog_contexts();
        for refusal in PriceRefusal::ALL {
            let key = crate::routes::price_refusal_key(refusal);
            let en = english.tr(key);
            let es = spanish.tr(key);
            assert_ne!(
                en, "Translation unavailable",
                "{refusal:?} is missing from the English catalog"
            );
            assert_ne!(
                es, "Translation unavailable",
                "{refusal:?} is missing from the Spanish catalog"
            );
            assert_ne!(
                es, en,
                "{refusal:?} falls back to English in the Spanish catalog: the operator needs a \
                 real translation, not a copy of the English row"
            );
            assert_eq!(
                en,
                refusal.as_str(),
                "{refusal:?}: the English catalog IS the API body, byte for byte, and the ladder \
                 and the save read the same row"
            );
        }
    }

    async fn post_json(app: axum::Router, uri: &str, body: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// No price refusal may be matched by string on the path that ANSWERS one.
    /// A rule that lives only in a literal can be re-worded in one place and
    /// matched in another, which is precisely how a ladder and a save end up
    /// disagreeing; the typed refusal removes the second home.
    ///
    /// The sentences are allowed in exactly two files: the `PriceRefusal`
    /// definition, which owns the English text every non-localized consumer
    /// still receives, and the catalogs, which own the translation. Every other
    /// file on the price path must not contain them at all — including the
    /// purchase record page's "apply line cost" action, which writes a product's
    /// cost and can therefore answer a price refusal of its own.
    ///
    /// Each file's test half is excluded, exactly as the `sale_repo` AC17 guard
    /// excludes its own migration fixture: a guard that matched the needles of
    /// the test asserting it would prove nothing.
    #[test]
    fn no_price_refusal_is_matched_by_string_on_the_price_path() {
        /// The owner of the English text, and the only file allowed to state it
        /// exactly once.
        const OWNER: &str = "src/models.rs";
        /// The catalogs hold the English sentence as the English translation.
        /// Any number of rows is fine here; the closed-parity test in
        /// `localization_tests` already pins the key set, and the totality test
        /// pins that the Spanish row is a real translation.
        const CATALOGS: &str = "src/localization/mod.rs";
        let sentences = [
            "markup_pct must be > -100",
            "cost_price must be > 0 when markup_pct is set",
            "markup_pct or cost_price is too large to derive a sale_price",
            "sale_price must be > 0 for products",
            "sale_price cannot be negative",
            "cost_price cannot be negative",
            "sale_price is required",
        ];
        let mut checked = 0;
        for path in [
            "src/error.rs",
            OWNER,
            "src/services/inventory.rs",
            "src/services/taxes.rs",
            "src/routes/inventory_web.rs",
            "src/routes/inventory_api.rs",
            "src/routes/purchases_web.rs",
            CATALOGS,
        ] {
            let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
            let source = std::fs::read_to_string(&full)
                .unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("every source has a production half");
            for sentence in sentences {
                let occurrences = production.matches(sentence).count();
                if path == OWNER {
                    assert_eq!(
                        occurrences, 1,
                        "{path} owns the English text of every price refusal exactly once: \
                         {sentence:?} appears {occurrences} times"
                    );
                } else if path == CATALOGS {
                    assert!(
                        occurrences >= 1,
                        "{path} must carry the English translation of {sentence:?}"
                    );
                } else {
                    assert_eq!(
                        occurrences, 0,
                        "{path} must not carry the string {sentence:?}: a price refusal has a \
                         typed identity, and a rule written as a literal here is a second rule \
                         waiting to drift from the one the save enforces"
                    );
                }
                checked += 1;
            }
        }
        assert_eq!(
            checked,
            8 * 7,
            "the guard must read every file and every sentence, or it proves nothing"
        );
    }

    /// Exactly ONE place in the whole source tree turns a price refusal into a
    /// sentence, and it is `routes::price_refusal_key` — not this module's, even
    /// though this module owns the ladder and both save routes. The purchase
    /// record page's "apply line cost" action writes a product's `cost_price`
    /// through the same service, so it answers price refusals too, and all four
    /// surfaces reach that one function.
    ///
    /// Two needles, because a second renderer can take either shape:
    ///
    /// * `=> MessageKey::PriceRefusal` — any renderer that maps a variant onto a
    ///   catalog key writes this, whether it is a function or a `match` inlined
    ///   in a handler. A renderer that answered with a raw sentence instead would
    ///   restate one of the English strings, and the sentence guard below refuses
    ///   that in every file but `models.rs` and the catalogs.
    /// * the three function names — a second `price_refusal_message` would be a
    ///   second renderer wearing a different name.
    ///
    /// Counted over every file `src/` holds, recursively: "no second mapping" is a
    /// claim about the tree, not about a chosen list of files. Each file's TEST
    /// half is excluded — this test's own needles live in one, and a guard that
    /// matches the needles of the test asserting it proves nothing at all.
    #[test]
    fn only_one_place_in_the_tree_maps_a_price_refusal_to_a_sentence() {
        const HOME: &str = "src/routes/mod.rs";
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources = rs_files(&root);
        assert!(
            sources.len() > 40,
            "the walk must reach the whole source tree, or the counts below prove nothing: \
             {} files",
            sources.len()
        );
        let production: Vec<(String, String)> = sources
            .iter()
            .map(|path| {
                let source = std::fs::read_to_string(path).unwrap_or_else(|error| {
                    panic!("cannot read {}: {error}", path.display());
                });
                let head = source
                    .split("#[cfg(test)]")
                    .next()
                    .expect("every source has a production half");
                (path.display().to_string(), head.to_string())
            })
            .collect();

        for needle in [
            "=> MessageKey::PriceRefusal",
            "fn price_refusal_key",
            "fn price_refusal_message",
            "fn localized_refusal_error",
        ] {
            let sites: Vec<&str> = production
                .iter()
                .filter(|(_, head)| head.contains(needle))
                .map(|(path, _)| path.as_str())
                .collect();
            assert!(
                !sites.is_empty(),
                "{needle} must exist exactly once: the guard found none, so it is scanning \
                 nothing and proves nothing"
            );
            assert_eq!(
                sites.len(),
                1,
                "{needle} must appear in exactly one place: a second one is a second wording of \
                 one rule, and it must go through {HOME} instead"
            );
            assert!(
                sites[0].ends_with(HOME),
                "{needle} is in {} and the only renderer lives in {HOME}",
                sites[0]
            );
        }
    }

    /// Every `.rs` file under `dir`, RECURSIVELY, sorted, so a failure names the
    /// same file in the same order on every machine. The recursion is the point:
    /// a walk that only read the top level would find 11 files and would happily
    /// report "exactly one renderer" while `routes/` went unread.
    fn rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        collect_rs_files(dir, &mut out);
        out
    }

    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("{dir:?} must be readable: {error}"))
            .map(|entry| entry.expect("a readable directory entry").path())
            .filter(|path| path.is_dir() || path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}
