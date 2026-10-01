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
    extract::{Extension, Query, State},
    response::Html,
    routing::get,
    Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{
    DocumentFilter, DocumentGroup, DocumentKind, DocumentRow, PurchaseRecord, PurchaseStatus,
    SaleRecord, SaleStatus,
};
use crate::routes::AppState;
use crate::security::authz::{
    CustomersRead, FinanceRead, InventoryRead, Nav, Permission, Principal, PurchasesCancel,
    PurchasesCreate, PurchasesRead, RequireAny, SalesCancel, SalesCreate, SalesRead,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/documents", get(documents_page))
        .route("/web/documents", get(web_document_list))
        .route(
            "/web/documents/detail/{kind}/{id}",
            get(web_document_detail),
        )
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
    localization: LocalizationContext,
    rows: Vec<DocumentView>,
    truncated: bool,
    limit: usize,
    truncated_message: String,
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
    localization: LocalizationContext,
    rows: Vec<DocumentView>,
    truncated: bool,
    limit: usize,
    truncated_message: String,
}

/// One row resolved for the page: the feed's facts plus the three things only
/// the route can know — the row's own id and kind token (the drawer's fragment
/// path segment), where the row opens, and who the actor is.
struct DocumentView {
    kind: DocumentKind,
    kind_label: String,
    /// The row's own id (for a payment, the payment's — not the owner's).
    id: i64,
    /// The family's URL token, the `{kind}` segment of the drawer fragment.
    kind_token: String,
    href: String,
    reference: String,
    party: String,
    date: chrono::NaiveDate,
    detail: String,
    amount: Option<Decimal>,
    /// The document-total refusal in the operator's language, when the row's
    /// amount could not be computed. `Some` exactly when a family that HAS money
    /// refused: the row then shows the sentence instead of a figure, and the
    /// page keeps every other document.
    total_refusal_message: Option<String>,
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
/// supplies. The family→code mapping is `DocumentKind::read_code` — the one
/// mapping the page, the drawer route and the kernel agreement test share.
fn permitted_kinds(principal: &Principal) -> Vec<DocumentKind> {
    DocumentKind::ALL
        .iter()
        .copied()
        .filter(|kind| principal.has(kind.read_code()))
        .collect()
}

/// The families the page reads: the requested type option (absent or unknown
/// means every group) expanded to kinds, intersected with what the principal
/// may read, deduped, in `DocumentKind::ALL` order. An empty intersection
/// renders an empty list — a narrower request never becomes a 403.
fn selected_kinds(
    permitted: &[DocumentKind],
    requested: Option<DocumentGroup>,
) -> Vec<DocumentKind> {
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
    localization: &LocalizationContext,
) -> Vec<DocumentGroupOption> {
    DocumentGroup::ALL
        .iter()
        .copied()
        .filter(|group| group.kinds().iter().any(|kind| permitted.contains(kind)))
        .map(|group| DocumentGroupOption {
            token: group.token().to_string(),
            label: match group {
                DocumentGroup::Sales => {
                    localization.tr(crate::localization::MessageKey::DocumentGroupSales)
                }
                DocumentGroup::Purchases => {
                    localization.tr(crate::localization::MessageKey::DocumentGroupPurchases)
                }
                DocumentGroup::Stock => {
                    localization.tr(crate::localization::MessageKey::DocumentGroupStock)
                }
                DocumentGroup::Payments => {
                    localization.tr(crate::localization::MessageKey::DocumentGroupPayments)
                }
            }
            .to_string(),
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
fn document_kind_label(kind: &DocumentKind, localization: &LocalizationContext) -> String {
    let key = match kind {
        DocumentKind::Sale => crate::localization::MessageKey::DocumentSale,
        DocumentKind::SalePayment => crate::localization::MessageKey::DocumentSalePayment,
        DocumentKind::Purchase => crate::localization::MessageKey::DocumentsPurchase,
        DocumentKind::PurchasePayment => crate::localization::MessageKey::DocumentPurchasePayment,
        DocumentKind::StockMovement => crate::localization::MessageKey::DocumentStockMovement,
        DocumentKind::Receipt => crate::localization::MessageKey::DocumentReceipt,
    };
    localization.tr(key).to_string()
}

fn document_truncation_message(
    truncated: bool,
    limit: usize,
    localization: &LocalizationContext,
) -> String {
    if truncated {
        localization.tr_with(
            crate::localization::MessageKey::DocumentsTruncated,
            &[("limit", &limit.to_string())],
        )
    } else {
        String::new()
    }
}

async fn resolve_views(
    state: &AppState,
    rows: Vec<DocumentRow>,
    localization: &LocalizationContext,
) -> AppResult<Vec<DocumentView>> {
    let actor_ids: Vec<i64> = rows.iter().map(|row| row.created_by).collect();
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| DocumentView {
            kind: row.kind,
            kind_label: document_kind_label(&row.kind, localization),
            id: row.id,
            kind_token: row.kind.token().to_string(),
            href: document_href(row.kind, row.owner_id),
            reference: row.reference,
            party: row.party,
            date: row.date,
            detail: row.detail,
            amount: row.amount,
            total_refusal_message: row
                .total_refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, localization)),
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
    localization: &LocalizationContext,
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
    let rows = resolve_views(state, feed.rows, localization).await?;
    Ok(DocumentsModel {
        rows,
        truncated,
        limit,
        groups: group_options(&permitted, requested, localization),
    })
}

// ---------------------------------------------------------------------------
// Page + fragment
// ---------------------------------------------------------------------------

async fn documents_page(
    State(state): State<AppState>,
    _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<Principal>,
    Query(query): Query<DocumentListQuery>,
) -> Result<Html<String>, AppError> {
    let model = documents_model(&state, &principal, &query, &localization).await?;
    let truncated_message =
        document_truncation_message(model.truncated, model.limit, &localization);
    let title = localization
        .tr(crate::localization::MessageKey::DocumentsAll)
        .to_string();
    let tmpl = DocumentsTemplate {
        title,
        localization,
        rows: model.rows,
        truncated: model.truncated,
        limit: model.limit,
        truncated_message,
        groups: model.groups,
        filter_user: query.user.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        filter_q: query.q.trim().to_string(),
        nav_key: "documents",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// The fragment the browser's filter form fetches and swaps into the list
/// region, rendered the way `render_list` works in the sibling pages.
async fn web_document_list(
    State(state): State<AppState>,
    _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<Principal>,
    Query(query): Query<DocumentListQuery>,
) -> Result<Html<String>, AppError> {
    let model = documents_model(&state, &principal, &query, &localization).await?;
    let truncated_message =
        document_truncation_message(model.truncated, model.limit, &localization);
    let html = DocumentListPartial {
        title: localization
            .tr(crate::localization::MessageKey::DocumentsAll)
            .to_string(),
        localization,
        rows: model.rows,
        truncated: model.truncated,
        limit: model.limit,
        truncated_message,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// The drawer: one detail fragment for all six families
//
// Six near-identical per-family templates would drift: the drawer is ONE
// component whose sections are filled per family. The route assembles the
// generic payload (`DocumentDetailPartial`) — every name the operator reads is
// resolved HERE, never in a template (AC20: actor names only through
// `crate::routes::audit_actor_names`, the wiring layer's resolver). The
// operator-facing copy (fact labels, section titles, links) is Spanish like
// the record pages it mirrors; the page chrome stays English like its
// neighbours. This slice is read-only: no action buttons render yet.
// ---------------------------------------------------------------------------

/// One `(label, value)` line of the drawer's fact list. `value` is already the
/// display form: the route resolved every name before building it.
struct DrawerFact {
    label: String,
    value: String,
}

/// A payment drawer's parent facts: the owning document's total, paid and
/// balance — or the same single refusal, on the same terms as
/// [`document_money_facts`]. A payment's own amount is a stored fact and is
/// always shown; what can be missing is the document it is measured against.
fn payment_money_facts(
    money: Option<crate::models::RecordMoney>,
    total_refusal: Option<crate::models::PriceRefusal>,
    localization: &LocalizationContext,
) -> Vec<DrawerFact> {
    use crate::localization::MessageKey;
    let Some(money) = money else {
        let sentence = total_refusal
            .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
            .unwrap_or_default();
        return vec![DrawerFact::new(
            copy(localization, MessageKey::CustomerTotal),
            sentence,
        )];
    };
    vec![
        DrawerFact::new(
            copy(localization, MessageKey::CustomerTotal),
            localization.format_currency(money.total),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::CustomerPaid),
            localization.format_currency(money.paid),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::DocumentsBalance),
            localization.format_currency(money.due),
        ),
    ]
}

/// A document's money as drawer facts, or the ONE refusal that says why the
/// figures are absent.
///
/// `None` money means the document's lines cannot be added up — each of them is
/// representable, and their sum is not — so the drawer states the refusal
/// through the one shared `price_refusal_key` mapping and shows no figure at
/// all. Publishing a placeholder number would be the one thing this work unit
/// exists to prevent, and repeating the same sentence six times would be noise
/// where one is a fact.
fn document_money_facts(
    money: Option<crate::models::RecordMoney>,
    total_refusal: Option<crate::models::PriceRefusal>,
    localization: &LocalizationContext,
) -> Vec<DrawerFact> {
    use crate::localization::MessageKey;
    let Some(money) = money else {
        let sentence = total_refusal
            .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
            .unwrap_or_default();
        return vec![DrawerFact::new(
            copy(localization, MessageKey::CustomerTotal),
            sentence,
        )];
    };
    vec![
        DrawerFact::new(
            copy(localization, MessageKey::TaxNetSubtotal),
            localization.format_currency(money.net_subtotal),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::TaxTotal),
            localization.format_currency(money.tax_total),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::CustomerTotal),
            localization.format_currency(money.total),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::CustomerPaid),
            localization.format_currency(money.paid),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::DocumentsBalance),
            localization.format_currency(money.due),
        ),
        DrawerFact::new(
            copy(localization, MessageKey::DocumentsPaymentStatus),
            status_copy(localization, &money.payment_status.to_string()),
        ),
    ]
}

impl DrawerFact {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }

    /// The fact only when `value` is non-empty: optional facts (notes, due
    /// dates, cancel reasons) vanish instead of rendering as empty rows.
    fn when_non_empty(label: impl Into<String>, value: Option<String>) -> Option<Self> {
        value.filter(|v| !v.is_empty()).map(|v| Self {
            label: label.into(),
            value: v,
        })
    }
}

/// One table the drawer renders (lines, payments, allocations). `href` binds
/// the row's FIRST cell when present — the allocation's sale number opens the
/// sale the money applied to.
struct DrawerTableRow {
    cells: Vec<String>,
    href: Option<String>,
}

struct DrawerTable {
    title: String,
    headers: Vec<String>,
    rows: Vec<DrawerTableRow>,
}

/// The payment families' parent document as a sub-block: the drawer shows the
/// payment's own facts AND the summary of the sale/purchase it belongs to,
/// because that is where the operator's attention goes next.
struct DrawerParent {
    label: String,
    title: String,
    status_line: String,
    facts: Vec<DrawerFact>,
    href: String,
}

/// One link at the drawer's foot: the page that owns the document (or its
/// ledger entry). A link renders only when the principal holds the code its
/// target route declares, so no link is a dead end — a reader who cannot
/// follow a link still reads the datum as text in the facts.
struct DrawerLink {
    label: String,
    href: String,
}

/// One action the drawer offers. It is built ONLY when the principal holds
/// the code the endpoint requires and the document's state allows it, so a
/// rendered button is always a button that can actually work — the drawer
/// never invents an action, it re-presents the ones the system has.
struct DrawerAction {
    label: String,
    /// "delete" or "post" — the HTTP verb the button must use.
    method: String,
    path: String,
    /// Hidden fields for a post action (the collection endpoints read the id
    /// from the body).
    fields: Vec<(String, String)>,
    /// Whether the action takes an optional free-text reason (both cancel
    /// endpoints store it in `cancel_reason`).
    reason: bool,
    /// The human action name the page's global error handler announces when
    /// the action fails, so a refusal reads like the sibling forms' and never
    /// as a raw path.
    data_action: String,
    /// The impact preview: what this action deletes or creates, one line each,
    /// computed from the same reads the endpoint will use. This is the warning
    /// the operator must read before acting.
    impact: Vec<String>,
    /// The native confirm question for a destructive action.
    confirm: Option<String>,
}

/// The one partial all six families render: a title (the identifier), a status
/// line, the fact list, optional tables, the optional parent summary, the
/// optional action block (built only from real, permitted actions), an
/// optional notice sentence and the links. The route decides what fills it.
#[derive(Template)]
#[template(path = "partials/document_detail.html")]
struct DocumentDetailPartial {
    localization: LocalizationContext,
    kind_label: String,
    title: String,
    status_line: String,
    facts: Vec<DrawerFact>,
    tables: Vec<DrawerTable>,
    parent: Option<DrawerParent>,
    actions: Vec<DrawerAction>,
    notice: Option<String>,
    links: Vec<DrawerLink>,
}

