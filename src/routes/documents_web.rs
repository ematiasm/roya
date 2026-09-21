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

/// The one partial all six families render: a title (the identifier), a status
/// line, the fact list, optional tables, the optional parent summary, an
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
        DocumentKind::Sale => sale_drawer(state, id).await,
        DocumentKind::SalePayment => sale_payment_drawer(state, id).await,
        DocumentKind::Purchase => purchase_drawer(state, id).await,
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
/// the product/account/method names are already resolved by `get_record`.
async fn sale_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
    let record = state.sales_service.get_record(id).await?;
    let sale = &record.sale;
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
        notice: None,
        links: vec![DrawerLink {
            label: "Abrir en Ventas".to_string(),
            href: format!("/sales/{}", sale.id),
        }],
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
        .expect("the parent record renders the payment this drawer opened");
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
        notice: None,
        links,
    })
}

/// The PURCHASE family: the mirror of the sale drawer with supplier, supplier
/// invoice and unit costs.
async fn purchase_drawer(state: &AppState, id: i64) -> AppResult<DocumentDetailPartial> {
    let record = state.purchases_service.get_record(id).await?;
    let purchase = &record.purchase;
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
        notice: None,
        links: vec![DrawerLink {
            label: "Abrir en Compras".to_string(),
            href: format!("/purchases/{}", purchase.id),
        }],
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
        .expect("the parent record renders the payment this drawer opened");
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
        notice: None,
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
        notice: Some(
            "El movimiento es historia append-only: no se edita ni se elimina.".to_string(),
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
        notice: None,
        links: vec![DrawerLink {
            label: "Abrir en Clientes".to_string(),
            href: format!("/customers/{}", detail.receipt.customer_id),
        }],
    })
}
