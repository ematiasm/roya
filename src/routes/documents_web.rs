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
use crate::models::{
    DocumentFilter, DocumentGroup, DocumentKind, DocumentRow, PurchaseRecord, PurchaseStatus,
    SaleRecord, SaleStatus,
};
use crate::routes::AppState;
use crate::security::authz::{
    CustomersRead, InventoryRead, Nav, Permission, Principal, PurchasesCancel, PurchasesCreate,
    PurchasesRead, RequireAny, SalesCancel, SalesCreate, SalesRead,
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
            id: row.id,
            kind_token: row.kind.token().to_string(),
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
    label: &'static str,
    value: String,
}

impl DrawerFact {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
        }
    }

    /// The fact only when `value` is non-empty: optional facts (notes, due
    /// dates, cancel reasons) vanish instead of rendering as empty rows.
    fn when_non_empty(label: &'static str, value: Option<String>) -> Option<Self> {
        value
            .filter(|v| !v.is_empty())
            .map(|v| Self { label, value: v })
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
    title: &'static str,
    headers: Vec<&'static str>,
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
/// ledger entry), always gated by the same code that opened the drawer.
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
    kind_token: String,
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
    axum::Extension(principal): axum::Extension<Principal>,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, i64)>,
) -> Result<Html<String>, AppError> {
    let tmpl = document_detail(&state, &principal, &kind, id).await?;
    let html = tmpl
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// The drawer payload for one document: resolve the family, narrow by
/// permission, then assemble. Missing/unknown documents are the standard 404
/// with the family's name in the message.
async fn document_detail(
    state: &AppState,
    principal: &Principal,
    kind_token: &str,
    id: i64,
) -> AppResult<DocumentDetailPartial> {
    let kind = DocumentKind::parse(kind_token).ok_or_else(|| {
        AppError::NotFound(format!(
            "unknown document kind \"{kind_token}\": the index knows {tokens}",
            tokens = DocumentKind::ALL
                .iter()
                .map(|k| k.token())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    if !permitted_kinds(principal).contains(&kind) {
        return Err(AppError::Forbidden(format!(
            "Se necesita el permiso «{}» para ver este documento",
            kind.read_code()
        )));
    }
    match kind {
        DocumentKind::Sale => sale_drawer(state, &principal, id).await,
        DocumentKind::SalePayment => sale_payment_drawer(state, id).await,
        DocumentKind::Purchase => purchase_drawer(state, &principal, id).await,
        DocumentKind::PurchasePayment => purchase_payment_drawer(state, id).await,
        DocumentKind::StockMovement => stock_movement_drawer(state, id).await,
        DocumentKind::Receipt => receipt_drawer(state, id).await,
    }
}

/// The identifier a document answers by, the way every list already shows it:
/// the assigned number, or `Draft #id` while a draft has none.
fn document_title(number: Option<&str>, id: i64) -> String {
    number.map(str::to_string).unwrap_or_else(|| format!("Draft #{id}"))
}

/// The edit affordance the user asked for as a real BUTTON-styled link: the
/// FIRST entry of the links list, labelled by what the state allows. It
/// deliberately navigates to the record page — the drawer never duplicates
/// the multi-field forms (header, lines, payments, confirm) that page owns.
/// The status arrives as its `Display` form — both families' status enums
/// share the exact three names — so sale and purchase call the same helper.
fn edit_affordance_link(status: &str, href: String) -> DrawerLink {
    let label = match status {
        "Draft" => "Editar cabecera",
        _ => "Abrir el documento",
    };
    DrawerLink {
        label: label.to_string(),
        href,
    }
}

/// The edit affordance as text: where the multi-field actions live, worded by
/// state. The drawer states it instead of building edit forms it would have
/// to keep in lockstep with the endpoints.
fn edit_affordance_notice(status: &str) -> String {
    match status {
        "Draft" => {
            "Para editar la cabecera, agregar líneas, confirmar o registrar pagos, abrí el documento."
                .to_string()
        }
        "Confirmed" => {
            "Para registrar pagos o ver el detalle completo, abrí el documento.".to_string()
        }
        _ => {
            "El documento está anulado. Para ver el detalle completo y su historia, abrí el documento."
                .to_string()
        }
    }
}

/// The SALE drawer's action block, per state and permission. Real actions
/// only — the drawer never invents one: the delete exists for a DRAFT (a
/// draft never touched stock, money or a customer's debt, so nothing
/// dangles), Anular/Descartar re-present the tested `cancel` endpoint, and a
/// cancelled document offers nothing because its inverse already happened.
async fn sale_actions(
    state: &AppState,
    principal: &Principal,
    record: &SaleRecord,
) -> AppResult<Vec<DrawerAction>> {
    let sale = &record.sale;
    let mut actions = Vec::new();
    match sale.status {
        SaleStatus::Draft => {
            if principal.has(SalesCreate::CODE) {
                let n = record.lines.len();
                actions.push(DrawerAction {
                    label: "Eliminar borrador".to_string(),
                    method: "delete".to_string(),
                    path: format!("/web/sales/{}", sale.id),
                    fields: vec![],
                    reason: false,
                    impact: vec![
                        format!("Se elimina el borrador y sus {n} líneas (listadas arriba)."),
                        "Nunca se confirmó: no dejó movimientos de stock, ni pagos, ni asientos de caja."
                            .to_string(),
                    ],
                    confirm: Some(format!(
                        "¿Eliminar el borrador y sus {n} líneas? Esta acción no se puede deshacer."
                    )),
                });
            }
            if principal.has(SalesCancel::CODE) {
                actions.push(DrawerAction {
                    label: "Descartar".to_string(),
                    method: "post".to_string(),
                    path: "/web/sales/cancel".to_string(),
                    fields: vec![("sale_id".to_string(), sale.id.to_string())],
                    reason: true,
                    impact: vec![
                        "El borrador pasa a Anulado y deja de aparecer como editable.".to_string(),
                        "No hay stock, ni pagos, ni asientos que revertir.".to_string(),
                    ],
                    confirm: None,
                });
            }
        }
        SaleStatus::Confirmed => {
            if principal.has(SalesCancel::CODE) {
                actions.push(sale_annul_action(state, record).await?);
            }
        }
        SaleStatus::Cancelled => {}
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
) -> AppResult<DrawerAction> {
    let sale = &record.sale;
    let mut impact = Vec::new();
    for line in &record.lines {
        if !line.tracks_stock {
            continue;
        }
        let product = state.inventory_service.get_product(line.product_id).await?;
        if !product.is_active {
            impact.push(format!(
                "No se puede anular: el producto «{}» está inactivo.",
                product.name
            ));
        } else {
            impact.push(format!(
                "Se devuelve el stock de «{}» ({}) con un movimiento In · Sale-return.",
                line.product_name, line.qty
            ));
        }
    }
    for payment in &record.payments {
        impact.push(format!(
            "Se reembolsa «{}» en «{}» con un asiento Expense.",
            payment.amount, payment.account_name
        ));
    }
    impact.push(
        "El documento pasa a Anulado y deja de contar como deuda del cliente.".to_string(),
    );
    if !state.allow_negative {
        impact.push(
            "Si algún reembolso dejaría una cuenta en negativo, la anulación se rechaza y verás el motivo."
                .to_string(),
        );
    }
    Ok(DrawerAction {
        label: "Anular".to_string(),
        method: "post".to_string(),
        path: "/web/sales/cancel".to_string(),
        fields: vec![("sale_id".to_string(), sale.id.to_string())],
        reason: true,
        impact,
        confirm: Some("¿Anular este documento? Esta acción no se puede deshacer.".to_string()),
    })
}

/// The PURCHASE drawer's action block — the mirror of the sale's with the
/// purchase wording: `Out · Purchase-return` movements (confirm writes
/// In/Purchase, cancel writes Out/Purchase-return), refunds that are `Income`
/// (money entering: no negative-balance refusal exists to preview), and the
/// purchase cancel endpoint.
async fn purchase_actions(
    state: &AppState,
    principal: &Principal,
    record: &PurchaseRecord,
) -> AppResult<Vec<DrawerAction>> {
    let purchase = &record.purchase;
    let mut actions = Vec::new();
    match purchase.status {
        PurchaseStatus::Draft => {
            if principal.has(PurchasesCreate::CODE) {
                let n = record.lines.len();
                actions.push(DrawerAction {
                    label: "Eliminar borrador".to_string(),
                    method: "delete".to_string(),
                    path: format!("/web/purchases/{}", purchase.id),
                    fields: vec![],
                    reason: false,
                    impact: vec![
                        format!("Se elimina el borrador y sus {n} líneas (listadas arriba)."),
                        "Nunca se confirmó: no dejó movimientos de stock, ni pagos, ni asientos de caja."
                            .to_string(),
                    ],
                    confirm: Some(format!(
                        "¿Eliminar el borrador y sus {n} líneas? Esta acción no se puede deshacer."
                    )),
                });
            }
            if principal.has(PurchasesCancel::CODE) {
                actions.push(DrawerAction {
                    label: "Descartar".to_string(),
                    method: "post".to_string(),
                    path: "/web/purchases/cancel".to_string(),
                    fields: vec![("purchase_id".to_string(), purchase.id.to_string())],
                    reason: true,
                    impact: vec![
                        "El borrador pasa a Anulado y deja de aparecer como editable.".to_string(),
                        "No hay stock, ni pagos, ni asientos que revertir.".to_string(),
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
                        impact.push(format!(
                            "No se puede anular: el producto «{}» está inactivo.",
                            product.name
                        ));
                    } else {
                        impact.push(format!(
                            "Se devuelve el stock de «{}» ({}) con un movimiento Out · Purchase-return.",
                            line.product_name, line.qty
                        ));
                    }
                }
                for payment in &record.payments {
                    impact.push(format!(
                        "Se reembolsa «{}» en «{}» con un asiento Income.",
                        payment.amount, payment.account_name
                    ));
                }
                impact.push(
                    "El documento pasa a Anulado y deja de contar como deuda con el proveedor."
                        .to_string(),
                );
                actions.push(DrawerAction {
                    label: "Anular".to_string(),
                    method: "post".to_string(),
                    path: "/web/purchases/cancel".to_string(),
                    fields: vec![("purchase_id".to_string(), purchase.id.to_string())],
                    reason: true,
                    impact,
                    confirm: Some(
                        "¿Anular este documento? Esta acción no se puede deshacer.".to_string(),
                    ),
                });
            }
        }
        PurchaseStatus::Cancelled => {}
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
) -> AppResult<(DrawerFact, Option<DrawerFact>)> {
    let mut ids = vec![created_by];
    ids.extend(updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &ids).await?;
    let name_for = |id: i64| {
        names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "(sistema)".to_string())
    };
    let created = DrawerFact::new("Registrado por", name_for(created_by));
    let updated = updated_by.map(|id| DrawerFact::new("Actualizado por", name_for(id)));
    Ok((created, updated))
}

/// The SALE family: the full record the `/sales/{id}` page renders as facts —
/// the product/account/method names are already resolved by `get_record` —
/// plus the action block, built only from real actions the principal may use.
async fn sale_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
) -> AppResult<DocumentDetailPartial> {
    let record = state.sales_service.get_record(id).await?;
    let sale = &record.sale;
    let actions = sale_actions(state, principal, &record).await?;
    let (created_by, updated_by) = actor_facts(state, sale.created_by, sale.updated_by).await?;

    let mut facts = vec![
        DrawerFact::new("Cliente", &sale.customer_name),
        DrawerFact::new("Tipo de pago", sale.payment_type.to_string()),
        DrawerFact::new("Fecha", sale.sale_date.to_string()),
    ];
    facts.extend(DrawerFact::when_non_empty(
        "Vencimiento",
        sale.due_date.map(|d| d.to_string()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Recibo",
        sale.receipt_no.clone(),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Notas",
        Some(sale.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Motivo de anulación",
        sale.cancel_reason.clone(),
    ));
    facts.push(DrawerFact::new("Total", record.total.to_string()));
    facts.push(DrawerFact::new("Pagado", record.paid.to_string()));
    facts.push(DrawerFact::new("Saldo", record.due.to_string()));
    facts.push(DrawerFact::new(
        "Estado de pago",
        record.payment_status.to_string(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![
        DrawerTable {
            title: "Líneas",
            headers: vec!["Producto", "SKU", "Cant.", "Precio unit.", "Subtotal"],
            rows: record
                .lines
                .iter()
                .map(|line| DrawerTableRow {
                    cells: vec![
                        line.product_name.clone(),
                        line.product_sku.clone(),
                        line.qty.to_string(),
                        line.unit_price.to_string(),
                        line.subtotal.to_string(),
                    ],
                    href: None,
                })
                .collect(),
        },
        DrawerTable {
            title: "Pagos",
            headers: vec!["Fecha", "Cuenta", "Medio", "Monto"],
            rows: record
                .payments
                .iter()
                .map(|payment| DrawerTableRow {
                    cells: vec![
                        payment.date.to_string(),
                        payment.account_name.clone(),
                        payment.method_name.clone(),
                        payment.amount.to_string(),
                    ],
                    href: None,
                })
                .collect(),
        },
    ];

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::Sale.token().to_string(),
        kind_label: DocumentKind::Sale.label().to_string(),
        title: document_title(sale.sale_number.as_deref(), sale.id),
        status_line: sale.status.to_string(),
        facts,
        tables,
        parent: None,
        actions,
        notice: Some(edit_affordance_notice(&sale.status.to_string())),
        links: vec![edit_affordance_link(
            &sale.status.to_string(),
            format!("/sales/{}", sale.id),
        )],
    })
}

/// The SALE-PAYMENTS family: the payment's own facts plus the parent sale's
/// summary, the finance transactions it produced (the original Income and, on
/// a cancelled sale, the refund Expense — linked to the account page that owns
/// the ledger, never a second account-name read), and the receipt that grouped
/// it when one did.
async fn sale_payment_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
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
        actor_facts(state, payment.created_by, payment.updated_by).await?;

    // The ledger links: the account page owns the transaction's name and
    // balance, so the drawer links there instead of re-reading an account.
    let mut links = Vec::new();
    let original = match payment.transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            links.push(DrawerLink {
                label: "Ver asiento en Caja".to_string(),
                href: format!("/accounts/{}", tx.account_id),
            });
            Some(tx)
        }
        None => None,
    };
    let refund = match payment.refund_transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            links.push(DrawerLink {
                label: "Ver reembolso en Caja".to_string(),
                href: format!("/accounts/{}", tx.account_id),
            });
            Some(tx)
        }
        None => None,
    };
    if let Some(receipt_id) = payment.receipt_id {
        links.push(DrawerLink {
            label: "Ver recibo del cliente".to_string(),
            href: format!("/customers/{}", sale.customer_id),
        });
    }
    links.push(DrawerLink {
        label: "Abrir en Ventas".to_string(),
        href: format!("/sales/{}", sale.id),
    });

    let mut facts = vec![
        DrawerFact::new("Monto", payment.amount.to_string()),
        DrawerFact::new("Fecha", payment.date.to_string()),
        DrawerFact::new("Cuenta", &view.account_name),
        DrawerFact::new("Medio", &view.method_name),
    ];
    if let Some(tx) = &original {
        facts.push(DrawerFact::new(
            "Asiento",
            format!("{} · {} · {}", tx.kind, tx.amount, tx.date),
        ));
    }
    if let Some(tx) = &refund {
        facts.push(DrawerFact::new(
            "Reembolso",
            format!("{} · {} · {}", tx.kind, tx.amount, tx.date),
        ));
    }
    if let Some(receipt_id) = payment.receipt_id {
        facts.push(DrawerFact::new("Recibo", format!("Recibo #{receipt_id}")));
    }
    facts.push(created_by);
    facts.extend(updated_by);

    let parent = DrawerParent {
        label: DocumentKind::Sale.label().to_string(),
        title: document_title(sale.sale_number.as_deref(), sale.id),
        status_line: sale.status.to_string(),
        facts: vec![
            DrawerFact::new("Total", record.total.to_string()),
            DrawerFact::new("Pagado", record.paid.to_string()),
            DrawerFact::new("Saldo", record.due.to_string()),
        ],
        href: format!("/sales/{}", sale.id),
    };

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::SalePayment.token().to_string(),
        kind_label: DocumentKind::SalePayment.label().to_string(),
        title: format!("Pago de {}", document_title(sale.sale_number.as_deref(), sale.id)),
        status_line: format!("Pago · {} · {}", payment.date, sale.status),
        facts,
        tables: Vec::new(),
        parent: Some(parent),
        actions: Vec::new(),
        notice: Some(
            "El pago no se edita ni se elimina: el dinero ya está en la caja y el asiento queda. Si el documento se anula, el reembolso lo registra la anulación, no una edición manual."
                .to_string(),
        ),
        links,
    })
}

/// The PURCHASE family: the mirror of the sale drawer with supplier, supplier
/// invoice, unit costs — and the purchase-family action block.
async fn purchase_drawer(
    state: &AppState,
    principal: &Principal,
    id: i64,
) -> AppResult<DocumentDetailPartial> {
    let record = state.purchases_service.get_record(id).await?;
    let purchase = &record.purchase;
    let actions = purchase_actions(state, principal, &record).await?;
    let (created_by, updated_by) =
        actor_facts(state, purchase.created_by, purchase.updated_by).await?;

    let mut facts = vec![
        DrawerFact::new("Proveedor", &record.supplier_name),
        DrawerFact::new("Tipo de pago", purchase.payment_type.to_string()),
        DrawerFact::new("Fecha", purchase.purchase_date.to_string()),
    ];
    facts.extend(DrawerFact::when_non_empty(
        "Vencimiento",
        purchase.due_date.map(|d| d.to_string()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Factura del proveedor",
        purchase.supplier_invoice_no.clone(),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Notas",
        Some(purchase.notes.clone()).filter(|n| !n.is_empty()),
    ));
    facts.extend(DrawerFact::when_non_empty(
        "Motivo de anulación",
        purchase.cancel_reason.clone(),
    ));
    facts.push(DrawerFact::new("Total", record.total.to_string()));
    facts.push(DrawerFact::new("Pagado", record.paid.to_string()));
    facts.push(DrawerFact::new("Saldo", record.due.to_string()));
    facts.push(DrawerFact::new(
        "Estado de pago",
        record.payment_status.to_string(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![
        DrawerTable {
            title: "Líneas",
            headers: vec!["Producto", "SKU", "Cant.", "Costo unit.", "Subtotal"],
            rows: record
                .lines
                .iter()
                .map(|line| DrawerTableRow {
                    cells: vec![
                        line.product_name.clone(),
                        line.product_sku.clone(),
                        line.qty.to_string(),
                        line.unit_cost.to_string(),
                        line.subtotal.to_string(),
                    ],
                    href: None,
                })
                .collect(),
        },
        DrawerTable {
            title: "Pagos",
            headers: vec!["Fecha", "Cuenta", "Medio", "Monto"],
            rows: record
                .payments
                .iter()
                .map(|payment| DrawerTableRow {
                    cells: vec![
                        payment.date.to_string(),
                        payment.account_name.clone(),
                        payment.method_name.clone(),
                        payment.amount.to_string(),
                    ],
                    href: None,
                })
                .collect(),
        },
    ];

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::Purchase.token().to_string(),
        kind_label: DocumentKind::Purchase.label().to_string(),
        title: document_title(purchase.purchase_number.as_deref(), purchase.id),
        status_line: purchase.status.to_string(),
        facts,
        tables,
        parent: None,
        actions,
        notice: Some(edit_affordance_notice(&purchase.status.to_string())),
        links: vec![edit_affordance_link(
            &purchase.status.to_string(),
            format!("/purchases/{}", purchase.id),
        )],
    })
}

/// The PURCHASE-PAYMENTS family: the mirror of the sale-payment drawer with
/// `purchase-changed`-family links.
async fn purchase_payment_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
    let payment = state.purchases_service.find_payment(id).await?;
    let record = state.purchases_service.get_record(payment.purchase_id).await?;
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
        actor_facts(state, payment.created_by, payment.updated_by).await?;

    let mut links = Vec::new();
    let original = match payment.transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            links.push(DrawerLink {
                label: "Ver asiento en Caja".to_string(),
                href: format!("/accounts/{}", tx.account_id),
            });
            Some(tx)
        }
        None => None,
    };
    let refund = match payment.refund_transaction_id {
        Some(tx_id) => {
            let tx = state.transaction_service.get(tx_id).await?;
            links.push(DrawerLink {
                label: "Ver reembolso en Caja".to_string(),
                href: format!("/accounts/{}", tx.account_id),
            });
            Some(tx)
        }
        None => None,
    };
    links.push(DrawerLink {
        label: "Abrir en Compras".to_string(),
        href: format!("/purchases/{}", purchase.id),
    });

    let mut facts = vec![
        DrawerFact::new("Monto", payment.amount.to_string()),
        DrawerFact::new("Fecha", payment.date.to_string()),
        DrawerFact::new("Cuenta", &view.account_name),
        DrawerFact::new("Medio", &view.method_name),
    ];
    if let Some(tx) = &original {
        facts.push(DrawerFact::new(
            "Asiento",
            format!("{} · {} · {}", tx.kind, tx.amount, tx.date),
        ));
    }
    if let Some(tx) = &refund {
        facts.push(DrawerFact::new(
            "Reembolso",
            format!("{} · {} · {}", tx.kind, tx.amount, tx.date),
        ));
    }
    facts.push(created_by);
    facts.extend(updated_by);

    let parent = DrawerParent {
        label: DocumentKind::Purchase.label().to_string(),
        title: document_title(purchase.purchase_number.as_deref(), purchase.id),
        status_line: purchase.status.to_string(),
        facts: vec![
            DrawerFact::new("Total", record.total.to_string()),
            DrawerFact::new("Pagado", record.paid.to_string()),
            DrawerFact::new("Saldo", record.due.to_string()),
        ],
        href: format!("/purchases/{}", purchase.id),
    };

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::PurchasePayment.token().to_string(),
        kind_label: DocumentKind::PurchasePayment.label().to_string(),
        title: format!(
            "Pago de {}",
            document_title(purchase.purchase_number.as_deref(), purchase.id)
        ),
        status_line: format!("Pago · {} · {}", payment.date, purchase.status),
        facts,
        tables: Vec::new(),
        parent: Some(parent),
        actions: Vec::new(),
        notice: Some(
            "El pago no se edita ni se elimina: el dinero ya está en la caja y el asiento queda. Si el documento se anula, el reembolso lo registra la anulación, no una edición manual."
                .to_string(),
        ),
        links,
    })
}