/// `GET /web/documents/detail/{kind}/{id}`: the drawer fragment. The any-of
/// gate admits any one of the four read tiers (the page itself opened with
/// one), and the NARROWING below is per family: the row the operator clicked
/// obeys the same rule here as it did in the list — a principal never opens
/// another tier's document, and the refusal names the `read_code` it lacks.
async fn web_document_detail(
    State(state): State<AppState>,
    _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    Extension(localization): Extension<LocalizationContext>,
    axum::Extension(principal): axum::Extension<Principal>,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, i64)>,
) -> Result<Html<String>, AppError> {
    let tmpl = document_detail(&state, &principal, &kind, id, &localization).await?;
    let html = tmpl
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// The two RETURN families the drawer opens, resolved from the `{kind}` segment
/// BEFORE `DocumentKind::parse` sees it.
///
/// **They are not `DocumentKind` variants, and that is a boundary rather than an
/// oversight.** `DocumentKind` is the feed's own vocabulary: it names the six
/// families `DocumentService` reads for the INDEX LIST, and it is closed by the
/// `read_code` agreement test in `models.rs`. A return has no row in that feed —
/// nothing lists it — so adding a variant would make the index's own closed key
/// set name two families it cannot render, and it would mean editing `models.rs`,
/// `services/documents.rs` and two repositories to say so.
///
/// What the return families DO need from the drawer is the ability to OPEN a
/// document the operator is already looking at, under a permission code of their
/// own. That is what this route-local enum carries, and it is why the permission
/// narrowing is spelled out here rather than inherited from `permitted_kinds`:
/// `purchases.read` opens a purchase return and `sales.read` opens a credit note,
/// the parent's own code, so a principal who can annul a sale can credit it and
/// one who can confirm a purchase can return it. No new permission code, so no new
/// migration and nothing in the catalog that gates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnDrawerKind {
    PurchaseReturn,
    CustomerReturn,
}

impl ReturnDrawerKind {
    /// The `{kind}` segment's token for a purchase return. Named, because the 404
    /// lists every token this route accepts and must not hand-write one it does
    /// not parse.
    const PURCHASE_RETURN_TOKEN: &'static str = "purchase_return";
    /// And for a credit note.
    const CUSTOMER_RETURN_TOKEN: &'static str = "customer_return";

    /// Parse a token. The same family names the document pages use
    /// (`/purchase-returns/{id}`, `/customer-returns/{id}`), so a link and a URL
    /// agree on one word.
    fn parse(token: &str) -> Option<Self> {
        match token {
            Self::PURCHASE_RETURN_TOKEN => Some(Self::PurchaseReturn),
            Self::CUSTOMER_RETURN_TOKEN => Some(Self::CustomerReturn),
            _ => None,
        }
    }

    /// The catalog code that READS this family — the parent's read code, for the
    /// reason the type's doc gives.
    fn read_code(&self) -> &'static str {
        match self {
            Self::PurchaseReturn => PurchasesRead::CODE,
            Self::CustomerReturn => SalesRead::CODE,
        }
    }
}

/// The drawer payload for one document: resolve the family, narrow by
/// permission, then assemble. Missing/unknown documents are the standard 404
/// with the family's name in the message.
async fn document_detail(
    state: &AppState,
    principal: &Principal,
    kind_token: &str,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    // The two return families resolve first: they are not `DocumentKind`
    // variants, so letting `DocumentKind::parse` see their tokens would 404 every
    // one of them.
    if let Some(kind) = ReturnDrawerKind::parse(kind_token) {
        if !principal.has(kind.read_code()) {
            return Err(AppError::Forbidden(format!(
                "Se necesita el permiso «{}» para ver este documento",
                kind.read_code()
            )));
        }
        return match kind {
            ReturnDrawerKind::PurchaseReturn => {
                purchase_return_drawer(state, principal, id, localization).await
            }
            ReturnDrawerKind::CustomerReturn => {
                customer_return_drawer(state, principal, id, localization).await
            }
        };
    }
    // The 404's token list names the return families TOO, because this route is
    // the one that accepts them: a caller who typed `purchase_returns` deserves to
    // be told the singular token the route knows, not a list that silently omits
    // the two families it resolves two branches above.
    let kind = DocumentKind::parse(kind_token).ok_or_else(|| {
        let mut tokens: Vec<&str> = DocumentKind::ALL.iter().map(|k| k.token()).collect();
        tokens.push(ReturnDrawerKind::PURCHASE_RETURN_TOKEN);
        tokens.push(ReturnDrawerKind::CUSTOMER_RETURN_TOKEN);
        AppError::NotFound(format!(
            "unknown document kind \"{kind_token}\": the drawer knows {tokens}",
            tokens = tokens.join(", ")
        ))
    })?;
    if !permitted_kinds(principal).contains(&kind) {
        return Err(AppError::Forbidden(format!(
            "Se necesita el permiso «{}» para ver este documento",
            kind.read_code()
        )));
    }
    match kind {
        DocumentKind::Sale => sale_drawer(state, &principal, id, localization).await,
        DocumentKind::SalePayment => sale_payment_drawer(state, &principal, id, localization).await,
        DocumentKind::Purchase => purchase_drawer(state, &principal, id, localization).await,
        DocumentKind::PurchasePayment => {
            purchase_payment_drawer(state, &principal, id, localization).await
        }
        DocumentKind::StockMovement => stock_movement_drawer(state, id, localization).await,
        DocumentKind::Receipt => receipt_drawer(state, &principal, id, localization).await,
    }
}

/// The identifier a document answers by, the way every list already shows it:
/// the assigned number, or `Draft #id` while a draft has none.
fn copy(localization: &LocalizationContext, key: crate::localization::MessageKey) -> String {
    localization.tr(key).to_string()
}

fn status_copy(localization: &LocalizationContext, status: &str) -> String {
    match status {
        "Draft" => copy(localization, crate::localization::MessageKey::StatusDraft),
        "Confirmed" => copy(
            localization,
            crate::localization::MessageKey::StatusConfirmed,
        ),
        "Cancelled" => copy(
            localization,
            crate::localization::MessageKey::StatusCancelled,
        ),
        "Paid" => copy(localization, crate::localization::MessageKey::StatusPaid),
        "Partial" => copy(localization, crate::localization::MessageKey::StatusPartial),
        _ => copy(localization, crate::localization::MessageKey::StatusUnpaid),
    }
}

fn transaction_kind_copy(localization: &LocalizationContext, kind: &str) -> String {
    if kind == "Income" {
        copy(
            localization,
            crate::localization::MessageKey::DashboardIncome,
        )
    } else {
        copy(
            localization,
            crate::localization::MessageKey::DashboardExpense,
        )
    }
}

fn movement_type_copy(localization: &LocalizationContext, movement_type: &str) -> String {
    match movement_type {
        "In" => copy(
            localization,
            crate::localization::MessageKey::ProductValueIn,
        ),
        "Out" => copy(
            localization,
            crate::localization::MessageKey::ProductValueOut,
        ),
        _ => copy(
            localization,
            crate::localization::MessageKey::ProductValueAdjust,
        ),
    }
}

fn reason_copy(localization: &LocalizationContext, reason: &str) -> String {
    let key = match reason {
        "Initial" => crate::localization::MessageKey::ProductReasonInitial,
        "Purchase" => crate::localization::MessageKey::ProductReasonPurchase,
        "Sale" => crate::localization::MessageKey::ProductReasonSale,
        "Loss" => crate::localization::MessageKey::ProductReasonLoss,
        _ => return reason.to_string(),
    };
    copy(localization, key)
}

fn payment_type_copy(localization: &LocalizationContext, payment_type: &str) -> String {
    if payment_type == "Cash" {
        copy(localization, crate::localization::MessageKey::ValueCash)
    } else {
        copy(localization, crate::localization::MessageKey::ValueCredit)
    }
}

fn document_title(number: Option<&str>, id: i64, localization: &LocalizationContext) -> String {
    number.map(str::to_string).unwrap_or_else(|| {
        localization.tr_with(
            crate::localization::MessageKey::PurchasesDraftRef,
            &[("id", &id.to_string())],
        )
    })
}

/// The edit affordance the user asked for as a real BUTTON-styled link: the
/// FIRST entry of the links list, labelled by what the state allows. It
/// deliberately navigates to the record page — the drawer never duplicates
/// the multi-field forms (header, lines, payments, confirm) that page owns.
/// The status arrives as its `Display` form — both families' status enums
/// share the exact three names — so sale and purchase call the same helper.
/// The draft label names the WHOLE document, not just its header: the record
/// page is where the identity fields, the lines and the confirmation are
/// worked on, and one order is one object — the header is not a separate
/// thing from its lines (user correction on the draft drawer's wording).
fn edit_affordance_link(
    status: &str,
    href: String,
    localization: &LocalizationContext,
) -> DrawerLink {
    let key = if status == "Draft" {
        crate::localization::MessageKey::DocumentsEditDraft
    } else {
        crate::localization::MessageKey::DocumentsOpenDocument
    };
    let label = copy(localization, key);
    DrawerLink {
        label: label.to_string(),
        href,
    }
}

/// The edit affordance as text: where the multi-field actions live, worded by
/// state. The drawer states it instead of building edit forms it would have
/// to keep in lockstep with the endpoints. A draft cannot take payments —
/// only a Confirmed document does — so the payments mention belongs to the
/// confirmed state's sentence, never to the draft's.
fn edit_affordance_notice(status: &str, localization: &LocalizationContext) -> String {
    let key = match status {
        "Draft" => crate::localization::MessageKey::DocumentsDraftNotice,
        "Confirmed" => crate::localization::MessageKey::DocumentsConfirmedNotice,
        _ => crate::localization::MessageKey::DocumentsCancelledNotice,
    };
    copy(localization, key)
}

/// The draft line count, worded once so the preview and the confirm question
/// cannot disagree: "1 línea" for a single line, "N líneas" otherwise, with
/// the possessive and the parenthetical agreeing.
fn draft_lines_phrase(n: usize, localization: &LocalizationContext) -> (String, String) {
    let lines = if n == 1 {
        copy(
            localization,
            crate::localization::MessageKey::DocumentsDraftLineOne,
        )
    } else {
        localization.tr_with(
            crate::localization::MessageKey::DocumentsDraftLineMany,
            &[("count", &n.to_string())],
        )
    };
    let listed = if n == 1 {
        copy(
            localization,
            crate::localization::MessageKey::DocumentsLinesListedOne,
        )
    } else {
        copy(
            localization,
            crate::localization::MessageKey::DocumentsLinesListedMany,
        )
    };
    (lines, listed)
}

/// The SALE drawer's action block, per state and permission. Real actions
/// only — the drawer never invents one: Anular/Descartar re-present the tested
/// `cancel` endpoint, and a cancelled sale re-offers the delete ONLY while it was
/// discarded before confirm (`sale_number` still NULL) — a cancelled sale that
/// carries a number was confirmed first and offers nothing because its inverse
/// already happened.
///
/// **WHY A DRAFT IS DELETABLE — the corrected premise.** The wording here used to
/// be "a draft never touched stock, money or a customer's debt, so nothing
/// dangles". That was never a property of the status; it was an ASSUMPTION, and
/// it was FALSE. `confirm` used to write the sequence number, one movement per
/// tracked line, the `Income` and the `sale_payments` row on separate autocommit
/// connections, with `set_confirmed` LAST. A failure at that last step left the row
/// still reading `("Draft", NULL)` — a draft that had already committed a payment
/// row and an orphan `Income` naming the burned number. Such a Draft MATCHED the
/// deletable predicate (`status = 'Draft' OR (status = 'Cancelled' AND
/// `sale_number IS NULL)`, `sale_repo.rs`), and deleting it took the payment row
/// with it through the CASCADE while the `Income` had no foreign key to `sales` at
/// all and survived: the document was lost and the money kept.
///
/// **THAT RESIDUE NO LONGER EXISTS, and not because the delete grew a
/// cleanliness check — it never will.** `confirm` is now ONE transaction, opened
/// immediately before `next_number` and committed after the last write, so a
/// failure at any step rolls the whole run back and the row is a Draft with no
/// number, no movements, no finance entry and no payment. The measured proof is
/// the absence asserted by `confirm_failure_*` in `services/sales.rs` and
/// `services/purchases.rs`.
///
/// So the honest statement of the rule is: the predicate is a backstop on STATUS,
/// and the Draft behind it is CLEAN because confirm is atomic. Delete the
/// transaction and this block is wrong again — which is why the reason is written
/// here rather than left to be inferred from the SQL.
async fn sale_actions(
    state: &AppState,
    principal: &Principal,
    record: &SaleRecord,
    localization: &LocalizationContext,
) -> AppResult<Vec<DrawerAction>> {
    let sale = &record.sale;
    let mut actions = Vec::new();
    match sale.status {
        SaleStatus::Draft => {
            if principal.has(SalesCreate::CODE) {
                let n = record.lines.len();
                let (lines_phrase, listed) = draft_lines_phrase(n, localization);
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/sales/{}", sale.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    impact: vec![
                        localization.tr_with(
                            if n == 1 {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactOne
                            } else {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactMany
                            },
                            &[("count", &n.to_string()), ("listed", listed.as_str())],
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNeverConfirmed,
                        ),
                    ],
                    confirm: Some(localization.tr_with(
                        crate::localization::MessageKey::DocumentsDeleteDraftConfirm,
                        &[("lines", lines_phrase.as_str())],
                    )),
                });
            }
            if principal.has(SalesCancel::CODE) {
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDiscard,
                    ),
                    method: "post".to_string(),
                    path: "/web/sales/cancel".to_string(),
                    fields: vec![("sale_id".to_string(), sale.id.to_string())],
                    reason: true,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::PurchasesDiscard,
                    ),
                    impact: vec![
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsDiscardImpact,
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNothingToReverse,
                        ),
                    ],
                    confirm: None,
                });
            }
        }
        SaleStatus::Confirmed => {
            if principal.has(SalesCancel::CODE) {
                actions.push(sale_annul_action(state, record, localization).await?);
            }
        }
        SaleStatus::Cancelled => {
            // Number still NULL → discarded before confirm → deletable. With
            // a number the sale was confirmed first: permanent audit trail
            // (refund transactions reference its payments), no action.
            if sale.sale_number.is_none() && principal.has(SalesCreate::CODE) {
                let n = record.lines.len();
                let (lines_phrase, listed) = draft_lines_phrase(n, localization);
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDiscard,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/sales/{}", sale.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDiscard,
                    ),
                    impact: vec![
                        localization.tr_with(
                            crate::localization::MessageKey::DocumentsDeleteDiscardImpact,
                            &[
                                ("lines", lines_phrase.as_str()),
                                ("listed", listed.as_str()),
                            ],
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNeverConfirmed,
                        ),
                    ],
                    confirm: Some(localization.tr_with(
                        crate::localization::MessageKey::DocumentsDeleteDiscardConfirm,
                        &[("lines", lines_phrase.as_str())],
                    )),
                });
            }
        }
    }
    Ok(actions)
}

