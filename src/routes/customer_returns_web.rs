// Credit notes web: the `/customer-returns` list and the
// `/customer-returns/{id}` record page, Askama + HTMX. Thin handlers over
// `CustomerReturnService`; the record body lives in
// `partials/customer_return_detail.html` and every action posts to
// `/web/customer-returns/{id}/...`, so the id always comes from the URL.
//
// THE MIRROR OF `purchase_returns_web.rs`, with the three nouns swapped. It is a
// separate module rather than one parameterised over a sign because the three
// things that differ are not parameters: the money is an `Expense` rather than an
// `Income`, so the overdraft guard DOES fire on a credit note's refund where it
// never fires on a purchase return's; the frozen figure is the parent sale
// line's `unit_price` rather than its `unit_cost`; and the document's number
// column is `credit_note_number`. A sign would hide all three behind a branch
// every reader has to re-derive, which is the mirror this codebase builds
// everywhere else instead.
//
// THE GATE IS THE PARENT'S, and there is no new permission code for it. A credit
// note is `sales.read` to see and `sales.create` to write: a principal who can
// sell can credit what they sold, and one who can cancel a sale can reverse the
// credit note. This is the same decision `purchase_returns_web.rs` takes and the
// same one `DocumentKind::read_code` already encodes for the documents drawer — a
// permission that gates nothing the catalog does not already say is worse than a
// coarse one. The cancel route carries `sales.cancel`, the parent's cancel code,
// because annulling money that has moved is not the same act as recording it.
//
// THE FORM HAS NO PRICE INPUT, and that is decision 1 of the design made visible
// rather than restricted in the template. `add_line` and `update_line` take no
// price argument at all — the frozen `unit_price` is written by `create_line`
// from the parent line's own figure — so there is nothing for a handler to pass
// and nothing for a form to post. A disabled or readonly input would still be an
// input: the next person to touch the template would re-enable it, and the
// service would then silently ignore it. So the price is RENDERED as text beside
// the quantity, taken from the parent's line, and never typed.
// `a_draft_customer_return_renders_with_no_price_input` is the test that makes it
// enforceable.
//
// WHERE THE PRODUCT IDENTITY COMES FROM. `CustomerReturnLine` carries no
// `product_id` — the model says so explicitly, and deliberately: the line names
// the PARENT SALE LINE and the product is one read away through it. The service's
// `get_detail` does not join that far, and adding a join there would be a service
// change this unit does not own. So the line editor reads the parent sale through
// `SalesService::get_record` — a real service call, on the documented read path —
// and takes each line's `product_name`, `product_sku`, `qty` and `unit_price`
// from the `SaleLineView` it already returns. The return line's `sale_line_id` is
// the join key in Rust, and the product is never looked up by a stored id
// because none is stored. The same read supplies the ALLOWANCE columns: `sold` is
// the parent line's `qty`, `taken` is `confirmed_qty_taken_by_sale_line` read
// through the repository the service holds, and `remaining` is their checked
// difference.
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
use crate::models::{CustomerReturn, CustomerReturnStatus};
use crate::repositories::customer_return_repo::CustomerReturnListFilter;
use crate::repositories::CustomerReturnRepository;
use crate::routes::{localized_refusal_error, AppState};
use crate::security::authz::{Nav, Require, SalesCancel, SalesCreate, SalesRead};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One credit note plus the party and parent facts a list row shows.
///
/// `refunded` is money PAID BACK to the customer, so the `Paid` chip means
/// refunded in full and the pending figure is what has not gone out yet. The
/// state derivation is deliberately a field rather than a template expression:
/// `Decimal::ZERO.is_sign_positive()` is `true`, so a settled note (pending
/// exactly zero) must compare NUMERICALLY and never by sign.
#[derive(Clone)]
pub struct CustomerReturnView {
    pub customer_return: CustomerReturn,
    pub line_count: usize,
    pub total: Decimal,
    /// Still to be refunded.
    pub pending: Decimal,
    /// Whether the refund is complete.
    pub refunded_in_full: bool,
    /// The customer's name — a credit note stores only the id.
    pub customer_name: String,
    /// The parent sale's number, so the row names the document it reverses.
    pub parent_number: String,
}

/// The line editor's view of one PARENT sale line: everything the operator needs
/// to choose a quantity, and nothing they could use to choose a price.
///
/// `on_this_draft` is resolved in the wiring layer because Askama cannot compare
/// decimals or build `Some(...)` in an expression.
#[derive(Clone)]
pub struct ParentLineOption {
    pub sale_line_id: i64,
    pub product_name: String,
    pub product_sku: String,
    /// What the parent line sold.
    pub sold: Decimal,
    /// What CONFIRMED credit notes of that same parent line already took. A draft
    /// reserves nothing, so this figure excludes the draft being edited — the
    /// same reading the service's own guard is measured against.
    pub taken: Decimal,
    /// `sold - taken`, checked. `None` only on overflow, which the service
    /// answers as `AggregateTooLarge` and this page shows as no allowance.
    pub remaining: Option<Decimal>,
    /// The frozen unit price, RENDERED AND NEVER TYPED. See the file header.
    pub unit_price: Decimal,
    /// The quantity this draft already claims of that line, when it claims any.
    pub on_this_draft: Option<Decimal>,
    /// The credit-note line's own id when `on_this_draft` is set, so the editor
    /// can route an inline quantity edit at the right row.
    pub return_line_id: Option<i64>,
    /// Whether this row can take a quantity at all, derived here because Askama
    /// cannot compare decimals. A row whose allowance is gone, or whose overflow
    /// refused, takes no input: an operator must not be offered a quantity the
    /// service will refuse. A row the draft ALREADY has a line for is still
    /// addable — a draft reserves nothing, so editing from 1 up to 3 is legal and
    /// only the SECOND CONFIRM refuses.
    pub can_add: bool,
}

/// One refund row resolved for display: the account and the method by NAME.
/// `CustomerReturnPayment` stores ids, and the interface never prints an internal
/// id where a name exists.
#[derive(Clone)]
pub struct RefundRow {
    pub id: i64,
    pub date: NaiveDate,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
}