/// The STOCK-MOVEMENT family: the movement's facts plus the product's current
/// derived stock. The movement is append-only history — no edit, no delete —
/// and the drawer says so; the action slice builds on that guarantee.
async fn stock_movement_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
    let movement = state.inventory_service.get_movement(id).await?;
    let stock = state
        .inventory_service
        .product_stock(movement.product_id)
        .await?;
    let (created_by, updated_by) =
        actor_facts(state, movement.created_by, movement.updated_by).await?;

    let mut facts = vec![
        DrawerFact::new(
            "Producto",
            format!("{} ({})", stock.product.name, stock.product.sku),
        ),
        DrawerFact::new("Tipo", movement.movement_type.to_string()),
        DrawerFact::new("Motivo", movement.reason.to_string()),
        DrawerFact::new("Cantidad", movement.qty.to_string()),
        DrawerFact::new("Referencia", &movement.reference),
        DrawerFact::new("Fecha", movement.date.to_string()),
        DrawerFact::new("Stock actual", stock.stock.to_string()),
    ];
    facts.push(created_by);
    facts.extend(updated_by);

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::StockMovement.token().to_string(),
        kind_label: DocumentKind::StockMovement.label().to_string(),
        title: format!("Movimiento #{}", movement.id),
        status_line: format!("{} · {}", movement.movement_type, movement.reason),
        facts,
        tables: Vec::new(),
        parent: None,
        actions: Vec::new(),
        notice: Some(
                "El movimiento es historia append-only: no se edita ni se elimina. Para compensarlo, registrá un ajuste en el producto."
                .to_string(),
        ),
        links: vec![DrawerLink {
            label: "Ver producto".to_string(),
            href: format!("/products#product-{}", movement.product_id),
        }],
    })
}