/// The annul impact preview, computed from the SAME reads `cancel` runs: one
/// line per tracked line (the view's `tracks_stock` IS the confirm/cancel
/// predicate), the product's CURRENT active flag from
/// `inventory_service.get_product` — the exact precondition `cancel` refuses
/// on, so the warning cannot drift from the refusal — one line per payment,
/// and the state change. An inactive tracked product renders a blocker line
/// instead of a movement line: the action stays rendered and says plainly it
/// will be refused, never hiding the operator's only path. With
/// `allow_negative = false` the refusal caveat the endpoint enforces renders
/// too.
async fn sale_annul_action(
    state: &AppState,
    record: &SaleRecord,
    localization: &LocalizationContext,
) -> AppResult<DrawerAction> {
    let sale = &record.sale;
    let mut impact = Vec::new();
    for line in &record.lines {
        if !line.tracks_stock {
            continue;
        }
        let product = state.inventory_service.get_product(line.product_id).await?;
        if !product.is_active {
            impact.push(localization.tr_with(
                crate::localization::MessageKey::DocumentsProductInactive,
                &[("name", product.name.as_str())],
            ));
        } else {
            impact.push(localization.tr_with(
                crate::localization::MessageKey::DocumentsSaleReturnStock,
                &[
                    ("name", line.product_name.as_str()),
                    ("quantity", &localization.format_quantity(line.qty)),
                ],
            ));
        }
    }
    for payment in &record.payments {
        impact.push(localization.tr_with(
            crate::localization::MessageKey::DocumentsRefundAccount,
            &[
                ("amount", &localization.format_currency(payment.amount)),
                ("account", payment.account_name.as_str()),
                ("kind", "Expense"),
            ],
        ));
    }
    impact.push(copy(
        localization,
        crate::localization::MessageKey::DocumentsAnnulStateSale,
    ));
    if !state.allow_negative {
        impact.push(copy(
            localization,
            crate::localization::MessageKey::DocumentsNegativeBlock,
        ));
    }
    Ok(DrawerAction {
        label: copy(
            localization,
            crate::localization::MessageKey::DocumentsAnnul,
        ),
        method: "post".to_string(),
        path: "/web/sales/cancel".to_string(),
        fields: vec![("sale_id".to_string(), sale.id.to_string())],
        reason: true,
        data_action: copy(
            localization,
            crate::localization::MessageKey::DocumentsAnnul,
        ),
        impact,
        confirm: Some(copy(
            localization,
            crate::localization::MessageKey::DocumentsAnnulConfirm,
        )),
    })
}

/// The PURCHASE drawer's action block — the mirror of the sale's with the
/// purchase wording: `Out · Purchase-return` movements (confirm writes
/// In/Purchase, cancel writes Out/Purchase-return), refunds that are `Income`
/// (money entering: no negative-balance refusal exists to preview), and the
/// purchase cancel endpoint. A DISCARDED purchase (Cancelled while never
/// confirmed: `purchase_number` NULL) re-offers the delete: it posted
/// nothing, so removing it strands no history; a cancelled purchase that
/// carries a number was confirmed first and offers nothing — its inverse
/// already happened and the audit trail stays.
async fn purchase_actions(
    state: &AppState,
    principal: &Principal,
    record: &PurchaseRecord,
    localization: &LocalizationContext,
) -> AppResult<Vec<DrawerAction>> {
    let purchase = &record.purchase;
    let mut actions = Vec::new();
    match purchase.status {
        PurchaseStatus::Draft => {
            if principal.has(PurchasesCreate::CODE) {
                let n = record.lines.len();
                let (lines_phrase, listed) = draft_lines_phrase(n, localization);
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/purchases/{}", purchase.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    impact: vec![
                        localization.tr_with(
                            if n == 1 {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactOne
                            } else {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactMany
                            },
                            &[("count", &n.to_string()), ("listed", listed.as_str())],
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNeverConfirmed,
                        ),
                    ],
                    confirm: Some(localization.tr_with(
                        crate::localization::MessageKey::DocumentsDeleteDraftConfirm,
                        &[("lines", lines_phrase.as_str())],
                    )),
                });
            }
            if principal.has(PurchasesCancel::CODE) {
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDiscard,
                    ),
                    method: "post".to_string(),
                    path: "/web/purchases/cancel".to_string(),
                    fields: vec![("purchase_id".to_string(), purchase.id.to_string())],
                    reason: true,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::PurchasesDiscard,
                    ),
                    impact: vec![
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsDiscardImpact,
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNothingToReverse,
                        ),
                    ],
                    confirm: None,
                });
            }
        }
        PurchaseStatus::Confirmed => {
            if principal.has(PurchasesCancel::CODE) {
                let mut impact = Vec::new();
                for line in &record.lines {
                    if !line.tracks_stock {
                        continue;
                    }
                    let product = state.inventory_service.get_product(line.product_id).await?;
                    if !product.is_active {
                        impact.push(localization.tr_with(
                            crate::localization::MessageKey::DocumentsProductInactive,
                            &[("name", product.name.as_str())],
                        ));
                    } else {
                        impact.push(localization.tr_with(
                            crate::localization::MessageKey::DocumentsPurchaseReturnStock,
                            &[
                                ("name", line.product_name.as_str()),
                                ("quantity", &localization.format_quantity(line.qty)),
                            ],
                        ));
                    }
                }
                for payment in &record.payments {
                    impact.push(localization.tr_with(
                        crate::localization::MessageKey::DocumentsRefundAccount,
                        &[
                            ("amount", &localization.format_currency(payment.amount)),
                            ("account", payment.account_name.as_str()),
                            ("kind", "Income"),
                        ],
                    ));
                }
                impact.push(copy(
                    localization,
                    crate::localization::MessageKey::DocumentsAnnulStatePurchase,
                ));
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsAnnul,
                    ),
                    method: "post".to_string(),
                    path: "/web/purchases/cancel".to_string(),
                    fields: vec![("purchase_id".to_string(), purchase.id.to_string())],
                    reason: true,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsAnnul,
                    ),
                    impact,
                    confirm: Some(copy(
                        localization,
                        crate::localization::MessageKey::DocumentsAnnulConfirm,
                    )),
                });
            }
        }
        PurchaseStatus::Cancelled => {
            // Number still NULL → discarded before confirm → deletable. With
            // a number the purchase was confirmed first: permanent audit
            // trail, no action.
            if purchase.purchase_number.is_none() && principal.has(PurchasesCreate::CODE) {
                let n = record.lines.len();
                let (lines_phrase, listed) = draft_lines_phrase(n, localization);
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDiscard,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/purchases/{}", purchase.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDiscard,
                    ),
                    impact: vec![
                        localization.tr_with(
                            crate::localization::MessageKey::DocumentsDeleteDiscardImpact,
                            &[
                                ("lines", lines_phrase.as_str()),
                                ("listed", listed.as_str()),
                            ],
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNeverConfirmed,
                        ),
                    ],
                    confirm: Some(localization.tr_with(
                        crate::localization::MessageKey::DocumentsDeleteDiscardConfirm,
                        &[("lines", lines_phrase.as_str())],
                    )),
                });
            }
        }
    }
    Ok(actions)
}

/// The PURCHASE-RETURN family: the drawer for a document that reverses part or
/// all of a confirmed purchase.
///
/// It is the purchase drawer's shape with the return's own facts, and the two
/// directions stated rather than assumed: stock goes OUT and the money comes
/// back IN, which is why the money row reads "received from the supplier" and not
/// "paid". The parent is shown as a sub-block with a link to the purchase, because
/// a return is evidence ABOUT a document and the operator's next question is
/// always "which one".
///
/// The delete action is offered ONLY for a draft and for a return discarded while
/// still draft. **A draft is deletable because `confirm` is ONE transaction, not
/// because "a draft never touched money"** — see the note on `sale_actions`, which
/// this mirrors word for word.
async fn purchase_return_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let detail = state.purchase_return_service.get_detail(id).await?;
    let purchase_return = &detail.purchase_return;
    let supplier = state
        .supplier_service
        .get_supplier(purchase_return.supplier_id)
        .await?;
    let parent = state
        .purchases_service
        .get_record(purchase_return.purchase_id)
        .await?;
    let (created_by, updated_by) = actor_facts(
        state,
        purchase_return.created_by,
        purchase_return.updated_by,
        localization,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsSupplier,
            ),
            &supplier.name,
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(purchase_return.return_date),
        ),
        // The refund is money ENTERING the shop. The verb is the family's own and
        // it is the reason this row does not read "paid".
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::PurchaseReturnsReceived,
            ),
            localization.format_currency(detail.paid),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::CustomerTotal),
            localization.format_currency(detail.total),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsBalance,
            ),
            localization.format_currency(detail.due),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsPaymentStatus,
            ),
            status_copy(localization, &detail.payment_status.to_string()),
        ),
    ];
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsNotes,
        ),
        Some(purchase_return.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsCancellationReason,
        ),
        purchase_return.cancel_reason.clone(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    // One line row per return line, resolved THROUGH the parent line for the
    // product's identity: a return line stores no product of its own, so the name
    // and the SKU come from the purchase line it names. A parent line that has
    // gone missing is not reachable through a RESTRICT foreign key, so the
    // `.get()` is over a key the document cannot hold.
    let lines: Vec<DrawerTableRow> = detail
        .lines
        .iter()
        .map(|line| {
            let parent_line = parent.lines.iter().find(|p| p.id == line.purchase_line_id);
            DrawerTableRow {
                cells: vec![
                    parent_line
                        .map(|p| p.product_name.clone())
                        .unwrap_or_default(),
                    localization.format_quantity(line.qty),
                    // The FROZEN cost, rendered and never typed: a return is
                    // always at the purchase price, so this figure is a fact about
                    // the parent line rather than something the return chose.
                    // `format_currency` and not `format_money`: this column sits
                    // beside the document's own Total, Balance and payment-status
                    // facts, and every one of those renders at the drawer's scale.
                    // Mixing the two here would put `7.00 USD` next to `21 USD` on
                    // one screen for no reason but a formatter difference.
                    localization.format_currency(line.unit_cost),
                    localization.format_currency(line.subtotal()),
                ],
                href: None,
            }
        })
        .collect();

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: copy(
            localization,
            crate::localization::MessageKey::PurchaseReturnsTitle,
        ),
        title: document_title(
            purchase_return.return_number.as_deref(),
            purchase_return.id,
            localization,
        ),
        status_line: status_copy(localization, &purchase_return.status.to_string()),
        facts,
        tables: vec![DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsLines,
            ),
            headers: vec![
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsProduct,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsQuantityShort,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsUnitCost,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxNetSubtotal,
                ),
            ],
            rows: lines,
        }],
        parent: Some(DrawerParent {
            label: copy(
                localization,
                crate::localization::MessageKey::PurchaseReturnsParentPurchase,
            ),
            title: document_title(
                parent.purchase.purchase_number.as_deref(),
                parent.purchase.id,
                localization,
            ),
            status_line: status_copy(localization, &parent.purchase.status.to_string()),
            facts: vec![DrawerFact::new(
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsSupplier,
                ),
                &parent.supplier_name,
            )],
            href: format!("/purchases/{}", parent.purchase.id),
        }),
        actions: purchase_return_actions(state, principal, &detail, localization).await?,
        notice: Some(edit_affordance_notice(
            &purchase_return.status.to_string(),
            localization,
        )),
        links: vec![edit_affordance_link(
            &purchase_return.status.to_string(),
            format!("/purchase-returns/{}", purchase_return.id),
            localization,
        )],
    })
}

