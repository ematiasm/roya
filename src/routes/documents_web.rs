// Documents web: the `/documents` cross-department index page and its
// `/web/documents` HTMX list fragment. The page is the first any-of screen:
// any ONE of `sales.read`, `purchases.read`, `inventory.read` or
// `customers.read` opens it, and the page itself narrows the content to the
// families that principal's codes own — the kernel answers which families, the
// page obeys, and never a code the request supplies decides what renders.
// Thin handlers over DocumentService (already permission-narrowed filters);
// every row links to the document page the same code gates, so no row links to
// a page its reader cannot open.
use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
    routing::get,
    Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::models::{DocumentFilter, DocumentGroup, DocumentKind, DocumentRow};
use crate::routes::AppState;
use crate::security::authz::{
    CustomersRead, InventoryRead, Nav, Permission, Principal, PurchasesRead, RequireAny, SalesRead,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/documents", get(documents_page))
        .route("/web/documents", get(web_document_list))
}

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

/// The `/documents` page: the shell with the shared filter bar and the initial
/// list (the fragment partial included for the first render, exactly how the
/// sibling list pages work).
#[derive(Template)]
#[template(path = "documents.html")]
struct DocumentsTemplate {
    title: String,
    rows: Vec<DocumentView>,
    truncated: bool,
    limit: usize,
    groups: Vec<DocumentGroupOption>,
    filter_user: String,
    filter_from: String,
    filter_to: String,
    filter_q: String,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

/// The `/web/documents` fragment the browser's filter form swaps into the list
/// region. No shell, no fields the list does not read.
#[derive(Template)]
#[template(path = "partials/document_list.html")]
struct DocumentListPartial {
    title: String,
    rows: Vec<DocumentView>,
    truncated: bool,
    limit: usize,
}

/// One row resolved for the page: the feed's facts plus the two things only
/// the route can know — where the row opens and who the actor is.
struct DocumentView {
    kind: DocumentKind,
    kind_label: String,
    href: String,
    reference: String,
    party: String,
    date: chrono::NaiveDate,
    detail: String,
    amount: Option<Decimal>,
    quantity: Option<Decimal>,
    actor_name: Option<String>,
}

/// One option of the type filter: the PERMITTED group's token and label, and
/// whether the URL selected it, so a bookmarkable `/documents?group=…`
/// re-renders with the same form state the server used for the list.
struct DocumentGroupOption {
    token: String,
    label: String,
    selected: bool,
}

// ---------------------------------------------------------------------------
// Permission narrowing (the heart of the page)
// ---------------------------------------------------------------------------

/// The document families this principal may read. The kernel answers, the page
/// obeys: `sales.read` opens the sale documents and nothing else,
/// `customers.read` opens the collection receipts. Never a code the request
/// supplies.
fn permitted_kinds(principal: &Principal) -> Vec<DocumentKind> {
    DocumentKind::ALL
        .iter()
        .copied()
        .filter(|kind| match kind {
            DocumentKind::Sale | DocumentKind::SalePayment => principal.has(SalesRead::CODE),
            DocumentKind::Purchase | DocumentKind::PurchasePayment => {
                principal.has(PurchasesRead::CODE)
            }
            DocumentKind::StockMovement => principal.has(InventoryRead::CODE),
            DocumentKind::Receipt => principal.has(CustomersRead::CODE),
        })
        .collect()
}

/// The families the page reads: the requested type option (absent or unknown
/// means every group) expanded to kinds, intersected with what the principal
/// may read, deduped, in `DocumentKind::ALL` order. An empty intersection
/// renders an empty list — a narrower request never becomes a 403.
fn selected_kinds(permitted: &[DocumentKind], requested: Option<DocumentGroup>) -> Vec<DocumentKind> {
    let requested_kinds: &[DocumentKind] = match requested {
        Some(group) => group.kinds(),
        None => DocumentKind::ALL,
    };
    DocumentKind::ALL
        .iter()
        .copied()
        .filter(|kind| permitted.contains(kind) && requested_kinds.contains(kind))
        .collect()
}

/// The groups a permitted family belongs to, in `DocumentGroup::ALL` order —
/// the option list the page renders. A family the principal may not read never
/// even surfaces as an option.
fn group_options(
    permitted: &[DocumentKind],
    requested: Option<DocumentGroup>,
) -> Vec<DocumentGroupOption> {
    DocumentGroup::ALL
        .iter()
        .copied()
        .filter(|group| group.kinds().iter().any(|kind| permitted.contains(kind)))
        .map(|group| DocumentGroupOption {
            token: group.token().to_string(),
            label: group.label().to_string(),
            selected: requested == Some(group),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Query parameters and the filter they build
// ---------------------------------------------------------------------------

/// Query parameters for the documents index. Every field is optional and an
/// empty or unparseable value is treated as absent, so a filterless or partial
/// URL is never an error.
#[derive(Debug, Deserialize, Default)]
pub struct DocumentListQuery {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub q: String,
}

/// Empty means "not provided"; an unparseable date is treated as absent, the
/// way the sibling list filters treat one.
fn parse_optional_date(raw: &str) -> Option<chrono::NaiveDate> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn clean_filter_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ---------------------------------------------------------------------------
// The view: the feed's facts plus the route-only facts
// ---------------------------------------------------------------------------

/// Where one row opens. Every href is gated by the same code that made the row
/// visible (a sale row only renders for a `sales.read` principal, and
/// `/sales/{id}` requires the same code), so no row links to a page its reader
/// cannot open.
fn document_href(kind: DocumentKind, owner_id: i64) -> String {
    match kind {
        DocumentKind::Sale | DocumentKind::SalePayment => format!("/sales/{owner_id}"),
        DocumentKind::Purchase | DocumentKind::PurchasePayment => format!("/purchases/{owner_id}"),
        DocumentKind::Receipt => format!("/customers/{owner_id}"),
        // There is NO product record page: `/products` is the list and the
        // product detail is an HTMX drawer fragment, so the stock row opens
        // the products list at that product's row — the anchor
        // `id="product-{id}"` already exists in `partials/product_list.html`.
        DocumentKind::StockMovement => format!("/products#product-{owner_id}"),
    }
}

/// Resolve the views of one feed: the two route-only facts are the href (from
/// the kind and the owning document) and the actor's display name, resolved in
/// ONE `audit_actor_names` call over the whole page and then mapped (AC20: the
/// route layer is the only layer allowed to resolve identity names).
async fn resolve_views(state: &AppState, rows: Vec<DocumentRow>) -> AppResult<Vec<DocumentView>> {
    let actor_ids: Vec<i64> = rows.iter().map(|row| row.created_by).collect();
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| DocumentView {
            kind: row.kind,
            kind_label: row.kind.label().to_string(),
            href: document_href(row.kind, row.owner_id),
            reference: row.reference,
            party: row.party,
            date: row.date,
            detail: row.detail,
            amount: row.amount,
            quantity: row.quantity,
            actor_name: names.get(&row.created_by).cloned(),
        })
        .collect())
}

/// Everything both handlers need: the permission-narrowed feed resolved into
/// views, plus the type options built from the permitted groups.
struct DocumentsModel {
    rows: Vec<DocumentView>,
    truncated: bool,
    limit: usize,
    groups: Vec<DocumentGroupOption>,
}

async fn documents_model(
    state: &AppState,
    principal: &Principal,
    query: &DocumentListQuery,
) -> AppResult<DocumentsModel> {
    let permitted = permitted_kinds(principal);
    let requested = DocumentGroup::parse(query.group.trim());
    let kinds = selected_kinds(&permitted, requested);
    // `Some(empty)` means the typed actor matched no user — a filter that
    // matches nothing, never an error and never a silent "all".
    let actor_ids = crate::routes::audit_actor_ids(&state.pool, &query.user).await?;
    let filter = DocumentFilter {
        kinds,
        actor_ids,
        from: parse_optional_date(&query.from),
        to: parse_optional_date(&query.to),
        search: clean_filter_text(&query.q),
    };
    let feed = state.document_service.list(&filter).await?;
    let truncated = feed.truncated;
    let limit = feed.limit;
    let rows = resolve_views(state, feed.rows).await?;
    Ok(DocumentsModel {
        rows,
        truncated,
        limit,
        groups: group_options(&permitted, requested),
    })
}

// ---------------------------------------------------------------------------
// Page + fragment
// ---------------------------------------------------------------------------

async fn documents_page(
    State(state): State<AppState>,
    _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    principal: axum::Extension<Principal>,
    Query(query): Query<DocumentListQuery>,
) -> Result<Html<String>, AppError> {
    let model = documents_model(&state, &principal, &query).await?;
    let tmpl = DocumentsTemplate {
        title: "All documents".to_string(),
        rows: model.rows,
        truncated: model.truncated,
        limit: model.limit,
        groups: model.groups,
        filter_user: query.user.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        filter_q: query.q.trim().to_string(),
        nav_key: "documents",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// The fragment the browser's filter form fetches and swaps into the list
/// region, rendered the way `render_list` works in the sibling pages.
async fn web_document_list(
    State(state): State<AppState>,
    _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    principal: axum::Extension<Principal>,
    Query(query): Query<DocumentListQuery>,
) -> Result<Html<String>, AppError> {
    let model = documents_model(&state, &principal, &query).await?;
    let html = DocumentListPartial {
        title: "All documents".to_string(),
        rows: model.rows,
        truncated: model.truncated,
        limit: model.limit,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}