/// The RECEIPTS family: the collection's facts plus the payments it grouped —
/// each allocation names its sale the way the operator does (the receipt read
/// resolves the sale numbers) and links to the sale it applied to.
async fn receipt_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
    let detail = state.customer_receipt_service.get_receipt(id).await?;
    let customer = state
        .customer_service
        .get_customer(detail.receipt.customer_id)
        .await?;
    let (created_by, updated_by) = actor_facts(
        state,
        detail.receipt.created_by,
        detail.receipt.updated_by,
    )
    .await?;

    let mut facts = vec![
        DrawerFact::new("Cliente", &customer.name),
        DrawerFact::new("Fecha", detail.receipt.date.to_string()),
        DrawerFact::new("Cuenta", &detail.account_name),
        DrawerFact::new("Medio", &detail.method_name),
    ];
    facts.extend(DrawerFact::when_non_empty(
        "Notas",
        detail.receipt.notes.clone(),
    ));
    facts.push(DrawerFact::new("Total", detail.total.to_string()));
    facts.push(DrawerFact::new(
        "Asignaciones",
        detail.allocations.len().to_string(),
    ));
    facts.push(created_by);
    facts.extend(updated_by);

    let tables = vec![DrawerTable {
        title: "Asignaciones",
        headers: vec!["Venta", "Fecha", "Monto"],
        rows: detail
            .allocations
            .iter()
            .map(|payment| DrawerTableRow {
                cells: vec![
                    payment
                        .sale_number
                        .clone()
                        .unwrap_or_else(|| format!("Venta #{}", payment.sale_id)),
                    payment.date.to_string(),
                    payment.amount.to_string(),
                ],
                href: Some(format!("/sales/{}", payment.sale_id)),
            })
            .collect(),
    }];

    Ok(DocumentDetailPartial {
        kind_token: DocumentKind::Receipt.token().to_string(),
        kind_label: DocumentKind::Receipt.label().to_string(),
        title: format!("Recibo #{}", detail.receipt.id),
        status_line: "Cobro".to_string(),
        facts,
        tables,
        parent: None,
        actions: Vec::new(),
        notice: Some(
            "El recibo agrupa los pagos de un cobro: mientras los explique, la base rechaza eliminarlo."
                .to_string(),
        ),
        links: vec![DrawerLink {
            label: "Abrir en Clientes".to_string(),
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
                },
            )
            .await
            .unwrap();
        let account = state
            .account_service
            .create(actor, "Caja")
            .await
            .unwrap();
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

    async fn seed_sale(state: &AppState, product_id: i64, confirm: bool) -> i64 {
        seed_sale_typed(state, product_id, confirm, crate::models::PaymentType::Cash, None).await
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
                    payment_days: None,
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
            html.contains("Editar cabecera"),
            "the edit affordance is a button-styled link: {html:.800}"
        );
        assert!(
            html.contains(&format!("hx-delete=\"/web/sales/{sale}\"")),
            "{html:.800}"
        );
        assert!(html.contains("hx-post=\"/web/sales/cancel\""), "{html:.800}");
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
        assert!(html.contains("Editar cabecera"), "{html:.800}");

        // Cancel permission without create: "Descartar" yes, delete no.
        let canceller = test_support::seed_session_with_permissions(
            &state.pool,
            &["sales.read", "sales.cancel"],
        )
        .await
        .unwrap();
        let (status, html) =
            get_drawer(app, &uri, &test_support::cookie_for(&canceller)).await;
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
        let sale =
            seed_sale_typed(&state, product, true, crate::models::PaymentType::Cash, Some(method))
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
            html.contains("In · Sale-return"),
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
            html.contains("Si algún reembolso dejaría una cuenta en negativo"),
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
        let sale =
            seed_sale_typed(&state, product, true, crate::models::PaymentType::Cash, Some(method))
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
            html.contains("No se puede anular"),
            "the blocker must be visible before the click: {html:.800}"
        );
        assert!(
            html.contains("está inactivo"),
            "{html:.800}"
        );
        assert!(
            html.contains("Anular"),
            "the action is NOT hidden: the refusal path is still the operator's path"
        );
    }

    /// A cancelled document offers no action at all and says why: the document
    /// is annulled, its inverse already happened.
    #[tokio::test]
    async fn document_drawer_cancelled_sale_offers_no_action() {
        let state = test_state().await;
        let (product, _, method) = seed_sale_kit(&state).await;
        let sale =
            seed_sale_typed(&state, product, true, crate::models::PaymentType::Cash, Some(method))
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
            html.contains("ya está en la caja"),
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
            html.contains("ajuste"),
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
        assert!(html.contains("Editar cabecera"), "{html:.800}");

        // Reader only: no actions.
        let reader =
            test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
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
        assert!(
            !html.contains("Eliminar borrador"),
            "{html:.800}"
        );
        assert!(
            html.contains("Out · Purchase-return"),
            "the purchase wording differs from the sale's: {html:.800}"
        );
        assert!(
            html.contains("asiento Income"),
            "a purchase refund is money entering: {html:.800}"
        );
        assert!(
            !html.contains("Si algún reembolso dejaría una cuenta en negativo"),
            "purchase refunds are Income: no negative-balance caveat exists to state: {html:.800}"
        );
    }
}