/// The purchase return's action block: a draft may be discarded and (before
/// confirmation) deleted, a confirmed return may be cancelled, and a confirmed
/// return offers NO delete — it is reversed instead.
///
/// **THE PREMISE THE DELETE RESTS ON, corrected.** The reasoning "a draft never
/// touched stock, money or a customer's debt, so nothing dangles" was FALSE when
/// it was written and it is false now for a different reason than it was then.
/// Before the confirm-atomicity refactor, a Draft could be DIRTY: `confirm`
/// wrote the sequence number, the movements, the finance row and the payment row
/// on separate autocommit connections, so a failure at the last step left a row
/// still reading `("Draft", NULL)` with a committed payment behind it — and such a
/// Draft MATCHED the deletable predicate. Deleting it removed the document and kept
/// the money.
///
/// That residue no longer exists, and not because the delete got smarter:
/// `confirm` opens ONE transaction immediately before taking the number and
/// commits after the last write, so a Draft of a return has nothing committed
/// behind it — no number, no movement, no finance row, no payment. The delete
/// predicate is a backstop on STATUS, never a cleanliness check, and this family
/// relies on the transaction rather than on the predicate. The wording below
/// therefore says WHY the draft is safe, so the next reader does not re-derive it
/// from the predicate.
async fn purchase_return_actions(
    state: &AppState,
    principal: &Principal,
    detail: &crate::models::PurchaseReturnDetail,
    localization: &LocalizationContext,
) -> AppResult<Vec<DrawerAction>> {
    let purchase_return = &detail.purchase_return;
    let mut actions = Vec::new();
    let deletable = |status: &crate::models::PurchaseReturnStatus| {
        status == &crate::models::PurchaseReturnStatus::Draft
            || (status == &crate::models::PurchaseReturnStatus::Cancelled
                && purchase_return.return_number.is_none())
    };
    match purchase_return.status {
        crate::models::PurchaseReturnStatus::Draft => {
            if principal.has(PurchasesCreate::CODE) {
                let n = detail.lines.len();
                let (lines_phrase, listed) = draft_lines_phrase(n, localization);
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/purchase-returns/{}", purchase_return.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    impact: vec![
                        localization.tr_with(
                            if n == 1 {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactOne
                            } else {
                                crate::localization::MessageKey::DocumentsDeleteDraftImpactMany
                            },
                            &[("count", &n.to_string()), ("listed", listed.as_str())],
                        ),
                        copy(
                            localization,
                            crate::localization::MessageKey::DocumentsNeverConfirmed,
                        ),
                    ],
                    confirm: Some(localization.tr_with(
                        crate::localization::MessageKey::DocumentsDeleteDraftConfirm,
                        &[("lines", lines_phrase.as_str())],
                    )),
                });
            }
        }
        crate::models::PurchaseReturnStatus::Confirmed => {
            if principal.has(PurchasesCancel::CODE) {
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::PurchaseReturnsCancel,
                    ),
                    method: "post".to_string(),
                    path: format!("/web/purchase-returns/{}/cancel", purchase_return.id),
                    fields: vec![],
                    reason: true,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::PurchaseReturnsCancel,
                    ),
                    // Stock comes back In and the refund is taken back OUT as an
                    // Expense, which is the one direction on this family where a
                    // reversal can be refused for want of funds.
                    impact: vec![copy(
                        localization,
                        crate::localization::MessageKey::PurchaseReturnsCancelConfirm,
                    )],
                    confirm: None,
                });
            }
        }
        crate::models::PurchaseReturnStatus::Cancelled => {
            if deletable(&purchase_return.status) && principal.has(PurchasesCreate::CODE) {
                actions.push(drawer_discard_delete(
                    format!("/web/purchase-returns/{}", purchase_return.id),
                    localization,
                ));
            }
        }
    }
    Ok(actions)
}

/// The shared "this discarded document posted nothing, delete it" action, so the
/// sale, purchase, purchase-return and credit-note drawers state the impact in the
/// same words instead of four of them drifting apart.
fn drawer_discard_delete(path: String, localization: &LocalizationContext) -> DrawerAction {
    DrawerAction {
        label: copy(localization, crate::localization::MessageKey::CommonDelete),
        method: "delete".to_string(),
        path,
        fields: vec![],
        reason: false,
        data_action: copy(localization, crate::localization::MessageKey::CommonDelete),
        impact: vec![copy(
            localization,
            crate::localization::MessageKey::DocumentsNeverConfirmed,
        )],
        confirm: None,
    }
}

/// The CUSTOMER-RETURN family: the credit note, the mirror of
/// [`purchase_return_drawer`] with the nouns and the two directions swapped. Stock
/// comes IN and the refund goes OUT, so the money row reads "refunded to the
/// customer" and the cancel's impact says the goods go back out.
async fn customer_return_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let detail = state.customer_return_service.get_detail(id).await?;
    let customer_return = &detail.customer_return;
    let customer = state
        .customer_service
        .get_customer(customer_return.customer_id)
        .await?;
    let parent = state
        .sales_service
        .get_record(customer_return.sale_id)
        .await?;
    let (created_by, updated_by) = actor_facts(
        state,
        customer_return.created_by,
        customer_return.updated_by,
        localization,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsCustomer,
            ),
            &customer.name,
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(customer_return.return_date),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::CustomerReturnsRefunded,
            ),
            localization.format_currency(detail.paid),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::CustomerTotal),
            localization.format_currency(detail.total),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsBalance,
            ),
            localization.format_currency(detail.due),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsPaymentStatus,
            ),
            status_copy(localization, &detail.payment_status.to_string()),
        ),
    ];
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsNotes,
        ),
        Some(customer_return.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsCancellationReason,
        ),
        customer_return.cancel_reason.clone(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    // Product identity read THROUGH the parent sale line, exactly as the purchase
    // return's: a credit-note line stores no product.
    let lines: Vec<DrawerTableRow> = detail
        .lines
        .iter()
        .map(|line| {
            let parent_line = parent.lines.iter().find(|p| p.id == line.sale_line_id);
            DrawerTableRow {
                cells: vec![
                    parent_line
                        .map(|p| p.product_name.clone())
                        .unwrap_or_default(),
                    localization.format_quantity(line.qty),
                    localization.format_currency(line.unit_price),
                    localization.format_currency(line.subtotal()),
                ],
                href: None,
            }
        })
        .collect();

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: copy(
            localization,
            crate::localization::MessageKey::CustomerReturnsTitle,
        ),
        title: document_title(
            customer_return.credit_note_number.as_deref(),
            customer_return.id,
            localization,
        ),
        status_line: status_copy(localization, &customer_return.status.to_string()),
        facts,
        tables: vec![DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsLines,
            ),
            headers: vec![
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsProduct,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsQuantityShort,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::SalesUnitPrice,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxNetSubtotal,
                ),
            ],
            rows: lines,
        }],
        parent: Some(DrawerParent {
            label: copy(
                localization,
                crate::localization::MessageKey::CustomerReturnsParentSale,
            ),
            title: document_title(
                parent.sale.sale_number.as_deref(),
                parent.sale.id,
                localization,
            ),
            status_line: status_copy(localization, &parent.sale.status.to_string()),
            facts: vec![DrawerFact::new(
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsCustomer,
                ),
                &parent.sale.customer_name,
            )],
            href: format!("/sales/{}", parent.sale.id),
        }),
        actions: customer_return_actions(state, principal, &detail, localization).await?,
        notice: Some(edit_affordance_notice(
            &customer_return.status.to_string(),
            localization,
        )),
        links: vec![edit_affordance_link(
            &customer_return.status.to_string(),
            format!("/customer-returns/{}", customer_return.id),
            localization,
        )],
    })
}

/// The credit note's action block, mirroring
/// [`purchase_return_actions`] with `SalesCreate` / `SalesCancel` in place of the
/// purchases codes. The delete premise is the same one, and the same reason: a
/// draft is clean because `confirm` is ONE transaction.
async fn customer_return_actions(
    state: &AppState,
    principal: &Principal,
    detail: &crate::models::CustomerReturnDetail,
    localization: &LocalizationContext,
) -> AppResult<Vec<DrawerAction>> {
    let customer_return = &detail.customer_return;
    let mut actions = Vec::new();
    match customer_return.status {
        crate::models::CustomerReturnStatus::Draft => {
            if principal.has(SalesCreate::CODE) {
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    method: "delete".to_string(),
                    path: format!("/web/customer-returns/{}", customer_return.id),
                    fields: vec![],
                    reason: false,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsDeleteDraft,
                    ),
                    impact: vec![copy(
                        localization,
                        crate::localization::MessageKey::DocumentsNeverConfirmed,
                    )],
                    confirm: None,
                });
            }
        }
        crate::models::CustomerReturnStatus::Confirmed => {
            if principal.has(SalesCancel::CODE) {
                actions.push(DrawerAction {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::CustomerReturnsCancel,
                    ),
                    method: "post".to_string(),
                    path: format!("/web/customer-returns/{}/cancel", customer_return.id),
                    fields: vec![],
                    reason: true,
                    data_action: copy(
                        localization,
                        crate::localization::MessageKey::CustomerReturnsCancel,
                    ),
                    impact: vec![copy(
                        localization,
                        crate::localization::MessageKey::CustomerReturnsCancelConfirm,
                    )],
                    confirm: None,
                });
            }
        }
        crate::models::CustomerReturnStatus::Cancelled => {
            if customer_return.credit_note_number.is_none() && principal.has(SalesCreate::CODE) {
                actions.push(drawer_discard_delete(
                    format!("/web/customer-returns/{}", customer_return.id),
                    localization,
                ));
            }
        }
    }
    Ok(actions)
}

/// Resolve the actor display names in ONE `audit_actor_names` call over the
/// document's creator and (when it exists) its last editor. The fallback for
/// an id that resolves to nothing is the wiring layer's explicit marker, not a
/// blank row.
async fn actor_facts(
    state: &AppState,
    created_by: i64,
    updated_by: Option<i64>,
    localization: &LocalizationContext,
) -> AppResult<(DrawerFact, Option<DrawerFact>)> {
    let mut ids = vec![created_by];
    ids.extend(updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &ids).await?;
    let name_for = |id: i64| {
        names.get(&id).cloned().unwrap_or_else(|| {
            copy(
                localization,
                crate::localization::MessageKey::DocumentsSystem,
            )
        })
    };
    let created = DrawerFact::new(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsActor,
        ),
        name_for(created_by),
    );
    let updated = updated_by.map(|id| {
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::AuditUpdatedBy,
            ),
            name_for(id),
        )
    });
    Ok((created, updated))
}

/// The SALE family: the full record the `/sales/{id}` page renders as facts —
/// the product/account/method names are already resolved by `get_record` —
/// plus the action block, built only from real actions the principal may use.
async fn sale_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let record = state.sales_service.get_record(id).await?;
    let sale = &record.sale;
    let actions = sale_actions(state, principal, &record, localization).await?;
    let (created_by, updated_by) =
        actor_facts(state, sale.created_by, sale.updated_by, localization).await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsCustomer,
            ),
            &sale.customer_name,
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsPaymentType,
            ),
            payment_type_copy(localization, &sale.payment_type.to_string()),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(sale.sale_date),
        ),
    ];
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsDueDate,
        ),
        sale.due_date.map(|d| localization.format_date(d)),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsReceipt,
        ),
        sale.receipt_no.clone(),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsNotes,
        ),
        Some(sale.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsCancellationReason,
        ),
        sale.cancel_reason.clone(),
    ));
    // Net, tax and tax-inclusive total (tax calculation T2). The document's
    // money is three figures, not one: showing only the total would make it
    // unauditable, and the tax figure comes from the lines' frozen snapshots so
    // it cannot drift when a tax is edited later.
    facts.extend(document_money_facts(
        record.money,
        record.total_refusal,
        localization,
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![
        DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsLines,
            ),
            headers: vec![
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsProduct,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsQuantityShort,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsUnitPrice,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxNetSubtotal,
                ),
                copy(localization, crate::localization::MessageKey::TaxTotal),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxInclusiveTotal,
                ),
            ],
            rows: record
                .lines
                .iter()
                .map(|line| DrawerTableRow {
                    cells: vec![
                        line.product_name.clone(),
                        localization.format_quantity(line.qty),
                        localization.format_currency(line.unit_price),
                        localization.format_currency(line.subtotal),
                        localization.format_currency(line.tax_total),
                        localization.format_currency(line.total),
                    ],
                    href: None,
                })
                .collect(),
        },
        DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsPayments,
            ),
            headers: vec![
                copy(localization, crate::localization::MessageKey::DocumentsDate),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsAccount,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsMethod,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsAmount,
                ),
            ],
            rows: record
                .payments
                .iter()
                .map(|payment| DrawerTableRow {
                    cells: vec![
                        localization.format_date(payment.date),
                        payment.account_name.clone(),
                        localization
                            .payment_method_display_name(&payment.method_name)
                            .into_owned(),
                        localization.format_currency(payment.amount),
                    ],
                    href: None,
                })
                .collect(),
        },
    ];

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::Sale, localization),
        title: document_title(sale.sale_number.as_deref(), sale.id, localization),
        status_line: status_copy(localization, &sale.status.to_string()),
        facts,
        tables,
        parent: None,
        actions,
        notice: Some(edit_affordance_notice(
            &sale.status.to_string(),
            localization,
        )),
        links: vec![edit_affordance_link(
            &sale.status.to_string(),
            format!("/sales/{}", sale.id),
            localization,
        )],
    })
}

