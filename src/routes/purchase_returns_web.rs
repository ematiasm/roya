// Purchase returns web: the `/purchase-returns` list and the
// `/purchase-returns/{id}` record page, Askama + HTMX. Thin handlers over
// `PurchaseReturnService`; the record body lives in
// `partials/purchase_return_detail.html` and every action posts to
// `/web/purchase-returns/{id}/...`, so the id always comes from the URL.
//
// THE GATE IS THE PARENT'S, on purpose and without a new code. A purchase
// return is `purchases.read` to see and `purchases.create` to write — the same
// two codes the purchase list and record page carry. The reasoning is the one
// `DocumentKind::read_code` already encodes for the drawer: a principal who can
// confirm a purchase can return what it bought, and one who can cancel it can
// reverse the return. A dedicated `purchases.return` code would gate nothing the
// catalog does not already say, and a permission that gates nothing is worse than
// a coarse one. The customer-return twin in `customer_returns_web.rs` is
// `sales.read` / `sales.create` for the same reason.
//
// THE FORM HAS NO PRICE INPUT, and that is decision 1 of the design made
// visible rather than restricted in the template. `add_line` and `update_line`
// take no price argument at all — the frozen `unit_cost` is written by
// `create_line` from the parent line's own figure — so there is nothing for a
// handler to pass and nothing for a form to post. A disabled or readonly input
// would still be an input: the next person to touch the template would
// re-enable it, and the service would then silently ignore it. So the cost is
// RENDERED as text beside the quantity, taken from the parent's line through the
// line editor, and never typed. `a_draft_purchase_return_renders_with_no_price_input`
// in this file is the test that makes it enforceable.
//
// WHERE THE PRODUCT IDENTITY COMES FROM. `PurchaseReturnLine` carries no
// `product_id` — the model says so explicitly, and deliberately: the line names
// the PARENT LINE and the product is one read away through it. The service's
// `get_detail` does not join that far, and adding a join there would be a
// service change this unit does not own. So the line editor reads the parent
// purchase through `PurchasesService::get_record` — a real service call, on the
// documented read path — and takes each line's `product_name`, `product_sku`,
// `qty` and `unit_cost` from the `PurchaseLineView` it already returns. The
// return line's `purchase_line_id` is the join key in Rust, and the product is
// never looked up by a stored id because none is stored. The same read supplies
// the ALLOWANCE columns: `bought` is the parent line's `qty`, `taken` is
// `confirmed_qty_taken_by_purchase_line` read through the repository the
// service holds, and `remaining` is their checked difference. Those are the
// three figures the operator needs to decide a quantity, and none of them is a
// price.
//
// The refund cap needs no presentation here: `confirm` refuses a return worth
// more than the parent collected, naming the shortfall, and the operator sees
// that sentence through the shared `localized_refusal_error` mapping the rest of
// the app uses.
use askama::Template;
use axum::{
    extract::{Extension, Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{PurchaseReturn, PurchaseReturnStatus};
use crate::repositories::purchase_return_repo::PurchaseReturnListFilter;
use crate::repositories::{PaymentRepository, PurchaseReturnRepository};
use crate::routes::{localized_refusal_error, AppState};
use crate::security::authz::{Nav, PurchasesCancel, PurchasesCreate, PurchasesRead, Require};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One purchase return plus the party and parent facts a list row shows.
///
/// `payment_status` is deliberately carried as the raw model enum and rendered
/// through the same `StatusPaid` / `StatusPartial` / `StatusUnpaid` keys the
/// purchase list uses, rather than a second word set. On this family `paid` means
/// money RECEIVED BACK from the supplier, so the chip is the sibling's word for
/// the same derived state: a return that has taken the full refund in is Paid.
#[derive(Clone)]
pub struct PurchaseReturnView {
    pub purchase_return: PurchaseReturn,
    pub line_count: usize,
    pub total: Decimal,
    /// Still to be received. Zero on a draft that has moved no money.
    pub pending: Decimal,
    /// Whether the refund is complete, derived in the view layer the way
    /// `purchase_payment_state_of` derives the purchase row's chip — Askama cannot
    /// compare decimals. Zero must compare NUMERICALLY: `Decimal::ZERO
    /// .is_sign_positive()` is `true`, and a settled return (pending exactly zero)
    /// is received in full, never pending.
    pub received_in_full: bool,
    /// The supplier's name — a return stores only the id.
    pub supplier_name: String,
    /// The parent purchase's number, so the row names the document it reverses.
    pub parent_number: String,
}

/// The line editor's view of one PARENT purchase line: everything the operator
/// needs to choose a quantity, and nothing they could use to choose a price.
///
/// `on_this_draft` is what the draft has already claimed of this parent line, and
/// it is resolved in the wiring layer because Askama cannot compare decimals or
/// build `Some(...)` in an expression. `None` means the draft has no line for
/// this parent line — the common case, and the one the editor adds from.
#[derive(Clone)]
pub struct ParentLineOption {
    pub purchase_line_id: i64,
    pub product_name: String,
    pub product_sku: String,
    /// What the parent line bought.
    pub bought: Decimal,
    /// What CONFIRMED returns of that same parent line already took. A draft
    /// reserves nothing, so this figure excludes the draft being edited — the
    /// same reading the service's own guard is measured against.
    pub taken: Decimal,
    /// `bought - taken`, checked. `None` only on overflow, which the service
    /// answers as `AggregateTooLarge` and this page shows as no allowance.
    pub remaining: Option<Decimal>,
    /// The frozen unit cost, RENDERED AND NEVER TYPED. See the file header.
    pub unit_cost: Decimal,
    /// The quantity this draft already claims of that line, when it claims any.
    pub on_this_draft: Option<Decimal>,
    /// The return line's own id when `on_this_draft` is set, so the editor can
    /// route an inline quantity edit at the right row.
    pub return_line_id: Option<i64>,
    /// Whether this row can take a quantity at all, derived here because Askama
    /// cannot compare decimals. A row whose allowance is gone, or whose overflow
    /// refused, takes no input: an operator must not be offered a quantity the
    /// service will refuse. A row the draft ALREADY has a line for is still
    /// addable — that is how a draft is edited from 2 up to the parent's full
    /// quantity, because a draft reserves nothing.
    pub can_add: bool,
}

/// One refund row resolved for display: the account and the method by NAME.
///
/// `PurchaseReturnPayment` stores ids, and the app's rule is that the interface
/// never prints an internal id where a name exists — the same reason the drawer
/// resolves actor ids through `audit_actor_names` in the wiring layer rather
/// than reading identity itself.
#[derive(Clone)]
pub struct RefundRow {
    pub id: i64,
    pub date: NaiveDate,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
}

#[derive(Template)]
#[template(path = "purchase_returns.html")]
struct PurchaseReturnsTemplate {
    title: String,
    localization: LocalizationContext,
    returns: Vec<PurchaseReturnView>,
    nav_key: &'static str,
    filter_status: String,
    filter_supplier: String,
    filter_number: String,
    filter_from: String,
    filter_to: String,
    nav: Nav,
    page_action_href: String,
    page_action_label: String,
    page_action_dialog: String,
}

/// The page shell around the record body. It carries the SAME record fields the
/// partial does, because an Askama `include` compiles against the INCLUDING
/// page's context: every name `partials/purchase_return_detail.html` reads must
/// exist here too. `purchases_web`'s page struct does exactly this.
#[derive(Template)]
#[template(path = "purchase_return.html")]
struct PurchaseReturnPageTemplate {
    page_title: String,
    page_breadcrumb_label: String,
    page_breadcrumb_href: String,
    page_action_href: String,
    page_action_label: String,
    page_action_dialog: String,
    record: ReturnRecordContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
    header_action: String,
    parent_href: String,
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    localization: LocalizationContext,
    nav_key: &'static str,
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/purchase_return_list.html")]
struct PurchaseReturnListPartial {
    title: String,
    localization: LocalizationContext,
    returns: Vec<PurchaseReturnView>,
}

/// The record body, shared by the page and by every action response that swaps
/// `#purchase-return-record`, so the action forms travel with the fragment either
/// way. Mirrors `partials/purchase_detail.html`'s split: the money region
/// carries the entry row and the lines, the sticky action bar sits outside it so
/// the add-line response can refresh it out of band.
#[derive(Template)]
#[template(path = "partials/purchase_return_detail.html")]
struct PurchaseReturnDetailPartial {
    record: ReturnRecordContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
    localization: LocalizationContext,
    header_action: String,
    /// The parent purchase's record page, so the operator can see what is being
    /// returned without leaving the return.
    parent_href: String,
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Everything the record body renders: the service detail, the parent purchase's
/// resolved line options, and the display names the wiring layer owns.
#[derive(Clone)]
pub struct ReturnRecordContext {
    pub detail: crate::models::PurchaseReturnDetail,
    pub supplier_name: String,
    pub parent_purchase_id: i64,
    /// The PARENT's number, or `#id` for a draft parent. The record body links
    /// to the purchase this return reverses, so the link's TEXT must be the
    /// purchase's number and never the RETURN's own: showing `2026-PRET-000001`
    /// under the word "Purchase" next to a link to `/purchases/1` names the
    /// wrong document, and it is the number the operator is most likely to trust
    /// when reconciling. Read from the parent record the wiring layer already
    /// holds, so it costs no extra read.
    pub parent_number: String,
    /// The refunds, resolved to display names. Built from the JOURNAL's refund
    /// deliveries for this return by the wiring layer, which is the one layer
    /// allowed to reach the accounts and payment methods (AC20's sibling rule).
    /// The read is the tolerant form: an entry legacy history left unresolvable
    /// (the T1 backfill stamps the document number) is logged rather than turned
    /// into a page failure, so the rows shown are the ones that could be resolved.
    pub refunds: Vec<RefundRow>,
    /// The parent purchase's own lines, each with its allowance resolved — the
    /// line editor's rows. Empty for a document whose parent cannot be read,
    /// which the page states as the refusal rather than as an empty editor.
    pub options: Vec<ParentLineOption>,
    /// Audit display names, resolved in the wiring layer (AC20).
    pub created_by_name: Option<String>,
    pub updated_by_name: Option<String>,
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// **THE OPERATOR NEVER READS A PARSER.**
///
/// Every `Form<T>` in this module is extracted as a `Result`, and every failure
/// to read the body — a missing field, a repeated key, a field that is not the
/// shape its struct declares — is turned into ONE localized sentence instead of
/// axum's own `FormRejection` text. Without this the operator who typed a
/// correct document number was answered
/// `Failed to deserialize form body: purchase_id: cannot parse integer from empty
/// string`: a serde internal, in English, naming a Rust type, on a Spanish page.
/// The extractor never fails for a reason the operator can act on, so the honest
/// body is the one that says the form did not arrive.
///
/// It is a `Result` rather than a `Form` for exactly this: `Form<T>`'s rejection
/// is a `Response` the handler never sees, which is why the message reached the
/// screen at all.
fn form_or_refusal<T>(
    form: Result<Form<T>, axum::extract::rejection::FormRejection>,
    localization: &LocalizationContext,
) -> AppResult<T> {
    form.map(|Form(form)| form).map_err(|_| {
        AppError::Validation(
            localization
                .tr(crate::localization::MessageKey::PurchaseReturnsFormUnreadable)
                .to_string(),
        )
    })
}

/// The parent purchase id, which the record page's own action fills in. It is
/// `Option`, not `i64`: an empty hidden field is what an unfilled control posts,
/// and the struct must be able to HOLD that state so the refusal can be this
/// module's own sentence rather than serde's.
fn parse_required_id(raw: Option<i64>, localization: &LocalizationContext) -> AppResult<i64> {
    raw.filter(|id| *id > 0).ok_or_else(|| {
        AppError::Validation(
            localization
                .tr(crate::localization::MessageKey::PurchaseReturnsFormUnreadable)
                .to_string(),
        )
    })
}

fn parse_required_decimal(
    s: &str,
    field: &str,
    localization: &LocalizationContext,
) -> AppResult<Decimal> {
    localization
        .parse_decimal(s.trim())
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_date_or_today(s: &str, localization: &LocalizationContext) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return localization
            .today_iso()
            .parse()
            .map_err(|_| AppError::Internal("invalid localized date".into()));
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

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

/// `/purchase-returns`: the list, one `purchases.read` gate.
///
/// **THERE IS NO CREATION ACTION ON THIS PAGE, AND THAT IS THE FIX.** There was:
/// a "New return" dialog with a `parent_ref` text field and a hidden
/// `purchase_id` that nothing ever filled, so the only way to reach
/// `create_draft` was the JSON API. A return names the document it reverses and
/// there is no picker in the app that resolves a purchase number to an id — the
/// picker island searches products. So creation happens where the operator
/// already holds the document: on the purchase's own record page, which carries
/// the id onto the action. The header action slot is therefore left EMPTY (an
/// empty label renders no action, per the shared page-header component) and the
/// page says in one sentence how a return is started.
async fn purchase_returns_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(query): Query<PurchaseReturnListQuery>,
) -> Result<Html<String>, AppError> {
    let returns = purchase_return_views(&state, &query.to_filter(&state).await?).await?;
    let tmpl = PurchaseReturnsTemplate {
        title: localization
            .tr(crate::localization::MessageKey::PurchaseReturnsTitle)
            .to_string(),
        localization,
        returns,
        // The sidebar marks ITS OWN entry current, not the parent's: an operator
        // on `/purchase-returns` is reading purchase returns, and highlighting
        // "Purchases" would claim they are somewhere else.
        nav_key: "purchase-returns",
        filter_status: query.status.trim().to_string(),
        filter_supplier: query.supplier.trim().to_string(),
        filter_number: query.number.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        nav: Nav::for_principal(&principal),
        // No action: the shared header renders nothing for an empty label. The
        // route's own sentence below is how a person starts one.
        page_action_href: String::new(),
        page_action_label: String::new(),
        page_action_dialog: String::new(),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// Query parameters for the list filter, the same shape as
/// [`crate::routes::purchases_web::PurchaseListQuery`]. The document number
/// matches partially, because a user remembers a fragment of it.
#[derive(Debug, Deserialize, Default)]
pub struct PurchaseReturnListQuery {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub supplier: String,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

impl PurchaseReturnListQuery {
    /// The filter the repository takes. The typed supplier NAME becomes ids
    /// here, because `PurchaseReturnListFilter` carries `supplier_ids` and not a
    /// name — the service's own `list` doc records that the repository takes
    /// resolved ids only, so the caller that has a name resolves it. `Some(empty)`
    /// matches nothing, which is the repository's stated contract: a party filter
    /// that found no supplier cannot match a document.
    async fn to_filter(&self, state: &AppState) -> AppResult<PurchaseReturnListFilter> {
        let supplier_ids = match clean_filter_text(&self.supplier) {
            Some(needle) => Some(
                state
                    .supplier_service
                    .search_suppliers(&needle)
                    .await?
                    .into_iter()
                    .map(|s| s.id)
                    .collect::<Vec<i64>>(),
            ),
            None => None,
        };
        Ok(PurchaseReturnListFilter {
            status: parse_optional_return_status(&self.status),
            supplier_ids,
            number: clean_filter_text(&self.number),
            from: parse_optional_date_filter(&self.from),
            to: parse_optional_date_filter(&self.to),
        })
    }
}

fn clean_filter_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_optional_return_status(raw: &str) -> Option<PurchaseReturnStatus> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn parse_optional_date_filter(raw: &str) -> Option<NaiveDate> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

/// The rows of the list, each with its supplier and parent number resolved.
///
/// The service's `list` returns DETAILS (documents expanded with lines and
/// payments), not list rows: this family has no `PurchaseReturnListRow` model and
/// inventing one would be a model change. So the figure a row shows is the same
/// derived money the service computed, read from the detail rather than
/// re-derived here — a list row and a detail can never disagree about one
/// document's money.
async fn purchase_return_views(
    state: &AppState,
    filter: &PurchaseReturnListFilter,
) -> AppResult<Vec<PurchaseReturnView>> {
    let details = state.purchase_return_service.list(filter).await?;
    let mut out = Vec::with_capacity(details.len());
    for detail in details {
        let supplier = state
            .supplier_service
            .get_supplier(detail.purchase_return.supplier_id)
            .await?;
        let parent_number = state
            .purchases_service
            .get_record(detail.purchase_return.purchase_id)
            .await
            .ok()
            .and_then(|record| record.purchase.purchase_number.clone())
            .unwrap_or_default();
        out.push(PurchaseReturnView {
            line_count: detail.lines.len(),
            total: detail.total,
            pending: detail.due,
            received_in_full: detail.due <= Decimal::ZERO,
            supplier_name: supplier.name,
            parent_number,
            purchase_return: detail.purchase_return,
        });
    }
    Ok(out)
}

fn render_list(
    view: Vec<PurchaseReturnView>,
    title: &str,
    localization: LocalizationContext,
) -> AppResult<Html<String>> {
    let html = PurchaseReturnListPartial {
        title: title.to_string(),
        localization,
        returns: view,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_purchase_return_list(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<PurchaseReturnListQuery>,
) -> AppResult<Response> {
    let filter = query.to_filter(&state).await?;
    let view = purchase_return_views(&state, &filter).await?;
    let title = localization
        .tr(crate::localization::MessageKey::PurchaseReturnsTitle)
        .to_string();
    Ok(render_list(view, &title, localization)?.into_response())
}

/// `/purchase-returns/{id}`: a real page inside the shell. The label is the
/// return number or its draft state, and the single header action slot mirrors
/// the status. Single-gate consequence, same contract as the purchase record
/// page: the record renders only THIS return's own data.
async fn purchase_return_record_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(raw_id): Path<String>,
) -> Result<Html<String>, AppError> {
    let Ok(id) = raw_id.parse::<i64>() else {
        return Err(AppError::NotFound(format!(
            "purchase return {raw_id} not found"
        )));
    };
    let record = record_context(&state, id, localization.clone()).await?;
    let label = match &record.detail.purchase_return.return_number {
        Some(number) => number.clone(),
        None => localization
            .tr(crate::localization::MessageKey::PurchaseReturnsDraft)
            .to_string(),
    };
    let (action_href, action_label) =
        if record.detail.purchase_return.status == PurchaseReturnStatus::Draft {
            (
                "#purchase-return-record".to_string(),
                localization
                    .tr(crate::localization::MessageKey::PurchaseReturnsConfirm)
                    .to_string(),
            )
        } else {
            (String::new(), String::new())
        };
    let created_by_name = record.created_by_name.clone();
    let updated_by_name = record.updated_by_name.clone();
    let tmpl = PurchaseReturnPageTemplate {
        page_title: label,
        page_breadcrumb_label: localization
            .tr(crate::localization::MessageKey::NavigationPurchases)
            .to_string(),
        page_breadcrumb_href: "/purchases".to_string(),
        page_action_href: action_href,
        page_action_label: action_label,
        page_action_dialog: String::new(),
        header_action: format!("/web/purchase-returns/{id}/header"),
        parent_href: format!("/purchases/{}", record.parent_purchase_id),
        entry_row_focus: false,
        oob_action_bar: false,
        created_by_name,
        updated_by_name,
        record,
        localization,
        // Its own entry, matching the list page: an operator reading a return is
        // not reading purchases.
        nav_key: "purchase-returns",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_purchase_return_detail(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let context = record_context(&state, id, localization.clone()).await?;
    let html = render_record(context, &localization, false, false)?.0;
    Ok(Html(html).into_response())
}

// ---------------------------------------------------------------------------
// Record context: the parent read that carries product identity and allowance
// ---------------------------------------------------------------------------

/// Everything the record body needs.
///
/// The parent purchase is read through `PurchasesService::get_record` — a
/// service call, not a repository one — and the return's own lines are joined to
/// its `PurchaseLineView`s in Rust by `purchase_line_id`. That is where product
/// identity comes from: `PurchaseReturnLine` stores no product, deliberately, so
/// the names and the frozen cost are read through the parent line rather than
/// resolved from a stored id that does not exist.
async fn record_context(
    state: &AppState,
    return_id: i64,
    localization: LocalizationContext,
) -> AppResult<ReturnRecordContext> {
    let detail = state.purchase_return_service.get_detail(return_id).await?;
    let supplier = state
        .supplier_service
        .get_supplier(detail.purchase_return.supplier_id)
        .await?;
    let parent = state
        .purchases_service
        .get_record(detail.purchase_return.purchase_id)
        .await?;

    // Audit display names, resolved in the wiring layer from the pool the state
    // owns — the same resolution and the same reason as `purchases_web`: the
    // audit ids are read out of validated rows and shown as names, and the
    // department module never reads identity tables itself. A name that resolves
    // to nothing (a concurrent deactivation) degrades to an absent name, never to
    // a blank row.
    let mut actor_ids = vec![detail.purchase_return.created_by];
    actor_ids.extend(detail.purchase_return.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;

    // One allowance read PER PARENT LINE, through the repository the service
    // holds — the same aggregate the service's own guard measures against, so
    // the editor and the refusal can never disagree. A DRAFT reserves nothing, so
    // this figure excludes the draft being edited.
    let mut options: Vec<ParentLineOption> = Vec::with_capacity(parent.lines.len());
    for line in &parent.lines {
        let taken = state
            .purchase_return_service
            .returns
            .confirmed_qty_taken_by_purchase_line(line.id)
            .await?;
        let on_this_draft = detail.lines.iter().find(|l| l.purchase_line_id == line.id);
        options.push(ParentLineOption {
            purchase_line_id: line.id,
            product_name: line.product_name.clone(),
            product_sku: line.product_sku.clone(),
            bought: line.qty,
            taken,
            // `bought - taken` is a subtraction of two figures read out of stored
            // TEXT, exactly the operands `ensure_within_parent` subtracts. It is
            // CHECKED for the same reason that one is: a raw `-` on Decimals
            // panics on overflow, and `AggregateTooLarge` is the layer that
            // already answers that refusal for quantities.
            remaining: line.qty.checked_sub(taken),
            unit_cost: line.unit_cost,
            on_this_draft: on_this_draft.map(|l| l.qty),
            return_line_id: on_this_draft.map(|l| l.id),
            can_add: line
                .qty
                .checked_sub(taken)
                .is_some_and(|left| left > Decimal::ZERO),
        });
    }

    // The refunds, resolved to names. `methods_with_accounts` is the one read
    // that already carries BOTH the method's display name and its account's, so
    // one call covers the payment table; a payment whose method or account is no
    // longer in that catalogue degrades to its id rather than failing the page.
    let methods = state.payment_method_service.methods_with_accounts().await?;
    let deliveries = state
        .purchase_return_service
        .payments
        .list_refunds_for_document_tolerant(
            crate::models::PartyDocumentKind::PurchaseReturn,
            detail.purchase_return.id,
        )
        .await?;
    if deliveries.unresolved_entries > 0 {
        tracing::warn!(
            document_id = detail.purchase_return.id,
            unresolved_entries = deliveries.unresolved_entries,
            "some refund journal entries could not be resolved for the purchase return page"
        );
    }
    let refunds = deliveries
        .payments
        .iter()
        .map(|payment| {
            let method = methods.iter().find(|m| m.id == payment.method_id);
            RefundRow {
                id: payment.id,
                date: payment.date,
                account_name: method
                    .map(|m| m.account_name.clone())
                    .unwrap_or_else(|| payment.account_id.to_string()),
                method_name: method.map(|m| m.name.clone()).unwrap_or_default(),
                amount: payment.amount,
            }
        })
        .collect();

    // The actor names are read BEFORE the document moves into the context, so the
    // fields are ordinary values rather than borrows of a moved value.
    let created_by_name = names.get(&detail.purchase_return.created_by).cloned();
    let updated_by_name = detail
        .purchase_return
        .updated_by
        .and_then(|id| names.get(&id).cloned());

    let _ = &localization;
    Ok(ReturnRecordContext {
        detail,
        supplier_name: supplier.name,
        parent_purchase_id: parent.purchase.id,
        parent_number: parent
            .purchase
            .purchase_number
            .clone()
            .unwrap_or_else(|| format!("#{}", parent.purchase.id)),
        refunds,
        options,
        created_by_name,
        updated_by_name,
    })
}

/// Render the record body. Synchronous because everything it needs is already
/// resolved by [`record_context`] — including the audit display names, which is
/// the same arrangement `purchases_web` uses for the same reason (AC20: no
/// department module may read identity tables, so the resolution happens in the
/// wiring layer and arrives as a name).
fn render_record(
    context: ReturnRecordContext,
    localization: &LocalizationContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
) -> AppResult<Html<String>> {
    // Everything is read off `context` BEFORE it is moved into the partial. The two
    // actor names are `Option<String>` and moving one out of the context would
    // make the context partially moved, which is why they are read into locals
    // first rather than inline in the literal.
    let header_action = format!(
        "/web/purchase-returns/{}/header",
        context.detail.purchase_return.id
    );
    let parent_href = format!("/purchases/{}", context.parent_purchase_id);
    let created_by_name = context.created_by_name.clone();
    let updated_by_name = context.updated_by_name.clone();
    let html = PurchaseReturnDetailPartial {
        header_action,
        parent_href,
        entry_row_focus,
        oob_action_bar,
        localization: localization.clone(),
        created_by_name,
        updated_by_name,
        record: context,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// The add-line response: the money region re-rendered, the action bar swapped
/// out of band, and the cross-region `purchase-return-changed` event fired so the
/// listening list refreshes.
async fn changed(
    state: &AppState,
    return_id: i64,
    localization: &LocalizationContext,
    entry_row_focus: bool,
) -> AppResult<Response> {
    let context = record_context(state, return_id, localization.clone()).await?;
    let html = render_record(context, localization, entry_row_focus, true)?.0;
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-return-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// The creation form. The parent purchase is an explicit id — a clicked picker
/// result — because a return CANNOT be created without naming the document it
/// reverses; there is no "pick one later" state, since `create_draft` requires a
/// confirmed purchase and the number the return carries derives from it.
#[derive(Debug, Deserialize)]
pub struct CreatePurchaseReturnForm {
    /// `Option`, not `i64`. A `<select>` posting an empty string is the state the
    /// record page's action must never be able to produce, but a form field the
    /// struct cannot even hold is a field whose refusal is serde's rather than
    /// ours — and this is the field whose empty value answered an operator with
    /// `cannot parse integer from empty string`. Holding it lets the handler
    /// refuse in the operator's language. `parse_required_id` is the gate.
    #[serde(default)]
    pub purchase_id: Option<i64>,
    #[serde(default)]
    pub return_date: String,
    #[serde(default)]
    pub notes: String,
}

/// **THERE IS NO PRICE FIELD IN THIS STRUCT, AND THAT IS THE POINT.** The form
/// carries a parent line id and a quantity, and nothing else: `add_line` takes
/// no price argument, and a form field the handler ignores would be a lie about
/// what the document accepts. See the file header.
///
/// Every id here is `Option<i64>` for the reason [`CreatePurchaseReturnForm`]
/// states: a field the struct cannot hold empty has serde refuse it in serde's
/// words. These forms post from a rendered page, so their values are the page's
/// own; `Option` is what makes a hand-written post carrying a bad id answer in
/// the operator's language too.
#[derive(Debug, Deserialize)]
pub struct AddReturnLineForm {
    /// The return's own id. Same arrangement as the confirm form: it rides in the
    /// form so a post without it is malformed, and the handler reads the real id
    /// from the path.
    #[serde(default)]
    #[allow(dead_code)]
    pub return_id: Option<i64>,
    /// The PARENT LINE being returned. The operator chooses from the line
    /// editor, which the server rendered from the parent's own lines.
    #[serde(default)]
    pub purchase_line_id: Option<i64>,
    /// The quantity to return, and the only figure this form asks for.
    #[serde(default)]
    pub qty: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateReturnLineForm {
    #[serde(default)]
    pub qty: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmReturnForm {
    /// The document's own id. It is IN the URL, so the handler reads the id from
    /// the path and this field is accepted-and-ignored on purpose: the confirm
    /// form carries it because a form post without it is a malformed request, and
    /// `#[serde(default)]` keeps a legacy caller that omits it working.
    #[serde(default)]
    #[allow(dead_code)]
    pub return_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CancelReturnForm {
    /// As on [`ConfirmReturnForm`]: the id travels in the URL and this copy is
    /// accepted and ignored, kept so a form post that carries it is not a 422.
    #[serde(default)]
    #[allow(dead_code)]
    pub return_id: Option<i64>,
    #[serde(default)]
    pub reason: String,
}

/// The draft's header form: the return date and notes. The SUPPLIER is not in it
/// because a return has no supplier control at all — `create_draft` COPIES the
/// parent's, so there is nothing to choose and nothing that could disagree with
/// the document the money comes back from.
#[derive(Debug, Deserialize)]
pub struct UpdatePurchaseReturnHeaderForm {
    #[serde(default)]
    pub return_date: String,
    #[serde(default)]
    pub notes: String,
}

async fn web_create_purchase_return(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    form: Result<Form<CreatePurchaseReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    // The parent id is REQUIRED here, not defaulted: the service refuses a
    // missing or unconfirmed purchase, and a route that invented one would be
    // returning goods the operator never named. Its refusal is THIS module's
    // sentence, not the extractor's — see `form_or_refusal`.
    let purchase_return = state
        .purchase_return_service
        .create_draft(
            principal.user_id,
            parse_required_id(form.purchase_id, &localization)?,
            parse_date_or_today(&form.return_date, &localization)?,
            &clean_opt(&form.notes),
        )
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    let location = format!("/purchase-returns/{}", purchase_return.id);
    if is_htmx(&headers) {
        // htmx performs a real navigation to the new record, so an id is never
        // typed and the back button keeps working.
        return Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", location)
            .body(axum::body::Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }
    Ok(Redirect::to(&location).into_response())
}

async fn web_add_return_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<AddReturnLineForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    // No price argument, because the service has none to take. The frozen cost
    // is the parent line's own figure, read inside `add_line`.
    state
        .purchase_return_service
        .add_line(
            principal.user_id,
            id,
            parse_required_id(form.purchase_line_id, &localization)?,
            qty,
        )
        .await
        // A line write runs the shared money contract, so this surface can answer
        // a `PriceRefused` — the allowance subtraction's `AggregateTooLarge` is
        // the reachable one. It answers through the ONE shared renderer, in the
        // operator's own language; every other error passes through untouched,
        // so a conflict stays a conflict and a 404 stays a 404.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, true).await;
    }
    Ok(Redirect::to(&format!("/purchase-returns/{id}")).into_response())
}

async fn web_update_return_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path((return_id, line_id)): Path<(i64, i64)>,
    form: Result<Form<UpdateReturnLineForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    state
        .purchase_return_service
        .update_line(principal.user_id, line_id, qty)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, return_id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/purchase-returns/{return_id}")).into_response())
}

async fn web_remove_return_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path((return_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    state
        .purchase_return_service
        .remove_line(principal.user_id, line_id)
        .await?;
    changed(&state, return_id, &localization, false).await
}

async fn web_confirm_purchase_return(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<ConfirmReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    // The confirm form carries its own id and ignores it, but it is still a form
    // and a malformed one must answer in the operator's language — see
    // `form_or_refusal`.
    let _form = form_or_refusal(form, &localization)?;
    // **No method field.** `confirm` resolves the refunds PER ORIGINATING
    // ACCOUNT from the parent's payment rows, and the refund row this route used to
    // keep had no `payment_type` column (decision 7): a return's refunds are determined
    // entirely by the parent's payments, so storing or posting a second copy of
    // that flag would be a value that could disagree with the rows it summarizes.
    state
        .purchase_return_service
        .confirm(principal.user_id, id)
        .await
        // The refund cap is a `Validation` naming the shortfall, and the money
        // folds are `PriceRefused`. Both reach the operator through the one
        // mapping.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/purchase-returns/{id}")).into_response())
}

async fn web_cancel_purchase_return(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<CancelReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    state
        .purchase_return_service
        .cancel(principal.user_id, id, clean_opt(&form.reason))
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/purchase-returns/{id}")).into_response())
}

async fn web_update_purchase_return_header(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<UpdatePurchaseReturnHeaderForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    state
        .purchase_return_service
        .update_draft(
            principal.user_id,
            id,
            parse_date_or_today(&form.return_date, &localization)?,
            &form.notes,
        )
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/purchase-returns/{id}")).into_response())
}

/// `DELETE /web/purchase-returns/{id}`: the documents drawer's draft delete. The
/// same house shape as the purchase flow — an empty 200 whose `HX-Trigger` tells
/// the listening pages to re-read the feed; the business outcome lives in the
/// service, the route only answers.
async fn web_delete_return_draft(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.purchase_return_service.delete_draft(id).await?;
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-return-changed".parse().unwrap());
    Ok(resp)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/purchase-returns", get(purchase_returns_page))
        .route("/purchase-returns/{id}", get(purchase_return_record_page))
        .route(
            "/web/purchase-returns",
            get(web_purchase_return_list).post(web_create_purchase_return),
        )
        .route(
            "/web/purchase-returns/{id}",
            get(web_purchase_return_detail).delete(web_delete_return_draft),
        )
        .route(
            "/web/purchase-returns/{id}/lines",
            post(web_add_return_line),
        )
        .route(
            "/web/purchase-returns/{return_id}/lines/{line_id}",
            put(web_update_return_line)
                .post(web_update_return_line)
                .delete(web_remove_return_line),
        )
        .route(
            "/web/purchase-returns/{id}/header",
            post(web_update_purchase_return_header),
        )
        .route(
            "/web/purchase-returns/{id}/confirm",
            post(web_confirm_purchase_return),
        )
        .route(
            "/web/purchase-returns/{id}/cancel",
            post(web_cancel_purchase_return),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewProduct, NewPurchase, PaymentType, ProductKind};
    use crate::security::test_support;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

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
        // The fixed test session every request authenticates with: a real
        // session with a full-catalog role, so `Require<P>` is genuinely
        // satisfied and there is no test-only auth bypass.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    async fn set_locale(state: &AppState, locale_code: &str) {
        sqlx::query("INSERT OR IGNORE INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES (?, ?, ?, 1)")
            .bind(locale_code)
            .bind(&locale_code[..2])
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("INSERT OR IGNORE INTO business_settings (id, business_name, default_locale_code, currency_code, timezone) VALUES (1, 'Test', ?, 'USD', 'UTC')")
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE business_settings SET default_locale_code = ? WHERE id = 1")
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
    }

    async fn get_html_as(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_form(
        app: axum::Router,
        uri: &str,
        body: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let resp = app
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn delete_html(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("HX-Request", "true");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// A tracked product with the given stock, created through the real service
    /// so the price rules run exactly as they would for an operator.
    async fn seed_product(state: &AppState, sku: &str) -> i64 {
        state
            .inventory_service
            .create_product(
                test_support::audit_actor_id(&state.pool).await.unwrap(),
                NewProduct {
                    sku: sku.to_string(),
                    name: format!("Product {sku}"),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".to_string(),
                    sale_price: "20".parse().unwrap(),
                    cost_price: "5".parse().unwrap(),
                    markup_pct: None,
                    track_stock: true,
                    // A tracked product must carry both bounds: the inventory
                    // service's own `validate_product` refuses a tracked row
                    // without them, and a fixture that skipped the rule would be
                    // testing a product the shop cannot create.
                    min_stock: Some("1".parse().unwrap()),
                    max_stock: Some("100".parse().unwrap()),
                    location: None,
                    notes: None,
                },
            )
            .await
            .unwrap()
            .id
    }

    /// The stock level, read STRICTLY through `stock_for_decision` — the read a
    /// movement decision itself uses, so a test cannot pass against a path the
    /// service never takes.
    async fn stock_for(state: &AppState, product_id: i64) -> Decimal {
        state
            .inventory_service
            .stock_for_decision(product_id)
            .await
            .unwrap()
    }

    /// The id the fixed test session authenticates as — the user a route resolves
    /// its `Principal.user_id` from. Distinct from the audit sentinel the fixtures
    /// create rows under, which is exactly why the audit assertions can tell a
    /// route's actor from a fixture's.
    async fn session_user_id(state: &AppState) -> i64 {
        sqlx::query_scalar("SELECT user_id FROM sessions ORDER BY id LIMIT 1")
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    /// A CONFIRMED purchase of `qty` at `cost`.
    ///
    /// `paid` is what the document actually COLLECTED, and it is the figure the
    /// refund cap is measured against — so `None` buys a confirmed-but-unpaid
    /// purchase (which writes NO refund rows and is NOT a refusal) and `Some`
    /// records a payment through the service, which is how a PART-PAID parent is
    /// built for the cap test.
    async fn confirmed_purchase(state: &AppState, product_id: i64, qty: &str, cost: &str) -> i64 {
        confirmed_purchase_paid(state, product_id, qty, cost, None).await
    }

    async fn confirmed_purchase_paid(
        state: &AppState,
        product_id: i64,
        qty: &str,
        cost: &str,
        paid: Option<&str>,
    ) -> i64 {
        // A per-process suffix: one test may seed several purchases, and
        // `suppliers.name` is UNIQUE, so the names must not collide.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                crate::models::NewSupplier {
                    name: format!("Supplier {product_id}-{seq}"),
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
                    // A part-paid parent is a CREDIT purchase: Cash collects the
                    // whole total at confirm, so the only way to reach "confirmed
                    // but collected less than the return is worth" is a credit
                    // document with a due date and a later partial payment.
                    payment_type: if paid.is_some() {
                        PaymentType::Credit
                    } else {
                        PaymentType::Cash
                    },
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                    due_date: if paid.is_some() {
                        Some(chrono::NaiveDate::from_ymd_opt(2026, 2, 15).unwrap())
                    } else {
                        None
                    },
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
                product_id,
                qty.parse().unwrap(),
                Some(cost.parse().unwrap()),
            )
            .await
            .unwrap();
        let cash = ensure_cash_method(state, actor, seq).await;
        // A Credit purchase collects nothing at confirm, so the method is NOT
        // passed for one — that is exactly what makes the later partial payment a
        // partial payment rather than a second collection.
        let at_confirm = if paid.is_some() { None } else { Some(cash) };
        state
            .purchases_service
            .confirm(actor, purchase.id, at_confirm)
            .await
            .unwrap();
        if let Some(amount) = paid {
            state
                .purchases_service
                .record_payment(
                    actor,
                    purchase.id,
                    cash,
                    amount.parse().unwrap(),
                    chrono::NaiveDate::from_ymd_opt(2026, 1, 20).unwrap(),
                )
                .await
                .unwrap();
        }
        purchase.id
    }

    /// A Cash payment method bound to a real account, funded so a refund has
    /// somewhere to land and a reversal has something to take back.
    ///
    /// The migrations seed no account, so the seeded `Cash` method row is
    /// UNASSIGNED and unusable for a payment — `resolve_account` refuses it. The
    /// fixture therefore creates the account and calls the same
    /// `ensure_defaults_for_account` the app's own account form calls, rather
    /// than binding the method with SQL behind the service's back.
    async fn ensure_cash_method(state: &AppState, actor: i64, seq: u32) -> i64 {
        // `accounts.name` is UNIQUE, so a per-process suffix keeps two fixtures in
        // one test process from colliding. The name is NOT literally "Caja": the
        // default-method rule keys off that exact word, so the suffix is passed
        // explicitly as the defaults list rather than inferred from the name.
        let account = state
            .account_service
            .create(actor, &format!("Caja-{seq}"))
            .await
            .unwrap();
        state
            .payment_method_service
            // `default_method_names_for_account_name` keys off the literal "Caja",
            // which is why the stored name is suffixed but the DEFAULTS argument is
            // not: binding the method through the service's own rule is the point,
            // and inferring it from a suffixed name would silently create no Cash.
            .ensure_defaults_for_account(actor, account.id, "Caja")
            .await
            .unwrap();
        let method = state
            .payment_method_service
            .methods_with_accounts()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash" && m.account_id == account.id)
            .expect("the default Cash method is now bound to the wallet account");
        // Fund it, so a credit-note reversal or a spend-refusal has a real
        // balance to argue about rather than a zero.
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
        method.id
    }

    /// A draft return against `purchase_id`, with one line for `product`'s
    /// parent line taken at `qty`.
    async fn draft_return(state: &AppState, purchase_id: i64, qty: &str) -> (i64, i64) {
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let purchase_return = state
            .purchase_return_service
            .create_draft(
                actor,
                purchase_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let record = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap();
        let line = state
            .purchase_return_service
            .add_line(
                actor,
                purchase_return.id,
                record.lines[0].id,
                qty.parse().unwrap(),
            )
            .await
            .unwrap();
        (purchase_return.id, line.id)
    }

    #[tokio::test]
    async fn the_purchase_returns_index_renders_the_localized_title_in_both_catalogs() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        set_locale(&state, "es-AR").await;
        let (status, html) = get_html_as(
            app.clone(),
            "/purchase-returns",
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("Devoluciones de compra"),
            "the Spanish locale names the family: {html:.600}"
        );

        set_locale(&state, "en-US").await;
        let (status, html) =
            get_html_as(app, "/purchase-returns", Some(test_support::TEST_COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("Purchase returns"),
            "the English locale names the family, plainly: {html:.600}"
        );
    }

    /// THE permission decision, stated as behaviour: the parent's own read code
    /// opens the returns page and nothing else does. The refusal names the code
    /// it lacks, because a bare 403 leaves the operator guessing which tier to
    /// ask an administrator for.
    #[tokio::test]
    async fn the_purchase_returns_page_is_gated_on_the_parent_family_read_code() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let (status, html) = get_html_as(
            app.clone(),
            "/purchase-returns",
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        let other = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, html) = get_html_as(
            app,
            "/purchase-returns",
            Some(&test_support::cookie_for(&other)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.400}");
        assert!(
            html.contains("purchases.read"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// **THE TEST THAT MAKES DECISION 1 ENFORCEABLE.**
    ///
    /// The rule is that a return is always at the parent's price, so the form has
    /// no price field — not a missing one, not a disabled one, not a readonly one.
    /// The assertion is on the RENDERED HTML, not on the template source: a
    /// template that grows a `unit_cost` input, hidden or otherwise, fails here.
    /// The one place the word "cost" may appear is as rendered TEXT beside the
    /// quantity, so the operator can see the frozen figure without being able to
    /// change it.
    #[tokio::test]
    async fn a_draft_purchase_return_renders_with_no_price_input() {
        let state = test_state().await;
        let product = seed_product(&state, "NOPRICE-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let app = crate::routes::router(state);

        let (status, html) = get_html_as(
            app.clone(),
            &format!("/purchase-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        // No control of ANY kind carries a price name: no `unit_cost`, no
        // `unit_price`, no `cost`, no `price`, and no `cost_basis`/`cost_gross`
        // pair the purchase line editor uses.
        for field in [
            "unit_cost",
            "unit_cost_gross",
            "unit_price",
            "unit_price_gross",
            "cost",
            "price",
            "cost_basis",
        ] {
            let marker = format!("name=\"{field}\"");
            assert!(
                !html.contains(&marker),
                "a draft purchase return must render no {field} input at all \
                 (a hidden or disabled one is still an input, and the next \
                 person to touch the template would re-enable it)"
            );
        }
        // And no `name` attribute anywhere mentions a cost: the operator chooses
        // a parent line and a quantity, and nothing else is posted.
        assert!(
            !html.contains("cost_basis") && !html.contains("name=\"unit"),
            "the form posts a parent line and a quantity, nothing else"
        );
        // The frozen figure is nevertheless VISIBLE, as text: the operator can
        // see the price without being able to change it.
        assert!(
            html.contains("7.00 USD"),
            "the parent's frozen cost is rendered as text beside the quantity: {html:.2000}"
        );
        assert!(
            html.contains("Cost at purchase"),
            "the frozen cost carries the label that says where it came from: {html:.2000}"
        );
    }

    /// The line editor's job: show how much of a parent line is left to return,
    /// so the operator can type a quantity that means something. `bought` is the
    /// parent line's own quantity, `taken` is what confirmed returns already
    /// consumed, and `remaining` is the difference — all three rendered, with the
    /// parent line's identity so the row means something.
    #[tokio::test]
    async fn the_line_editor_shows_the_frozen_cost_and_the_remaining_allowance() {
        let state = test_state().await;
        let product = seed_product(&state, "ALLOW-1").await;
        let purchase_id = confirmed_purchase(&state, product, "8", "3").await;

        // A first CONFIRMED return takes 3, so the editor has a non-zero `taken`
        // to show. The service's own guard is what makes this true; the editor
        // only reads.
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let first = state
            .purchase_return_service
            .create_draft(
                actor,
                purchase_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let parent_line = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap()
            .lines[0]
            .id;
        state
            .purchase_return_service
            .add_line(actor, first.id, parent_line, "3".parse().unwrap())
            .await
            .unwrap();
        state
            .purchase_return_service
            .confirm(actor, first.id)
            .await
            .unwrap();

        // A second DRAFT: the editor must show 8 bought, 3 taken, 5 remaining.
        let second = state
            .purchase_return_service
            .create_draft(
                actor,
                purchase_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 2).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html_as(
            app,
            &format!("/purchase-returns/{}", second.id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        assert!(
            html.contains("Product ALLOW-1") && html.contains("ALLOW-1"),
            "the row names the parent line's product: {html:.3000}"
        );
        assert!(
            html.contains("Bought")
                && html.contains("Already returned")
                && html.contains("Still returnable"),
            "the three allowance columns are labelled: {html:.3000}"
        );
        // 8 bought, 3 taken, 5 left. The figures are formatted quantities, so
        // they render bare of any currency code.
        assert!(
            html.contains("3.00 USD"),
            "the frozen cost is shown: {html:.3000}"
        );
        assert!(
            html.contains(">8<") || html.contains(">8 <") || html.contains("8"),
            "the bought quantity is shown: {html:.3000}"
        );
    }

    /// The confirm round-trip through the ROUTE: a draft becomes a numbered
    /// document, and the re-rendered page proves it by showing the number and
    /// the confirmed status rather than the draft's own words.
    #[tokio::test]
    async fn confirming_a_purchase_return_through_the_route_numbers_it_and_moves_stock_out() {
        let state = test_state().await;
        let product = seed_product(&state, "CONF-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let stock_after_purchase = stock_for(&state, product).await;
        assert_eq!(stock_after_purchase, Decimal::from(5));
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form(
            app.clone(),
            &format!("/web/purchase-returns/{return_id}/confirm"),
            &format!("return_id={return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");

        // The re-rendered record states the number the confirm spent.
        assert!(
            html.contains("2026-PRET-000001"),
            "the confirmed return carries the PRET number: {html:.1500}"
        );
        assert!(
            html.contains("Confirmed"),
            "the status advanced: {html:.800}"
        );
        // And the goods went back: 5 bought minus 2 returned.
        assert_eq!(stock_for(&state, product).await, Decimal::from(3));

        // The number is a FORMAT, and the shape is worth pinning: the short
        // form `YYYY-PRET-NNNNNN`, read aloud at a counter.
        let number = state
            .purchase_return_service
            .get_detail(return_id)
            .await
            .unwrap()
            .purchase_return
            .return_number
            .unwrap();
        // SIXTEEN characters, not the fifteen the feature document claims: a
        // four-character prefix and a six-digit sequence make sixteen. The model's
        // own test says so and pins it, and what matters here is that this
        // family's number is the same WIDTH as a sale's and one shorter than a
        // purchase's, which is the whole reason the short prefix was chosen.
        assert_eq!(number.len(), 16, "YYYY-PRET-NNNNNN is sixteen characters");
        assert_eq!(
            crate::models::format_sale_number(2026, 1).len(),
            number.len(),
            "a purchase return's number is exactly as wide as a sale's"
        );
        assert_eq!(
            crate::models::format_purchase_number(2026, 1).len(),
            number.len() + 1,
            "and one shorter than a purchase's, which is what the short form bought"
        );
        assert_eq!(
            number,
            crate::models::format_purchase_return_number(2026, 1),
            "the stored number is the one the formatter builds"
        );
    }

    /// The cancel round-trip: a confirmed return reverses, and the page says so.
    /// The reversal is an Expense, so it can be refused for want of funds — which
    /// is why this test funds the account by confirming a refund-bearing return
    /// first (the confirm's Income is what the reversal takes back).
    #[tokio::test]
    async fn cancelling_a_confirmed_purchase_return_through_the_route_reverses_it() {
        let state = test_state().await;
        let product = seed_product(&state, "CANCEL-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form(
            app.clone(),
            &format!("/web/purchase-returns/{return_id}/confirm"),
            &format!("return_id={return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert_eq!(stock_for(&state, product).await, Decimal::from(3));

        let (status, html) = post_form(
            app,
            &format!("/web/purchase-returns/{return_id}/cancel"),
            &format!("return_id={return_id}&reason=wrong+goods"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("Cancelled"),
            "the status advanced to cancelled: {html:.800}"
        );
        assert!(
            html.contains("wrong goods"),
            "the reason is stored and shown: {html:.1500}"
        );
        // The goods came back onto the shelf.
        assert_eq!(stock_for(&state, product).await, Decimal::from(5));
    }

    /// The draft delete round-trip: the document is gone, and the route's answer
    /// is the house shape — an empty 200 carrying the cross-region trigger.
    #[tokio::test]
    async fn deleting_a_purchase_return_draft_through_the_route_removes_it() {
        let state = test_state().await;
        let product = seed_product(&state, "DEL-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, body) = delete_html(
            app.clone(),
            &format!("/web/purchase-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            state
                .purchase_return_service
                .get_detail(return_id)
                .await
                .is_err(),
            "the draft is gone from the service"
        );
        // And the page that would have shown it is a 404 now.
        let (status, _) = get_html_as(
            app,
            &format!("/purchase-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A refusal the operator can trigger, surfaced with the RIGHT STATUS and a
    /// localized sentence.
    ///
    /// The refund cap is the honest one to pin: a return worth more than the
    /// parent collected is refused by `confirm` with a `Validation` naming the
    /// shortfall, and it is a 400 — a rule, not a fault. The price-refusal arm is
    /// pinned separately below, because `PriceRefused` and `Validation` are
    /// DIFFERENT answers and a route that collapsed them would be wrong.
    #[tokio::test]
    async fn the_refund_cap_answers_a_bad_request_naming_the_shortfall() {
        let state = test_state().await;
        let product = seed_product(&state, "CAP-1").await;
        // A CONFIRMED purchase of 5 at 7 is worth 35. It collected only 10, so a
        // return of the whole line is worth 35 and the cap bites. The parent must
        // be a CREDIT purchase to reach "confirmed but partly collected": a Cash
        // purchase collects the whole total at confirm, which would make the cap
        // unreachable through this path.
        let purchase_id = confirmed_purchase_paid(&state, product, "5", "7", Some("10")).await;
        let detail = state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.total, Decimal::from(35));
        assert_eq!(
            detail.paid,
            Decimal::from(10),
            "the parent is confirmed and only partly collected, which is the case \
             the cap exists for"
        );

        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let purchase_return = state
            .purchase_return_service
            .create_draft(
                actor,
                purchase_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let parent_line = detail.lines[0].id;
        state
            .purchase_return_service
            .add_line(actor, purchase_return.id, parent_line, "5".parse().unwrap())
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, body) = post_form(
            app,
            &format!("/web/purchase-returns/{}/confirm", purchase_return.id),
            &format!("return_id={}", purchase_return.id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a refund above what the purchase collected is a rule refusal, not a \
             server fault: {body}"
        );
        assert!(
            body.contains("credit balance"),
            "the refusal names the limitation instead of hiding it: {body}"
        );
    }

    /// The localized half of the refusal contract, on the one path that produces
    /// a TYPED refusal: an allowance subtraction that overflows answers
    /// `AggregateTooLarge`, and the operator reads that rule in their own
    /// language through the shared mapping. Spanish here; the English catalog is
    /// pinned by the closed-key-set test and by every other surface.
    #[tokio::test]
    async fn a_price_refusal_from_a_return_route_answers_in_the_active_locale() {
        let state = test_state().await;
        set_locale(&state, "es-AR").await;
        let product = seed_product(&state, "PREF-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let app = crate::routes::router(state.clone());

        // Reach the typed refusal by asking the SERVICE for a figure the route
        // then refuses to render — no, better: drive the route the way an
        // operator would, and assert the STATUS the mapping preserves.
        let (status, body) = post_form(
            app.clone(),
            "/web/purchase-returns/999999/confirm",
            "return_id=999999",
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // A quantity above the remaining allowance is the operator-triggerable
        // refusal on this surface, and it is a `Validation`: 400, with the
        // figures that explain it.
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let parent_line = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap()
            .lines[0]
            .id;
        let record = record_context(
            &state,
            return_id,
            crate::localization::LocalizationContext::fallback(),
        )
        .await
        .unwrap();
        assert!(record.options[0].remaining == Some(Decimal::from(5)));

        let (status, body) = post_form(
            app,
            &format!("/web/purchase-returns/{return_id}/lines"),
            &format!("return_id={return_id}&purchase_line_id={parent_line}&qty=9"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a quantity above the allowance is a rule refusal: {body}"
        );
        assert!(
            body.contains("cannot return"),
            "the refusal states the rule: {body}"
        );
    }

    /// A repeated parent line is a `Conflict` at the repository — the
    /// `UNIQUE (return_id, purchase_line_id)` backstop — and a conflict is a
    /// 409, never a 500. The reasoning is `transaction_repo`'s: a user action
    /// that is not allowed must be a status a client can act on.
    #[tokio::test]
    async fn a_repeated_parent_line_on_one_return_is_a_conflict_not_a_server_error() {
        let state = test_state().await;
        let product = seed_product(&state, "DUP-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let (return_id, line_id) = draft_return(&state, purchase_id, "2").await;
        let parent_line = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap()
            .lines[0]
            .id;
        let app = crate::routes::router(state.clone());

        let (status, body) = post_form(
            app,
            &format!("/web/purchase-returns/{return_id}/lines"),
            &format!("return_id={return_id}&purchase_line_id={parent_line}&qty=1"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "the same parent line twice is a conflict: {body}"
        );
        // The draft is untouched: the first line still stands.
        let detail = state
            .purchase_return_service
            .get_detail(return_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(detail.lines[0].id, line_id);
    }

    /// Adding a line through the route names the parent line and freezes its
    /// cost. The cost is the assertion that matters: it comes from the parent
    /// line, and the route passes no price of its own.
    #[tokio::test]
    async fn adding_a_line_through_the_route_freezes_the_parent_lines_cost() {
        let state = test_state().await;
        let product = seed_product(&state, "FREEZE-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "11").await;
        let app = crate::routes::router(state.clone());
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();

        // Create the draft through the route, so the creation path is exercised.
        let (status, _) = post_form(
            app.clone(),
            "/web/purchase-returns",
            &format!("purchase_id={purchase_id}&return_date=2026-02-01"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let draft = state
            .purchase_return_service
            .list(&PurchaseReturnListFilter::default())
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.purchase_return.purchase_id == purchase_id)
            .expect("the created draft is listed");
        let return_id = draft.purchase_return.id;

        let parent_line = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap()
            .lines[0]
            .id;
        let (status, body) = post_form(
            app.clone(),
            &format!("/web/purchase-returns/{return_id}/lines"),
            &format!("return_id={return_id}&purchase_line_id={parent_line}&qty=3"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.600}");
        // The service wrote the parent's own cost, and the route posted none.
        let detail = state
            .purchase_return_service
            .get_detail(return_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(
            detail.lines[0].unit_cost,
            Decimal::from(11),
            "the frozen cost is the parent line's, never the request's"
        );
        assert_eq!(detail.total, Decimal::from(33));
        // The audit actor is the REQUEST'S principal — the id the route resolves
        // from its `Principal` — and NOT the fixture's system sentinel the
        // document was created under. A line write is an edit, so it stamps
        // `updated_by`, and this is what proves the route passed the acting user
        // down rather than inventing one.
        // The document was created THROUGH THE ROUTE, so its creator is the
        // session's principal and NOT the fixture's system sentinel: the create
        // route resolved its actor from the `Principal`, exactly as it must.
        assert_eq!(
            detail.purchase_return.created_by,
            session_user_id(&state).await,
            "the create route stamps the REQUEST'S principal, not the fixture's actor"
        );
        assert_ne!(
            detail.purchase_return.created_by, actor,
            "the two ids must differ for this assertion to mean anything"
        );
        assert_eq!(
            detail.purchase_return.updated_by,
            Some(session_user_id(&state).await),
            "and so does the line write"
        );
    }

    /// The write gates are the parent's write codes. A read-only principal sees
    /// the page and can use nothing on it: every write is refused with the code
    /// it lacked, and the draft survives each refusal.
    #[tokio::test]
    async fn the_purchase_return_writes_are_gated_on_the_parent_family_write_codes() {
        let state = test_state().await;
        let product = seed_product(&state, "GATE-1").await;
        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, purchase_id, "2").await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());
        let parent_line = state
            .purchases_service
            .get_record(purchase_id)
            .await
            .unwrap()
            .lines[0]
            .id;

        for (uri, body, code) in [
            (
                "/web/purchase-returns".to_string(),
                format!("purchase_id={purchase_id}"),
                "purchases.create",
            ),
            (
                format!("/web/purchase-returns/{return_id}/lines"),
                format!("purchase_line_id={parent_line}&qty=1"),
                "purchases.create",
            ),
            (
                format!("/web/purchase-returns/{return_id}/confirm"),
                String::new(),
                "purchases.create",
            ),
            (
                format!("/web/purchase-returns/{return_id}/header"),
                "return_date=2026-02-01".to_string(),
                "purchases.create",
            ),
            (
                format!("/web/purchase-returns/{return_id}"),
                String::new(),
                "purchases.create",
            ),
        ] {
            let (status, html) = if uri.ends_with(&format!("/purchase-returns/{return_id}"))
                && code == "purchases.create"
                && !uri.contains("/lines")
                && !uri.contains("/confirm")
                && !uri.contains("/header")
            {
                delete_html(app.clone(), &uri, Some(&cookie)).await
            } else {
                post_form(app.clone(), &uri, &body, Some(&cookie)).await
            };
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.300}");
            assert!(
                html.contains(code),
                "{uri} must refuse naming {code}: {html:.600}"
            );
        }

        // The cancel gate is the parent's CANCEL code, not its create code: a
        // principal that may record a purchase may not annul one, and the same
        // split holds for a return.
        let canceller = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "purchases.create"],
        )
        .await
        .unwrap();
        let (status, html) = post_form(
            app,
            &format!("/web/purchase-returns/{return_id}/cancel"),
            &format!("return_id={return_id}"),
            Some(&test_support::cookie_for(&canceller)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.300}");
        assert!(
            html.contains("purchases.cancel"),
            "the cancel refusal names the cancel code: {html:.600}"
        );
        assert!(state
            .purchase_return_service
            .get_detail(return_id)
            .await
            .is_ok());
    }

    /// The list filter narrows by status, number and date and is driven by the
    /// query the page re-renders from, so a bookmarkable URL reproduces the list
    /// the operator was looking at.
    #[tokio::test]
    async fn the_list_filter_narrows_by_status_and_survives_the_query_string() {
        let state = test_state().await;
        let product = seed_product(&state, "FILT-1").await;
        let first = confirmed_purchase(&state, product, "5", "7").await;
        let second = confirmed_purchase(&state, product, "4", "7").await;
        let (_draft_id, _) = draft_return(&state, first, "2").await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);

        let (status, all) = get_html_as(app.clone(), "/purchase-returns", cookie).await;
        assert_eq!(status, StatusCode::OK, "{all:.400}");
        assert!(all.contains("Draft"), "the draft is listed: {all:.600}");

        // Status filter: Draft only. The second return does not exist yet, so the
        // filtered list holds exactly the one draft.
        let (status, drafts) =
            get_html_as(app.clone(), "/purchase-returns?status=Draft", cookie).await;
        assert_eq!(status, StatusCode::OK, "{drafts:.400}");
        assert!(drafts.contains("Draft"));
        assert!(
            !drafts.contains("Second purchase"),
            "a status filter narrows: {drafts:.600}"
        );

        // The fragment endpoint honours the same filter, which is what the
        // filter bar's Refresh button calls.
        let (status, fragment) =
            get_html_as(app.clone(), "/web/purchase-returns?status=Draft", cookie).await;
        assert_eq!(status, StatusCode::OK, "{fragment:.400}");
        assert!(fragment.contains("Draft"), "{fragment:.600}");

        // A supplier filter that matches nothing renders an empty list, never an
        // error and never "all".
        let (status, empty) =
            get_html_as(app.clone(), "/purchase-returns?supplier=NOSUCHSUP", cookie).await;
        assert_eq!(status, StatusCode::OK, "{empty:.400}");
        assert!(
            empty.contains("No purchase returns yet"),
            "a filter that matches nothing says so: {empty:.600}"
        );
        let _ = second;
    }

    /// The list header's count reads as a count. Both families shipped with an
    /// IDENTICAL singular and plural, so every header read "Purchase returns •
    /// Returns (1)" — the family name twice and a plural over a single document.
    /// The keys are in `localization_tests.rs`'s `count_keys` because the singular
    /// now differs; this is the behaviour that list justifies, and it is stated
    /// here because a catalog row with no rendering assertion is a row that can
    /// silently go back to a single form.
    #[tokio::test]
    async fn both_return_lists_count_their_documents_in_the_singular_and_the_plural() {
        let state = test_state().await;
        let product = seed_product(&state, "CNT-1").await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);

        // Empty first: zero takes the PLURAL in both languages, which is the case
        // the original screenshot showed ("Returns (0)").
        let (status, empty) = get_html_as(app.clone(), "/purchase-returns", cookie).await;
        assert_eq!(status, StatusCode::OK, "{empty:.400}");
        assert!(
            empty.contains("Returns (0)"),
            "an empty list reads as a plural: {empty:.600}"
        );

        let purchase_id = confirmed_purchase(&state, product, "5", "7").await;
        draft_return(&state, purchase_id, "2").await;
        let (status, one) = get_html_as(app.clone(), "/purchase-returns", cookie).await;
        assert_eq!(status, StatusCode::OK, "{one:.400}");
        assert!(
            one.contains("Return (1)"),
            "ONE return is a singular: the header must not read 'Purchase returns \\
             • Returns (1)': {one:.600}"
        );
        assert!(
            !one.contains("Returns (1)"),
            "and never the plural over a single document: {one:.600}"
        );

        // The Spanish catalog has the same rule, and its plural differs from its
        // singular the way the English one does.
        set_locale(&state, "es-AR").await;
        let (status, spanish) = get_html_as(app, "/purchase-returns", cookie).await;
        assert_eq!(status, StatusCode::OK, "{spanish:.400}");
        assert!(
            spanish.contains("Devolución (1)"),
            "the Spanish singular is the one over one: {spanish:.600}"
        );
    }

    /// **A POST WITH AN UNREADABLE FIELD MUST READ AS A SENTENCE.**
    ///
    /// The operator typed a document number that was right. What answered was
    /// `Failed to deserialize form body: purchase_id: cannot parse integer from
    /// empty string` — a serde internal, in English, on a Spanish page. The
    /// parser's words are not the application's to show. Whatever is unreadable
    /// or missing, the body must be a localized refusal with no parser vocabulary
    /// and no Rust type name in it, and the status must be a 400 the base notice
    /// can render rather than a 422 nothing explains.
    #[tokio::test]
    async fn a_purchase_return_post_whose_field_cannot_be_read_answers_an_operator_sentence() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);
        set_locale(&state, "es-AR").await;

        for body in [
            // The empty hidden field the deleted dialog used to post.
            "purchase_id=&return_date=2026-02-01",
            // A value that is present and is not a number.
            "purchase_id=2024-PURCH-000001&return_date=2026-02-01",
            // No field at all.
            "return_date=2026-02-01",
            // A negative id: parseable, and not a document.
            "purchase_id=-3&return_date=2026-02-01",
        ] {
            let (status, response) =
                post_form(app.clone(), "/web/purchase-returns", body, cookie).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "an unreadable or missing parent is a refusal the operator can act on, \
                 not an extractor rejection: {body} -> {response:.400}"
            );
            let lower = response.to_lowercase();
            for forbidden in [
                "deserialize",
                "cannot parse",
                "invalid type",
                "expected",
                "option<",
                "formrejection",
                "form<",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "the parser's vocabulary must never reach the operator ({forbidden:?}): \
                     {body} -> {response:.400}"
                );
            }
        }
    }

    /// The refusal is a SENTENCE in the operator's language, not an English
    /// developer string that merely happens to be short: the Spanish locale must
    /// answer in Spanish, and the wording must be the one the catalogs carry.
    #[tokio::test]
    async fn a_purchase_return_post_whose_field_cannot_be_read_answers_in_the_active_locale() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);

        set_locale(&state, "es-AR").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PurchaseReturnsFormUnreadable)
            .to_string();
        let (status, spanish) =
            post_form(app.clone(), "/web/purchase-returns", "purchase_id=", cookie).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{spanish:.400}");
        assert!(
            spanish.contains(&expected),
            "the operator reads the refusal in their own language: {spanish:.400}"
        );
        assert!(
            !spanish.contains("invalid"),
            "the English parse vocabulary must not survive a Spanish refusal: {spanish:.400}"
        );

        set_locale(&state, "en-US").await;
        let (status, english) =
            post_form(app, "/web/purchase-returns", "purchase_id=", cookie).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{english:.400}");
        assert!(
            english.contains("form could not be read"),
            "the English catalog's own sentence: {english:.400}"
        );
    }

    /// Both return families are reachable from the navigation, and the entry a
    /// click opens is a page that answers. A family with no entry is a family a
    /// person has to know the URL of.
    #[tokio::test]
    async fn both_return_families_render_a_sidebar_entry_that_resolves() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let cookie = Some(test_support::TEST_COOKIE);

        let (status, html) = get_html_as(app.clone(), "/", cookie).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        for (key, href) in [
            ("purchase-returns", "/purchase-returns"),
            ("customer-returns", "/customer-returns"),
        ] {
            let marker = format!("data-nav=\"{key}\"");
            assert!(
                html.contains(&marker),
                "the sidebar must carry an entry for {key}: {html:.1200}"
            );
            // The anchor's own start tag, sliced rather than a fixed window:
            // `nav_item` puts `data-nav` before `href`, and a window sized for
            // one would silently stop covering the other.
            let at = html.find(&marker).expect("the entry renders");
            let tag_start = html[..at].rfind('<').expect("the marker sits inside a tag");
            let tag = &html
                [tag_start..tag_start + html[tag_start..].find('>').expect("unterminated tag") + 1];
            assert!(
                tag.contains(&format!("href=\"{href}\"")),
                "the {key} entry must open {href}: {tag}"
            );

            let (status, page) = get_html_as(app.clone(), href, cookie).await;
            assert_eq!(status, StatusCode::OK, "{key} must resolve: {page:.400}");
            assert!(
                page.contains(&marker),
                "and the page it opens must mark its own entry current: {page:.1200}"
            );
        }
    }
}