#[derive(Template)]
#[template(path = "customer_returns.html")]
struct CustomerReturnsTemplate {
    title: String,
    localization: LocalizationContext,
    returns: Vec<CustomerReturnView>,
    nav_key: &'static str,
    filter_status: String,
    filter_customer: String,
    filter_number: String,
    filter_from: String,
    filter_to: String,
    nav: Nav,
    page_action_href: String,
    page_action_label: String,
    page_action_dialog: String,
}

/// The page shell around the record body. It carries the SAME record fields the
/// partial does, because an Askama `include` compiles against the INCLUDING page's
/// context: every name `partials/customer_return_detail.html` reads must exist here
/// too. `sales_web`'s page struct does exactly this.
#[derive(Template)]
#[template(path = "customer_return.html")]
struct CustomerReturnPageTemplate {
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
#[template(path = "partials/customer_return_list.html")]
struct CustomerReturnListPartial {
    title: String,
    localization: LocalizationContext,
    returns: Vec<CustomerReturnView>,
}

/// The record body, shared by the page and by every action response that swaps
/// `#customer-return-record`, so the action forms travel with the fragment either
/// way.
#[derive(Template)]
#[template(path = "partials/customer_return_detail.html")]
struct CustomerReturnDetailPartial {
    record: ReturnRecordContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
    localization: LocalizationContext,
    header_action: String,
    parent_href: String,
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Everything the record body renders: the service detail, the parent sale's
/// resolved line options, the refunds named, and the audit display names.
#[derive(Clone)]
pub struct ReturnRecordContext {
    pub detail: crate::models::CustomerReturnDetail,
    pub customer_name: String,
    pub parent_sale_id: i64,
    /// The PARENT's number, or `#id` for a draft parent. The record body links to
    /// the sale this note credits, so the link's TEXT must be the sale's number
    /// and never the NOTE's own — see the purchase family's twin for why.
    pub parent_number: String,
    pub refunds: Vec<RefundRow>,
    pub options: Vec<ParentLineOption>,
    pub created_by_name: Option<String>,
    pub updated_by_name: Option<String>,
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// **THE OPERATOR NEVER READS A PARSER.** The purchase twin's rule, with this
/// family's own sentence: every `Form<T>` below is extracted as a `Result` and
/// every unreadable body answers `CustomerReturnsFormUnreadable` in the active
/// locale. A principal who typed a correct sale number was answered
/// `Failed to deserialize form body: sale_id: cannot parse integer from empty
/// string` — a serde internal, in English, naming a Rust type, on a Spanish page.
fn form_or_refusal<T>(
    form: Result<Form<T>, axum::extract::rejection::FormRejection>,
    localization: &LocalizationContext,
) -> AppResult<T> {
    form.map(|Form(form)| form).map_err(|_| {
        AppError::Validation(
            localization
                .tr(crate::localization::MessageKey::CustomerReturnsFormUnreadable)
                .to_string(),
        )
    })
}

/// The parent sale id, which the sale record page's own action fills in. See
/// `form_or_refusal` for why the struct holds it as `Option`.
fn parse_required_id(raw: Option<i64>, localization: &LocalizationContext) -> AppResult<i64> {
    raw.filter(|id| *id > 0).ok_or_else(|| {
        AppError::Validation(
            localization
                .tr(crate::localization::MessageKey::CustomerReturnsFormUnreadable)
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

/// `/customer-returns`: the list, one `sales.read` gate.
///
/// **THERE IS NO CREATION ACTION ON THIS PAGE, AND THAT IS THE FIX.** There was a
/// dialog with a `parent_ref` text field and a hidden `sale_id` nothing filled,
/// so only the JSON API could create a credit note. Creation is now on the SALE's
/// own record page, which carries the id; the header action slot is empty and one
/// sentence says how to start one.
async fn customer_returns_page(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(query): Query<CustomerReturnListQuery>,
) -> Result<Html<String>, AppError> {
    let filter = query.to_filter(&state).await?;
    let returns = customer_return_views(&state, &filter).await?;
    let tmpl = CustomerReturnsTemplate {
        title: localization
            .tr(crate::localization::MessageKey::CustomerReturnsTitle)
            .to_string(),
        localization,
        returns,
        // Its own sidebar entry, for the reason the purchase family's list does.
        nav_key: "customer-returns",
        filter_status: query.status.trim().to_string(),
        filter_customer: query.customer.trim().to_string(),
        filter_number: query.number.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        nav: Nav::for_principal(&principal),
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
/// [`crate::routes::sales_web::SaleListQuery`].
#[derive(Debug, Deserialize, Default)]
pub struct CustomerReturnListQuery {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub customer: String,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

impl CustomerReturnListQuery {
    /// The filter the repository takes. The typed customer NAME becomes ids here,
    /// because `CustomerReturnListFilter` carries `customer_ids` and not a name —
    /// the service's own `list` doc records that the repository takes resolved ids
    /// only, so the caller that has a name resolves it. `Some(empty)` matches
    /// nothing, which is the repository's stated contract: a party filter that
    /// found no customer cannot match a document.
    async fn to_filter(&self, state: &AppState) -> AppResult<CustomerReturnListFilter> {
        let customer_ids = match clean_filter_text(&self.customer) {
            Some(needle) => {
                let normalized = crate::models::normalize_search(&needle);
                Some(
                    state
                        .customer_service
                        .list_customers(false)
                        .await?
                        .into_iter()
                        .filter(|c| crate::models::normalize_search(&c.name).contains(&normalized))
                        .map(|c| c.id)
                        .collect::<Vec<i64>>(),
                )
            }
            None => None,
        };
        Ok(CustomerReturnListFilter {
            status: parse_optional_return_status(&self.status),
            customer_ids,
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

fn parse_optional_return_status(raw: &str) -> Option<CustomerReturnStatus> {
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

/// The rows of the list. The service's `list` returns DETAILS, not list rows:
/// this family has no `CustomerReturnListRow` model and inventing one would be a
/// model change. So a row's money is the same derived figure the service computed,
/// read from the detail rather than re-derived here — a list row and a detail can
/// never disagree about one document's money.
async fn customer_return_views(
    state: &AppState,
    filter: &CustomerReturnListFilter,
) -> AppResult<Vec<CustomerReturnView>> {
    let details = state.customer_return_service.list(filter).await?;
    let mut out = Vec::with_capacity(details.len());
    for detail in details {
        let customer = state
            .customer_service
            .get_customer(detail.customer_return.customer_id)
            .await?;
        let parent_number = state
            .sales_service
            .get_record(detail.customer_return.sale_id)
            .await
            .ok()
            .map(|record| record.sale.sale_number.unwrap_or_default())
            .unwrap_or_default();
        out.push(CustomerReturnView {
            line_count: detail.lines.len(),
            total: detail.total,
            pending: detail.due,
            refunded_in_full: detail.due <= Decimal::ZERO,
            customer_name: customer.name,
            parent_number,
            customer_return: detail.customer_return,
        });
    }
    Ok(out)
}

fn render_list(
    view: Vec<CustomerReturnView>,
    title: &str,
    localization: LocalizationContext,
) -> AppResult<Html<String>> {
    let html = CustomerReturnListPartial {
        title: title.to_string(),
        localization,
        returns: view,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_customer_return_list(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<CustomerReturnListQuery>,
) -> AppResult<Response> {
    let filter = query.to_filter(&state).await?;
    let view = customer_return_views(&state, &filter).await?;
    let title = localization
        .tr(crate::localization::MessageKey::CustomerReturnsTitle)
        .to_string();
    Ok(render_list(view, &title, localization)?.into_response())
}

/// `/customer-returns/{id}`: a real page inside the shell. Single-gate
/// consequence, same contract as the sale record page: the record renders only
/// THIS credit note's own data.
async fn customer_return_record_page(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(raw_id): Path<String>,
) -> Result<Html<String>, AppError> {
    let Ok(id) = raw_id.parse::<i64>() else {
        return Err(AppError::NotFound(format!(
            "customer return {raw_id} not found"
        )));
    };
    let record = record_context(&state, id, localization.clone()).await?;
    let label = match &record.detail.customer_return.credit_note_number {
        Some(number) => number.clone(),
        None => localization
            .tr(crate::localization::MessageKey::CustomerReturnsDraft)
            .to_string(),
    };
    let (action_href, action_label) =
        if record.detail.customer_return.status == CustomerReturnStatus::Draft {
            (
                "#customer-return-record".to_string(),
                localization
                    .tr(crate::localization::MessageKey::CustomerReturnsConfirm)
                    .to_string(),
            )
        } else {
            (String::new(), String::new())
        };
    let created_by_name = record.created_by_name.clone();
    let updated_by_name = record.updated_by_name.clone();
    let tmpl = CustomerReturnPageTemplate {
        page_title: label,
        page_breadcrumb_label: localization
            .tr(crate::localization::MessageKey::NavigationSales)
            .to_string(),
        page_breadcrumb_href: "/sales".to_string(),
        page_action_href: action_href,
        page_action_label: action_label,
        page_action_dialog: String::new(),
        header_action: format!("/web/customer-returns/{id}/header"),
        parent_href: format!("/sales/{}", record.parent_sale_id),
        entry_row_focus: false,
        oob_action_bar: false,
        created_by_name,
        updated_by_name,
        record,
        localization,
        // Its own entry, matching the list page: an operator reading a credit
        // note is not reading sales.
        nav_key: "customer-returns",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_customer_return_detail(
    State(state): State<AppState>,
    _: Require<SalesRead>,
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
/// The parent sale is read through `SalesService::get_record` — a service call,
/// not a repository one — and the credit note's own lines are joined to its
/// `SaleLineView`s in Rust by `sale_line_id`. That is where product identity comes
/// from: `CustomerReturnLine` stores no product, deliberately, so the names and
/// the frozen price are read through the parent line rather than resolved from a
/// stored id that does not exist.
async fn record_context(
    state: &AppState,
    return_id: i64,
    localization: LocalizationContext,
) -> AppResult<ReturnRecordContext> {
    let detail = state.customer_return_service.get_detail(return_id).await?;
    let customer = state
        .customer_service
        .get_customer(detail.customer_return.customer_id)
        .await?;
    let parent = state
        .sales_service
        .get_record(detail.customer_return.sale_id)
        .await?;

    // Audit display names, resolved in the wiring layer from the pool the state
    // owns — the same reason as `purchases_web`: the department module never reads
    // identity tables itself, and a name that resolves to nothing degrades to an
    // absent name rather than a blank row.
    let mut actor_ids = vec![detail.customer_return.created_by];
    actor_ids.extend(detail.customer_return.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;

    // One allowance read PER PARENT LINE, through the repository the service
    // holds — the same aggregate the service's own guard measures against, so the
    // editor and the refusal can never disagree. A DRAFT reserves nothing, so this
    // figure excludes the draft being edited.
    let mut options: Vec<ParentLineOption> = Vec::with_capacity(parent.lines.len());
    for line in &parent.lines {
        let taken = state
            .customer_return_service
            .returns
            .confirmed_qty_taken_by_sale_line(line.id)
            .await?;
        let on_this_draft = detail.lines.iter().find(|l| l.sale_line_id == line.id);
        options.push(ParentLineOption {
            sale_line_id: line.id,
            product_name: line.product_name.clone(),
            product_sku: line.product_sku.clone(),
            sold: line.qty,
            taken,
            // `sold - taken` is a subtraction of two figures read out of stored
            // TEXT, exactly the operands `ensure_within_parent` subtracts. It is
            // CHECKED for the same reason that one is: a raw `-` on Decimals panics
            // on overflow, and `AggregateTooLarge` is the layer that already answers
            // that refusal for quantities.
            remaining: line.qty.checked_sub(taken),
            unit_price: line.unit_price,
            on_this_draft: on_this_draft.map(|l| l.qty),
            return_line_id: on_this_draft.map(|l| l.id),
            can_add: line
                .qty
                .checked_sub(taken)
                .is_some_and(|left| left > Decimal::ZERO),
        });
    }

    // The refunds, resolved to names. `methods_with_accounts` is the one read that
    // already carries BOTH the method's display name and its account's, so one
    // call covers the payment table; a payment whose method or account is no longer
    // in that catalogue degrades to its id rather than failing the page.
    let methods = state.payment_method_service.methods_with_accounts().await?;
    let refunds = detail
        .payments
        .iter()
        .map(|payment| {
            let method = methods.iter().find(|m| m.id == payment.method_id);
            RefundRow {
                id: payment.id,
                date: payment.date,
                account_name: method
                    .and_then(|m| m.account_name.clone())
                    .unwrap_or_else(|| payment.account_id.to_string()),
                method_name: method.map(|m| m.name.clone()).unwrap_or_default(),
                amount: payment.amount,
            }
        })
        .collect();

    // The actor names are read BEFORE the document moves into the context, so the
    // fields are ordinary values rather than borrows of a moved value.
    let created_by_name = names.get(&detail.customer_return.created_by).cloned();
    let updated_by_name = detail
        .customer_return
        .updated_by
        .and_then(|id| names.get(&id).cloned());

    let _ = &localization;
    Ok(ReturnRecordContext {
        detail,
        customer_name: customer.name,
        parent_sale_id: parent.sale.id,
        parent_number: parent
            .sale
            .sale_number
            .clone()
            .unwrap_or_else(|| format!("#{}", parent.sale.id)),
        refunds,
        options,
        created_by_name,
        updated_by_name,
    })
}

/// Render the record body. Synchronous because everything it needs is already
/// resolved by [`record_context`] — including the audit display names, for the
/// reason `purchases_web` resolves them there too.
fn render_record(
    context: ReturnRecordContext,
    localization: &LocalizationContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
) -> AppResult<Html<String>> {
    let header_action = format!(
        "/web/customer-returns/{}/header",
        context.detail.customer_return.id
    );
    let parent_href = format!("/sales/{}", context.parent_sale_id);
    let created_by_name = context.created_by_name.clone();
    let updated_by_name = context.updated_by_name.clone();
    let html = CustomerReturnDetailPartial {
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
/// out of band, and the cross-region `customer-return-changed` event fired so the
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
        .insert("HX-Trigger", "customer-return-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// The creation form. The parent sale is an explicit id — a clicked picker result
/// — because a credit note CANNOT be created without naming the document it
/// reverses; there is no "pick one later" state, since `create_draft` requires a
/// confirmed sale and the number the note carries derives from it.
#[derive(Debug, Deserialize)]
pub struct CreateCustomerReturnForm {
    /// `Option`, not `i64`: an empty hidden field is what an unfilled control
    /// posts, and a field the struct cannot hold has serde refuse it in serde's
    /// words. Holding it lets `parse_required_id` refuse in the operator's
    /// language instead.
    #[serde(default)]
    pub sale_id: Option<i64>,
    #[serde(default)]
    pub return_date: String,
    #[serde(default)]
    pub notes: String,
}

/// **THERE IS NO PRICE FIELD IN THIS STRUCT, AND THAT IS THE POINT.** The form
/// carries a parent line id and a quantity, and nothing else: `add_line` takes no
/// price argument, and a form field the handler ignores would be a lie about what
/// the document accepts. See the file header.
#[derive(Debug, Deserialize)]
pub struct AddReturnLineForm {
    /// The return's own id. Same arrangement as the confirm form: it rides in the
    /// form so a post without it is malformed, and the handler reads the real id
    /// from the path.
    #[serde(default)]
    #[allow(dead_code)]
    pub return_id: Option<i64>,
    /// The PARENT SALE LINE being credited. The operator chooses from the line
    /// editor, which the server rendered from the parent's own lines.
    #[serde(default)]
    pub sale_line_id: Option<i64>,
    /// The quantity to credit, and the only figure this form asks for.
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

/// The draft's header form: the return date and notes. The CUSTOMER is not in it
/// because a credit note has no customer control at all — `create_draft` COPIES
/// the parent's, so there is nothing to choose and nothing that could disagree
/// with the document the money goes back to.
#[derive(Debug, Deserialize)]
pub struct UpdateCustomerReturnHeaderForm {
    #[serde(default)]
    pub return_date: String,
    #[serde(default)]
    pub notes: String,
}

async fn web_create_customer_return(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    form: Result<Form<CreateCustomerReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    // The parent id is REQUIRED here, not defaulted: the service refuses a missing
    // or unconfirmed sale, and a route that invented one would be crediting goods
    // the operator never named. Its refusal is this module's own sentence — see
    // `form_or_refusal`.
    let customer_return = state
        .customer_return_service
        .create_draft(
            principal.user_id,
            parse_required_id(form.sale_id, &localization)?,
            parse_date_or_today(&form.return_date, &localization)?,
            &clean_opt(&form.notes),
        )
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    let location = format!("/customer-returns/{}", customer_return.id);
    if is_htmx(&headers) {
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
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<AddReturnLineForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    // No price argument, because the service has none to take. The frozen price is
    // the parent line's own figure, read inside `add_line`.
    state
        .customer_return_service
        .add_line(
            principal.user_id,
            id,
            parse_required_id(form.sale_line_id, &localization)?,
            qty,
        )
        .await
        // A line write runs the shared money contract, so this surface can answer
        // a `PriceRefused` — the allowance subtraction's `AggregateTooLarge` is the
        // reachable one. It answers through the ONE shared renderer, in the
        // operator's own language; every other error passes through untouched, so a
        // conflict stays a conflict and a 404 stays a 404.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, true).await;
    }
    Ok(Redirect::to(&format!("/customer-returns/{id}")).into_response())
}

async fn web_update_return_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path((return_id, line_id)): Path<(i64, i64)>,
    form: Result<Form<UpdateReturnLineForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    state
        .customer_return_service
        .update_line(principal.user_id, line_id, qty)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, return_id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/customer-returns/{return_id}")).into_response())
}

async fn web_remove_return_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path((return_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    state
        .customer_return_service
        .remove_line(principal.user_id, line_id)
        .await?;
    changed(&state, return_id, &localization, false).await
}

async fn web_confirm_customer_return(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<ConfirmReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    // The confirm form carries its own id and ignores it, but it is still a form
    // and a malformed one must answer in the operator's language.
    let _form = form_or_refusal(form, &localization)?;
    // **No method field.** `confirm` resolves the refunds PER ORIGINATING ACCOUNT
    // from the parent's payment rows, and `customer_return_payments` has no
    // `payment_type` column (decision 7 of the design): a credit note's refunds are
    // determined entirely by the parent's payments, so posting a second copy of
    // that flag would be a value that could disagree with the rows it summarizes.
    state
        .customer_return_service
        .confirm(principal.user_id, id)
        .await
        // The refund cap is a `Validation` naming the shortfall, and the money folds
        // are `PriceRefused`. Both reach the operator through the one mapping. So
        // does the OVERDRAFT refusal, which is this family's own: a credit note's
        // refund is an `Expense`, so unlike a purchase return's `Income` it CAN be
        // refused for want of funds in the account the sale was paid into.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/customer-returns/{id}")).into_response())
}

async fn web_cancel_customer_return(
    State(state): State<AppState>,
    _: Require<SalesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<CancelReturnForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    state
        .customer_return_service
        .cancel(principal.user_id, id, clean_opt(&form.reason))
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, false).await;
    }
    Ok(Redirect::to(&format!("/customer-returns/{id}")).into_response())
}

async fn web_update_customer_return_header(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    form: Result<Form<UpdateCustomerReturnHeaderForm>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let form = form_or_refusal(form, &localization)?;
    state
        .customer_return_service
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
    Ok(Redirect::to(&format!("/customer-returns/{id}")).into_response())
}

/// `DELETE /web/customer-returns/{id}`: the documents drawer's draft delete. The
/// same house shape as the purchase flow — an empty 200 whose `HX-Trigger` tells
/// the listening pages to re-read the feed; the business outcome lives in the
/// service, the route only answers.
async fn web_delete_return_draft(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.customer_return_service.delete_draft(id).await?;
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "customer-return-changed".parse().unwrap());
    Ok(resp)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/customer-returns", get(customer_returns_page))
        .route("/customer-returns/{id}", get(customer_return_record_page))
        .route(
            "/web/customer-returns",
            get(web_customer_return_list).post(web_create_customer_return),
        )
        .route(
            "/web/customer-returns/{id}",
            get(web_customer_return_detail).delete(web_delete_return_draft),
        )
        .route(
            "/web/customer-returns/{id}/lines",
            post(web_add_return_line),
        )
        .route(
            "/web/customer-returns/{return_id}/lines/{line_id}",
            put(web_update_return_line)
                .post(web_update_return_line)
                .delete(web_remove_return_line),
        )
        .route(
            "/web/customer-returns/{id}/header",
            post(web_update_customer_return_header),
        )
        .route(
            "/web/customer-returns/{id}/confirm",
            post(web_confirm_customer_return),
        )
        .route(
            "/web/customer-returns/{id}/cancel",
            post(web_cancel_customer_return),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewProduct, NewSale, PaymentType, ProductKind};
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
        // The fixed test session every request authenticates with: a real session
        // with a full-catalog role, so `Require<P>` is genuinely satisfied and there
        // is no test-only auth bypass.
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

    /// Move stock IN so a sale has something to sell: `create_product` does not
    /// open a balance, and a credit note's confirm writes stock In, so an empty
    /// shelf would make the sale itself refuse.
    async fn restock(state: &AppState, product_id: i64, qty: &str) {
        state
            .inventory_service
            .record_movement(
                test_support::audit_actor_id(&state.pool).await.unwrap(),
                crate::models::NewMovement {
                    product_id,
                    qty: qty.parse().unwrap(),
                    movement_type: crate::models::MovementType::In,
                    reason: crate::models::MovementReason::Initial,
                    // `NewMovement::reference` is a plain `String`, not an
                    // `Option`: an opening balance is the one movement the app
                    // attributes to no document, and the column carries an empty
                    // string rather than a NULL for it.
                    reference: String::new(),
                    date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                },
            )
            .await
            .unwrap();
    }

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

    /// A CONFIRMED cash sale of `qty` at `price`, paid in full through the seeded
    /// Cash account, so a credit note against it has a refund to write.
    async fn confirmed_sale(state: &AppState, product_id: i64, qty: &str, price: &str) -> i64 {
        // A per-process suffix: one test may seed several sales, and
        // `customers.name` and `accounts.name` are both UNIQUE.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let customer = state
            .customer_service
            .create_customer(
                actor,
                crate::models::NewCustomer {
                    name: format!("Customer {product_id}-{seq}"),
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
                product_id,
                qty.parse().unwrap(),
                Some(price.parse().unwrap()),
            )
            .await
            .unwrap();
        let cash = ensure_cash_method(state, actor, seq).await;
        state
            .sales_service
            .confirm(actor, sale.id, Some(cash))
            .await
            .unwrap();
        sale.id
    }

    /// A Cash payment method bound to a real account, funded.
    ///
    /// The migrations seed no account, so the seeded `Cash` method row is
    /// UNASSIGNED and unusable for a payment — `resolve_account` refuses it. This
    /// matters MORE on the credit-note side than the purchase side: a credit
    /// note's refund is an `Expense`, so the overdraft guard reads the account's
    /// balance, and an unfunded one would make every confirm refuse. The fixture
    /// therefore creates the account and calls the same
    /// `ensure_defaults_for_account` the app's own account form calls, rather than
    /// binding the method with SQL behind the service's back.
    async fn ensure_cash_method(state: &AppState, actor: i64, seq: u32) -> i64 {
        // `accounts.name` is UNIQUE, so a per-process suffix keeps two fixtures in
        // one test process from colliding. The name is NOT literally "Caja": the
        // default-method rule keys off that exact word, so the suffix is passed
        // explicitly as the defaults argument rather than inferred from the name.
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
            .find(|m| m.name == "Cash" && m.account_id == Some(account.id))
            .expect("the default Cash method is now bound to the wallet account");
        // Fund it, so the sale's own collection and the note's refund both have a
        // real balance rather than a zero the guard would refuse against.
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

    async fn draft_return(state: &AppState, sale_id: i64, qty: &str) -> (i64, i64) {
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        let customer_return = state
            .customer_return_service
            .create_draft(
                actor,
                sale_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let record = state.sales_service.get_record(sale_id).await.unwrap();
        let line = state
            .customer_return_service
            .add_line(
                actor,
                customer_return.id,
                record.lines[0].id,
                qty.parse().unwrap(),
            )
            .await
            .unwrap();
        (customer_return.id, line.id)
    }

    /// **THE TEST THAT PINS DECISION 5 AS REVERSED ON 2026-10-01.**
    ///
    /// It used to pin the opposite: this page asserted the English catalog said
    /// "Customer returns" AND that the words "credit note" appeared NOWHERE on the
    /// English surface. The domain owner reversed that — "Take goods back" names a
    /// warehouse action, what renders here is a document, and the Spanish was
    /// already right. A test that pinned a naming decision must pin its
    /// replacement, or the next reader is free to undo it silently.
    ///
    /// Three assertions, and the middle one is the load-bearing one: the hint must
    /// name the button by the label the button actually carries. Title and hint can
    /// never agree by accident.
    #[tokio::test]
    async fn the_customer_returns_index_renders_the_localized_title_in_both_catalogs() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        set_locale(&state, "es-AR").await;
        let (status, html) = get_html_as(
            app.clone(),
            "/customer-returns",
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("Notas de crédito"),
            "the Spanish name is the specific accounting term, decision 4: {html:.600}"
        );

        set_locale(&state, "en-US").await;
        let (status, html) =
            get_html_as(app, "/customer-returns", Some(test_support::TEST_COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("Credit notes"),
            "the English page names the DOCUMENT. Decision 5 reversed 2026-10-01: the Spanish \
             already said 'Nota de crédito' and the English was the outlier, not the Spanish: \
             {html:.600}"
        );
        assert!(
            html.contains("choose Credit note"),
            "the hint names the button by the label it actually carries, so the two can never \
             drift apart: {html:.600}"
        );
        assert!(
            !html.contains("Take goods back"),
            "the label decision 5 first chose is retired. If it is back on the English surface, \
             the reversal was undone somewhere quiet: {html:.600}"
        );
    }

    /// THE permission decision, stated as behaviour: the parent's own read code
    /// opens the page, and nothing else does. The refusal names the code.
    #[tokio::test]
    async fn the_customer_returns_page_is_gated_on_the_parent_family_read_code() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, html) = get_html_as(
            app.clone(),
            "/customer-returns",
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        let other = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let (status, html) = get_html_as(
            app,
            "/customer-returns",
            Some(&test_support::cookie_for(&other)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.400}");
        assert!(
            html.contains("sales.read"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// **THE TEST THAT MAKES DECISION 1 ENFORCEABLE**, credit-note twin.
    ///
    /// The assertion is on the RENDERED HTML, not the template source: a template
    /// that grows a `unit_price` input, hidden or otherwise, fails here.
    #[tokio::test]
    async fn a_draft_customer_return_renders_with_no_price_input() {
        let state = test_state().await;
        let product = seed_product(&state, "CNOPRICE-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "9").await;
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let app = crate::routes::router(state);

        let (status, html) = get_html_as(
            app.clone(),
            &format!("/customer-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        for field in [
            "unit_price",
            "unit_price_gross",
            "unit_cost",
            "cost",
            "price",
            "price_basis",
        ] {
            let marker = format!("name=\"{field}\"");
            assert!(
                !html.contains(&marker),
                "a draft credit note must render no {field} input at all \
                 (a hidden or disabled one is still an input, and the next \
                 person to touch the template would re-enable it)"
            );
        }
        assert!(
            !html.contains("name=\"unit"),
            "the form posts a parent line and a quantity, nothing else"
        );
        // The frozen figure is nevertheless VISIBLE, as text.
        assert!(
            html.contains("9.00 USD"),
            "the parent sale's frozen price is rendered as text beside the quantity: {html:.2000}"
        );
        assert!(
            html.contains("Price at sale"),
            "the frozen price carries the label that says where it came from: {html:.2000}"
        );
    }

    #[tokio::test]
    async fn the_line_editor_shows_the_frozen_price_and_the_remaining_allowance() {
        let state = test_state().await;
        let product = seed_product(&state, "CALL-1").await;
        restock(&state, product, "20").await;
        let sale_id = confirmed_sale(&state, product, "8", "4").await;
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();

        // A first CONFIRMED credit note takes 3, so the editor has a non-zero
        // `taken` to show. The service's own guard is what makes this true; the
        // editor only reads.
        let sale_line = state.sales_service.get_record(sale_id).await.unwrap().lines[0].id;
        let first = state
            .customer_return_service
            .create_draft(
                actor,
                sale_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
                &None,
            )
            .await
            .unwrap();
        state
            .customer_return_service
            .add_line(actor, first.id, sale_line, "3".parse().unwrap())
            .await
            .unwrap();
        state
            .customer_return_service
            .confirm(actor, first.id)
            .await
            .unwrap();

        // A second DRAFT: the editor must show 8 sold, 3 taken, 5 remaining.
        let second = state
            .customer_return_service
            .create_draft(
                actor,
                sale_id,
                chrono::NaiveDate::from_ymd_opt(2026, 2, 2).unwrap(),
                &None,
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let (status, html) = get_html_as(
            app,
            &format!("/customer-returns/{}", second.id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        assert!(
            html.contains("Product CALL-1") && html.contains("CALL-1"),
            "the row names the parent line's product, read THROUGH the parent line: {html:.3000}"
        );
        assert!(
            html.contains("Sold")
                && html.contains("Already credited")
                && html.contains("Still creditable"),
            "the three allowance columns are labelled: {html:.3000}"
        );
        assert!(
            html.contains("4.00 USD"),
            "the frozen price is shown: {html:.3000}"
        );
    }

    /// The confirm round-trip through the ROUTE. A credit note's confirm writes
    /// stock IN (reason `Sale-return`), which is the direction opposite its parent
    /// sale's OUT — the assertion is that stock goes back UP.
    #[tokio::test]
    async fn confirming_a_customer_return_through_the_route_numbers_it_and_moves_stock_in() {
        let state = test_state().await;
        let product = seed_product(&state, "CCONF-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let stock_after_sale = stock_for(&state, product).await;
        assert_eq!(stock_after_sale, Decimal::from(5));
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form(
            app.clone(),
            &format!("/web/customer-returns/{return_id}/confirm"),
            &format!("return_id={return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("2026-SRET-000001"),
            "the confirmed note carries the SRET number: {html:.1500}"
        );
        assert!(
            html.contains("Confirmed"),
            "the status advanced: {html:.800}"
        );
        // The goods came BACK: 10 in, 5 sold, 2 credited.
        assert_eq!(stock_for(&state, product).await, Decimal::from(7));

        let number = state
            .customer_return_service
            .get_detail(return_id)
            .await
            .unwrap()
            .customer_return
            .credit_note_number
            .unwrap();
        // SIXTEEN characters, not the fifteen the feature document claims: a
        // four-character prefix and a six-digit sequence make sixteen, and the
        // model's own test says so and pins it. What matters here is the RELATIVE
        // width, which is what the short form was chosen for.
        assert_eq!(number.len(), 16, "YYYY-SRET-NNNNNN is sixteen characters");
        assert_eq!(
            crate::models::format_sale_number(2026, 1).len(),
            number.len(),
            "a credit note's number is exactly as wide as a sale's"
        );
        assert_eq!(
            crate::models::format_purchase_number(2026, 1).len(),
            number.len() + 1,
            "and one shorter than a purchase's"
        );
        assert_eq!(
            number,
            crate::models::format_customer_return_number(2026, 1),
            "the stored number is the one the formatter builds"
        );
    }

    /// The cancel round-trip: a confirmed note reverses, and the goods go back
    /// OUT.
    #[tokio::test]
    async fn cancelling_a_confirmed_customer_return_through_the_route_reverses_it() {
        let state = test_state().await;
        let product = seed_product(&state, "CCANCEL-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form(
            app.clone(),
            &format!("/web/customer-returns/{return_id}/confirm"),
            &format!("return_id={return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert_eq!(stock_for(&state, product).await, Decimal::from(7));

        let (status, html) = post_form(
            app,
            &format!("/web/customer-returns/{return_id}/cancel"),
            &format!("return_id={return_id}&reason=wrong+item"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            html.contains("Cancelled"),
            "the status advanced to cancelled: {html:.800}"
        );
        assert!(
            html.contains("wrong item"),
            "the reason is stored and shown: {html:.1500}"
        );
        assert_eq!(stock_for(&state, product).await, Decimal::from(5));
    }

    #[tokio::test]
    async fn deleting_a_customer_return_draft_through_the_route_removes_it() {
        let state = test_state().await;
        let product = seed_product(&state, "CDEL-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let app = crate::routes::router(state.clone());

        let (status, body) = delete_html(
            app.clone(),
            &format!("/web/customer-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(state
            .customer_return_service
            .get_detail(return_id)
            .await
            .is_err());
        let (status, _) = get_html_as(
            app,
            &format!("/customer-returns/{return_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A repeated parent line is a `Conflict` at the repository — the `UNIQUE
    /// (return_id, sale_line_id)` backstop — and a conflict is a 409, never a 500.
    #[tokio::test]
    async fn a_repeated_parent_line_on_one_credit_note_is_a_conflict_not_a_server_error() {
        let state = test_state().await;
        let product = seed_product(&state, "CDUP-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let (return_id, line_id) = draft_return(&state, sale_id, "2").await;
        let sale_line = state.sales_service.get_record(sale_id).await.unwrap().lines[0].id;
        let app = crate::routes::router(state.clone());

        let (status, body) = post_form(
            app,
            &format!("/web/customer-returns/{return_id}/lines"),
            &format!("return_id={return_id}&sale_line_id={sale_line}&qty=1"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "the same parent line twice is a conflict: {body}"
        );
        let detail = state
            .customer_return_service
            .get_detail(return_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(detail.lines[0].id, line_id);
    }

    #[tokio::test]
    async fn adding_a_line_through_the_route_freezes_the_parent_lines_price() {
        let state = test_state().await;
        let product = seed_product(&state, "CFREEZE-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "11").await;
        let app = crate::routes::router(state.clone());
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();

        let (status, body) = post_form(
            app.clone(),
            "/web/customer-returns",
            &format!("sale_id={sale_id}&return_date=2026-02-01"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.600}");
        let draft = state
            .customer_return_service
            .list(&CustomerReturnListFilter::default())
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.customer_return.sale_id == sale_id)
            .expect("the created draft is listed");
        let return_id = draft.customer_return.id;

        let sale_line = state.sales_service.get_record(sale_id).await.unwrap().lines[0].id;
        let (status, body) = post_form(
            app.clone(),
            &format!("/web/customer-returns/{return_id}/lines"),
            &format!("return_id={return_id}&sale_line_id={sale_line}&qty=3"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.600}");
        let detail = state
            .customer_return_service
            .get_detail(return_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(
            detail.lines[0].unit_price,
            Decimal::from(11),
            "the frozen price is the parent line's, never the request's"
        );
        assert_eq!(detail.total, Decimal::from(33));
        // The document was created THROUGH THE ROUTE, so its creator is the
        // session's principal and NOT the fixture's system sentinel: the create
        // route resolved its actor from the `Principal`, exactly as it must, and
        // this assertion is what says so.
        assert_eq!(
            detail.customer_return.created_by,
            session_user_id(&state).await,
            "the create route stamps the REQUEST'S principal, not the fixture's actor"
        );
        assert_ne!(
            detail.customer_return.created_by, actor,
            "the two ids must differ for this assertion to mean anything"
        );
        assert_eq!(
            detail.customer_return.updated_by,
            Some(session_user_id(&state).await),
            "and so does the line write"
        );
    }

    /// The write gates are the parent's write codes, and the cancel gate is the
    /// parent's CANCEL code rather than its create code.
    #[tokio::test]
    async fn the_customer_return_writes_are_gated_on_the_parent_family_write_codes() {
        let state = test_state().await;
        let product = seed_product(&state, "CGATE-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());
        let sale_line = state.sales_service.get_record(sale_id).await.unwrap().lines[0].id;

        for (uri, body, code) in [
            (
                "/web/customer-returns".to_string(),
                format!("sale_id={sale_id}"),
                "sales.create",
            ),
            (
                format!("/web/customer-returns/{return_id}/lines"),
                format!("sale_line_id={sale_line}&qty=1"),
                "sales.create",
            ),
            (
                format!("/web/customer-returns/{return_id}/confirm"),
                String::new(),
                "sales.create",
            ),
            (
                format!("/web/customer-returns/{return_id}/header"),
                "return_date=2026-02-01".to_string(),
                "sales.create",
            ),
        ] {
            let (status, html) = post_form(app.clone(), &uri, &body, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.300}");
            assert!(
                html.contains(code),
                "{uri} must refuse naming {code}: {html:.600}"
            );
        }

        let (status, html) = delete_html(
            app.clone(),
            &format!("/web/customer-returns/{return_id}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.300}");
        assert!(
            html.contains("sales.create"),
            "the delete refusal names the create code: {html:.600}"
        );

        let canceller = test_support::seed_session_with_permissions(
            &state.pool,
            &["sales.read", "sales.create"],
        )
        .await
        .unwrap();
        let (status, html) = post_form(
            app,
            &format!("/web/customer-returns/{return_id}/cancel"),
            &format!("return_id={return_id}"),
            Some(&test_support::cookie_for(&canceller)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.300}");
        assert!(
            html.contains("sales.cancel"),
            "the cancel refusal names the cancel code: {html:.600}"
        );
        assert!(state
            .customer_return_service
            .get_detail(return_id)
            .await
            .is_ok());
    }

    /// A quantity above the remaining allowance is the operator-triggerable
    /// refusal on this surface: a 400 with the figures that explain it, NOT a 500.
    #[tokio::test]
    async fn a_quantity_above_the_allowance_is_a_bad_request_naming_the_rule() {
        let state = test_state().await;
        let product = seed_product(&state, "CALLOW-1").await;
        restock(&state, product, "10").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        let (return_id, _) = draft_return(&state, sale_id, "2").await;
        let sale_line = state.sales_service.get_record(sale_id).await.unwrap().lines[0].id;
        let app = crate::routes::router(state);

        let (status, body) = post_form(
            app,
            &format!("/web/customer-returns/{return_id}/lines"),
            &format!("return_id={return_id}&sale_line_id={sale_line}&qty=9"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a quantity above the allowance is a rule refusal: {body}"
        );
        assert!(body.contains("cannot credit"), "{body}");
    }

    /// The list filter narrows and the empty result says so rather than erroring.
    #[tokio::test]
    async fn the_list_filter_narrows_by_status_and_an_unmatched_party_filter_is_empty() {
        let state = test_state().await;
        let product = seed_product(&state, "CFILT-1").await;
        restock(&state, product, "20").await;
        let sale_id = confirmed_sale(&state, product, "5", "7").await;
        draft_return(&state, sale_id, "2").await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);

        let (status, all) = get_html_as(app.clone(), "/customer-returns", cookie).await;
        assert_eq!(status, StatusCode::OK, "{all:.400}");
        assert!(all.contains("Draft"), "{all:.600}");

        let (status, drafts) =
            get_html_as(app.clone(), "/customer-returns?status=Draft", cookie).await;
        assert_eq!(status, StatusCode::OK, "{drafts:.400}");
        assert!(drafts.contains("Draft"), "{drafts:.600}");

        let (status, empty) =
            get_html_as(app.clone(), "/customer-returns?customer=NOSUCHCUST", cookie).await;
        assert_eq!(status, StatusCode::OK, "{empty:.400}");
        assert!(
            empty.contains("No customer returns yet"),
            "a filter that matches nothing says so: {empty:.600}"
        );
    }

    /// The mirror of the purchase family's form-error rule: whatever is
    /// unreadable or missing on the credit-note post, the operator reads a
    /// localized sentence — never a serde internal in English on a Spanish page.
    #[tokio::test]
    async fn a_credit_note_post_whose_field_cannot_be_read_answers_an_operator_sentence() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);
        set_locale(&state, "es-AR").await;

        for body in [
            "sale_id=&return_date=2026-02-01",
            "sale_id=2024-SALE-000001&return_date=2026-02-01",
            "return_date=2026-02-01",
            "sale_id=-3&return_date=2026-02-01",
        ] {
            let (status, response) =
                post_form(app.clone(), "/web/customer-returns", body, cookie).await;
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

    /// And the refusal is the catalog's own sentence in the active locale, not an
    /// English developer string that merely happens to be short.
    #[tokio::test]
    async fn a_credit_note_post_whose_field_cannot_be_read_answers_in_the_active_locale() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let cookie = Some(test_support::TEST_COOKIE);

        set_locale(&state, "es-AR").await;
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::CustomerReturnsFormUnreadable)
            .to_string();
        let (status, spanish) =
            post_form(app.clone(), "/web/customer-returns", "sale_id=", cookie).await;
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
        let (status, english) = post_form(app, "/web/customer-returns", "sale_id=", cookie).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{english:.400}");
        assert!(
            english.contains("form could not be read"),
            "the English catalog's own sentence: {english:.400}"
        );
    }
}