/// The SALE-PAYMENTS family: the payment's own facts plus the parent sale's
/// summary, the finance transactions it produced (the original Income and, on
/// a cancelled sale, the refund Expense — linked to the account page that owns
/// the ledger, never a second account-name read), and the receipt that grouped
/// it when one did.
async fn sale_payment_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let payment = state.sales_service.find_payment(id).await?;
    let record = state.sales_service.get_record(payment.sale_id).await?;
    let sale = &record.sale;
    let view = record
        .payments
        .iter()
        .find(|v| v.id == payment.id)
        .ok_or_else(|| {
            AppError::Internal(format!(
                "payment {} missing from its own sale's record",
                payment.id
            ))
        })?;
    let (created_by, updated_by) =
        actor_facts(state, payment.created_by, payment.updated_by, localization).await?;

    // The ledger links: the account page owns the transaction's name and
    // balance, so the drawer links there instead of re-reading an account.
    // The account page declares `finance.read`, which a payment reader does
    // not necessarily hold, so the link renders only for a finance reader —
    // the entry itself stays a fact below either way.
    let mut links = Vec::new();
    let original = match payment.transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            if principal.has(FinanceRead::CODE) {
                links.push(DrawerLink {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsViewLedger,
                    ),
                    href: format!("/accounts/{}", tx.account_id),
                });
            }
            Some(tx)
        }
        None => None,
    };
    let refund = match payment.refund_transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            if principal.has(FinanceRead::CODE) {
                links.push(DrawerLink {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsViewRefund,
                    ),
                    href: format!("/accounts/{}", tx.account_id),
                });
            }
            Some(tx)
        }
        None => None,
    };
    // The receipt grouped this payment; the customer page declares
    // `customers.read`, so the link obeys the same rule. The receipt NUMBER
    // stays a fact below either way.
    if payment.receipt_id.is_some() && principal.has(CustomersRead::CODE) {
        links.push(DrawerLink {
            label: copy(
                localization,
                crate::localization::MessageKey::DocumentsViewCustomer,
            ),
            href: format!("/customers/{}", sale.customer_id),
        });
    }
    links.push(DrawerLink {
        label: copy(
            localization,
            crate::localization::MessageKey::DocumentsOpenSales,
        ),
        href: format!("/sales/{}", sale.id),
    });

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAmount,
            ),
            localization.format_currency(payment.amount),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(payment.date),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAccount,
            ),
            &view.account_name,
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsMethod,
            ),
            &localization
                .payment_method_display_name(&view.method_name)
                .into_owned(),
        ),
    ];
    if let Some(tx) = &original {
        facts.push(DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsEntry,
            ),
            format!(
                "{} · {} · {}",
                transaction_kind_copy(localization, &tx.kind.to_string()),
                localization.format_currency(tx.amount),
                localization.format_date(tx.date),
            ),
        ));
    }
    if let Some(tx) = &refund {
        facts.push(DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsRefund,
            ),
            format!(
                "{} · {} · {}",
                transaction_kind_copy(localization, &tx.kind.to_string()),
                localization.format_currency(tx.amount),
                localization.format_date(tx.date),
            ),
        ));
    }
    if let Some(receipt_id) = payment.receipt_id {
        facts.push(DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsReceipt,
            ),
            format!(
                "{} #{receipt_id}",
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsReceipt
                )
            ),
        ));
    }
    facts.push(created_by);
    facts.extend(updated_by);

    let parent = DrawerParent {
        label: document_kind_label(&DocumentKind::Sale, localization),
        title: document_title(sale.sale_number.as_deref(), sale.id, localization),
        status_line: status_copy(localization, &sale.status.to_string()),
        facts: payment_money_facts(record.money, record.total_refusal, localization),
        href: format!("/sales/{}", sale.id),
    };

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::SalePayment, localization),
        title: localization.tr_with(
            crate::localization::MessageKey::DocumentsPaymentOf,
            &[(
                "title",
                document_title(sale.sale_number.as_deref(), sale.id, localization).as_str(),
            )],
        ),
        status_line: localization.tr_with(
            crate::localization::MessageKey::DocumentsPaymentStatusLine,
            &[
                ("date", localization.format_date(payment.date).as_str()),
                (
                    "status",
                    status_copy(localization, &sale.status.to_string()).as_str(),
                ),
            ],
        ),
        facts,
        tables: Vec::new(),
        parent: Some(parent),
        actions: Vec::new(),
        notice: Some(copy(
            localization,
            crate::localization::MessageKey::DocumentsPaymentImmutable,
        )),
        links,
    })
}

/// The PURCHASE family: the mirror of the sale drawer with supplier, supplier
/// invoice, unit costs — and the purchase-family action block.
async fn purchase_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let record = state.purchases_service.get_record(id).await?;
    let purchase = &record.purchase;
    let actions = purchase_actions(state, principal, &record, localization).await?;
    let (created_by, updated_by) = actor_facts(
        state,
        purchase.created_by,
        purchase.updated_by,
        localization,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsSupplier,
            ),
            &record.supplier_name,
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsPaymentType,
            ),
            payment_type_copy(localization, &purchase.payment_type.to_string()),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(purchase.purchase_date),
        ),
    ];
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsDueDate,
        ),
        purchase.due_date.map(|d| localization.format_date(d)),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsSupplierInvoice,
        ),
        purchase.supplier_invoice_no.clone(),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsNotes,
        ),
        Some(purchase.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsCancellationReason,
        ),
        purchase.cancel_reason.clone(),
    ));
    // Net, tax and tax-inclusive total (tax calculation T2). The document's
    // money is three figures, not one: showing only the total would make it
    // unauditable, and the tax figure comes from the lines' frozen snapshots so
    // it cannot drift when a tax is edited later.
    facts.extend(document_money_facts(
        record.money,
        record.total_refusal,
        localization,
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![
        DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsLines,
            ),
            headers: vec![
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsProduct,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsQuantityShort,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsUnitCost,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxNetSubtotal,
                ),
                copy(localization, crate::localization::MessageKey::TaxTotal),
                copy(
                    localization,
                    crate::localization::MessageKey::TaxInclusiveTotal,
                ),
            ],
            rows: record
                .lines
                .iter()
                .map(|line| DrawerTableRow {
                    cells: vec![
                        line.product_name.clone(),
                        localization.format_quantity(line.qty),
                        localization.format_currency(line.unit_cost),
                        localization.format_currency(line.subtotal),
                        localization.format_currency(line.tax_total),
                        localization.format_currency(line.total),
                    ],
                    href: None,
                })
                .collect(),
        },
        DrawerTable {
            title: copy(
                localization,
                crate::localization::MessageKey::DocumentsPayments,
            ),
            headers: vec![
                copy(localization, crate::localization::MessageKey::DocumentsDate),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsAccount,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsMethod,
                ),
                copy(
                    localization,
                    crate::localization::MessageKey::DocumentsAmount,
                ),
            ],
            rows: record
                .payments
                .iter()
                .map(|payment| DrawerTableRow {
                    cells: vec![
                        localization.format_date(payment.date),
                        payment.account_name.clone(),
                        localization
                            .payment_method_display_name(&payment.method_name)
                            .into_owned(),
                        localization.format_currency(payment.amount),
                    ],
                    href: None,
                })
                .collect(),
        },
    ];

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::Purchase, localization),
        title: document_title(
            purchase.purchase_number.as_deref(),
            purchase.id,
            localization,
        ),
        status_line: status_copy(localization, &purchase.status.to_string()),
        facts,
        tables,
        parent: None,
        actions,
        notice: Some(edit_affordance_notice(
            &purchase.status.to_string(),
            localization,
        )),
        links: vec![edit_affordance_link(
            &purchase.status.to_string(),
            format!("/purchases/{}", purchase.id),
            localization,
        )],
    })
}

/// The PURCHASE-PAYMENTS family: the mirror of the sale-payment drawer with
/// `purchase-changed`-family links.
async fn purchase_payment_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let payment = state.purchases_service.find_payment(id).await?;
    let record = state
        .purchases_service
        .get_record(payment.purchase_id)
        .await?;
    let purchase = &record.purchase;
    let view = record
        .payments
        .iter()
        .find(|v| v.id == payment.id)
        .ok_or_else(|| {
            AppError::Internal(format!(
                "payment {} missing from its own purchase's record",
                payment.id
            ))
        })?;
    let (created_by, updated_by) =
        actor_facts(state, payment.created_by, payment.updated_by, localization).await?;

    // The ledger links, gated like the sale side: `/accounts/{id}` declares
    // `finance.read`, so the link renders only for a finance reader while the
    // entry stays a fact below either way.
    let mut links = Vec::new();
    let original = match payment.transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            if principal.has(FinanceRead::CODE) {
                links.push(DrawerLink {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsViewLedger,
                    ),
                    href: format!("/accounts/{}", tx.account_id),
                });
            }
            Some(tx)
        }
        None => None,
    };
    let refund = match payment.refund_transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            if principal.has(FinanceRead::CODE) {
                links.push(DrawerLink {
                    label: copy(
                        localization,
                        crate::localization::MessageKey::DocumentsViewRefund,
                    ),
                    href: format!("/accounts/{}", tx.account_id),
                });
            }
            Some(tx)
        }
        None => None,
    };
    links.push(DrawerLink {
        label: copy(
            localization,
            crate::localization::MessageKey::DocumentsOpenPurchases,
        ),
        href: format!("/purchases/{}", purchase.id),
    });

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAmount,
            ),
            localization.format_currency(payment.amount),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(payment.date),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAccount,
            ),
            &view.account_name,
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsMethod,
            ),
            &localization
                .payment_method_display_name(&view.method_name)
                .into_owned(),
        ),
    ];
    if let Some(tx) = &original {
        facts.push(DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsEntry,
            ),
            format!(
                "{} · {} · {}",
                transaction_kind_copy(localization, &tx.kind.to_string()),
                localization.format_currency(tx.amount),
                localization.format_date(tx.date),
            ),
        ));
    }
    if let Some(tx) = &refund {
        facts.push(DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsRefund,
            ),
            format!(
                "{} · {} · {}",
                transaction_kind_copy(localization, &tx.kind.to_string()),
                localization.format_currency(tx.amount),
                localization.format_date(tx.date),
            ),
        ));
    }
    facts.push(created_by);
    facts.extend(updated_by);

    let parent = DrawerParent {
        label: document_kind_label(&DocumentKind::Purchase, localization),
        title: document_title(
            purchase.purchase_number.as_deref(),
            purchase.id,
            localization,
        ),
        status_line: status_copy(localization, &purchase.status.to_string()),
        facts: payment_money_facts(record.money, record.total_refusal, localization),
        href: format!("/purchases/{}", purchase.id),
    };

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::PurchasePayment, localization),
        title: localization.tr_with(
            crate::localization::MessageKey::DocumentsPaymentOf,
            &[(
                "title",
                document_title(
                    purchase.purchase_number.as_deref(),
                    purchase.id,
                    localization,
                )
                .as_str(),
            )],
        ),
        status_line: localization.tr_with(
            crate::localization::MessageKey::DocumentsPaymentStatusLine,
            &[
                ("date", localization.format_date(payment.date).as_str()),
                (
                    "status",
                    status_copy(localization, &purchase.status.to_string()).as_str(),
                ),
            ],
        ),
        facts,
        tables: Vec::new(),
        parent: Some(parent),
        actions: Vec::new(),
        notice: Some(copy(
            localization,
            crate::localization::MessageKey::DocumentsPaymentImmutable,
        )),
        links,
    })
}

/// The STOCK-MOVEMENT family: the movement's facts plus the product's current
/// derived stock. The movement is append-only history — no edit, no delete —
/// and the drawer says so; the action slice builds on that guarantee.
async fn stock_movement_drawer(
    state: &AppState,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let movement = state.inventory_service.get_movement(id).await?;
    let stock = state
        .inventory_service
        .product_stock(movement.product_id)
        .await?;
    let (created_by, updated_by) = actor_facts(
        state,
        movement.created_by,
        movement.updated_by,
        localization,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsProduct,
            ),
            format!("{} ({})", stock.product.name, stock.product.sku),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::ProductKind),
            movement_type_copy(localization, &movement.movement_type.to_string()),
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::ProductReason),
            reason_copy(localization, &movement.reason.to_string()),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::ProductQuantity,
            ),
            localization.format_quantity(movement.qty),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::ProductMovementReference,
            ),
            &movement.reference,
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(movement.date),
        ),
        // The product's CURRENT level is a set sum over its movements, so it
        // states the rule in this fact's place when it refused — through the same
        // one renderer the drawer uses for every other refusal. The movement's OWN
        // quantity above is a single bounded write and stays a figure either way.
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::ProductStock),
            match stock.stock.amount {
                Some(level) => localization.format_quantity(level),
                None => stock
                    .stock
                    .refusal
                    .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                    .unwrap_or_default(),
            },
        ),
    ];
    facts.push(created_by);
    facts.extend(updated_by);

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::StockMovement, localization),
        title: localization.tr_with(
            crate::localization::MessageKey::DocumentsMovement,
            &[("id", &movement.id.to_string())],
        ),
        status_line: format!(
            "{} · {}",
            movement_type_copy(localization, &movement.movement_type.to_string()),
            reason_copy(localization, &movement.reason.to_string())
        ),
        facts,
        tables: Vec::new(),
        parent: None,
        actions: Vec::new(),
        notice: Some(copy(
            localization,
            crate::localization::MessageKey::DocumentsMovementImmutable,
        )),
        links: vec![DrawerLink {
            label: copy(
                localization,
                crate::localization::MessageKey::DocumentsViewProduct,
            ),
            href: format!("/products#product-{}", movement.product_id),
        }],
    })
}

/// The RECEIPTS family: the collection's facts plus the payments it grouped —
/// each allocation names its sale the way the operator does (the receipt read
/// resolves the sale numbers) and links to the sale it applied to, the link
/// only for a principal holding the `sales.read` the sale page declares (the
/// number stays as text for a reader without it).
async fn receipt_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
    localization: &LocalizationContext,
) -> AppResult<DocumentDetailPartial> {
    let detail = state.customer_receipt_service.get_receipt(id).await?;
    let customer = state
        .customer_service
        .get_customer(detail.receipt.customer_id)
        .await?;
    let (created_by, updated_by) = actor_facts(
        state,
        detail.receipt.created_by,
        detail.receipt.updated_by,
        localization,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsCustomer,
            ),
            &customer.name,
        ),
        DrawerFact::new(
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            localization.format_date(detail.receipt.date),
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAccount,
            ),
            &detail.account_name,
        ),
        DrawerFact::new(
            copy(
                localization,
                crate::localization::MessageKey::DocumentsMethod,
            ),
            &localization
                .payment_method_display_name(&detail.method_name)
                .into_owned(),
        ),
    ];
    facts.extend(DrawerFact::when_non_empty(
        copy(
            localization,
            crate::localization::MessageKey::DocumentsNotes,
        ),
        detail.receipt.notes.clone(),
    ));
    facts.push(DrawerFact::new(
        copy(localization, crate::localization::MessageKey::CustomerTotal),
        localization.format_currency(detail.total),
    ));
    facts.push(DrawerFact::new(
        copy(
            localization,
            crate::localization::MessageKey::CustomerAllocations,
        ),
        detail.allocations.len().to_string(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![DrawerTable {
        title: copy(
            localization,
            crate::localization::MessageKey::CustomerAllocations,
        ),
        headers: vec![
            copy(localization, crate::localization::MessageKey::DocumentSale),
            copy(localization, crate::localization::MessageKey::DocumentsDate),
            copy(
                localization,
                crate::localization::MessageKey::DocumentsAmount,
            ),
        ],
        rows: detail
            .allocations
            .iter()
            .map(|payment| DrawerTableRow {
                cells: vec![
                    payment.sale_number.clone().unwrap_or_else(|| {
                        localization.tr_with(
                            crate::localization::MessageKey::CustomerSaleNumber,
                            &[("id", &payment.sale_id.to_string())],
                        )
                    }),
                    localization.format_date(payment.date),
                    localization.format_currency(payment.amount),
                ],
                // The sale page declares `sales.read`; the receipt drawer
                // opens with `customers.read`, so the link renders only for
                // a principal holding both. The number stays as text.
                href: if principal.has(SalesRead::CODE) {
                    Some(format!("/sales/{}", payment.sale_id))
                } else {
                    None
                },
            })
            .collect(),
    }];

    Ok(DocumentDetailPartial {
        localization: localization.clone(),
        kind_label: document_kind_label(&DocumentKind::Receipt, localization),
        title: format!(
            "{} #{}",
            copy(
                localization,
                crate::localization::MessageKey::DocumentsReceipt
            ),
            detail.receipt.id
        ),
        status_line: copy(
            localization,
            crate::localization::MessageKey::DocumentsCollection,
        ),
        facts,
        tables,
        parent: None,
        actions: Vec::new(),
        notice: Some(copy(
            localization,
            crate::localization::MessageKey::DocumentsReceiptNotice,
        )),
        links: vec![DrawerLink {
            label: copy(
                localization,
                crate::localization::MessageKey::DocumentsOpenCustomers,
            ),
            href: format!("/customers/{}", detail.receipt.customer_id),
        }],
    })
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

    async fn test_state() -> AppState {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool: the walk-in triggers fire like
            // they do in production.
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES ('es-ES', 'es', 'Español (España)', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO business_settings (id, business_name, default_locale_code, currency_code, timezone) VALUES (1, 'Test', 'es-ES', 'USD', 'UTC')")
            .execute(&pool)
            .await
            .unwrap();
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    /// The same state with `allow_negative = true`: the purchase mirror test
    /// confirms a CASH purchase whose Expense posts from a zero balance, and
    /// that refusal belongs to finance's own tests, not this one.
    async fn test_state_allow_negative() -> AppState {
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
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES ('es-ES', 'es', 'Español (España)', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO business_settings (id, business_name, default_locale_code, currency_code, timezone) VALUES (1, 'Test', 'es-ES', 'USD', 'UTC')")
            .execute(&pool)
            .await
            .unwrap();
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, true, true)
    }

    async fn audit_actor(state: &AppState) -> i64 {
        test_support::audit_actor_id(&state.pool).await.unwrap()
    }

    async fn get_drawer(app: axum::Router, uri: &str, cookie: &str) -> (StatusCode, String) {
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

    /// One tracked product + one cash account with its Cash method, the
    /// minimum a sale drawer test needs to reach Draft and Confirmed states.
    async fn seed_sale_kit(state: &AppState) -> (i64, i64, i64) {
        use crate::models::NewProduct;
        use rust_decimal::Decimal;

        let actor = audit_actor(state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-S".into(),
                    name: "Drawer product".into(),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price: Decimal::from(10),
                    // Tracked products require min/max stock in this shop's
                    // rules, even in a fixture.
                    track_stock: true,
                    min_stock: Some(rust_decimal::Decimal::ZERO),
                    max_stock: Some(rust_decimal::Decimal::from(100)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        let account = state.account_service.create(actor, "Caja").await.unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(actor, account.id, "Caja")
            .await
            .unwrap();
        // The account's OWN Cash: a method is usable only while an account
        // owns it, so the fixture picks the one `ensure_defaults_for_account`
        // just attached.
        let method = state
            .payment_method_service
            .catalog_for_account(account.id)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("the account defaults include Cash");
        (product.id, account.id, method.id)
    }

    // -----------------------------------------------------------------------
    // Document-level accumulation (tax contract overflow T3).
    //
    // The index is the FOURTH surface, and it is the one with a different
    // failure shape: `list_document_rows` folds every document's lines into one
    // amount per document with the same raw `+=`, so one document that cannot be
    // added up takes the WHOLE PAGE down with it. Every other document in the
    // shop's history is on this page too, and they have nothing to do with it.
    //
    // The document is built the real way — the same checked line write the
    // operator's form reaches, at `4e28`, twice — so the two lines are each
    // individually carryable and each is stored, and the sum `8e28` is not.
    // -----------------------------------------------------------------------

    /// `4e28`: individually carryable (`Decimal::MAX ≈ 7.92e28`), and two of
    /// them are `8e28`, which the `Decimal` range does not hold.
    const FOUR_E28: &str = "40000000000000000000000000000";

    /// A draft sale carrying two lines of `4e28` and nothing else.
    async fn seed_untotalable_sale(state: &AppState) -> i64 {
        use crate::models::{NewProduct, NewSale};
        use rust_decimal::Decimal;

        let actor = audit_actor(state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DOC-TOTAL-IDX".into(),
                    name: "Document total product".into(),
                    kind: crate::models::ProductKind::Product,
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
        let customer = state
            .customer_service
            .list_customers(true)
            .await
            .unwrap()
            .into_iter()
            .find(|customer| customer.is_walkin)
            .expect("the walk-in is seeded by migrations");
        let sale = state
            .sales_service
            .create_draft(
                actor,
                NewSale {
                    customer_id: customer.id,
                    payment_type: crate::models::PaymentType::Cash,
                    sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let unit_price = Decimal::from_str(FOUR_E28).unwrap();
        for _ in 0..2 {
            state
                .sales_service
                .add_line(sale.id, product.id, Decimal::from(1), Some(unit_price))
                .await
                .unwrap();
        }
        sale.id
    }

    /// THE INDEX ANSWERS. One document whose lines cannot be added up must not
    /// cost the operator the rest of the page: the document is listed, the
    /// refusal is stated on the row, and every other document is still there.
    #[tokio::test]
    async fn the_index_lists_a_document_whose_lines_cannot_be_added_up() {
        let state = test_state().await;
        let untotalable = seed_untotalable_sale(&state).await;
        let (product, _, method) = seed_sale_kit(&state).await;
        let ordinary = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Cash,
            Some(method),
        )
        .await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();

        let (status, html) = get_drawer(app.clone(), "/documents", test_support::TEST_COOKIE).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "one document that cannot be totaled must not take the index down: {html:.800}"
        );
        assert!(
            html.contains(&format!("Draft #{untotalable}")),
            "the document is listed: an operator who cannot see it cannot act on it: {html:.2000}"
        );
        assert!(
            html.contains("data-document-total-refusal"),
            "and its row states the refusal: {html:.2000}"
        );
        assert!(
            html.contains(&expected),
            "in the operator's own language, through the one shared mapping: {html:.2000}"
        );
        assert!(
            html.contains(&format!("{ordinary}")),
            "and the rest of the page is unaffected: {html:.2000}"
        );

        // The drawer for that same document answers too: it is the fourth
        // surface's own detail view, and it states the same sentence.
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{untotalable}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains(&expected),
            "the drawer states the same refusal instead of publishing a total that does not \
             exist: {html:.2000}"
        );
        assert!(
            !html.contains("data-document-total-refusal-line"),
            "and it does not restate the refusal on every line: {html:.2000}"
        );
    }

    async fn seed_sale(state: &AppState, product_id: i64, confirm: bool) -> i64 {
        seed_sale_typed(
            state,
            product_id,
            confirm,
            crate::models::PaymentType::Cash,
            None,
        )
        .await
    }

    /// The same fixture with an explicit payment type: the receipt family test
    /// needs a CREDIT sale, because a collection never exceeds the customer's
    /// outstanding debt and only credit creates one.
    async fn seed_sale_typed(
        state: &AppState,
        product_id: i64,
        confirm: bool,
        payment_type: crate::models::PaymentType,
        method_id: Option<i64>,
    ) -> i64 {
        use crate::models::NewSale;
        use rust_decimal::Decimal;

        let actor = audit_actor(state).await;
        let customer = state
            .customer_service
            .create_customer(
                actor,
                crate::models::NewCustomer {
                    name: "Drawer Buyer".into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: None,
                    due_days: None,
                },
            )
            .await
            .unwrap()
            .customer;
        let sale = state
            .sales_service
            .create_draft(
                actor,
                NewSale {
                    customer_id: customer.id,
                    payment_type,
                    sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    // Credit requires a due date; Cash requires none.
                    due_date: match payment_type {
                        crate::models::PaymentType::Credit => {
                            Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap())
                        }
                        crate::models::PaymentType::Cash => None,
                    },
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .sales_service
            .add_line(sale.id, product_id, Decimal::from(2), None)
            .await
            .unwrap();
        if confirm {
            // A cash confirm carries the method; a credit one never does.
            let confirm_method = match payment_type {
                crate::models::PaymentType::Cash => method_id,
                crate::models::PaymentType::Credit => None,
            };
            state
                .sales_service
                .confirm(actor, sale.id, confirm_method)
                .await
                .unwrap();
        }
        sale.id
    }

    /// One draft sale seen by three principals: the drawer renders each action
    /// ONLY for the code its endpoint requires — "Eliminar borrador" needs
    /// `sales.create` and "Descartar" needs `sales.cancel` — while the edit
    /// affordance (a LINK to the record page, never a duplicated form) stays
    /// for every reader.
    #[tokio::test]
    async fn document_drawer_draft_sale_shows_each_action_only_for_its_code() {
        let state = test_state().await;
        let (product, _, _) = seed_sale_kit(&state).await;
        let sale = seed_sale(&state, product, false).await;
        let app = crate::routes::router(state.clone());
        let uri = format!("/web/documents/detail/sale/{sale}");

        // Full permission: both actions plus the state-labelled edit link.
        let (status, html) = get_drawer(app.clone(), &uri, test_support::TEST_COOKIE).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Eliminar borrador"), "{html:.800}");
        assert!(html.contains("Descartar"), "{html:.800}");
        assert!(
            html.contains("Editar documento"),
            "the edit affordance is a button-styled link: {html:.800}"
        );
        assert!(
            !html.contains("Abrir el documento"),
            "the label is state-dependent: a draft edits, a confirmed one opens — the two states name one document differently: {html:.800}"
        );
        assert!(
            html.contains(&format!("hx-delete=\"/web/sales/{sale}\"")),
            "{html:.800}"
        );
        assert!(
            html.contains("hx-post=\"/web/sales/cancel\""),
            "{html:.800}"
        );
        assert!(
            html.contains("Nunca se confirmó"),
            "the delete impact must say what a draft never did: {html:.800}"
        );

        // Reader only: no action buttons at all, but the edit link stays.
        let reader = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, html) =
            get_drawer(app.clone(), &uri, &test_support::cookie_for(&reader)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("Eliminar borrador"), "{html:.800}");
        assert!(!html.contains("Descartar"), "{html:.800}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(
            html.contains("Editar documento"),
            "the edit affordance is a LINK every reader keeps, whatever their code: {html:.800}"
        );

        // Cancel permission without create: "Descartar" yes, delete no.
        let canceller = test_support::seed_session_with_permissions(
            &state.pool,
            &["sales.read", "sales.cancel"],
        )
        .await
        .unwrap();
        let (status, html) = get_drawer(app, &uri, &test_support::cookie_for(&canceller)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Descartar"), "{html:.800}");
        assert!(!html.contains("Eliminar borrador"), "{html:.800}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
    }

    /// A confirmed sale offers Anular (never the draft delete) and the impact
    /// preview lists EXACTLY what cancel will create: one `In · Sale-return`
    /// movement per tracked line, one `Expense` refund per payment, and the
    /// state change that stops the customer debt. This state was built with
    /// `allow_negative = false`, so the negative-balance caveat renders too.
    #[tokio::test]
    async fn document_drawer_confirmed_sale_offers_anular_and_lists_the_impact() {
        let state = test_state().await;
        let (product, account, method) = seed_sale_kit(&state).await;
        // A CASH confirm pays in full with the method it carries: the drawer
        // then previews exactly one refund Expense for that payment.
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Cash,
            Some(method),
        )
        .await;
        let app = crate::routes::router(state);

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{sale}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Anular"), "{html:.800}");
        assert!(
            !html.contains("Eliminar borrador"),
            "a confirmed document is annulled, never deleted: {html:.800}"
        );
        assert!(
            html.contains("Entrada · Devolución de venta"),
            "one movement per tracked line: {html:.800}"
        );
        assert!(
            html.contains("Drawer product"),
            "the movement line names the product: {html:.800}"
        );
        assert!(
            html.contains("asiento Expense"),
            "one refund per payment: {html:.800}"
        );
        assert!(
            html.contains("Caja"),
            "the refund line names the account: {html:.800}"
        );
        assert!(
            html.contains("deja de contar como deuda del cliente"),
            "{html:.800}"
        );
        assert!(
            html.contains("Si algún reembolso deja una cuenta en negativo"),
            "allow_negative = false, so the caveat renders: {html:.800}"
        );
        let _ = account;
    }

    /// The same pre-condition check `cancel` refuses on — an inactive
    /// tracked product — is surfaced BEFORE the operator presses the button:
    /// the action stays rendered, and the preview says it will be refused.
    #[tokio::test]
    async fn document_drawer_confirmed_sale_warns_about_an_inactive_product_before_the_refusal() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Cash,
            Some(method),
        )
        .await;
        state
            .inventory_service
            .set_product_active(audit_actor(&state).await, product, false)
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{sale}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("no se puede anular"),
            "the blocker must be visible before the click: {html:.800}"
        );
        assert!(html.contains("está inactivo"), "{html:.800}");
        assert!(
            html.contains("Anular"),
            "the action is NOT hidden: the refusal path is still the operator's path"
        );
    }

    /// A cancelled document offers no action at all and says why: the document
    /// is annulled, its inverse already happened. The fixture confirms FIRST,
    /// so its number marks it confirmed-then-cancelled — the state that must
    /// stay actionless even after discarded sales gained their delete.
    #[tokio::test]
    async fn document_drawer_confirmed_then_cancelled_sale_offers_no_action() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Cash,
            Some(method),
        )
        .await;
        state
            .sales_service
            .cancel(
                audit_actor(&state).await,
                sale,
                Some("test annulment".into()),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{sale}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(!html.contains("hx-post"), "{html:.800}");
        assert!(
            html.contains("anulado"),
            "the drawer must say the document is annulled: {html:.800}"
        );
    }

    /// T3: the drawer — home of the existing draft delete — offers the delete
    /// for a DISCARDED sale (Cancelled while never confirmed: no number)
    /// and nothing for a confirmed-then-cancelled one, whose number proves it
    /// must stay as audit trail.
    #[tokio::test]
    async fn document_drawer_discarded_sale_offers_delete_but_annulled_does_not() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;

        // Discarded: cancelled before confirm, number stays NULL → delete renders.
        let discarded_id = seed_sale(&state, product, false).await;
        state
            .sales_service
            .cancel(audit_actor(&state).await, discarded_id, None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/sale/{discarded_id}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("hx-delete=\"/web/sales/{discarded_id}\"")),
            "a discarded sale must offer its delete: {html:.800}"
        );
        assert!(
            html.contains("hx-confirm"),
            "the delete must ask first: {html:.800}"
        );

        // Confirmed then cancelled: the number proves it → NO delete renders.
        let annulled_id = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Cash,
            Some(method),
        )
        .await;
        state
            .sales_service
            .cancel(
                audit_actor(&state).await,
                annulled_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{annulled_id}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains(&format!("hx-delete=\"/web/sales/{annulled_id}\"")),
            "a confirmed-then-cancelled sale must offer no delete: {html:.800}"
        );
    }

    /// The families without a real action show NO button — not even a dead
    /// one — and say why instead: a payment's money is already in the ledger,
    /// a movement is append-only history (compensate with an adjustment), a
    /// receipt groups payments the database refuses to orphan.
    #[tokio::test]
    async fn document_drawer_payment_movement_and_receipt_families_render_no_action() {
        let state = test_state().await;
        let (product, account, method) = seed_sale_kit(&state).await;
        let actor = audit_actor(&state).await;

        // A paid confirmed sale gives the payment family its row.
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Credit,
            None,
        )
        .await;
        let payment = state
            .sales_service
            .record_payment(
                actor,
                sale,
                method,
                rust_decimal::Decimal::from(10),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 3).unwrap(),
            )
            .await
            .unwrap();
        let movement = state
            .inventory_service
            .record_movement(
                actor,
                crate::models::NewMovement {
                    product_id: product,
                    qty: rust_decimal::Decimal::from(3),
                    movement_type: crate::models::MovementType::Adjust,
                    reason: crate::models::MovementReason::Adjust,
                    reference: "stocktake".into(),
                    date: chrono::NaiveDate::from_ymd_opt(2024, 5, 4).unwrap(),
                },
            )
            .await
            .unwrap();
        // The receipt needs a collection through the web form (customer debt
        // from the confirmed Credit path is not required for the drawer test:
        // any receipt id renders the same family shape).
        let app = crate::routes::router(state.clone());
        let _ = account;

        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/sale_payment/{}", payment.id),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(!html.contains("hx-post"), "{html:.800}");
        assert!(
            html.contains("El pago no se edita"),
            "the payment drawer explains why there is no action: {html:.800}"
        );

        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/stock_movement/{}", movement.id),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(!html.contains("hx-post"), "{html:.800}");
        assert!(
            html.contains("Ajuste"),
            "the movement drawer points at the compensation path: {html:.800}"
        );

        // A receipt for the paid sale: collect through the real web endpoint.
        let body = format!(
            "customer_id={}&method_id={method}&amount=1&date=2024-05-05",
            state
                .sales_service
                .get_detail(sale)
                .await
                .unwrap()
                .sale
                .customer_id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/customer-receipts")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (receipt_id,): (i64,) =
            sqlx::query_as("SELECT id FROM customer_receipts ORDER BY id DESC LIMIT 1")
                .fetch_one(&state.pool)
                .await
                .unwrap();

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/receipt/{receipt_id}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(!html.contains("hx-post"), "{html:.800}");
        assert!(
            html.contains("agrupa"),
            "the receipt drawer explains why it cannot be deleted: {html:.800}"
        );
    }

    /// The link rule on the payment family: a link renders ONLY when the
    /// principal holds the code its target route declares, so no link is a
    /// dead end. The ledger links point at `/accounts/{id}`, a page that
    /// declares `finance.read`, and the receipt's customer link points at
    /// `/customers/{id}`, which declares `customers.read` — but the drawer
    /// itself opens with `sales.read`. The FACTS never hide: the ledger
    /// entry and the receipt number stay readable as text.
    #[tokio::test]
    async fn document_drawer_payment_links_render_only_for_the_code_the_target_route_declares() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;
        // A credit sale carries debt, so a collection can apply to it; the
        // receipt then creates the payment it groups (with `receipt_id` set),
        // exactly how production links a payment to its receipt.
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Credit,
            None,
        )
        .await;
        let customer_id = state
            .sales_service
            .get_detail(sale)
            .await
            .unwrap()
            .sale
            .customer_id;
        let app = crate::routes::router(state.clone());
        let body = format!("customer_id={customer_id}&method_id={method}&amount=1&date=2024-05-05");
        let req = Request::builder()
            .method("POST")
            .uri("/web/customer-receipts")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "seed receipt");
        let (payment,): (i64,) = sqlx::query_as(
            "SELECT id FROM sale_payments WHERE receipt_id IS NOT NULL ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();

        let uri = format!("/web/documents/detail/sale_payment/{}", payment);

        // Full permission: every link renders, the ledger's included.
        let (status, html) = get_drawer(app.clone(), &uri, test_support::TEST_COOKIE).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Ver asiento en Caja"), "{html:.800}");
        assert!(html.contains("/accounts/"), "{html:.800}");
        assert!(html.contains("Ver cliente del recibo"), "{html:.800}");
        assert!(html.contains("Recibo #"), "the receipt fact: {html:.800}");

        // `sales.read` only: the drawer still opens, but no link may point
        // where this principal would be refused — while the facts stay.
        let reader = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, html) = get_drawer(app, &uri, &test_support::cookie_for(&reader)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("/accounts/"),
            "the account page declares finance.read: {html:.800}"
        );
        assert!(!html.contains("Ver asiento en Caja"), "{html:.800}");
        assert!(!html.contains("Ver cliente del recibo"), "{html:.800}");
        assert!(
            html.contains("Asiento"),
            "the ledger entry stays readable as a fact: {html:.800}"
        );
        assert!(
            html.contains("Ingreso"),
            "the entry's kind stays visible: {html:.800}"
        );
        assert!(
            html.contains("Recibo #"),
            "the receipt number stays readable as a fact: {html:.800}"
        );
        assert!(
            html.contains("Abrir en Ventas"),
            "the sale page declares only sales.read, which this principal holds: {html:.800}"
        );
    }

    /// The link rule on the receipt family: each allocation row names its
    /// sale as text, but the row links to `/sales/{sale_id}` — a page that
    /// declares `sales.read` — ONLY when the principal holds that code,
    /// because the receipt drawer itself opens with `customers.read`.
    #[tokio::test]
    async fn document_drawer_receipt_allocation_links_render_only_for_sales_read() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;
        let sale = seed_sale_typed(
            &state,
            product,
            true,
            crate::models::PaymentType::Credit,
            None,
        )
        .await;
        let detail = state.sales_service.get_detail(sale).await.unwrap();
        let customer_id = detail.sale.customer_id;
        let sale_number = detail
            .sale
            .sale_number
            .expect("a confirmed sale has a number");
        let app = crate::routes::router(state.clone());
        let body = format!("customer_id={customer_id}&method_id={method}&amount=1&date=2024-05-05");
        let req = Request::builder()
            .method("POST")
            .uri("/web/customer-receipts")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "seed receipt");
        let (receipt_id,): (i64,) =
            sqlx::query_as("SELECT id FROM customer_receipts ORDER BY id DESC LIMIT 1")
                .fetch_one(&state.pool)
                .await
                .unwrap();

        let uri = format!("/web/documents/detail/receipt/{receipt_id}");

        // Full permission: the allocation's sale is a link to its page.
        let (status, html) = get_drawer(app.clone(), &uri, test_support::TEST_COOKIE).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("href=\"/sales/{}\"", sale)),
            "the allocation links the sale it applied to: {html:.800}"
        );
        assert!(html.contains(&sale_number), "{html:.800}");

        // `customers.read` only: the rows and their sale numbers stay, the
        // links do not.
        let reader = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let (status, html) = get_drawer(app, &uri, &test_support::cookie_for(&reader)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("/sales/"),
            "the sale page declares sales.read: {html:.800}"
        );
        assert!(
            html.contains(&sale_number),
            "the allocation keeps its sale number as text: {html:.800}"
        );
        assert!(
            html.contains("Asignaciones"),
            "the table still renders: {html:.800}"
        );
    }

    /// T3: the drawer — home of the existing draft delete — offers the delete
    /// for a DISCARDED purchase (Cancelled while never confirmed: no number)
    /// and nothing for a confirmed-then-cancelled one, whose number proves it
    /// must stay as audit trail.
    #[tokio::test]
    async fn document_drawer_discarded_purchase_offers_delete_but_annulled_does_not() {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, PaymentType, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state().await;
        let actor = audit_actor(&state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-DC".into(),
                    name: "Drawer discarded product".into(),
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
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                NewSupplier {
                    name: "Drawer Discard Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        async fn draft_with_line(state: &AppState, supplier_id: i64, product_id: i64) -> i64 {
            use rust_decimal::Decimal;
            let actor = audit_actor(state).await;
            let purchase = state
                .purchases_service
                .create_draft(
                    actor,
                    NewPurchase {
                        supplier_id,
                        payment_type: PaymentType::Credit,
                        purchase_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                        due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        supplier_invoice_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap();
            state
                .purchases_service
                .add_line(actor, purchase.id, product_id, Decimal::from(2), None)
                .await
                .unwrap();
            purchase.id
        }

        // Discarded: cancelled before confirm, number stays NULL → delete renders.
        let discarded_id = draft_with_line(&state, supplier.id, product.id).await;
        state
            .purchases_service
            .cancel(actor, discarded_id, None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/purchase/{discarded_id}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("hx-delete=\"/web/purchases/{discarded_id}\"")),
            "a discarded purchase must offer its delete: {html:.800}"
        );
        assert!(
            html.contains("hx-confirm"),
            "the delete must ask first: {html:.800}"
        );

        // Confirmed then cancelled: the number proves it → NO delete renders.
        let annulled_id = draft_with_line(&state, supplier.id, product.id).await;
        state
            .purchases_service
            .confirm(actor, annulled_id, None)
            .await
            .unwrap();
        state
            .purchases_service
            .cancel(actor, annulled_id, Some("wrong order".to_string()))
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/purchase/{annulled_id}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains(&format!("hx-delete=\"/web/purchases/{annulled_id}\"")),
            "a confirmed-then-cancelled purchase must offer no delete: {html:.800}"
        );
    }

    /// The purchase mirror: the draft delete renders only for a
    /// `purchases.create` holder, and a confirmed purchase's impact names the
    /// `Out · Purchase-return` movement and the Income refund.
    #[tokio::test]
    async fn document_drawer_purchase_actions_mirror_sales_with_their_own_wording() {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, PaymentType, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state_allow_negative().await;
        let actor = audit_actor(&state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-P".into(),
                    name: "Drawer purchase product".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price: Decimal::from(10),
                    // Tracked products require min/max stock in this shop's
                    // rules, even in a fixture.
                    track_stock: true,
                    min_stock: Some(rust_decimal::Decimal::ZERO),
                    max_stock: Some(rust_decimal::Decimal::from(100)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                NewSupplier {
                    name: "Drawer Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        // A cash confirm needs a method the account owns.
        let account = state.account_service.create(actor, "Caja").await.unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(actor, account.id, "Caja")
            .await
            .unwrap();
        let method = state
            .payment_method_service
            .catalog_for_account(account.id)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("the account defaults include Cash");
        let purchase = state
            .purchases_service
            .create_draft(
                actor,
                NewPurchase {
                    supplier_id: supplier.id,
                    payment_type: PaymentType::Cash,
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(actor, purchase.id, product.id, Decimal::from(2), None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Draft, full permission: "Eliminar borrador" with the purchase path.
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/purchase/{}", purchase.id),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Eliminar borrador"), "{html:.800}");
        assert!(
            html.contains(&format!("hx-delete=\"/web/purchases/{}\"", purchase.id)),
            "{html:.800}"
        );
        assert!(
            html.contains("Editar documento"),
            "the purchase mirror keeps the same state-labelled edit link: {html:.800}"
        );

        // Reader only: no actions.
        let reader = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/purchase/{}", purchase.id),
            &test_support::cookie_for(&reader),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(!html.contains("hx-delete"), "{html:.800}");
        assert!(!html.contains("hx-post"), "{html:.800}");

        // Confirmed: Anular with the purchase return wording.
        state
            .purchases_service
            .confirm(actor, purchase.id, Some(method.id))
            .await
            .unwrap();
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/purchase/{}", purchase.id),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Anular"), "{html:.800}");
        assert!(!html.contains("Eliminar borrador"), "{html:.800}");
        assert!(
            html.contains("Salida · Devolución de compra"),
            "the purchase wording differs from the sale's: {html:.800}"
        );
        assert!(
            html.contains("asiento Income"),
            "a purchase refund is money entering: {html:.800}"
        );
        assert!(
            !html.contains("Si algún reembolso deja una cuenta en negativo"),
            "purchase refunds are Income: no negative-balance caveat exists to state: {html:.800}"
        );
    }

    // -- S3: the drawer line tables drop the SKU column ------------------------

    /// The headers of the named table as the drawer template rendered them,
    /// in order. The drawer route builds `DrawerTable`s; reading the rendered
    /// fragment pins the contract the operator sees, so a header/cell drift
    /// fails loudly instead of rendering a misaligned table silently.
    fn table_headers(html: &str, title: &str) -> Vec<String> {
        let start = html
            .find(&format!("{title} ("))
            .unwrap_or_else(|| panic!("table {title:?} is rendered"));
        let rest = &html[start..];
        let rest = &rest[rest.find("<thead>").expect("the table renders a thead")..];
        let end = rest.find("</thead>").expect("the thead closes");
        rest[..end]
            .split("<th class=\"px-2 py-1 font-medium\">")
            .skip(1)
            .map(|chunk| chunk.split("</th>").next().unwrap().to_string())
            .collect()
    }

    /// Cells in the FIRST data row of the named table: the count the
    /// operator reads must equal the header count, or the table renders
    /// misaligned without any error.
    fn first_row_cell_count(html: &str, title: &str) -> usize {
        let start = html
            .find(&format!("{title} ("))
            .unwrap_or_else(|| panic!("table {title:?} is rendered"));
        let rest = &html[start..];
        let rest = &rest[rest.find("<tbody>").expect("the table renders a tbody")..];
        let rest = &rest[rest
            .find("<tr class=\"border-t")
            .expect("a data row renders")..];
        let row_end = rest.find("</tr>").expect("the data row closes");
        rest[..row_end].matches("<td").count()
    }

    /// The sale drawer's line table names six columns, not seven: SKU is not
    /// worth a column at the drawer's width, and the data row must align with
    /// its headers. The three money columns are the net subtotal, the tax and
    /// the tax-inclusive total (tax calculation T2), so the drawer's amount is
    /// auditable instead of opaque.
    #[tokio::test]
    async fn s3_the_sale_drawer_lines_table_drops_the_sku_column() {
        let state = test_state().await;
        let (product, _, _) = seed_sale_kit(&state).await;
        let sale = seed_sale(&state, product, false).await;
        let app = crate::routes::router(state);

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/sale/{sale}"),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let headers = table_headers(&html, "Líneas");
        assert_eq!(
            headers,
            vec![
                "Producto",
                "Cant.",
                "Precio unitario",
                "Subtotal neto",
                "Total de impuestos",
                "Total con impuestos",
            ],
            "{html:.800}"
        );
        assert_eq!(
            first_row_cell_count(&html, "Líneas"),
            headers.len(),
            "the data row must align with its headers: {html:.800}"
        );
    }

    /// The purchase drawer's line table mirrors the sale's: six columns, no
    /// SKU, and a data row that aligns with its headers.
    #[tokio::test]
    async fn s3_the_purchase_drawer_lines_table_drops_the_sku_column() {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state().await;
        let actor = audit_actor(&state).await;
        // An untracked product: the drawer line table is the subject, not the
        // stock rules a tracked fixture would drag in.
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-LS".into(),
                    name: "Drawer line product".into(),
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
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                NewSupplier {
                    name: "Drawer line supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let purchase = state
            .purchases_service
            .create_draft(
                actor,
                NewPurchase {
                    supplier_id: supplier.id,
                    payment_type: crate::models::PaymentType::Cash,
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(actor, purchase.id, product.id, Decimal::from(2), None)
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/purchase/{}", purchase.id),
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let headers = table_headers(&html, "Líneas");
        assert_eq!(
            headers,
            vec![
                "Producto",
                "Cant.",
                "Costo unitario",
                "Subtotal neto",
                "Total de impuestos",
                "Total con impuestos",
            ],
            "{html:.800}"
        );
        assert_eq!(
            first_row_cell_count(&html, "Líneas"),
            headers.len(),
            "the data row must align with its headers: {html:.800}"
        );
    }

    // -----------------------------------------------------------------------
    // M-purchase returns: the two return families in the drawer.
    //
    // They are NOT `DocumentKind` variants (see `ReturnDrawerKind`'s doc), so
    // these tests are what prove the drawer route resolves their tokens at all
    // and narrows them by the PARENT's read code.
    // -----------------------------------------------------------------------

    /// A confirmed purchase with a line, and a draft purchase return against it.
    /// Returns `(purchase_id, return_id, product_id)`.
    async fn seed_purchase_return(state: &AppState) -> (i64, i64, i64) {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, PaymentType, ProductKind};
        use rust_decimal::Decimal;

        let actor = audit_actor(state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-PR".into(),
                    name: "Drawer return product".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(20),
                    cost_price: Decimal::from(5),
                    markup_pct: None,
                    track_stock: true,
                    min_stock: Some(Decimal::ONE),
                    max_stock: Some(Decimal::from(100)),
                    location: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let (_, _, cash) = seed_purchase_kit(state).await;
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                NewSupplier {
                    name: "Drawer Return Supplier".into(),
                    phone: None,
                    due_days: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let purchase = state
            .purchases_service
            .create_draft(
                actor,
                NewPurchase {
                    supplier_id: supplier.id,
                    payment_type: PaymentType::Cash,
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .purchases_service
            .add_line(
                actor,
                purchase.id,
                product.id,
                Decimal::from(5),
                Some(Decimal::from(7)),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(actor, purchase.id, Some(cash))
            .await
            .unwrap();
        let purchase_return = state
            .purchase_return_service
            .create_draft(
                actor,
                purchase.id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let parent_line = state
            .purchases_service
            .get_record(purchase.id)
            .await
            .unwrap()
            .lines[0]
            .id;
        state
            .purchase_return_service
            .add_line(actor, purchase_return.id, parent_line, Decimal::from(2))
            .await
            .unwrap();
        (purchase.id, purchase_return.id, product.id)
    }

    /// The purchase side of the same kit: an account with a Cash method and an
    /// opening balance, so a confirmed purchase's cash Expense can post.
    async fn seed_purchase_kit(state: &AppState) -> (i64, i64, i64) {
        let actor = audit_actor(state).await;
        let account = state
            .account_service
            .create(actor, "Caja-Drawer")
            .await
            .unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(actor, account.id, "Caja")
            .await
            .unwrap();
        let cash = state
            .payment_method_service
            .methods_with_accounts()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash" && m.account_id == Some(account.id))
            .unwrap()
            .id;
        state
            .transaction_service
            .create(
                actor,
                account.id,
                crate::models::TransactionKind::Income,
                "10000".parse().unwrap(),
                None,
                chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            )
            .await
            .unwrap();
        (account.id, cash, cash)
    }

    /// A confirmed sale with a line, and a draft credit note against it.
    async fn seed_customer_return(state: &AppState) -> (i64, i64, i64) {
        use crate::models::{NewProduct, NewSale, PaymentType, ProductKind};
        use rust_decimal::Decimal;

        let actor = audit_actor(state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "DRAW-CR".into(),
                    name: "Drawer credit product".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(20),
                    cost_price: Decimal::from(5),
                    markup_pct: None,
                    track_stock: true,
                    min_stock: Some(Decimal::ONE),
                    max_stock: Some(Decimal::from(100)),
                    location: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        // Stock in, so the sale has something to sell.
        state
            .inventory_service
            .record_movement(
                actor,
                crate::models::NewMovement {
                    product_id: product.id,
                    qty: Decimal::from(20),
                    movement_type: crate::models::MovementType::In,
                    reason: crate::models::MovementReason::Initial,
                    reference: String::new(),
                    date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                },
            )
            .await
            .unwrap();
        let (_, _, cash) = seed_purchase_kit(state).await;
        let customer = state
            .customer_service
            .create_customer(
                actor,
                crate::models::NewCustomer {
                    name: "Drawer Credit Customer".into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: None,
                    due_days: None,
                },
            )
            .await
            .unwrap()
            .customer
            .id;
        let sale = state
            .sales_service
            .create_draft(
                actor,
                NewSale {
                    customer_id: customer,
                    payment_type: PaymentType::Cash,
                    sale_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        state
            .sales_service
            .add_line(
                sale.id,
                product.id,
                Decimal::from(5),
                Some(Decimal::from(9)),
            )
            .await
            .unwrap();
        state
            .sales_service
            .confirm(actor, sale.id, Some(cash))
            .await
            .unwrap();
        let customer_return = state
            .customer_return_service
            .create_draft(
                actor,
                sale.id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let parent_line = state.sales_service.get_record(sale.id).await.unwrap().lines[0].id;
        state
            .customer_return_service
            .add_line(actor, customer_return.id, parent_line, Decimal::from(2))
            .await
            .unwrap();
        (sale.id, customer_return.id, product.id)
    }

    /// **The drawer opens a purchase return for a principal holding the PARENT's
    /// read code, and refuses it otherwise.**
    ///
    /// The refusal has to name the code, or an operator who was refused cannot
    /// tell which tier to ask an administrator for.
    #[tokio::test]
    async fn the_documents_drawer_opens_a_purchase_return_under_purchases_read_and_refuses_it_otherwise(
    ) {
        let state = test_state().await;
        let (_purchase_id, return_id, _product) = seed_purchase_return(&state).await;

        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/purchase_return/{return_id}"),
            &test_support::cookie_for(&probe),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("Devoluciones de compra"),
            "the family is named, not rendered as an unknown kind: {html:.1200}"
        );
        assert!(
            html.contains("Drawer Return Supplier"),
            "the supplier's NAME, never the id: {html:.1200}"
        );
        assert!(
            html.contains("Drawer return product"),
            "the line's product identity is read THROUGH the parent purchase line: {html:.1600}"
        );
        // The frozen cost is SHOWN and there is no control to change it: the
        // drawer is a read surface, and the price is a fact about the parent.
        // `format_currency` is the drawer's house formatter (`format_money` is the
        // record page's), and at scale 0 it renders `7 USD`.
        assert!(
            html.contains("7 USD"),
            "the frozen cost renders as text: {html:.2000}"
        );
        assert!(
            !html.contains("unit_cost") && !html.contains("name=\"qty\""),
            "the drawer renders no price input and no line editor: {html:.2000}"
        );
        assert!(
            !html.contains("Eliminar borrador"),
            "a purchases.read-ONLY principal sees NO delete: the drawer offers an \
             action only for the code its endpoint requires, and the delete needs \
             purchases.create: {html:.2000}"
        );

        // And a principal that DOES hold the write code is offered the delete.
        let writer = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "purchases.create"],
        )
        .await
        .unwrap();
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/purchase_return/{return_id}"),
            &test_support::cookie_for(&writer),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("Eliminar borrador"),
            "a purchases.create principal is offered the draft delete: {html:.2000}"
        );

        let other = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/purchase_return/{return_id}"),
            &test_support::cookie_for(&other),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.600}");
        assert!(
            html.contains("purchases.read"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// The credit-note twin, under `sales.read`.
    #[tokio::test]
    async fn the_documents_drawer_opens_a_credit_note_under_sales_read_and_refuses_it_otherwise() {
        let state = test_state().await;
        let (_sale_id, return_id, _product) = seed_customer_return(&state).await;

        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let (status, html) = get_drawer(
            app.clone(),
            &format!("/web/documents/detail/customer_return/{return_id}"),
            &test_support::cookie_for(&probe),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("Notas de crédito"),
            "the Spanish name is the specific term, decision 4: {html:.1200}"
        );
        assert!(
            html.contains("Drawer Credit Customer"),
            "the customer's NAME, never the id: {html:.1200}"
        );
        assert!(
            html.contains("Drawer credit product"),
            "the line's product identity is read THROUGH the parent sale line: {html:.1600}"
        );
        assert!(
            html.contains("9 USD"),
            "the frozen price renders as text, at the drawer's own scale: {html:.2000}"
        );
        assert!(
            !html.contains("unit_price") && !html.contains("name=\"qty\""),
            "the drawer renders no price input and no line editor: {html:.2000}"
        );

        let other = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let (status, html) = get_drawer(
            app,
            &format!("/web/documents/detail/customer_return/{return_id}"),
            &test_support::cookie_for(&other),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.600}");
        assert!(
            html.contains("sales.read"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// An unknown kind token is still the standard 404 naming the known tokens —
    /// the return families must not have widened the set into anything vague.
    #[tokio::test]
    async fn the_documents_drawer_still_404s_an_unknown_kind_token() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (status, body) = get_drawer(
            app,
            "/web/documents/detail/not_a_family/1",
            test_support::TEST_COOKIE,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body:.400}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let message = json["error"].as_str().unwrap();
        assert!(
            message.contains("sale") && message.contains("purchase_return"),
            "the 404 names the known tokens, both the DocumentKind ones and the \
             return ones: {message}"
        );
    }
}
