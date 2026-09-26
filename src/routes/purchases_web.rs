// Purchases web: the `/purchases` list and the `/purchases/{id}` record page,
// Askama + HTMX. Thin handlers over PurchasesService; the record body lives in
// partials/purchase_detail.html and every action posts to
// `/web/purchases/{id}/...`, so the id always comes from the URL. The old
// collection endpoints (id in the form body) stay registered for existing
// callers.
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
use crate::models::{
    NewPurchase, PaymentType, PurchaseDetail, PurchaseListFilter, PurchaseRecord, PurchaseStatus,
    PurchaseSuggestions, UpdateProduct,
};
use crate::routes::{localized_refusal_error, AppState};
use crate::services::purchases::LineAddOutcome;

// S7 enforcement: every registered handler declares the permission its action
// needs (AC10); the collection adapters and the path handlers share ungated
// `*_impl` bodies so each registered boundary carries its own real gate. The
// mapping and its judgement calls are recorded in
// openspec/changes/2026-09-18-add-identity-module/tasks.md (S7 section).
use crate::security::authz::{
    InventoryRead, InventoryWrite, Nav, PurchasesCancel, PurchasesCreate, PurchasesRead, Require,
};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// The three sibling field ids (past the picker's supplier field) the draft's
/// inline header form owns, included with every post through the picker: the
/// header route maps invoice and notes unconditionally, so a post that omits
/// one would arrive as empty and silently wipe the stored value. One constant
/// because the page and the fragment must carry the identical field set.
const HEADER_SIBLING_INCLUDE: &str = "#record-purchase-date, #record-invoice-no, #record-notes";

/// A purchase detail plus the resolved supplier name (purchases store only the id),
/// the row's payment state, derived against `today` so the template never parses
/// a date, and whether some but not all of the money is already down (the meta
/// line names what was paid only then — settled rows say it once in the chip).
#[derive(Clone)]
pub struct PurchaseView {
    pub detail: PurchaseDetail,
    pub supplier_name: String,
    pub payment_state: PurchasePaymentState,
    pub partially_paid: bool,
}

/// The one word a row's status chip starts with (S6): the chip replaces the
/// badge cloud, so money and due date resolve to a single state per row. The
/// owed states render the word plus the amount still owed ("Due 42",
/// "Overdue 17"); Paid — nothing owed — is the one bare word. Draft and
/// Cancelled purchases keep their lifecycle chip regardless of money, so the
/// template branches on the status first, this second.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PurchasePaymentState {
    Paid,
    Due,
    Overdue,
}

impl std::fmt::Display for PurchasePaymentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paid => write!(f, "Paid"),
            Self::Due => write!(f, "Due"),
            Self::Overdue => write!(f, "Overdue"),
        }
    }
}

/// Nothing owed — due is zero or a refund — is Paid; owed with the due date
/// already past is Overdue; owed with the due date today or later — or absent
/// — is Due. Derived in the view layer, never in the template. Zero must
/// compare numerically, not by sign: `Decimal::ZERO.is_sign_positive()` is
/// `true`, and a settled purchase (due exactly 0) is Paid, never Due.
fn purchase_payment_state(detail: &PurchaseDetail, today: NaiveDate) -> PurchasePaymentState {
    if detail.due <= Decimal::ZERO {
        PurchasePaymentState::Paid
    } else if detail.purchase.due_date.map(|d| d < today) == Some(true) {
        PurchasePaymentState::Overdue
    } else {
        PurchasePaymentState::Due
    }
}

#[derive(Template)]
#[template(path = "purchases.html")]
struct PurchasesTemplate {
    title: String,
    localization: LocalizationContext,
    purchases: Vec<PurchaseView>,
    suggestions: PurchaseSuggestions,
    has_suggestions: bool,
    /// Live: the included `partials/suggestion_list.html` renders `{{ today }}`
    /// in its seed-options date input, so this is not the deleted creation
    /// card's leftover — the page include shares this struct's context.
    today: String,
    nav_key: &'static str,
    /// Current filter values, so a bookmarkable `/purchases?supplier=…`
    /// re-renders with the same form state the server used for the list.
    filter_status: String,
    filter_supplier: String,
    filter_number: String,
    filter_from: String,
    filter_to: String,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
    /// Whether the acting principal may refresh the reorder suggestions
    /// (`inventory.read`, the gate the suggestions fragment and API carry).
    /// When false the page renders no suggestions block at all — the coherent
    /// half of the old consequence where a `purchases.read`-only principal
    /// saw suggestions it could not refresh.
    show_suggestions: bool,
    /// The page header's primary action (the shared component reads the
    /// struct fields without locals): "New purchase" when the principal
    /// holds `purchases.create`, empty label = no action rendered (AC21:
    /// never an entry the principal cannot use).
    ///
    /// T3: the action is a dialog-opening button, not a navigation —
    /// `page_action_dialog` carries the dialog id and `page_action_href`
    /// stays empty (the component still compiles the anchor branch against
    /// this context, so the field must exist).
    page_action_href: String,
    page_action_label: String,
    /// The dialog the action opens (`new-purchase-dialog`), empty for a
    /// principal without `purchases.create` — the choosing is what creates,
    /// so a principal that cannot create is offered no dialog at all (AC7).
    page_action_dialog: String,
    /// The LAST USED supplier, pre-filled into the dialog's picker as the
    /// default: its NAME renders in the text field and resolves server-side —
    /// the field's text is the picker's whole contract, so no id travels
    /// with the form (an id wins only on a clicked result). Empty when no
    /// purchase exists yet — an empty database has no default, so the field
    /// opens empty and the operator chooses (the supplier is never silently
    /// guessed).
    current_supplier_name: String,
}

/// The `/purchases/{id}` record page. The page-header values are struct fields,
/// so the shared component and the record body read them straight from the shell.
#[derive(Template)]
#[template(path = "purchase.html")]
struct PurchasePageTemplate {
    /// Purchase number, or "Draft purchase" before confirmation.
    page_title: String,
    page_breadcrumb_label: String,
    page_breadcrumb_href: String,
    /// Empty label = no primary action (a cancelled purchase is read-only).
    page_action_href: String,
    page_action_label: String,
    /// T3: every page including the shared header carries the dialog id
    /// name; this page's action navigates (Record payment), so it stays
    /// empty and the component renders the anchor.
    page_action_dialog: String,
    record: PurchaseRecord,
    /// Visible confirm-dialog default. This is presentation derived from the
    /// supplier term; it is not written until the operator submits the form.
    confirm_due_date: Option<NaiveDate>,
    /// The entry row renders inside the money region and carries `autofocus`
    /// only on the add-line response, so the swapped-in copy claims focus for
    /// the next scan; the page itself always renders without it.
    entry_row_focus: bool,
    /// The action bar swaps out of band on the add-line response, so its
    /// enabled state (Confirm disabled at zero lines) follows the line count
    /// while the main swap only takes the money region.
    oob_action_bar: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    localization: LocalizationContext,
    /// Audit display names for the record body the page includes: resolved in
    /// the wiring layer (AC20).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    /// T4: the record body (included here) reads these for its inline
    /// header form — see `PurchaseDetailPartial` for the contract.
    header_action: String,
    header_include: &'static str,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/purchase_list.html")]
struct PurchaseListPartial {
    title: String,
    localization: LocalizationContext,
    purchases: Vec<PurchaseView>,
}

/// The server-rendered merge notice (S5b): a repeat scan at the same resolved
/// cost merged into the existing line, so the answer announces it instead of
/// silently changing a quantity. Markup and contract are documented in
/// `templates/partials/purchase_merge_notice.html`; the classes mirror
/// `partials/notice.html` and reuse only tokens already present there.
#[derive(Template)]
#[template(path = "partials/purchase_merge_notice.html")]
struct PurchaseMergeNotice {
    message: String,
    dismiss_label: String,
}

/// The record body, shared by the page and by every action response that swaps
/// `#purchase-record`, so the action forms travel with the fragment either way.
#[derive(Template)]
#[template(path = "partials/purchase_detail.html")]
struct PurchaseDetailPartial {
    record: PurchaseRecord,
    /// Visible confirm-dialog default. An explicit draft due date wins;
    /// otherwise this is purchase_date + the supplier's default term.
    confirm_due_date: Option<NaiveDate>,
    /// The entry row is persistent inside the money region; this flag only
    /// adds `autofocus` to its product field on the add-line response, so
    /// the swapped-in row claims focus for the next scan (htmx restores
    /// focus by id after the money-region swap).
    entry_row_focus: bool,
    /// The action bar swaps out of band on the add-line response, so its
    /// enabled state (Confirm disabled at zero lines) follows the line count
    /// while the main swap only takes the money region.
    oob_action_bar: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    localization: LocalizationContext,
    /// The purchase's creator and its last editor, as display names the
    /// wiring layer resolved (never the ids).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    /// T4: the draft's inline header posts the existing header route, and
    /// the picker's `include` names the three sibling field ids so every
    /// post (Enter, Save, a clicked result) carries the same field set —
    /// the header route maps invoice and notes unconditionally, so an
    /// absent field would arrive as empty and wipe the stored value.
    /// Computed in the wiring layer: Askama 0.12 has no string
    /// concatenation and the codebase passes such strings from Rust.
    header_action: String,
    header_include: &'static str,
}

#[derive(Template)]
#[template(path = "partials/suggestion_list.html")]
struct SuggestionListPartial {
    localization: LocalizationContext,
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

fn parse_required_decimal(
    s: &str,
    field: &str,
    localization: &LocalizationContext,
) -> AppResult<Decimal> {
    localization
        .parse_decimal(s.trim())
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_decimal(
    s: &str,
    field: &str,
    localization: &LocalizationContext,
) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    localization
        .parse_decimal(t)
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

async fn purchase_views(
    state: &AppState,
    filter: &PurchaseListFilter,
    today: NaiveDate,
) -> AppResult<Vec<PurchaseView>> {
    let details = state
        .purchases_service
        .list_details_filtered(filter)
        .await?;
    let mut out = Vec::with_capacity(details.len());
    for detail in details {
        let supplier = state
            .supplier_service
            .get_supplier(detail.purchase.supplier_id)
            .await?;
        out.push(PurchaseView {
            payment_state: purchase_payment_state(&detail, today),
            partially_paid: detail.paid > Decimal::ZERO && detail.due > Decimal::ZERO,
            detail,
            supplier_name: supplier.name,
        });
    }
    Ok(out)
}

fn render_list(
    view: Vec<PurchaseView>,
    title: &str,
    localization: LocalizationContext,
) -> AppResult<Html<String>> {
    let html = PurchaseListPartial {
        title: title.to_string(),
        localization,
        purchases: view,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Everything the record body renders: the resolved record plus the
/// method-with-account options its action forms need. The product picker
/// searches `/web/product-search.json` instead of carrying the whole catalogue.
struct PurchaseRecordContext {
    record: PurchaseRecord,
    confirm_due_date: Option<NaiveDate>,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    localization: LocalizationContext,
    /// Audit display names: the purchase's creator and its last editor (a
    /// header edit, a line change, the confirm or the cancel), resolved here
    /// in the wiring layer (AC20: the service never reads identity).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

async fn record_context(
    state: &AppState,
    purchase_id: i64,
    localization: LocalizationContext,
) -> AppResult<PurchaseRecordContext> {
    let record = state.purchases_service.get_record(purchase_id).await?;
    let supplier = state
        .supplier_service
        .get_supplier(record.purchase.supplier_id)
        .await?;
    let confirm_due_date = record.purchase.due_date.or_else(|| {
        supplier
            .due_days
            .map(|days| record.purchase.purchase_date + chrono::Duration::days(days))
    });
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let today = localization.today_iso();
    let mut actor_ids = vec![record.purchase.created_by];
    actor_ids.extend(record.purchase.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let created_by_name = name_for(record.purchase.created_by);
    let updated_by_name = record.purchase.updated_by.and_then(name_for);
    Ok(PurchaseRecordContext {
        record,
        confirm_due_date,
        method_options,
        today,
        localization,
        created_by_name,
        updated_by_name,
    })
}

fn render_record(
    context: PurchaseRecordContext,
    entry_row_focus: bool,
    oob_action_bar: bool,
) -> AppResult<Html<String>> {
    let header_action = format!("/web/purchases/{}/header", context.record.purchase.id);
    let html = PurchaseDetailPartial {
        record: context.record,
        confirm_due_date: context.confirm_due_date,
        entry_row_focus,
        oob_action_bar,
        method_options: context.method_options,
        today: context.today,
        localization: context.localization,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
        header_action,
        header_include: HEADER_SIBLING_INCLUDE,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Record-body response that keeps the cross-region `purchase-changed` refresh
/// event, so the subscribed list region updates after an action.
async fn changed(
    state: &AppState,
    purchase_id: i64,
    localization: &LocalizationContext,
) -> AppResult<Response> {
    changed_with_notice(state, purchase_id, localization, false, None).await
}

/// The add-line response with an optional out-of-band server notice prepended
/// to the body. The notice is a merge announcement (S5b): htmx strips the
/// `hx-swap-oob` wrapper before the `hx-select` main swap, so the money region
/// and the entry row's focus contract are untouched — the same mechanism the
/// create-under-filter case uses (`hidden_by_filter_notice_html` in
/// inventory_web.rs).
async fn changed_with_notice(
    state: &AppState,
    purchase_id: i64,
    localization: &LocalizationContext,
    entry_row_focus: bool,
    notice_html: Option<String>,
) -> AppResult<Response> {
    let mut html = render_record(
        record_context(state, purchase_id, localization.clone()).await?,
        entry_row_focus,
        true,
    )?
    .0;
    if let Some(notice) = notice_html {
        html = notice + &html;
    }
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

/// The purchases page is a single `purchases.read` gate. Creation happens in
/// the page's dialog (T3): the action opens it, and the dialog's post goes to
/// `POST /web/purchases` below. The reorder suggestions are
/// stock-derived data: the block renders only when the principal holds
/// `inventory.read`, the same gate the suggestions fragment and API carry, so
/// a purchases-only principal sees no suggestions block it could not refresh
/// (S7 part 2 closed the consequence the part 1 review recorded).
async fn purchases_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(query): Query<PurchaseListQuery>,
) -> Result<Html<String>, AppError> {
    let today = localization
        .today_iso()
        .parse()
        .map_err(|_| AppError::Internal("invalid localized date".into()))?;
    let purchases = purchase_views(&state, &query.to_filter(), today).await?;
    // The suggestion block renders only when the principal may refresh it:
    // the fragment (`/web/purchases/suggestions`) and the API twin are gated
    // `inventory.read` because the suggestion is stock-derived data, so the
    // server-rendered block obeys the same gate. A purchases-only principal
    // sees the purchases list without the suggestions section, never a block
    // that answers 403 on refresh.
    let show_suggestions = principal.has_permission::<InventoryRead>();
    let (suggestions, has_suggestions) = if show_suggestions {
        let suggestions = state.purchases_service.suggestions().await?;
        let has = !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
        (suggestions, has)
    } else {
        (PurchaseSuggestions::default(), false)
    };
    // The header's primary action opens the creation dialog (T3), so it is
    // offered only when the principal can create (AC21/AC7, the same rule
    // the sidebar applies): a `purchases.read`-only principal renders no
    // action and no dialog, and the choosing is what creates.
    let (page_action_href, page_action_label, page_action_dialog, current_supplier_name) =
        if principal.has_permission::<PurchasesCreate>() {
            // The last used supplier is the DIALOG's default, never a silent
            // guess: it renders as a real, editable NAME the operator can
            // override (feature doc, "The hazard this design has to respect").
            // Only the name travels — the picker's form resolves the field's
            // text server-side; no id rides along.
            let last = state.purchases_service.last_used_supplier().await?;
            (
                String::new(),
                localization
                    .tr(crate::localization::MessageKey::PurchasesNew)
                    .to_string(),
                "new-purchase-dialog".to_string(),
                last.map(|s| s.name).unwrap_or_default(),
            )
        } else {
            (String::new(), String::new(), String::new(), String::new())
        };
    // `today` stays: the included `partials/suggestion_list.html` renders it
    // as the seed form's default purchase date (see the struct field comment).
    let today = today.to_string();
    let tmpl = PurchasesTemplate {
        title: localization
            .tr(crate::localization::MessageKey::PurchasesAll)
            .to_string(),
        localization,
        purchases,
        suggestions,
        has_suggestions,
        today,
        nav_key: "purchases",
        filter_status: query.status.trim().to_string(),
        filter_supplier: query.supplier.trim().to_string(),
        filter_number: query.number.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        nav: Nav::for_principal(&principal),
        show_suggestions,
        page_action_href,
        page_action_label,
        page_action_dialog,
        current_supplier_name,
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// Query parameters for the purchases list filter, the same shape as
/// [`SaleListQuery`](crate::routes::sales_web::SaleListQuery). The document number
/// matches partially, because a user remembers a fragment of it.
#[derive(Debug, Deserialize, Default)]
pub struct PurchaseListQuery {
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

impl PurchaseListQuery {
    fn to_filter(&self) -> PurchaseListFilter {
        PurchaseListFilter {
            status: parse_optional_status(&self.status),
            supplier: clean_filter_text(&self.supplier),
            supplier_ids: None,
            number: clean_filter_text(&self.number),
            from: parse_optional_date_filter(&self.from),
            to: parse_optional_date_filter(&self.to),
        }
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

fn parse_optional_status(raw: &str) -> Option<PurchaseStatus> {
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

/// `/purchases/{id}`: a real page inside the shell. The label is the purchase
/// number or its draft state, and the single header action slot mirrors the
/// status. Deliberate single-gate consequence (same contract as S6's customer
/// statement): the record renders only THAT purchase's own data, so the gate
/// is `purchases.read` alone — a purchases-only principal never reaches the
/// purchases list or another supplier's documents.
async fn purchase_record_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(raw_id): Path<String>,
) -> Result<Html<String>, AppError> {
    // The id is parsed here rather than in the `Path<i64>` extractor so a
    // non-numeric segment — `/purchases/new` above all, since the creation
    // page was deleted (T3, AC2) — answers the 404 a missing record answers,
    // not the extractor's 400.
    let Ok(id) = raw_id.parse::<i64>() else {
        return Err(AppError::NotFound(format!("purchase {raw_id} not found")));
    };
    let context = record_context(&state, id, localization).await?;
    let label = match &context.record.purchase.purchase_number {
        Some(number) => number.clone(),
        None => context
            .localization
            .tr(crate::localization::MessageKey::PurchasesDraft)
            .to_string(),
    };
    let (action_href, action_label) = if context.record.purchase.status == PurchaseStatus::Confirmed
        && context.record.purchase.payment_type == PaymentType::Credit
    {
        (
            "#record-payment".to_string(),
            context
                .localization
                .tr(crate::localization::MessageKey::SalesRecordPayment)
                .to_string(),
        )
    } else {
        (String::new(), String::new())
    };
    let tmpl = PurchasePageTemplate {
        page_title: label,
        page_breadcrumb_label: context
            .localization
            .tr(crate::localization::MessageKey::NavigationPurchases)
            .to_string(),
        page_breadcrumb_href: "/purchases".to_string(),
        page_action_href: action_href,
        page_action_label: action_label,
        page_action_dialog: String::new(),
        record: context.record,
        confirm_due_date: context.confirm_due_date,
        entry_row_focus: false,
        oob_action_bar: false,
        method_options: context.method_options,
        today: context.today,
        localization: context.localization,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
        header_action: format!("/web/purchases/{}/header", id),
        header_include: HEADER_SIBLING_INCLUDE,
        nav_key: "purchases",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_purchase_list(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<PurchaseListQuery>,
) -> AppResult<Response> {
    let today = localization
        .today_iso()
        .parse()
        .map_err(|_| AppError::Internal("invalid localized date".into()))?;
    let view = purchase_views(&state, &query.to_filter(), today).await?;
    let title = localization
        .tr(crate::localization::MessageKey::PurchasesAll)
        .to_string();
    Ok(render_list(view, &title, localization)?.into_response())
}

async fn web_purchase_detail(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let html = render_record(
        record_context(&state, id, localization).await?,
        false,
        false,
    )?
    .0;
    Ok(Html(html).into_response())
}

/// `DELETE /web/purchases/{id}`: the documents drawer's draft delete — the
/// mirror of the sale flow, plus the discarded (never-confirmed) cancelled
/// purchase the service now admits. The same house shape as the other HTMX
/// writes: an empty 200 whose `HX-Trigger` tells the listening pages to
/// re-read the feed; the business outcome lives in the service, the route
/// only answers.
async fn web_delete_draft(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.purchases_service.delete_draft(id).await?;
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-changed".parse().unwrap());
    Ok(resp)
}

/// The suggestions fragment is stock-derived data (the list the reorder
/// panel renders): `inventory.read`, the same gate as its JSON twin. The
/// judgement call and its cost are recorded in the S7 mapping.
async fn web_purchase_suggestions(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> AppResult<Html<String>> {
    let suggestions = state.purchases_service.suggestions().await?;
    let has_suggestions =
        !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
    let today = localization.today_iso();
    let html = SuggestionListPartial {
        localization,
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
    /// An explicit id (a clicked picker result) wins over the typed name
    /// (the T2 picker's host contract). Empty/absent falls through to the
    /// name below.
    #[serde(default)]
    pub supplier_id: Option<i64>,
    /// The typed supplier name (the dialog's pre-filled or edited value);
    /// resolved server-side, never silently guessed.
    #[serde(default)]
    pub supplier_name: String,
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
    #[serde(default)]
    pub purchase_id: i64,
    /// An explicit id arrives from a clicked result; a scan arrives as `product`.
    #[serde(default)]
    pub product_id: Option<i64>,
    /// The typed or scanned value. The inventory service resolves it: exact
    /// barcode, then exact SKU (case-insensitive), then a numeric id.
    #[serde(default)]
    pub product: String,
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
    #[serde(default)]
    pub purchase_id: i64,
    #[serde(default)]
    pub method_id: String,
    /// Payment decided AT confirm (radio in the dialog). Empty/absent = a
    /// legacy caller that never asked: the stored header travels untouched.
    #[serde(default)]
    pub payment_type: String,
    /// The due date the dialog posts for Credit (empty for Cash → cleared).
    #[serde(default)]
    pub due_date: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    #[serde(default)]
    pub purchase_id: i64,
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelPurchaseForm {
    #[serde(default)]
    pub purchase_id: i64,
    #[serde(default)]
    pub reason: String,
}

/// The draft's identity form (purchases-create-and-header T4): the inline
/// header form on a draft's record page posts supplier, purchase date,
/// supplier invoice no and notes. The supplier resolves exactly as the
/// creation route does: an explicit id (a clicked picker result) wins;
/// otherwise the typed name must resolve exactly through
/// `SupplierService::resolve_supplier_name` or the route refuses, naming the
/// value — never a silent guess. The service remains the authority and
/// refuses a non-draft. The payment type stays out of this form because it
/// is decided at confirm, and the due date is untouched (`due_date: None` =
/// no-change), so a header edit cannot clear a Credit draft's stored due.
#[derive(Debug, Deserialize)]
pub struct UpdatePurchaseHeaderForm {
    /// An explicit id (a clicked picker result) wins over the typed name —
    /// the same precedence the creation route applies.
    #[serde(default)]
    pub supplier_id: Option<i64>,
    /// The typed supplier name; resolved exactly or the route refuses,
    /// naming the value (the supplier is never silently guessed).
    #[serde(default)]
    pub supplier_name: String,
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub supplier_invoice_no: String,
    #[serde(default)]
    pub notes: String,
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
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreatePurchaseForm>,
) -> AppResult<Response> {
    // The supplier is never silently guessed (the feature doc's hazard: the
    // supplier resolves every line's default cost): an explicit id — a
    // clicked picker result — wins (the T2 host contract); otherwise the
    // typed name resolves exactly or the route refuses, naming the value.
    // Neither id nor name is the required-field refusal. `purchase_date`
    // defaults to today when absent (the dialog carries no date field).
    let supplier_id = match form.supplier_id.filter(|id| *id > 0) {
        Some(id) => id,
        None => {
            state
                .supplier_service
                .resolve_supplier_name(&form.supplier_name)
                .await?
                .id
        }
    };
    let purchase = state
        .purchases_service
        .create_draft(
            principal.user_id,
            NewPurchase {
                supplier_id,
                payment_type: parse_payment_type(&form.payment_type)?,
                purchase_date: parse_date_or_today(&form.purchase_date, &localization)?,
                due_date: parse_opt_date(&form.due_date, "due_date")?,
                supplier_invoice_no: clean_opt(&form.supplier_invoice_no),
                notes: clean_opt(&form.notes),
            },
        )
        .await?;
    let location = format!("/purchases/{}", purchase.id);
    if is_htmx(&headers) {
        // AC4: htmx performs a real navigation to the new record, so an id is
        // never typed and the back button keeps working.
        return Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", location)
            .body(axum::body::Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }
    Ok(Redirect::to(&location).into_response())
}

async fn web_add_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<AddLineForm>,
) -> AppResult<Response> {
    web_add_line_impl(state, principal.user_id, headers, localization, id, form).await
}

async fn web_add_line_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: AddLineForm,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    let unit_cost = parse_opt_decimal(&form.unit_cost, "unit_cost", &localization)?;
    // An explicit product id (a clicked result) wins over the typed text; a scan
    // or an Enter carries only the value and resolves through inventory.
    let product_id = match form.product_id.filter(|id| *id > 0) {
        Some(id) => id,
        None => {
            state
                .inventory_service
                .resolve_product_ref(&form.product)
                .await?
                .id
        }
    };
    // The web route takes the merging method (S5b): a repeat product at the
    // same resolved cost increments the existing line, with a visible notice;
    // a different cost still answers the same 400. The JSON API keeps the
    // strict rule instead (`purchases_api.rs` add_line): a machine client is
    // told to use the line-update endpoint rather than have its request
    // silently reinterpreted. That asymmetry is deliberate and pinned by
    // `web_purchase_line_same_cost_repeat_merges_and_different_cost_stays_400`
    // and `api_purchase_line_repeated_product_is_still_a_clear_400`.
    let outcome = state
        .purchases_service
        .add_or_increment_line(actor, id, product_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        let notice = match &outcome {
            LineAddOutcome::Merged { line, product_name } => Some(
                PurchaseMergeNotice {
                    message: localization.tr_with(
                        crate::localization::MessageKey::NoticeMerged,
                        &[
                            ("product_name", product_name),
                            ("quantity", &localization.format_quantity(line.qty)),
                        ],
                    ),
                    dismiss_label: localization
                        .tr(crate::localization::MessageKey::AccessibilityDismiss)
                        .to_string(),
                }
                .render()
                .map_err(|e| AppError::Internal(e.to_string()))?,
            ),
            LineAddOutcome::Added(_) => None,
        };
        return changed_with_notice(&state, id, &localization, true, notice).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

/// Collection adapter: the typed-id form posts the purchase id in the body and
/// delegates to the path-based handler, so both URL shapes keep working.
async fn web_add_line_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<AddLineForm>,
) -> AppResult<Response> {
    web_add_line_impl(
        state,
        principal.user_id,
        headers,
        localization,
        form.purchase_id,
        form,
    )
    .await
}

async fn web_update_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
    Form(form): Form<UpdateLineForm>,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    let unit_cost = parse_required_decimal(&form.unit_cost, "unit_cost", &localization)?;
    state
        .purchases_service
        .update_line(principal.user_id, line_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, purchase_id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{purchase_id}")).into_response())
}

async fn web_remove_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    state
        .purchases_service
        .remove_line(principal.user_id, line_id)
        .await?;
    changed(&state, purchase_id, &localization).await
}

/// Cost-freshness T5 (gate corrected in T9): the stale-cost warning's action.
/// Applies a confirmed purchase line's recorded cost to its product — the one
/// `products.cost_price` write that is not a human product edit.
///
/// The client sends ONLY the line id (already in the URL): the product and the
/// cost are resolved server-side from the stored line, and the request body is
/// never read, so no caller can inject an amount. The whole point of the
/// button is that the price landing on the product is the one the supplier's
/// line recorded, not whatever a request carries.
///
/// Gate: this action writes a PRODUCT, so it carries `inventory.write` like
/// every other product write, never `purchases.create`. The button renders
/// on a confirmed purchase by design: the purchase page is gated
/// `purchases.read`, so
/// any operator who can open it sees the button, and one who lacks
/// `inventory.write` gets a visible refusal on click — the application's error
/// notice for the htmx request, not the forbidden page, which is what a
/// full-page navigation gets. The audit actor is the acting user, the way `web_edit_product`
/// passes it under the same permission.
///
/// Confirmed-only: the rule is the inverse of what it once was. Applying
/// makes sense precisely AFTER the document exists — that is when the line's
/// cost stops being provisional (a draft line can still be edited or deleted,
/// and the purchase may never be confirmed at all) and becomes a fact. The
/// button renders only on a confirmed purchase, and the handler refuses
/// anything else itself. `PurchasesService::ensure_draft` stays private
/// inside the service, so the handler holds the same single status comparison
/// against `PurchaseStatus::Confirmed` here, in the service's own message
/// shape, instead of silently duplicating the guard.
///
/// The write goes through `InventoryService::update_product` with a patch
/// carrying only `cost_price` — never SQL, never the purchase flow. That path
/// recomputes a markup-derived `sale_price`, which a raw cost write would
/// leave stale behind the new cost.
async fn web_apply_line_cost(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let detail = state.purchases_service.get_detail(purchase_id).await?;
    if detail.purchase.status != PurchaseStatus::Confirmed {
        return Err(AppError::Validation(format!(
            "purchase {purchase_id} is not confirmed (status {})",
            detail.purchase.status
        )));
    }
    let line = detail
        .lines
        .iter()
        .find(|line| line.id == line_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "purchase line {line_id} not found in purchase {purchase_id}"
            ))
        })?;
    state
        .inventory_service
        .update_product(
            principal.user_id,
            line.product_id,
            UpdateProduct {
                cost_price: Some(line.unit_cost),
                ..Default::default()
            },
        )
        .await
        // A cost-only patch re-runs the price rules, so this surface can answer a
        // PRICE refusal — a zero cost on a product carrying a markup, most of all.
        // It answers it through the shared renderer the product form and the
        // ladder use, not a wording of its own: one rule, one sentence, in every
        // locale. `localized_refusal_error` passes every other error through
        // untouched, so the status and the body of a non-price refusal are
        // exactly what they were.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, purchase_id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{purchase_id}")).into_response())
}

async fn web_confirm_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<ConfirmPurchaseForm>,
) -> AppResult<Response> {
    web_confirm_purchase_impl(state, principal.user_id, headers, &localization, id, form).await
}

async fn web_confirm_purchase_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    localization: &LocalizationContext,
    id: i64,
    form: ConfirmPurchaseForm,
) -> AppResult<Response> {
    let method_id = parse_opt_i64(&form.method_id, "method_id")?;
    // Decision #7: validate/update the header FIRST (type + due, clearing the
    // due for Cash), THEN call the existing confirm — service rules stay
    // untouched. The patch mirrors the header-edit form's exact semantics:
    // `due_date: Some(parse_opt_date(..))`, so an empty value CLEARS the due
    // date and an absent type skips the update entirely (a legacy caller can
    // never silently flip Credit to Cash through parse's Cash default).
    // A confirm refusal after a successful update leaves the draft updated
    // but unconfirmed — pinned by web_confirm_failure_leaves_the_draft_updated_but_unconfirmed.
    if !form.payment_type.trim().is_empty() {
        let payment_type = parse_payment_type(&form.payment_type)?;
        let due_date = parse_opt_date(&form.due_date, "due_date")?;
        state
            .purchases_service
            .update_draft(
                actor,
                id,
                crate::models::UpdatePurchaseDraft {
                    payment_type: Some(payment_type),
                    due_date: Some(due_date),
                    ..Default::default()
                },
            )
            .await?;
    }
    state
        .purchases_service
        .confirm(actor, id, method_id)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_confirm_purchase_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<ConfirmPurchaseForm>,
) -> AppResult<Response> {
    web_confirm_purchase_impl(
        state,
        principal.user_id,
        headers,
        &localization,
        form.purchase_id,
        form,
    )
    .await
}

async fn web_record_payment(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<RecordPaymentForm>,
) -> AppResult<Response> {
    web_record_payment_impl(state, principal.user_id, headers, localization, id, form).await
}

async fn web_record_payment_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: RecordPaymentForm,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount", &localization)?;
    let date = parse_date_or_today(&form.date, &localization)?;
    state
        .purchases_service
        .record_payment(actor, id, form.method_id, amount, date)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_record_payment_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<RecordPaymentForm>,
) -> AppResult<Response> {
    web_record_payment_impl(
        state,
        principal.user_id,
        headers,
        localization,
        form.purchase_id,
        form,
    )
    .await
}

async fn web_cancel_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<CancelPurchaseForm>,
) -> AppResult<Response> {
    web_cancel_purchase_impl(state, principal.user_id, headers, &localization, id, form).await
}

async fn web_cancel_purchase_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    localization: &LocalizationContext,
    id: i64,
    form: CancelPurchaseForm,
) -> AppResult<Response> {
    state
        .purchases_service
        .cancel(actor, id, clean_opt(&form.reason))
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_cancel_purchase_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<CancelPurchaseForm>,
) -> AppResult<Response> {
    web_cancel_purchase_impl(
        state,
        principal.user_id,
        headers,
        &localization,
        form.purchase_id,
        form,
    )
    .await
}

/// Edit the draft header in place (purchases-create-and-header T4): the
/// inline header form on a draft's record page posts here — supplier,
/// purchase date, supplier invoice no and notes. The supplier resolves
/// exactly as the creation route does: an explicit id (a clicked picker
/// result) wins; otherwise the typed name must resolve exactly through
/// `SupplierService::resolve_supplier_name` or the route refuses, naming the
/// value — never a silent guess. The service remains the authority and
/// refuses a non-draft. The payment type stays out of this form because it
/// is decided at confirm (purchase-payment-at-confirm T4), and the due date
/// is untouched (`due_date: None` = no-change), so a header edit cannot
/// clear a Credit draft's stored due.
async fn web_update_purchase_header(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<UpdatePurchaseHeaderForm>,
) -> AppResult<Response> {
    let purchase_date = parse_opt_date(&form.purchase_date, "purchase_date")?;
    // An explicit id wins (the T2 host contract); otherwise the typed name
    // resolves exactly or the route refuses — an absent/empty name is the
    // same refusal, never a silent keep-or-guess.
    let supplier_id = match form.supplier_id.filter(|id| *id > 0) {
        Some(id) => id,
        None => {
            state
                .supplier_service
                .resolve_supplier_name(&form.supplier_name)
                .await?
                .id
        }
    };
    state
        .purchases_service
        .update_draft(
            principal.user_id,
            id,
            crate::models::UpdatePurchaseDraft {
                supplier_id: Some(supplier_id),
                purchase_date,
                due_date: None,
                supplier_invoice_no: Some(clean_opt(&form.supplier_invoice_no)),
                notes: Some(form.notes),
                ..Default::default()
            },
        )
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

/// Seed a Draft pedido from one suggested low-stock product: the service
/// re-derives the suggestion (chosen supplier, qty, satellite cost) so the form
/// never decides business values.
/// One draft pedido seeded from one suggested product: the suggestion is
/// re-derived by the service (a service composition, never a permission
/// grant), and the document it creates is a purchase — `purchases.create`.
async fn web_seed_from_suggestion(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
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
        .create_draft(
            principal.user_id,
            NewPurchase {
                supplier_id: item.supplier_id,
                payment_type: parse_payment_type(&form.payment_type)?,
                purchase_date: parse_date_or_today(&form.purchase_date, &localization)?,
                due_date: parse_opt_date(&form.due_date, "due_date")?,
                supplier_invoice_no: None,
                notes: None,
            },
        )
        .await?;
    state
        .purchases_service
        .add_line(
            principal.user_id,
            purchase.id,
            item.product.id,
            item.suggested_qty,
            Some(item.unit_cost),
        )
        .await?;
    if is_htmx(&headers) {
        // The seeded draft opens its record, so the suggestion ends on the page
        // where its line can be reviewed and confirmed.
        return Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", format!("/purchases/{}", purchase.id))
            .body(axum::body::Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }
    Ok(Redirect::to(&format!("/purchases/{}", purchase.id)).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/purchases", get(purchases_page))
        .route("/purchases/{id}", get(purchase_record_page))
        .route(
            "/web/purchases",
            get(web_purchase_list).post(web_create_purchase),
        )
        .route("/web/purchases/suggestions", get(web_purchase_suggestions))
        .route(
            "/web/purchases/from-suggestion",
            post(web_seed_from_suggestion),
        )
        .route("/web/purchases/lines", post(web_add_line_collection))
        .route(
            "/web/purchases/confirm",
            post(web_confirm_purchase_collection),
        )
        .route(
            "/web/purchases/payments",
            post(web_record_payment_collection),
        )
        .route(
            "/web/purchases/cancel",
            post(web_cancel_purchase_collection),
        )
        .route(
            "/web/purchases/{id}",
            get(web_purchase_detail).delete(web_delete_draft),
        )
        .route("/web/purchases/{id}/lines", post(web_add_line))
        .route(
            "/web/purchases/{purchase_id}/lines/{line_id}",
            put(web_update_line)
                .post(web_update_line)
                .delete(web_remove_line),
        )
        .route(
            "/web/purchases/{purchase_id}/lines/{line_id}/apply-cost",
            post(web_apply_line_cost),
        )
        .route(
            "/web/purchases/{id}/header",
            post(web_update_purchase_header),
        )
        .route("/web/purchases/{id}/confirm", post(web_confirm_purchase))
        .route("/web/purchases/{id}/payments", post(web_record_payment))
        .route("/web/purchases/{id}/cancel", post(web_cancel_purchase))
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

    use crate::models::PaymentType;
    use crate::routes::AppState;
    use crate::security::test_support;
    use rust_decimal::Decimal;
    // T5's markup test builds an `UpdateProduct` patch through the service.
    use crate::models::UpdateProduct;

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests seed their own users instead, because there
    /// the point is telling two actors apart.
    async fn audit_actor(state: &AppState) -> i64 {
        test_support::audit_actor_id(&state.pool).await.unwrap()
    }

    /// A decimal literal for service-layer assertions.
    fn dec_web(s: &str) -> Decimal {
        s.parse().unwrap()
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

    async fn set_locale(state: &AppState, locale_code: &str, language_code: &str) {
        sqlx::query("INSERT OR IGNORE INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES (?, ?, ?, 1)")
            .bind(locale_code)
            .bind(language_code)
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

    async fn post_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
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
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    async fn delete_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
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
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// POST like the record page does, keeping the response status, the
    /// `HX-Redirect` header and the body for assertions.
    async fn post_form_response(
        app: axum::Router,
        uri: &str,
        body: &str,
    ) -> (StatusCode, Option<String>, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let redirect = resp
            .headers()
            .get("HX-Redirect")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            redirect,
            String::from_utf8_lossy(&bytes).to_string(),
        )
    }

    /// `post_form_response` with the PUT verb: the inline line edit.
    async fn put_form_response(
        app: axum::Router,
        uri: &str,
        body: &str,
    ) -> (StatusCode, Option<String>, String) {
        let req = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let redirect = resp
            .headers()
            .get("HX-Redirect")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            redirect,
            String::from_utf8_lossy(&bytes).to_string(),
        )
    }

    /// The opening tag that carries `needle`, for attribute assertions such as
    /// `hx-confirm` on the cancel control.
    fn element_tag_containing<'a>(html: &'a str, needle: &str) -> &'a str {
        let pos = html
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
        let start = html[..pos]
            .rfind('<')
            .expect("the attribute must sit inside a tag");
        let end = pos + html[pos..].find('>').expect("unterminated tag");
        &html[start..=end]
    }

    /// A row's rendered HTML, sliced from its `id` to the closing `</tr>`.
    fn row_with_id<'a>(html: &'a str, id: &str) -> &'a str {
        let start = html
            .find(&format!("id=\"{id}\""))
            .unwrap_or_else(|| panic!("row {id} not rendered: {html:.600}"));
        let after = &html[start..];
        let end = after
            .find("</tr>")
            .unwrap_or_else(|| panic!("row {id} has no closing tag"));
        &after[..end]
    }

    /// Cuts the `<form>...</form>` region that contains `needle`, for structural
    /// assertions such as "the results container is not inside the picker form".
    fn enclosing_form<'a>(html: &'a str, needle: &str) -> &'a str {
        let pos = html
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
        let start = html[..pos]
            .rfind("<form")
            .expect("needle must sit in a form");
        let end = html[pos..].find("</form>").expect("form must close");
        &html[start..pos + end + "</form>".len()]
    }

    /// Cuts one `<dialog id="{id}">…</dialog>` region, for dialog-scoped
    /// assertions (confirm type/due controls).
    fn slice_dialog<'a>(html: &'a str, id: &str) -> &'a str {
        let start_tag = format!("<dialog id=\"{id}\"");
        let start = html
            .find(&start_tag)
            .unwrap_or_else(|| panic!("the {id} dialog renders: {html:.400}"));
        let end =
            html[start..].find("</dialog>").expect("the dialog closes") + start + "</dialog>".len();
        &html[start..end]
    }

    /// The add-line response must bring the entry row back inside the swapped
    /// money region, empty and focused, so the next scan lands without a
    /// click. The picker is no longer out of band on purchases: it travels
    /// inside `#purchase-record-money` (the action bar alone rides OOB).
    fn assert_entry_row_is_empty_and_focused(html: &str) {
        // Exactly one entry row renders per record, inside the money region
        // the add response swaps — never out of band.
        assert_eq!(
            html.matches("id=\"line-picker\"").count(),
            1,
            "the entry row renders exactly once, inside the money region: {html:.800}"
        );
        let row_pos = html
            .find("id=\"line-picker\"")
            .expect("the entry row renders on the add-line response");
        let row_start = html[..row_pos]
            .rfind('<')
            .expect("the id must sit inside a tag");
        let tag_end = row_start + html[row_start..].find('>').expect("unterminated tag");
        let row_tag = &html[row_start..=tag_end];
        assert!(
            !row_tag.contains("hx-swap-oob"),
            "the picker is no longer out of band; it travels inside the money region: {row_tag}"
        );
        let row = &html[row_pos..];
        let input_pos = row
            .find("id=\"product-picker\"")
            .expect("the entry row renders its product field");
        let input_start = row[..input_pos].rfind('<').unwrap();
        let input_end = input_pos + row[input_pos..].find('>').unwrap();
        let input_tag = &row[input_start..=input_end];
        assert!(
            input_tag.contains("autofocus"),
            "the entry row must come back focused: {input_tag}"
        );
        assert!(
            !input_tag.contains("value="),
            "the entry row must come back empty: {input_tag}"
        );
        for id in ["id=\"line-qty\"", "id=\"line-unit-cost\""] {
            assert!(
                row.contains(id),
                "the entry row carries the qty and the cost field: {id}: {row:.600}"
            );
        }
    }

    /// Everything a record-page test needs to address the seeded document.
    struct RecordFixture {
        purchase_id: i64,
        line_id: i64,
        product_id: i64,
        product_name: String,
        product_sku: String,
        supplier_id: i64,
        supplier_name: String,
        account_id: i64,
        method_id: i64,
        account_name: String,
        method_name: String,
    }

    /// One draft purchase with one line, plus an account configured with Cash, so a
    /// record-page test can drive draft, confirmed, paid and cancelled states.
    async fn seed_record_fixture(state: &AppState, payment_type: PaymentType) -> RecordFixture {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, ProductKind};
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        // A per-process suffix: one test may seed several fixtures, and the
        // product SKU / supplier name must not collide across them.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sku = format!("REC-PUR-{seq}");

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku,
                    name: "Record purchase product".into(),
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
                audit_actor(&state).await,
                NewSupplier {
                    name: format!("Record Supplier {seq}"),
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
                audit_actor(&state).await,
                NewPurchase {
                    supplier_id: supplier.id,
                    payment_type,
                    purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: match payment_type {
                        PaymentType::Credit => Some(NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        PaymentType::Cash => None,
                    },
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let line = state
            .purchases_service
            .add_line(
                audit_actor(&state).await,
                purchase.id,
                product.id,
                Decimal::from(2),
                None,
            )
            .await
            .unwrap();
        // accounts.name is UNIQUE: suffix it per fixture, and still pass the
        // canonical "Caja" to the defaults helper so the Cash method seeds.
        let account = state
            .account_service
            .create(audit_actor(&state).await, &format!("Caja {seq}"))
            .await
            .unwrap();
        // Purchase payments are Expenses; fund the account so the guard flag under
        // test is the record shape, not a zero balance.
        state
            .transaction_service
            .create(
                audit_actor(&state).await,
                account.id,
                crate::models::TransactionKind::Income,
                Decimal::from(1000),
                Some("fixture funding".into()),
                NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(audit_actor(&state).await, account.id, "Caja")
            .await
            .unwrap();
        let method = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("Cash is seeded by migrations");

        RecordFixture {
            purchase_id: purchase.id,
            line_id: line.id,
            product_id: product.id,
            product_name: product.name,
            product_sku: product.sku,
            supplier_id: supplier.id,
            supplier_name: supplier.name,
            account_id: account.id,
            method_id: method.id,
            account_name: account.name,
            method_name: method.name,
        }
    }

    #[tokio::test]
    async fn seeded_payment_method_labels_are_bilingual_and_ids_stay_canonical() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        set_locale(&state, "en-US", "en").await;
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let method_options = html.split("name=\"method_id\"").nth(1).unwrap_or(&html);
        assert!(method_options.contains("Cash — Caja"), "{method_options}");
        assert!(
            method_options.contains("Bank transfer — unassigned"),
            "{method_options}"
        );
        assert!(
            html.contains(&format!("value=\"{}\"", fixture.method_id)),
            "the canonical method id must remain unchanged: {html:.1200}"
        );

        set_locale(&state, "es-ES", "es").await;
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Efectivo — Caja"), "{html:.1200}");
        assert!(
            html.contains("Transferencia bancaria — sin asignar"),
            "{html:.1200}"
        );
    }

    /// A second product for scan and click flows, so the fixture's own line is
    /// never repeated by accident.
    async fn seed_extra_product(
        state: &AppState,
        sku: &str,
        barcode: Option<&str>,
    ) -> crate::models::Product {
        use crate::models::{NewProduct, ProductKind};
        use rust_decimal::Decimal;

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku: sku.into(),
                    name: format!("prod {sku}"),
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
        if let Some(code) = barcode {
            state
                .inventory_service
                .add_barcode(product.id, code)
                .await
                .unwrap();
        }
        product
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
        assert!(
            html.contains("Suggestions"),
            "page should have the suggestion panel"
        );
    }

    // -- N3: the purchase record page ------------------------------------------

    /// The typed-id forms are gone. The purchases list renders no `purchase_id`
    /// input, drops the side-panel detail target and links every row to its record
    /// page, so an id is never typed. (redesign-interface N3)
    #[tokio::test]
    async fn web_purchases_page_has_no_typed_id_forms_and_links_records() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("name=\"purchase_id\""),
            "the purchases list must not ask for a typed purchase id: {html:.600}"
        );
        assert!(
            !html.contains("id=\"purchase-detail\""),
            "the side-panel detail must be gone: {html:.600}"
        );
        assert!(
            html.contains(&format!("href=\"/purchases/{}\"", fixture.purchase_id)),
            "every row must link to its record: {html:.600}"
        );
        assert!(
            html.contains("Suggestions"),
            "the suggestion panel stays on the list page: {html:.600}"
        );
    }

    /// The creation dialog (purchases-create-and-header T3): the /purchases
    /// page renders no supplier roster select and never a payment input — the
    /// dialog holds the T2 picker (a text field, not a `<select>`) and the
    /// Sugerido seed options ask purchase date only.
    #[tokio::test]
    async fn web_purchase_creation_dialog_asks_no_payment_and_no_roster_select() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let (status, list_html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !list_html.contains("name=\"payment_type\""),
            "no payment-type input on the purchases page: {list_html:.600}"
        );
        assert!(
            !list_html.contains("name=\"due_date\""),
            "no due-date input on the purchases page: {list_html:.600}"
        );
        assert!(
            !list_html.contains("Draft type") && !list_html.contains("Due date"),
            "the seed options ask purchase date only: {list_html:.600}"
        );
        // The old creation page's roster select is gone with it; the picker
        // is a text field, never a `<select>` over the roster.
        assert!(
            !list_html.contains("<select name=\"supplier_id\""),
            "the dialog must not render a supplier roster select: {list_html:.600}"
        );
    }

    /// The server default is pinned (decision #4): a create whose form omits
    /// `payment_type` stores Cash, never a rejected submit — the form no
    /// longer offers the field, so omitting it is the only path.
    #[tokio::test]
    async fn web_create_purchase_omitting_payment_type_defaults_to_cash() {
        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Default Cash Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, redirect, resp) = post_form_response(
            app,
            "/web/purchases",
            &format!("supplier_id={}&purchase_date=2024-05-10", supplier.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("the create must land on the record");
        let purchase_id: i64 = redirect["/purchases/".len()..].parse().unwrap();
        let detail = state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.payment_type,
            PaymentType::Cash,
            "an omitted type defaults to Cash server-side"
        );
        assert!(detail.purchase.due_date.is_none(), "no due at creation");
    }

    /// Same server default on the Sugerido seed: the seed options post only
    /// product + purchase date, and the draft lands Cash.
    #[tokio::test]
    async fn web_seed_from_suggestion_omitting_payment_type_defaults_to_cash() {
        use crate::models::{NewProduct, ProductKind};
        use rust_decimal::Decimal;

        let state = test_state().await;
        let actor = audit_actor(&state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "SEED-NOTYPE".into(),
                    name: "seed no type".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price: Decimal::from(10),
                    track_stock: true,
                    min_stock: Some(Decimal::from(5)),
                    max_stock: Some(Decimal::from(50)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        state
            .inventory_service
            .record_movement(
                actor,
                crate::models::NewMovement {
                    product_id: product.id,
                    qty: Decimal::from(2),
                    movement_type: crate::models::MovementType::In,
                    reason: crate::models::MovementReason::Initial,
                    reference: "seed".into(),
                    date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                },
            )
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(
                actor,
                crate::models::NewSupplier {
                    name: "Seed NoType Sup".into(),
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
                product.id,
                supplier.id,
                Decimal::from(7),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Exactly what the seed options post after T1: product + date only.
        let (status, redirect, resp) = post_form_response(
            app,
            "/web/purchases/from-suggestion",
            &format!("product_id={}&purchase_date=2024-05-10", product.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("the seed must land on the record");
        let purchase_id: i64 = redirect["/purchases/".len()..].parse().unwrap();
        let detail = state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.payment_type,
            PaymentType::Cash,
            "an omitted seed type defaults to Cash server-side"
        );
        assert!(detail.purchase.due_date.is_none());
        assert_eq!(detail.lines.len(), 1, "the seed still adds its line");
    }

    /// T2 markup: the confirm dialog is where payment is decided. It carries
    /// a Cash/Credit radio prefilled from the stored value, the due-date
    /// input active (required + prefilled) ONLY for Credit, and the method
    /// select active (enabled + required) ONLY for Cash — the inactive side
    /// is hidden AND disabled so it can neither be seen nor submitted.
    #[tokio::test]
    async fn web_confirm_dialog_decides_payment_type_and_due_at_confirm() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        // -- Cash draft: Cash checked, method active, due block inert -------
        let cash = seed_record_fixture(&state, PaymentType::Cash).await;
        let (_, cash_html) =
            get_html(app.clone(), &format!("/purchases/{}", cash.purchase_id)).await;
        let cash_dialog = slice_dialog(&cash_html, "confirm-purchase");
        assert!(
            cash_dialog.contains("name=\"payment_type\""),
            "the confirm dialog carries the type radio: {cash_dialog:.500}"
        );
        let cash_radio = element_tag_containing(cash_dialog, "value=\"Cash\"");
        let credit_radio = element_tag_containing(cash_dialog, "value=\"Credit\"");
        assert!(
            cash_radio.contains("checked"),
            "Cash prefills: {cash_radio}"
        );
        assert!(
            !credit_radio.contains("checked"),
            "only the stored type is checked: {credit_radio}"
        );
        let method_tag = element_tag_containing(cash_dialog, "id=\"confirm-method\"");
        assert!(
            method_tag.contains("required") && !method_tag.contains("disabled"),
            "a Cash confirm requires the method: {method_tag}"
        );
        let due_block = element_tag_containing(cash_dialog, "id=\"confirm-due-block\"");
        assert!(
            due_block.contains("hidden"),
            "no due input shows for Cash: {due_block}"
        );
        let due_tag = element_tag_containing(cash_dialog, "id=\"confirm-due-date\"");
        assert!(
            !due_tag.contains("required"),
            "the hidden due input must not block a Cash submit: {due_tag}"
        );

        // -- Credit draft: Credit checked, due active + prefilled, method inert
        let credit = seed_record_fixture(&state, PaymentType::Credit).await;
        let (_, credit_html) =
            get_html(app.clone(), &format!("/purchases/{}", credit.purchase_id)).await;
        let credit_dialog = slice_dialog(&credit_html, "confirm-purchase");
        let credit_radio = element_tag_containing(credit_dialog, "value=\"Credit\"");
        let cash_radio = element_tag_containing(credit_dialog, "value=\"Cash\"");
        assert!(
            credit_radio.contains("checked"),
            "Credit prefills: {credit_radio}"
        );
        assert!(!cash_radio.contains("checked"), "{cash_radio}");
        let due_tag = element_tag_containing(credit_dialog, "id=\"confirm-due-date\"");
        assert!(
            due_tag.contains("required") && due_tag.contains("value=\"2024-06-02\""),
            "the Credit due input is required and prefilled from storage: {due_tag}"
        );
        let due_block = element_tag_containing(credit_dialog, "id=\"confirm-due-block\"");
        assert!(
            !due_block.contains("hidden"),
            "the Credit due input shows: {due_block}"
        );
        let method_tag = element_tag_containing(credit_dialog, "id=\"confirm-method\"");
        assert!(
            method_tag.contains("disabled") && !method_tag.contains("required"),
            "a Credit submit must not carry a method: {method_tag}"
        );
        let method_block = element_tag_containing(credit_dialog, "id=\"confirm-method-block\"");
        assert!(
            method_block.contains("hidden"),
            "the Credit dialog hides the method: {method_block}"
        );
    }

    /// T2 route, Cash path: the dialog posts type + due (empty) + method;
    /// the route updates the draft header first — clearing the due date with
    /// the exact header-edit semantics (`due_date: Some(parse)`, empty →
    /// clear) — then confirms. A Credit draft switched to Cash confirms with
    /// no due date left behind.
    #[tokio::test]
    async fn web_confirm_cash_path_clears_due_then_confirms() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            &format!(
                "payment_type=Cash&due_date=&method_id={}",
                fixture.method_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.status,
            crate::models::PurchaseStatus::Confirmed
        );
        assert_eq!(detail.purchase.payment_type, PaymentType::Cash);
        assert!(
            detail.purchase.due_date.is_none(),
            "a Cash confirm leaves no due date: {:?}",
            detail.purchase.due_date
        );
    }

    /// T2 route, Credit path: the dialog's chosen due date persists through
    /// the header update and survives the confirm (service rule: Credit
    /// requires it).
    #[tokio::test]
    async fn web_confirm_credit_path_persists_the_chosen_due_date() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "payment_type=Credit&due_date=2024-07-15&method_id=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.status,
            crate::models::PurchaseStatus::Confirmed
        );
        assert_eq!(detail.purchase.payment_type, PaymentType::Credit);
        assert_eq!(
            detail.purchase.due_date.map(|d| d.to_string()),
            Some("2024-07-15".to_string())
        );
    }

    /// T2 route gating: a form that omits `payment_type` (the legacy
    /// collection callers) leaves the header untouched — the stored type and
    /// due date travel to confirm exactly as they are, so no caller can
    /// silently flip Credit to Cash (parse would default an empty type).
    #[tokio::test]
    async fn web_confirm_omitting_payment_type_leaves_the_header_untouched() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "method_id=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.status,
            crate::models::PurchaseStatus::Confirmed
        );
        assert_eq!(detail.purchase.payment_type, PaymentType::Credit);
        assert_eq!(
            detail.purchase.due_date.map(|d| d.to_string()),
            Some("2024-06-02".to_string()),
            "the stored due date is unchanged when the form omits the type"
        );
    }

    /// Decision #7 pinned: confirm route order is update-then-confirm, and a
    /// confirm failure leaves the draft UPDATED but unconfirmed. Here the
    /// header update succeeds (Credit + a new due date), then the service
    /// refuses the confirm (a Credit confirm may not carry a method), so the
    /// new due date is already stored while the status stays Draft.
    #[tokio::test]
    async fn web_confirm_failure_leaves_the_draft_updated_but_unconfirmed() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            &format!(
                "payment_type=Credit&due_date=2024-07-15&method_id={}",
                fixture.method_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
        assert!(
            resp.contains("must not include a payment method"),
            "the refusal is the service's credit rule: {resp}"
        );
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.status,
            crate::models::PurchaseStatus::Draft,
            "a failed confirm must not flip the status"
        );
        assert_eq!(
            detail.purchase.due_date.map(|d| d.to_string()),
            Some("2024-07-15".to_string()),
            "the header update already happened before the confirm failed"
        );
    }

    /// AC4: creating a purchase answers `HX-Redirect` to its record, so htmx
    /// performs a real navigation and no id is typed.
    #[tokio::test]
    async fn web_create_purchase_redirects_to_the_record() {
        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Redirect Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let body = format!(
            "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
            supplier.id
        );
        let (status, redirect, resp) = post_form_response(app, "/web/purchases", &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("AC4: the create response must carry HX-Redirect");
        assert!(redirect.starts_with("/purchases/"), "{redirect}");
        let purchase_id: i64 = redirect["/purchases/".len()..]
            .parse()
            .unwrap_or_else(|_| panic!("HX-Redirect must end in the purchase id: {redirect}"));
        let detail = state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.supplier_id, supplier.id);
    }

    /// The non-HTMX form path also lands on the record page, so a browser without
    /// htmx still never sees a list to retype an id from.
    #[tokio::test]
    async fn web_create_purchase_redirects_a_plain_form_to_the_record() {
        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Plain Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let body = format!(
            "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
            supplier.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get("location")
            .expect("a plain create must redirect to the record")
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with("/purchases/"), "{location}");
    }

    /// AC5 + name resolution: `/purchases/{id}` is a real page inside the shell
    /// carrying the supplier name, product names and SKUs, and account and method
    /// names; an unknown id is 404 with the existing error shape.
    #[tokio::test]
    async fn web_purchase_record_page_resolves_names_and_unknown_id_is_404() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        state
            .purchases_service
            .record_payment(
                audit_actor(&state).await,
                fixture.purchase_id,
                fixture.method_id,
                Decimal::from(10),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
            )
            .await
            .unwrap();
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let payment_id = detail.payments[0].id;
        let purchase_number = detail
            .purchase
            .purchase_number
            .clone()
            .expect("a confirmed purchase carries its number");
        let app = crate::routes::router(state);

        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("data-page-header"),
            "record uses the page header"
        );
        assert!(
            html.contains("data-nav=\"purchases\"") && html.contains("aria-current=\"page\""),
            "record page keeps the purchases nav key"
        );
        assert!(html.contains(&fixture.supplier_name), "{html:.600}");
        assert!(html.contains(&purchase_number), "{html:.600}");

        let line_row = row_with_id(&html, &format!("purchase-line-{}", fixture.line_id));
        assert!(line_row.contains(&fixture.product_name), "{line_row}");
        assert!(line_row.contains(&fixture.product_sku), "{line_row}");
        assert!(!line_row.contains("product #"), "{line_row}");

        let payment_row = row_with_id(&html, &format!("purchase-payment-{payment_id}"));
        assert!(payment_row.contains(&fixture.account_name), "{payment_row}");
        assert!(payment_row.contains(&fixture.method_name), "{payment_row}");
        assert!(!payment_row.contains("account #"), "{payment_row}");
        assert!(!payment_row.contains("method #"), "{payment_row}");

        let (status, body) = get_html(app, "/purchases/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("purchase 999999 not found"), "{body}");
        assert!(!body.contains("route not found"), "{body}");
    }

    /// AC6: the actions offered match the document status. A draft can add a line,
    /// edit its header, confirm and discard; a confirmed credit purchase can record
    /// payments and cancel but cannot edit lines or the header; a cancelled one is
    /// read-only and shows its reason.
    #[tokio::test]
    async fn web_purchase_record_actions_are_status_gated() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        for target in [
            format!("{base}/lines"),
            format!("{base}/header"),
            format!("{base}/confirm"),
            format!("{base}/cancel"),
        ] {
            assert!(
                html.contains(&target),
                "a draft must offer {target}: {html:.400}"
            );
        }
        assert!(
            !html.contains(&format!("{base}/payments")),
            "a draft must not offer payment recording"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!("{base}/payments")),
            "a confirmed credit purchase must offer payment recording"
        );
        assert!(html.contains(&format!("{base}/cancel")));
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed purchase must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed purchase must not edit its header"
        );

        state
            .purchases_service
            .cancel(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("wrong order"),
            "a cancelled purchase must show its reason: {html:.400}"
        );
        for target in [
            format!("{base}/lines"),
            format!("{base}/header"),
            format!("{base}/confirm"),
            format!("{base}/payments"),
            format!("{base}/cancel"),
        ] {
            assert!(
                !html.contains(&target),
                "a cancelled purchase must be read-only, found {target}"
            );
        }
    }

    /// Triangulation for AC6: a confirmed Cash purchase is settled at confirm, so
    /// it offers cancel but no payment form, and its lines and header stay frozen.
    #[tokio::test]
    async fn web_purchase_record_confirmed_cash_has_no_payment_form() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("{base}/cancel")),
            "a confirmed cash purchase can still be cancelled"
        );
        assert!(
            !html.contains(&format!("{base}/payments")),
            "a confirmed cash purchase must not offer payment recording"
        );
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed cash purchase must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed cash purchase must not edit its header"
        );
    }

    /// Cost-freshness T9: a confirmed purchase whose line cost rose above the
    /// product's stored cost renders the stale-cost warning with BOTH numbers,
    /// as a sub-row underneath the line row (never inside it, so the line
    /// row's text order stays product · qty · cost, which the browser suite
    /// asserts as-is). The response that confirms swaps the record body, so
    /// the warning appears right after confirming, inside the region the
    /// operator is already looking at: `#purchase-record-money`.
    #[tokio::test]
    async fn web_purchase_record_confirmed_flags_a_rising_line_cost_on_the_fragment() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        // The fixture's line sits at the product's stored cost (10); push the
        // line's cost above it to reach the warning's condition. The purchase
        // is still a draft here, and the warning must NOT appear yet — that
        // is exactly what the inversion test pins.
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Confirming via the route is the journey's own moment: the response
        // that confirms swaps the refreshed record body, so it must carry the
        // warning inside `#purchase-record-money` — where the operator
        // already is, right after the click. (On a draft this used to be
        // asserted through the add-line response; on a confirmed purchase
        // add-line is refused, and the confirm response is what creates the
        // warning.)
        let (status, _, confirmed) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            &format!("method_id={}", fixture.method_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{confirmed:.800}");
        let money_start = confirmed
            .find("id=\"purchase-record-money\"")
            .expect("the confirm response must render the money region");
        let money_end = confirmed[money_start..]
            .find(">Payments (")
            .expect("the money region must be followed by the payments heading");
        let money = &confirmed[money_start..money_start + money_end];
        assert!(
            money.contains(">stale cost<"),
            "the confirm response must carry the warning inside #purchase-record-money: {money:.800}"
        );
        assert!(
            money.contains("line cost 12 USD") && money.contains("stored 10 USD"),
            "the fragment must show both numbers: {money:.800}"
        );

        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(">stale cost<"),
            "a rising line cost must be flagged on the confirmed record page: {html:.800}"
        );
        assert!(
            html.contains("line cost 12 USD"),
            "the warning must show the line's cost: {html:.800}"
        );
        assert!(
            html.contains("stored 10 USD"),
            "the warning must show the stored cost it is behind: {html:.800}"
        );

        // Placement: the line row itself keeps its text order — the warning is
        // a separate sub-row after it, still inside the lines table.
        let line_row = row_with_id(&html, &format!("purchase-line-{}", fixture.line_id));
        assert!(
            !line_row.contains("stale cost"),
            "the warning must not sit inside the line row: {line_row}"
        );
        let after_line_row = &html[html.find(line_row).unwrap() + line_row.len()..];
        let table_end = after_line_row
            .find("</tbody>")
            .expect("the lines table must close");
        assert!(
            after_line_row[..table_end].contains(">stale cost<"),
            "the warning must render as a sub-row right below its line: {}",
            &after_line_row[..table_end]
        );
    }

    /// Cost-freshness T4 triangulation: an equal cost is not stale, so the
    /// confirmed purchase renders no warning at all.
    #[tokio::test]
    async fn web_purchase_record_confirmed_hides_the_cost_warning_when_costs_are_equal() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            !html.contains("stale cost"),
            "an equal cost is not stale: {html:.800}"
        );
    }

    /// Cost-freshness T9 triangulation: the warning is confirmed-only. The
    /// inverse of the rule it used to assert: on a DRAFT the cost is still
    /// provisional — the line can be edited or deleted, and the purchase may
    /// never be confirmed — so even a genuinely rising line cost renders no
    /// warning. The product drawer already carries the permanent badge for
    /// the standing disagreement.
    #[tokio::test]
    async fn web_purchase_record_draft_purchase_hides_the_rising_cost_warning() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            !html.contains("stale cost"),
            "a draft purchase must not carry the warning: its cost is provisional: {html:.800}"
        );
    }

    /// Triangulation for the status gate: the template is presentation only.
    /// Posting the hidden draft actions directly at a confirmed purchase still
    /// reaches the service, which refuses them (400) and leaves the document
    /// untouched.
    #[tokio::test]
    async fn web_purchase_record_confirmed_refuses_draft_actions_at_the_service() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        // Header edit: refused, and the header keeps its values.
        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("{base}/header"),
            "purchase_date=2024-05-03&due_date=&supplier_invoice_no=X&notes=nope",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-02");
        assert!(detail.purchase.notes.is_empty());

        // Line add: the same route that works on a draft is refused by the service.
        let (status, _, body) = post_form_response(
            app,
            &format!("{base}/lines"),
            &format!("product={}&qty=1&unit_cost=", fixture.product_sku),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(after.lines.len(), detail.lines.len());
    }

    // -- Cost-freshness T5: the stale-cost warning ACTS -----------------------

    /// T5: applying writes the line's recorded cost into the product. The line
    /// sits at 12 after the rise, the product's stored cost at 10, so after
    /// the POST the product must carry the line's 12 — the whole feature: the
    /// supplier's recorded price becomes the product's cost.
    #[tokio::test]
    async fn web_apply_line_cost_writes_the_line_cost_into_the_product() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        // The action belongs after the document exists: confirming is when
        // the line's cost becomes a fact.
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let product = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(
            product.cost_price,
            Decimal::from(12),
            "the product must carry the line's cost, not its old stored one"
        );
    }

    /// T5: the load-bearing link to the earlier feature. The product carries a
    /// markup, so `update_product` DERIVES `sale_price` from the incoming
    /// cost; a raw SQL cost write (or a write through the purchase flow) would
    /// leave the sale price stale behind the new cost. 12.00 * 1.50 = 18.00.
    #[tokio::test]
    async fn web_apply_line_cost_recomputes_a_markup_derived_sale_price() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        // Give the fixture's product a 50% markup (its stored cost 10 > 0, so
        // the markup validates). The service then derives every sale price.
        state
            .inventory_service
            .update_product(
                audit_actor(&state).await,
                fixture.product_id,
                UpdateProduct {
                    markup_pct: Some(Some(Decimal::from(50))),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let product = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(product.cost_price, Decimal::from(12), "{product:?}");
        assert_eq!(
            product.sale_price,
            Decimal::from(18),
            "the markup must re-derive the sale price from the applied cost"
        );
    }

    /// A THIRD surface writes a product's price inputs, and it is the one the
    /// product drawer cannot warn about: this action patches `cost_price` only,
    /// through `InventoryService::update_product`, so it re-runs the very price
    /// rules the drawer previews. A zero cost on a product that carries a markup
    /// is refused — there is nothing to derive a price from — and a Spanish
    /// operator must read that refusal in Spanish, through the SAME shared
    /// renderer the product form and the ladder go through. A second mapping here
    /// would be a second wording of one rule, which is the failure the shared
    /// renderer exists to make impossible.
    #[tokio::test]
    async fn web_apply_line_cost_answers_a_price_refusal_in_the_active_locale() {
        for (locale_code, language_code, expected) in [
            (
                "en-US",
                "en",
                "cost_price must be > 0 when markup_pct is set",
            ),
            (
                "es-AR",
                "es",
                "El costo debe ser mayor que 0 cuando se indica un margen.",
            ),
        ] {
            let state = test_state().await;
            let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
            set_locale(&state, locale_code, language_code).await;
            // The product now DERIVES its price from the cost (its stored cost
            // is 10, so the markup validates), and the line the operator applies
            // costs 0: the derivation has nothing to derive from.
            state
                .inventory_service
                .update_product(
                    audit_actor(&state).await,
                    fixture.product_id,
                    UpdateProduct {
                        markup_pct: Some(Some(Decimal::from(50))),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            state
                .purchases_service
                .update_line(
                    audit_actor(&state).await,
                    fixture.line_id,
                    Decimal::from(2),
                    Decimal::ZERO,
                )
                .await
                .unwrap();
            state
                .purchases_service
                .confirm(
                    audit_actor(&state).await,
                    fixture.purchase_id,
                    Some(fixture.method_id),
                )
                .await
                .unwrap();
            let app = crate::routes::router(state.clone());

            let (status, _, body) = post_form_response(
                app,
                &format!(
                    "/web/purchases/{}/lines/{}/apply-cost",
                    fixture.purchase_id, fixture.line_id
                ),
                "",
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{locale_code}: a refused cost is still a 400: {body}"
            );
            assert!(
                body.contains(expected),
                "{locale_code}: the purchase flow must answer this price refusal in the \
                 operator's own language: {body}"
            );
            if language_code == "es" {
                assert!(
                    !body.contains("cost_price must be"),
                    "a Spanish operator must never read the English refusal: {body}"
                );
            }
            // And the refusal wrote nothing: the product still carries the cost
            // the fixture gave it, because a language change is not a rule change.
            let product = state
                .inventory_service
                .get_product(fixture.product_id)
                .await
                .unwrap();
            assert_eq!(
                product.cost_price,
                dec_web("10"),
                "{locale_code}: a refused apply must leave the product untouched"
            );
        }
    }

    /// T5: the action writes a PRODUCT, so it is gated `inventory.write` like
    /// every other product write — never `purchases.create`. A principal
    /// holding only the purchase permission (the one the button's screen is
    /// about) is refused with the forbidden page; the product is untouched.
    #[tokio::test]
    async fn web_apply_line_cost_refuses_a_principal_without_inventory_write() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.create"])
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body) = post_form_as(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
            &[("HX-Request", "true")],
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("inventory.write") || body.contains("Action not permitted"),
            "the refusal must be visible, not silent: {body:.400}"
        );

        let product = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(
            product.cost_price,
            Decimal::from(10),
            "a refused principal must not move the product's cost"
        );
    }

    /// T9: the gate is now Confirmed-only, so the refused one is the DRAFT —
    /// the inverse of the old rule, and the test that pins it. The handler
    /// refuses anything that is not a confirmed purchase (the template is
    /// presentation only), and a refused purchase leaves the product carrying
    /// its old stored cost: a draft line's cost is provisional, precisely
    /// because the line can still be edited or the purchase never confirmed.
    #[tokio::test]
    async fn web_apply_line_cost_refuses_a_draft_purchase_and_leaves_the_product_untouched() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a draft purchase must be refused: {body}"
        );
        assert!(
            body.contains("is not confirmed"),
            "the refusal must say why: the cost is not a fact yet: {body:.400}"
        );

        let product = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(
            product.cost_price,
            Decimal::from(10),
            "a refused draft must leave the product untouched"
        );
    }

    /// T5: the price being applied is the one RECORDED on the line. The route
    /// takes no cost field at all (the client sends only the line id in the
    /// URL), so a body stuffed with attacker-chosen amounts is simply ignored
    /// and the stored line cost is what lands on the product.
    #[tokio::test]
    async fn web_apply_line_cost_ignores_a_client_supplied_cost() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // The same body an attacker would send to write an arbitrary cost.
        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "cost_price=999&unit_cost=0.01&product_id=7",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let product = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(
            product.cost_price,
            Decimal::from(12),
            "the STORED line cost must land, never a client-supplied one"
        );
    }

    /// T5: the response swaps the refreshed record body, so the warning for a
    /// line whose cost now matches the product is gone from the very response
    /// that applied it.
    #[tokio::test]
    async fn web_apply_line_cost_response_drops_the_warning_when_the_cost_matches() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.contains(">stale cost<"),
            "the applied line must lose its warning: {body:.800}"
        );
        let row = row_with_id(&body, &format!("purchase-line-{}", fixture.line_id));
        assert!(
            row.contains("12 USD"),
            "the refreshed fragment still shows the line at its applied cost: {row}"
        );
    }

    /// T5: the line is resolved only INSIDE the purchase's own lines. Posting
    /// purchase A's id with purchase B's line id must answer NotFound and leave
    /// BOTH products' costs untouched — a handler that returned 404 after
    /// writing would slip past a status-only assertion, so the stored costs are
    /// checked too. The matching pair still applies in the same state, so the
    /// 404 is caused by the mismatch, not by a broken setup.
    #[tokio::test]
    async fn web_apply_line_cost_refuses_a_line_from_another_purchase_and_leaves_both_products_untouched(
    ) {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        // The second purchase reuses the fixture's supplier and follows the
        // established two-purchase shape: `seed_extra_product` for its own
        // product (a purchase takes one line per product), then a draft and
        // its line through the service like the fixture does.
        let product_b = seed_extra_product(&state, "CROSS-APPLY", None).await;
        let purchase_b = state
            .purchases_service
            .create_draft(
                audit_actor(&state).await,
                crate::models::NewPurchase {
                    supplier_id: fixture.supplier_id,
                    payment_type: PaymentType::Cash,
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let line_b = state
            .purchases_service
            .add_line(
                audit_actor(&state).await,
                purchase_b.id,
                product_b.id,
                Decimal::from(1),
                None,
            )
            .await
            .unwrap();
        // Distinct recorded costs, both above the stored 10, so a stray write
        // of either cost onto either product is visible.
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                line_b.id,
                Decimal::from(1),
                Decimal::from(15),
            )
            .await
            .unwrap();
        // Both purchases confirmed: the action exists only after the document
        // does, so the boundary test must run in the state where applying is
        // possible at all.
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                purchase_b.id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Purchase A's id, purchase B's line id: the mismatched pair.
        let (status, _, body) = post_form_response(
            app.clone(),
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, line_b.id
            ),
            "",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "another purchase's line is not this purchase's line: {body:.400}"
        );

        let product_a = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        let product_b_after = state
            .inventory_service
            .get_product(product_b.id)
            .await
            .unwrap();
        assert_eq!(
            product_a.cost_price,
            Decimal::from(10),
            "the refused pair must not move product A's cost"
        );
        assert_eq!(
            product_b_after.cost_price,
            Decimal::from(10),
            "the refused pair must not move product B's cost"
        );

        // The matching pair in the same state still applies: the 404 above is
        // the boundary, not a broken fixture.
        let (status, _, body) = post_form_response(
            app,
            &format!(
                "/web/purchases/{}/lines/{}/apply-cost",
                fixture.purchase_id, fixture.line_id
            ),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let product_a = state
            .inventory_service
            .get_product(fixture.product_id)
            .await
            .unwrap();
        assert_eq!(
            product_a.cost_price,
            Decimal::from(12),
            "the matching pair applies the line's recorded cost"
        );
        let product_b_after = state
            .inventory_service
            .get_product(product_b.id)
            .await
            .unwrap();
        assert_eq!(
            product_b_after.cost_price,
            Decimal::from(10),
            "applying A's line must not touch B's product"
        );
    }

    /// AC7: cancelling asks for confirmation before the request is sent; the
    /// confirm control carries `hx-confirm`. The same holds for discarding a draft.
    #[tokio::test]
    async fn web_purchase_record_cancel_asks_for_confirmation() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let cancel_needle = format!("hx-post=\"/web/purchases/{}/cancel\"", fixture.purchase_id);
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let discard = element_tag_containing(&html, &cancel_needle);
        assert!(
            discard.contains("hx-confirm"),
            "discarding a draft must ask first: {discard}"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let cancel = element_tag_containing(&html, &cancel_needle);
        assert!(
            cancel.contains("hx-confirm"),
            "cancelling a confirmed purchase must ask first: {cancel}"
        );
    }

    /// Receiving-desk T3: the four-card action grid is replaced by a sticky
    /// action bar (one primary action per status plus a `⋯` secondary menu),
    /// and every action form lives in a `<dialog>` the bar opens. The legacy
    /// ids keep their meaning so in-page anchors still resolve: `#confirm-
    /// purchase` / `#discard-purchase` / `#record-payment` /
    /// `#cancel-purchase` are the dialogs. The Edit header dialog is deleted
    /// (T4): a draft's identity fields are the always-visible inline header
    /// form, so no `#edit-header` anchor remains. The add-line drawer is gone: the
    /// entry row is persistent inside the money region, so the bar carries no
    /// drawer button and the page offers no `#add-line` anchor. Cancelled is
    /// read-only: no bar, no menu, no dialogs, no entry row. The add-line
    /// response carries the bar out of band, because its main swap only takes
    /// the money region (HTMX processes OOB before `hx-select`).
    #[tokio::test]
    async fn web_purchase_record_renders_sticky_action_bar_and_status_dialogs() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        // -- Draft: sticky bar + secondary menu + entry row + dialogs --------
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let bar_tag = element_tag_containing(&html, "id=\"purchase-action-bar\"");
        assert!(
            bar_tag.contains("sticky"),
            "the action bar must stick under the header: {bar_tag}"
        );
        assert!(
            html.contains("id=\"record-menu\""),
            "the secondary menu renders: {html:.400}"
        );
        assert!(
            !html.contains(">Edit header</button>"),
            "the menu no longer offers Edit header (T4): {html:.400}"
        );
        assert!(
            !html.contains("id=\"line-drawer\""),
            "the add-line drawer is deleted; the entry row replaces it: {html:.400}"
        );
        assert!(
            !html.contains("id=\"add-line\""),
            "the bar carries no drawer button and the page no drawer anchor: {html:.400}"
        );
        // The entry row is persistent: product, qty, cost and the Add action
        // render without any click, inside the money region adds swap.
        assert!(
            html.contains("id=\"line-picker\"")
                && html.contains("id=\"product-picker\"")
                && html.contains("id=\"line-qty\"")
                && html.contains("id=\"line-unit-cost\""),
            "the entry row renders its fields persistent: {html:.600}"
        );
        for dialog in ["confirm-purchase", "discard-purchase"] {
            assert!(
                html.contains(&format!("<dialog id=\"{dialog}\"")),
                "the draft renders the {dialog} dialog: {html:.400}"
            );
        }
        // T4: the draft's header is the always-visible editable form, so the
        // Edit header dialog and its menu entry are gone for good.
        assert!(
            html.contains("id=\"purchase-header-form\""),
            "the draft renders the inline header form: {html:.400}"
        );
        assert!(
            !html.contains("id=\"edit-header\"")
                && !html.contains("openRecordDialog('edit-header')"),
            "the Edit header dialog and its menu entry are gone: {html:.400}"
        );
        assert!(
            !html.contains("min-[760px]:grid-cols-2"),
            "the four-card action grid is gone: {html:.400}"
        );
        // The discard form sits inside its dialog, not in a loose card.
        let dialog_start = html
            .find("<dialog id=\"discard-purchase\"")
            .expect("the discard dialog renders");
        let form_pos = html
            .find(&format!("hx-post=\"{base}/cancel\""))
            .expect("the discard form posts cancel");
        let dialog_end = dialog_start
            + html[dialog_start..]
                .find("</dialog>")
                .expect("the discard dialog closes");
        assert!(
            form_pos > dialog_start && form_pos < dialog_end,
            "the discard form must live inside its dialog"
        );

        // The add-line response refreshes the bar out of band: the main swap
        // takes only `#purchase-record-money`, and the bar's enabled state
        // (Confirm disabled at zero lines) must follow the line count.
        let extra = seed_extra_product(&state, "BAR-EXTRA", None).await;
        let (status, _, added) = post_form_response(
            app.clone(),
            &format!("{base}/lines"),
            &format!("product_id={}&qty=1&unit_cost=", extra.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{added:.400}");
        let oob_bar = element_tag_containing(&added, "id=\"purchase-action-bar\"");
        assert!(
            oob_bar.contains("hx-swap-oob"),
            "the bar must ride out of band on add-line: {oob_bar}"
        );

        // -- Confirmed (credit): payment primary + cancel in the menu --------
        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("id=\"purchase-action-bar\"") && html.contains("id=\"record-menu\""),
            "a confirmed purchase keeps the bar and its menu: {html:.400}"
        );
        assert!(
            html.contains("<dialog id=\"record-payment\""),
            "the payment form moves into a dialog: {html:.400}"
        );
        assert!(
            html.contains("<dialog id=\"cancel-purchase\""),
            "cancelling a confirmed purchase asks via its dialog: {html:.400}"
        );
        assert!(
            !html.contains("id=\"line-drawer\"") && !html.contains("id=\"line-picker\""),
            "a confirmed purchase cannot add lines: {html:.400}"
        );
        // The bare id is not matched: the record page's shell script names
        // #purchase-header-form as a selector string, so only the element
        // itself (id="purchase-header-form") proves a rendered form.
        assert!(
            !html.contains("<dialog id=\"confirm-purchase\"")
                && !html.contains("id=\"purchase-header-form\"")
                && !html.contains("id=\"record-supplier\""),
            "the header form and the confirm dialog freeze once confirmed: {html:.400}"
        );
        // The read-only facts stay: the supplier name, the due date and the
        // audit line render exactly as before.
        assert!(
            html.contains(&fixture.supplier_name) && html.contains("due 06/02/2024"),
            "a confirmed purchase still shows its header facts: {html:.400}"
        );
        assert!(
            html.contains("data-purchase-actor"),
            "a confirmed purchase keeps the audit line: {html:.400}"
        );

        // -- Cancelled: read-only, no bar, no menu, no dialogs ---------------
        state
            .purchases_service
            .cancel(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        for id in [
            "purchase-action-bar",
            "record-menu",
            "line-picker",
            "confirm-purchase",
            "record-payment",
            "purchase-header-form",
            "record-supplier",
        ] {
            assert!(
                !html.contains(&format!("id=\"{id}\"")),
                "a cancelled purchase is read-only, found {id}: {html:.400}"
            );
        }
        // Read-only facts stay on the cancelled record too.
        assert!(
            html.contains(&fixture.supplier_name) && html.contains("due 06/02/2024"),
            "a cancelled purchase still shows its header facts: {html:.400}"
        );
    }

    /// The record page's header action slot holds the next obvious action,
    /// and for a draft that action is no longer the header's to offer: S5a
    /// removed the add-line drawer, so a draft's line entry lives in the
    /// entry row inside the document and its primary action is Confirm in
    /// the sticky action bar — the header renders no `data-page-action` at
    /// all (an empty slot pair, so the partial drops the anchor; the old
    /// `#add-line` target no longer exists). A confirmed credit purchase's
    /// next obvious action IS the header's: "Record payment" anchored at
    /// `#record-payment`. A confirmed cash purchase was settled at confirm,
    /// so nothing is left to pay and it too renders none. Pinned in both
    /// directions: a dead anchor must not creep back, and the payment
    /// action must not silently drop.
    #[tokio::test]
    async fn web_purchase_record_page_action_draft_offers_none_and_confirmed_credit_offers_record_payment(
    ) {
        let state = test_state().await;
        let credit = seed_record_fixture(&state, PaymentType::Credit).await;
        let cash = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        // -- Draft: no page action; lines go through the entry row ----------
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", credit.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("data-page-action"),
            "a draft's line entry lives in the document's entry row and its primary \
             action is Confirm in the sticky bar, so the header offers no action: {html:.400}"
        );

        // -- Confirmed credit: the next obvious action is paying ------------
        state
            .purchases_service
            .confirm(audit_actor(&state).await, credit.purchase_id, None)
            .await
            .unwrap();
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", credit.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let header_start = html
            .find("data-page-header")
            .expect("the record page renders its page header");
        let header_end = header_start
            + html[header_start..]
                .find("</header>")
                .expect("the page header closes");
        let header = &html[header_start..header_end];
        let action = element_tag_containing(header, "data-page-action");
        assert!(
            action.contains("href=\"#record-payment\""),
            "a confirmed credit purchase's next obvious action opens the payment \
             dialog: {action}"
        );
        assert!(
            header.contains(">Record payment</a>"),
            "the confirmed credit action must be labelled Record payment: {header:.600}"
        );

        // -- Confirmed cash: settled at confirm, nothing left to pay --------
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                cash.purchase_id,
                Some(cash.method_id),
            )
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", cash.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("data-page-action"),
            "a confirmed cash purchase has nothing left to pay, so the header \
             offers no action: {html:.400}"
        );
    }

    /// Receiving-desk T4: a draft's money region carries an effects preview —
    /// projections only (stock/cash/due), never a payment input — built from
    /// the record's own `tracked_units` so "+N units (tracked)" cannot drift
    /// from what confirm will move. Purchase-payment-at-confirm T3: the
    /// preview no longer trusts the stored type — BOTH scenarios render until
    /// confirm. The bar's Confirm primary is disabled at
    /// zero lines and enabled once a line exists.
    #[tokio::test]
    async fn web_purchase_record_effects_preview_projects_confirm_without_payment_inputs() {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, ProductKind};
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        let state = test_state().await;
        let actor = audit_actor(&state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "FX-TRK".into(),
                    name: "FX tracked".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price: Decimal::from(10),
                    track_stock: true,
                    min_stock: Some(Decimal::from(5)),
                    max_stock: Some(Decimal::from(50)),
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
                    name: "FX Preview Sup".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();

        async fn new_draft(
            state: &AppState,
            actor: i64,
            supplier_id: i64,
            payment_type: PaymentType,
            due: bool,
        ) -> crate::models::Purchase {
            use crate::models::NewPurchase;
            state
                .purchases_service
                .create_draft(
                    actor,
                    NewPurchase {
                        supplier_id,
                        payment_type,
                        purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                        due_date: due.then(|| NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        supplier_invoice_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap()
        }

        // -- Cash draft with one tracked line: stock + cash projections ------
        let cash = new_draft(&state, actor, supplier.id, PaymentType::Cash, false).await;
        state
            .purchases_service
            .add_line(
                actor,
                cash.id,
                product.id,
                Decimal::from(3),
                Some(Decimal::from(10)),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let (status, html) = get_html(app.clone(), &format!("/purchases/{}", cash.id)).await;
        assert_eq!(status, StatusCode::OK);
        let preview_start = html
            .find("id=\"effects-preview\"")
            .unwrap_or_else(|| panic!("the draft renders an effects preview: {html:.600}"));
        let preview_end = preview_start
            + html[preview_start..]
                .find("id=\"line-picker\"")
                .expect("the preview sits before the persistent entry row");
        let preview = &html[preview_start..preview_end];
        assert!(
            preview.contains("+3 units (tracked)"),
            "the stock projection counts tracked lines: {preview}"
        );
        let total = state
            .purchases_service
            .get_record(cash.id)
            .await
            .unwrap()
            .total
            .to_string();
        assert!(
            preview.contains(&format!(
                "Cash scenario · −{total} USD at confirmation · no amount due"
            )),
            "the cash projection is the document total: {preview}"
        );
        assert!(
            preview.contains(&format!(
                "Credit scenario · at confirmation · due +{total} USD"
            )),
            "a draft previews BOTH scenarios until confirm: {preview}"
        );
        assert!(
            !preview.contains("<select") && !preview.contains("<input"),
            "the preview is projections only, no payment inputs: {preview}"
        );
        // A draft with a line may confirm.
        let confirm_btn = element_tag_containing(&html, "openRecordDialog('confirm-purchase')");
        assert!(
            !confirm_btn.contains("disabled"),
            "one line is enough to confirm: {confirm_btn}"
        );

        // -- Credit draft: due projection, no cash movement ------------------
        let credit = new_draft(&state, actor, supplier.id, PaymentType::Credit, true).await;
        state
            .purchases_service
            .add_line(
                actor,
                credit.id,
                product.id,
                Decimal::from(2),
                Some(Decimal::from(10)),
            )
            .await
            .unwrap();
        let (status, html) = get_html(app.clone(), &format!("/purchases/{}", credit.id)).await;
        assert_eq!(status, StatusCode::OK);
        let preview_start = html
            .find("id=\"effects-preview\"")
            .expect("the credit draft renders its preview too");
        let preview_end = preview_start
            + html[preview_start..]
                .find("id=\"line-picker\"")
                .expect("the preview sits before the persistent entry row");
        let preview = &html[preview_start..preview_end];
        let credit_total = state
            .purchases_service
            .get_record(credit.id)
            .await
            .unwrap()
            .total
            .to_string();
        assert!(
            preview.contains(&format!(
                "Credit scenario · at confirmation · due +{credit_total} USD"
            )),
            "the due projection is what confirm establishes: {preview}"
        );
        assert!(
            preview.contains(&format!(
                "Cash scenario · −{credit_total} USD at confirmation · no amount due"
            )),
            "the preview does not trust the stored type: both scenarios show: {preview}"
        );
        assert!(
            preview.contains("+2 units (tracked)"),
            "stock projection follows the lines: {preview}"
        );

        // -- Zero lines: Confirm is disabled --------------------------------
        let empty = new_draft(&state, actor, supplier.id, PaymentType::Cash, false).await;
        let (status, html) = get_html(app, &format!("/purchases/{}", empty.id)).await;
        assert_eq!(status, StatusCode::OK);
        let confirm_btn = element_tag_containing(&html, "openRecordDialog('confirm-purchase')");
        assert!(
            confirm_btn.contains("disabled"),
            "confirm must be disabled with zero lines: {confirm_btn}"
        );
        assert!(
            html.contains("Stock · no stock movement"),
            "an empty draft previews no stock movement: {html:.600}"
        );
    }

    /// T3: payment is decided at confirm, so the stored type is invisible
    /// while the purchase is a Draft — the record header shows no payment-type
    /// badge until then. Since S6 the list row never shows one at all: Cash is
    /// the default and Credit rides the meta line, so the row's only chip is
    /// the status chip (the confirmed row proves which row the Paid chip came
    /// from by the exact-once count: every draft this test seeded stays
    /// badgeless too).
    #[tokio::test]
    async fn web_draft_hides_the_payment_type_badge_until_confirm() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        // -- Draft record pages: no type badge -----------------------------
        let cash = seed_record_fixture(&state, PaymentType::Cash).await;
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", cash.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("uppercase\">Cash</span>"),
            "a Cash draft shows no payment-type badge: {html:.600}"
        );
        let credit = seed_record_fixture(&state, PaymentType::Credit).await;
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", credit.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("uppercase\">Credit</span>"),
            "a Credit draft shows no payment-type badge: {html:.600}"
        );

        // -- The list hides it on both draft rows --------------------------
        let (status, list) = get_html(app.clone(), "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !list.contains("uppercase\">Cash</span>")
                && !list.contains("uppercase\">Credit</span>"),
            "draft rows carry no payment-type badge: {list:.600}"
        );

        // -- Confirm: the badge returns on the record page only; the list
        // keeps no type chip at all ---------------------------------------
        let (status, _, resp) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/confirm", cash.purchase_id),
            &format!("payment_type=Cash&due_date=&method_id={}", cash.method_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", cash.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("uppercase\">Cash</span>"),
            "a confirmed purchase keeps its type badge: {html:.600}"
        );
        let (status, list) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            list.matches("uppercase\">Cash</span>").count(),
            0,
            "a confirmed row still shows no payment-type chip: {list:.600}"
        );
        assert!(
            !list.contains("uppercase\">Credit</span>"),
            "no row ever shows a payment-type chip: {list:.600}"
        );
        // The confirmed Cash purchase settled at confirm, so its one chip is
        // the Paid state, never the type.
        assert_eq!(
            list.matches(">Paid</span>").count(),
            1,
            "the confirmed Cash row carries the Paid chip exactly once: {list:.600}"
        );
    }

    /// The payment-method control appears ONLY inside the confirm dialog, and
    /// is ACTIVE only for Cash: a Cash draft must pick a method before it can
    /// submit (the account is derived from it), while a Credit draft renders
    /// the method hidden AND disabled so the client cannot send the method
    /// the server rejects for Credit — the due-date input takes its place,
    /// required and prefilled from the stored value. (purchase-payment-at-confirm
    /// T2: both sides of the choice now live in the dialog, and the inactive
    /// side is inert rather than absent.)
    #[tokio::test]
    async fn web_purchase_record_confirm_dialog_activates_method_only_for_cash() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        // -- Cash draft: the method select lives in the confirm dialog -----
        let cash = seed_record_fixture(&state, PaymentType::Cash).await;
        let (_, cash_html) =
            get_html(app.clone(), &format!("/purchases/{}", cash.purchase_id)).await;
        let cash_dialog = slice_dialog(&cash_html, "confirm-purchase");
        let method_tag = element_tag_containing(&cash_dialog, "id=\"confirm-method\"");
        assert!(
            method_tag.contains("required") && !method_tag.contains("disabled"),
            "a Cash draft requires an active method in the confirm dialog: {method_tag:.500}"
        );
        assert!(
            !cash_dialog.contains("none (Credit)"),
            "Cash offers no empty method: {cash_dialog:.500}"
        );
        assert_eq!(
            cash_html.matches("name=\"method_id\"").count(),
            1,
            "the draft's only method control is the confirm dialog: {cash_html:.500}"
        );

        // -- Credit draft: the method control is present but inert ---------
        let credit = seed_record_fixture(&state, PaymentType::Credit).await;
        let (_, credit_html) =
            get_html(app.clone(), &format!("/purchases/{}", credit.purchase_id)).await;
        let credit_dialog = slice_dialog(&credit_html, "confirm-purchase");
        let method_tag = element_tag_containing(&credit_dialog, "id=\"confirm-method\"");
        assert!(
            method_tag.contains("disabled"),
            "the Credit confirm dialog must disable the method control: {method_tag:.500}"
        );
        assert!(
            credit_dialog.contains("value=\"2024-06-02\""),
            "the Credit confirm dialog shows the due date: {credit_dialog:.500}"
        );
        assert_eq!(
            credit_html.matches("name=\"method_id\"").count(),
            1,
            "the method control exists once, inside the confirm dialog: {credit_html:.500}"
        );
    }

    /// The entry row replaces the add-line drawer: it renders persistent
    /// inside the money region — product, qty, cost and the Add action,
    /// visible without any click — and the drawer's "Keep open after adding"
    /// preference is gone with the drawer: no checkbox, no localStorage key,
    /// and no page-shell code that opened, closed or focused a drawer.
    #[tokio::test]
    async fn web_purchase_entry_row_replaces_the_drawer_and_drops_the_keep_open_preference() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);

        // The drawer and its keep-open preference are deleted outright.
        assert!(
            !html.contains("id=\"line-drawer\"") && !html.contains("closeLineDrawer"),
            "the add-line drawer is deleted: {html:.600}"
        );
        assert!(
            !html.contains("keep-open-lines") && !html.contains("keepOpen"),
            "the keep-open preference is gone with the drawer: {html:.600}"
        );
        assert!(
            !html.contains("purchases.addLine.keepOpen"),
            "the localStorage preference is gone: {html:.600}"
        );
        assert!(
            !html.contains("openLineDrawer"),
            "no code opens a drawer that no longer exists: {html:.600}"
        );

        // The entry row sits inside the money region every add response
        // swaps, above the lines: results appearing below cannot move the
        // Add button, and the region swap re-renders it empty and focused.
        let money = html
            .find("id=\"purchase-record-money\"")
            .expect("the money region renders");
        let row = html
            .find("id=\"line-picker\"")
            .expect("the entry row renders");
        let results = html
            .find("id=\"product-search-results\"")
            .expect("the entry row renders the results container");
        let lines = html.find(">Lines (").expect("the lines heading renders");
        assert!(
            money < row && row < results && results < lines,
            "the entry row and its results sit inside the money region, above the lines: money={money} row={row} results={results} lines={lines}"
        );
        let form = enclosing_form(&html, "id=\"product-picker\"");
        assert!(
            form.contains("data-action=\"Add line\"") && form.contains("type=\"submit\""),
            "the entry row carries the Add action in its flex row: {form:.600}"
        );
    }

    /// Draft lines are edited in place: qty/unit_cost render as inputs that
    /// PUT to the existing update-line route (registered for PUT, not just
    /// POST) with `change delay:400ms`, and the page shell reverts the input
    /// to its last server-rendered value when the server refuses (4xx), while
    /// the base notice announces the refusal.
    #[tokio::test]
    async fn web_purchase_line_edit_is_inline_via_put() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let edit_url = format!(
            "/web/purchases/{}/lines/{}",
            fixture.purchase_id, fixture.line_id
        );

        // -- The route accepts PUT ------------------------------------------
        let (status, _, body) =
            put_form_response(app.clone(), &edit_url, "qty=3&unit_cost=6").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "PUT must reach the update-line handler: {status} {body:.300}"
        );

        // -- The server still refuses invalid values (authority unchanged) --
        let (status, _, _) = put_form_response(app.clone(), &edit_url, "qty=0&unit_cost=6").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "qty must stay > 0 server-side"
        );

        // -- The draft page renders the inputs ------------------------------
        let (_, html) = get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert!(
            html.contains(&format!("hx-put=\"{edit_url}\"")),
            "the qty/cost cells PUT inline: {html:.900}"
        );
        assert!(
            html.contains("hx-trigger=\"change delay:400ms\""),
            "the inline edit fires on change delay:400ms: {html:.900}"
        );
        assert!(
            html.contains(&format!("id=\"line-qty-{}\"", fixture.line_id)),
            "the qty cell is an addressable input: {html:.900}"
        );

        // -- Revert wiring lives in the page shell --------------------------
        assert!(
            html.contains("input[hx-put]") && html.contains("defaultValue"),
            "a refused edit reverts the input to its rendered value: {html:.900}"
        );
    }

    /// T4: the record page's header becomes an ALWAYS-VISIBLE EDITABLE FORM for
    /// drafts — supplier picker, purchase date, supplier invoice no and notes —
    /// and stays read-only text for a confirmed or cancelled purchase (the
    /// service is the authority; the gate here is presentation). The due date
    /// is decided at confirm and the header keeps not touching it, so a header
    /// post that omits it leaves the stored due date untouched — clearing a
    /// Credit draft's due is the confirm dialog's Cash path alone
    /// (`due_date: Some(None)`), never a header edit.
    #[tokio::test]
    async fn web_edit_header_form_drops_due_date_and_keeps_the_stored_due() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);

        // -- The inline header region: no due input, the four editable
        // fields (the supplier field lives in the picker's own form; the
        // other three in the Save form, which `hx-include`s the supplier) --
        let region_start = html
            .find("id=\"purchase-header\"")
            .expect("the draft renders the inline header region");
        let form_start = html
            .find("data-action=\"Save header\"")
            .expect("the draft renders the inline header form");
        let form_end = form_start
            + html[form_start..]
                .find("</form>")
                .expect("the header form closes");
        let form = &html[region_start..form_end];
        assert!(
            !form.contains("name=\"due_date\""),
            "the inline header form drops the due date: {form:.600}"
        );
        for field in [
            "name=\"supplier_name\"",
            "name=\"purchase_date\"",
            "name=\"supplier_invoice_no\"",
            "name=\"notes\"",
        ] {
            assert!(
                form.contains(field),
                "the header form keeps {field}: {form:.600}"
            );
        }
        // The supplier field arrives pre-filled with the stored supplier's
        // name, and the other fields with the stored values.
        assert!(
            form.contains(&format!("value=\"{}\"", fixture.supplier_name)),
            "the supplier field is pre-filled with the document's supplier: {form:.600}"
        );

        // -- The route: omitting the fields must not wipe the stored values.
        // The form maps `supplier_invoice_no: Some(clean_opt(..))` and
        // `notes: Some(..)` unconditionally, so an ABSENT field arrives as an
        // empty string and clears it — the picker's own Enter form must always
        // carry the three sibling fields pinned by `include`.
        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            &format!(
                "supplier_id={}&purchase_date=2024-05-03&supplier_invoice_no=A-9&notes=edited",
                fixture.supplier_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-03");
        assert_eq!(detail.purchase.supplier_invoice_no.as_deref(), Some("A-9"));
        assert_eq!(
            detail.purchase.notes, "edited",
            "the header post carries the notes field: {:?}",
            detail.purchase.notes
        );
        assert_eq!(
            detail.purchase.due_date.map(|d| d.to_string()),
            Some("2024-06-02".to_string()),
            "a header edit leaves the stored due date untouched: {:?}",
            detail.purchase.due_date
        );
    }

    /// T4: the inline header form carries a Save button posting the existing
    /// header route, and the picker's `include` names the three sibling field
    /// ids so every path (Save, Enter in the supplier field, a clicked
    /// result) posts the same field set.
    #[tokio::test]
    async fn inline_header_form_includes_the_sibling_fields_and_save_posts_the_header_route() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);

        // The Save button posts the existing header route.
        let save = element_tag_containing(&html, "data-action=\"Save header\"");
        assert!(
            save.contains(&format!(
                "hx-post=\"/web/purchases/{}/header\"",
                fixture.purchase_id
            )),
            "the Save button posts the header route: {save}"
        );

        // The picker's own form (the Enter path) carries the sibling fields:
        // its `include` names the date, invoice and notes ids and NEVER a
        // supplier_id (htmx accumulates colliding values — T3's defect).
        let form_pos = html
            .find("data-action=\"Save\"")
            .expect("the record hosts the picker's own form");
        let picker_form = enclosing_form(&html[..], &format!("data-action=\"Save\""));
        let _ = form_pos;
        let include = picker_form
            .split("hx-include=\"")
            .nth(1)
            .map(|s| s.split('"').next().unwrap().to_string());
        let include = include.expect("the picker's form names an include");
        for id in ["record-purchase-date", "record-invoice-no", "record-notes"] {
            assert!(
                include.contains(id),
                "the include must carry {id} so the Enter path posts it: {include}"
            );
        }
        assert!(
            !include.contains("supplier_id"),
            "the include must never carry a supplier_id: {include}"
        );

        // The date, invoice and notes inputs carry those exact ids.
        for id in ["record-purchase-date", "record-invoice-no", "record-notes"] {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "the header renders #{id}: {html:.400}"
            );
        }
    }

    /// The TRAP (T4): the picker's own form — the Enter path inside the
    /// supplier field — posts the header route with ONLY the supplier field
    /// unless the picker's `include` carries the siblings. The route maps
    /// invoice and notes unconditionally (`Some(clean_opt(..))` / `Some(..)`),
    /// so an absent field would arrive as empty and WIPE the stored values.
    /// The request body is built from the inputs the rendered form actually
    /// carries, the way the T3 Enter-path test does — so a missing `include`
    /// (or a field rendered outside it) fails the test, not just the intent.
    #[tokio::test]
    async fn web_header_enter_path_from_the_picker_preserves_invoice_and_notes() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        // The supplier the operator will type into the field on the record
        // page (its exact name must resolve).
        let typed = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Enter Header Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        // Seed values the trap would silently wipe.
        state
            .purchases_service
            .update_draft(
                audit_actor(&state).await,
                fixture.purchase_id,
                crate::models::UpdatePurchaseDraft {
                    supplier_invoice_no: Some(Some("INV-TRAP".to_string())),
                    notes: Some("trap notes".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, html) =
            get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);

        // The picker's OWN form is what Enter submits. Collect its inputs —
        // exactly what the browser posts — from the form tag plus everything
        // `hx-include` names (the sibling inputs live outside the form tag).
        let form = enclosing_form(&html, "data-action=\"Save\"");
        let include = form
            .split("hx-include=\"")
            .nth(1)
            .map(|s| s.split('"').next().unwrap().to_string())
            .expect("the picker's form names an include");
        let mut scope = form.to_string();
        for id in include.split(", ") {
            let needle = format!("id=\"{}\"", id.trim_start_matches('#'));
            let pos = html.find(&needle).unwrap_or_else(|| {
                panic!("the include names {needle} but the page does not render it")
            });
            let start = html[..pos].rfind('<').expect("the id sits inside a tag");
            let end = start + html[start..].find('>').expect("unterminated tag");
            scope.push_str(&html[start..=end]);
        }

        // Collect (name, value) from every input in scope, the body Enter sends.
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut rest = scope.as_str();
        while let Some(i) = rest.find("<input") {
            rest = &rest[i..];
            let tag_end = rest.find('>').expect("unterminated input tag");
            let tag = &rest[..=tag_end];
            if let Some(name) = tag
                .split("name=\"")
                .nth(1)
                .map(|s| s.split('"').next().unwrap())
            {
                let value = tag
                    .split("value=\"")
                    .nth(1)
                    .map(|s| s.split('"').next().unwrap())
                    .unwrap_or_default();
                fields.push((name.to_string(), value.to_string()));
            }
            rest = &rest[tag_end..];
        }
        assert!(
            fields.iter().any(|(n, _)| n == "supplier_name"),
            "the picker's field must travel: {fields:?}"
        );
        for name in ["purchase_date", "supplier_invoice_no", "notes"] {
            assert!(
                fields.iter().any(|(n, _)| n == name),
                "the Enter path must post {name} or the route wipes it: {fields:?}"
            );
        }

        // The operator edits only the supplier's name and presses Enter.
        let body = fields
            .iter()
            .map(|(name, value)| {
                let value = if name == "supplier_name" {
                    "Enter Header Supplier"
                } else {
                    value
                };
                format!("{name}={value}")
            })
            .collect::<Vec<_>>()
            .join("&");
        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");

        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.supplier_id, typed.id,
            "the typed supplier resolves to the one the operator typed, never the stored one"
        );
        assert_eq!(
            detail.purchase.supplier_invoice_no.as_deref(),
            Some("INV-TRAP"),
            "the invoice must SURVIVE the Enter-path header edit: {:?}",
            detail.purchase.supplier_invoice_no
        );
        assert_eq!(
            detail.purchase.notes, "trap notes",
            "the notes must SURVIVE the Enter-path header edit: {:?}",
            detail.purchase.notes
        );
    }

    /// Saving a CHANGED supplier through the inline header: the typed name
    /// resolves server-side and the document's supplier moves — asserted
    /// through the service, not the markup.
    #[tokio::test]
    async fn web_header_save_updates_the_supplier_date_invoice_and_notes() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let other = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Header Change Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let body = "supplier_name=Header+Change+Supplier&purchase_date=2024-05-03&supplier_invoice_no=INV-77&notes=changed+header";
        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");

        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.supplier_id, other.id);
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-03");
        assert_eq!(
            detail.purchase.supplier_invoice_no.as_deref(),
            Some("INV-77")
        );
        assert_eq!(
            detail.purchase.notes, "changed header",
            "the header post carries the notes: {:?}",
            detail.purchase.notes
        );
    }

    /// An unknown typed supplier refuses with a 400 naming the value and
    /// changes nothing — the name is never silently guessed (the same hazard
    /// rule the creation flow obeys).
    #[tokio::test]
    async fn web_header_unknown_typed_supplier_refuses_naming_the_value_and_changes_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            "supplier_name=No+Such+Supplier&purchase_date=2024-05-03&supplier_invoice_no=X&notes=y",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{resp:.400}");
        assert!(
            resp.contains("No Such Supplier"),
            "the refusal names the typed value: {resp:.400}"
        );
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.purchase.supplier_id, before.purchase.supplier_id,
            "a refused supplier change must not move the supplier"
        );
        assert_eq!(
            after.purchase.purchase_date, before.purchase.purchase_date,
            "a refused supplier change must not touch the date"
        );
        assert_eq!(
            after.purchase.notes, before.purchase.notes,
            "a refused supplier change must not touch the notes"
        );
    }

    /// An explicit supplier_id from a clicked picker result wins over the
    /// typed name, the same precedence the creation route applies.
    #[tokio::test]
    async fn web_header_an_explicit_supplier_id_wins_over_the_typed_name() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let typed = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Header Typed Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let clicked = state
            .supplier_service
            .create_supplier(
                audit_actor(&state).await,
                crate::models::NewSupplier {
                    name: "Header Clicked Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let body = format!(
            "supplier_id={}&supplier_name=Header+Typed+Supplier&purchase_date=2024-05-03",
            clicked.id
        );
        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resp:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            detail.purchase.supplier_id, clicked.id,
            "the explicit id must win over the typed name"
        );
        assert_ne!(detail.purchase.supplier_id, typed.id);
    }

    /// A header post against a CONFIRMED purchase still refuses: the service
    /// is the authority; the template gate is presentation only. The body
    /// carries a VALID supplier id so the route resolves it and reaches
    /// `PurchasesService::update_draft` — only the service's draft check can
    /// then refuse. Route and service refusals are not distinguishable in the
    /// response (both surface as a 400 with a plain message body), so the
    /// exact refusing layer is asserted indirectly: a valid supplier means
    /// the route-side resolution cannot be the refusal, and nothing changed
    /// rules out any write having happened.
    #[tokio::test]
    async fn web_header_service_still_refuses_a_confirmed_purchase() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // The supplier id is required: without it the route's own
        // `resolve_supplier_name` refusal would 400 before the service ever
        // runs, and the test would pass with the service gate removed.
        let body = format!(
            "supplier_id={}&purchase_date=2024-05-03",
            fixture.supplier_id
        );
        let (status, _, resp) = post_form_response(
            app,
            &format!("/web/purchases/{}/header", fixture.purchase_id),
            &body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the service must refuse a confirmed header edit: {resp:.400}"
        );
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            before.purchase.purchase_date, after.purchase.purchase_date,
            "the route's supplier resolution cannot refuse a valid id, so the 400 above can only be the SERVICE's draft check; nothing must have changed"
        );
        assert_eq!(
            before.purchase.supplier_id, after.purchase.supplier_id,
            "the service refused before any write landed: {resp:.400}"
        );
    }

    /// The record-page actions swap the record body and keep the
    /// `purchase-changed` refresh event, so the URL stays stable and subscribed
    /// regions update.
    #[tokio::test]
    async fn web_purchase_record_header_edit_swaps_the_body_and_triggers_refresh() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let body = format!(
            "supplier_id={}&purchase_date=2024-05-03&due_date=&supplier_invoice_no=A-9&notes=edited+note",
            fixture.supplier_id
        );
        let req = Request::builder()
            .method("POST")
            .uri(format!("/web/purchases/{}/header", fixture.purchase_id))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("HX-Trigger")
                .map(|v| v.to_str().unwrap()),
            Some("purchase-changed")
        );
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes).to_string();
        assert!(html.contains("purchase-record-inner"), "{html:.400}");
        assert!(html.contains("edited note"), "{html:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.notes, "edited note");
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-03");
        assert_eq!(detail.purchase.supplier_invoice_no.as_deref(), Some("A-9"));
    }

    // -- N4: the product picker on the purchase record page --------------------

    /// The catalogue `<select>` is replaced by the PICKER ISLAND's entry row:
    /// one field that searches with the island's debounced JSON read, submits on
    /// Enter and clears on Escape (base.html's keydown handler), and the
    /// results container is a sibling of the form. The entry row is persistent
    /// inside the money region, above the lines.
    #[tokio::test]
    async fn n4_purchase_record_offers_the_picker_instead_of_the_catalogue_select() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("<select name=\"product_id\""),
            "the whole-catalogue select must be gone: {html:.800}"
        );

        let picker = enclosing_form(&html, "id=\"product-picker\"");
        assert!(
            picker.contains(&format!(
                "hx-post=\"/web/purchases/{}/lines\"",
                fixture.purchase_id
            )),
            "{picker}"
        );
        assert!(
            picker.contains("data-action=\"Add line\""),
            "the notice must name the failed line action: {picker}"
        );
        assert!(
            picker.contains("name=\"product_id\""),
            "the form carries the island-owned hidden product id: {picker}"
        );
        assert!(
            picker.contains("name=\"qty\"") && picker.contains("value=\"1\""),
            "a scan and a click must both carry the default quantity: {picker}"
        );

        // The field carries no declarative search transport: the island owns
        // the read, the debounce and the Escape semantics.
        let input_pos = html
            .find("id=\"product-picker\"")
            .expect("the record page renders the picker field");
        let input_start = html[..input_pos]
            .rfind('<')
            .expect("the id must sit inside a tag");
        let input_end = input_pos + html[input_pos..].find('>').expect("unterminated tag");
        let input_tag = &html[input_start..=input_end];
        for transport in [
            "hx-get",
            "hx-trigger",
            "hx-target",
            "hx-vals",
            "hx-on:keyup",
        ] {
            assert!(
                !input_tag.contains(transport),
                "the field must not carry {transport}: {input_tag}"
            );
        }

        // The island's mount point carries its calling context: one picker,
        // priced for a purchase — the island renders cost where the old
        // `show_cost` fragment rendered it.
        let container_pos = html
            .find("data-picker")
            .expect("the record page renders the island mount point");
        let container_start = html[..container_pos]
            .rfind('<')
            .expect("the attribute must sit inside a tag");
        let container_end =
            container_start + html[container_start..].find('>').expect("unterminated tag");
        let container_tag = &html[container_start..=container_end];
        assert!(
            container_tag.contains("id=\"line-picker\"")
                && container_tag.contains("data-price-kind=\"cost\""),
            "{container_tag}"
        );

        // The debounce moved with the island: the island file declares it, so
        // the search cannot lose its debounce by a markup edit alone.
        assert!(
            include_str!("../../static/picker.js").contains("DEBOUNCE_MS = 250"),
            "the search must be debounced by static/picker.js"
        );
        assert!(
            !picker.contains("id=\"product-search-results\""),
            "the results container must be a sibling of the picker form, never inside it: {picker}"
        );
        assert!(
            html.contains("id=\"product-search-results\""),
            "the page must render the sibling results container: {html:.600}"
        );
        assert!(
            html.contains("id=\"purchase-record-money\""),
            "adding a line swaps the money region, which carries the total and the lines"
        );
        // Placement: the entry row and its results live inside the money
        // region, above the lines heading — one flex row, then the results.
        let money = html.find("id=\"purchase-record-money\"").unwrap();
        let row = html.find("id=\"line-picker\"").unwrap();
        let results = html.find("id=\"product-search-results\"").unwrap();
        let lines = html.find(">Lines (").unwrap();
        assert!(
            money < row && row < results && results < lines,
            "the entry row and its results sit inside the money region, above the lines: money={money} row={row} results={results} lines={lines}"
        );
    }

    /// AC9 + AC10: an exact barcode submits the line in one step, the same
    /// response carries the updated lines, the running total and the entry
    /// row — inside the swapped money region, empty and focused — and an
    /// empty cost falls back to the product's cost price.
    #[tokio::test]
    async fn n4_purchase_line_scan_adds_in_one_step_and_resets_the_picker() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let scanned = seed_extra_product(&state, "SCAN-PUR", Some("7791234567891")).await;
        let app = crate::routes::router(state.clone());

        // Exactly what the picker form posts on Enter: the typed value and the
        // quantity, no product id and no click.
        let (status, _, added) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=7791234567891&qty=2&unit_cost=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{added}");

        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 2, "the scan adds its own line");
        let line = detail
            .lines
            .iter()
            .find(|line| line.product_id == scanned.id)
            .expect("the scanned line");
        assert_eq!(line.qty, Decimal::from(2));
        assert_eq!(
            line.unit_cost,
            Decimal::from(10),
            "an empty cost uses the product cost price only when the supplier has no satellite row"
        );

        // One response carries the lines, the running total and the entry row
        // (inside the money region), so lines and total can never drift.
        assert!(added.contains(&scanned.name), "{added:.600}");
        assert!(
            added.contains("40 USD"),
            "the running total travels with the lines: {added:.800}"
        );
        assert_entry_row_is_empty_and_focused(&added);
    }

    /// AC10 (clicked result): a result is its own add action; the request includes
    /// the picker form, so the quantity travels, and supplies the product id
    /// itself. The typed text is not an exact match on purpose.
    #[tokio::test]
    async fn n4_purchase_line_clicked_result_uses_the_picker_quantity() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let clicked_product = seed_extra_product(&state, "CLICK-PUR", None).await;
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!(
                "product=record&qty=3&unit_cost=&product_id={}",
                clicked_product.id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let clicked = detail
            .lines
            .iter()
            .find(|line| line.product_id == clicked_product.id)
            .expect("the clicked line");
        assert_eq!(clicked.qty, Decimal::from(3));
        assert_eq!(clicked.unit_cost, Decimal::from(10));
        assert_eq!(detail.total, Decimal::from(50));
        assert!(body.contains("50 USD"), "{body:.800}");
    }

    /// AC12: an unresolvable value is a clear 400 that names the number of partial
    /// matches the search found, and it adds nothing.
    #[tokio::test]
    async fn n4_purchase_line_unknown_value_is_400_and_adds_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();

        let (status, _, body) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=record&qty=1&unit_cost=",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no exact match"), "{body}");
        assert!(
            body.contains("1 match"),
            "the message must name the search count: {body}"
        );

        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "a failed resolution adds no line"
        );
        assert_eq!(after.total, before.total);
    }

    /// S5b rewrote the old `web_purchase_line_repeated_product_is_a_clear_400`
    /// pin: a same-cost repeat through the web route now merges, so this test
    /// pins the split — the 400 (same message, same status) survives only for a
    /// different explicit cost, and the merge is announced with a visible
    /// notice. The strict rule-for-machines is pinned on the API twin below.
    #[tokio::test]
    async fn web_purchase_line_same_cost_repeat_merges_and_different_cost_stays_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();

        // The fixture line was priced from the product column (10) with an empty
        // cost, so a repeat scan with an empty cost resolves to the same cost:
        // merging loses nothing.
        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!("product={}&qty=3&unit_cost=", fixture.product_sku),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The merge is a visible success notice, not a silent quantity change.
        assert!(body.contains("data-notice-server"), "{body}");
        assert!(body.contains("merged"), "{body}");
        assert!(body.contains(&fixture.product_name), "{body}");
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "the repeat adds no second line"
        );
        let line = after
            .lines
            .iter()
            .find(|l| l.product_id == fixture.product_id)
            .unwrap();
        assert_eq!(line.qty, dec_web("5"), "the merged quantity is the sum");
        assert_eq!(
            line.unit_cost,
            dec_web("10"),
            "the stored cost does not move"
        );

        // The 400 survives only for a different explicit cost, with the exact
        // message the strict rule always produced.
        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!("product={}&qty=1&unit_cost=99", fixture.product_sku),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("already has a line"), "{body}");
        assert!(
            body.contains("separate purchase"),
            "the message must point at the supported path: {body}"
        );
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "the refused repeat adds no line"
        );
        let line = after
            .lines
            .iter()
            .find(|l| l.product_id == fixture.product_id)
            .unwrap();
        assert_eq!(
            line.qty,
            dec_web("5"),
            "the refusal leaves the quantity untouched"
        );
    }

    // The asymmetry is deliberate (S5b): the web route merges a same-cost
    // repeat because the operator is a scanner, but the JSON API keeps the
    // strict rule — a machine client is told to use the line-update endpoint
    // instead of having its request silently reinterpreted.
    #[tokio::test]
    async fn api_purchase_line_repeated_product_is_still_a_clear_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        let (status, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{}/lines", fixture.purchase_id),
            serde_json::json!({ "product_id": fixture.product_id, "qty": "1" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        let msg = v.to_string();
        assert!(msg.contains("already has a line"), "{msg}");
        assert!(msg.contains("separate purchase"), "{msg}");

        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(after.lines.len(), 1, "the refused repeat adds no line");
        assert_eq!(after.lines[0].qty, dec_web("2"), "quantity unchanged");
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
            html.contains("Draft #"),
            "draft without number should show as Draft #id: {html:.400}"
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
            html.contains("Seed Sup")
                && html.contains("48")
                && html.contains("Without supplier cost"),
            "suggestion fragment should render costed and unsourced rows: {html:.400}"
        );

        let body = format!("product_id={pid}&payment_type=Cash&purchase_date=2024-05-02");
        let (status, redirect, resp) =
            post_form_response(app.clone(), "/web/purchases/from-suggestion", &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("seeding a draft must land on its record");
        assert!(redirect.starts_with("/purchases/"), "{redirect}");
        let redirected_id: i64 = redirect["/purchases/".len()..]
            .parse()
            .unwrap_or_else(|_| panic!("HX-Redirect must end in the purchase id: {redirect}"));

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
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let purchase_id = v["purchases"][0]["purchase"]["id"].as_i64().unwrap();
        assert_eq!(
            redirected_id, purchase_id,
            "the seed must navigate to the draft it just created"
        );
        let (st, detail) = get_html(app.clone(), &format!("/web/purchases/{purchase_id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            detail.contains("prod WEB-SUG") && detail.contains("48"),
            "seeded line should show the product name and suggested qty: {detail:.400}"
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

    /// Like [`post_form`], but with an explicit cookie and optional headers:
    /// an empty `HX-Request` set means the plain browser post the full-page
    /// refusal shape needs. Returns the full body so refusals can be asserted.
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

    /// The read gates are real too: a principal WITHOUT `purchases.read` (it
    /// holds an unrelated permission, so this is not a broken fixture) is
    /// refused every purchases page and fragment with the full-page refusal
    /// card. The suggestions fragment names the stock-derived gate instead.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        for (uri, code) in [
            ("/purchases".to_string(), "purchases.read"),
            (
                format!("/purchases/{}", fixture.purchase_id),
                "purchases.read",
            ),
            ("/web/purchases".to_string(), "purchases.read"),
            (
                format!("/web/purchases/{}", fixture.purchase_id),
                "purchases.read",
            ),
            ("/web/purchases/suggestions".to_string(), "inventory.read"),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.200}");
            assert!(
                html.contains("Action not permitted") && html.contains(code),
                "{uri} must refuse naming {code}: {html:.300}"
            );
        }
    }

    // -- S7 part 2: the page and its fragment can no longer disagree ----------

    /// The old consequence (the S7 part 1 review's UX item): a
    /// `purchases.read`-only principal saw the reorder suggestions
    /// server-rendered into `/purchases` and was refused them on refresh,
    /// because the fragment and the API are gated `inventory.read`. Now the
    /// page renders the suggestions block only when the principal holds that
    /// same gate: no block that answers 403 on its own refresh button.
    #[tokio::test]
    async fn ac21_the_suggestions_block_hides_from_a_principal_that_cannot_refresh_it() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let (status, html) = get_html_as(app.clone(), "/purchases", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("id=\"suggestion-section\""),
            "the suggestions block must not render for a principal the suggestions \
             fragment would refuse: {html:.600}"
        );
        assert!(!html.contains("Suggestions"), "{html:.600}");

        // The same page for a principal holding BOTH codes: the block is back.
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "inventory.read"],
        )
        .await
        .unwrap();
        let (status, html) =
            get_html_as(app, "/purchases", Some(&test_support::cookie_for(&holder))).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("id=\"suggestion-section\"") && html.contains("Suggestions"),
            "a principal that may refresh the suggestions must see the block: {html:.600}"
        );
    }

    // -- S4: creation is a full page, so the list's primary action is a gate --

    /// The list page is gated `purchases.read`, but its header action
    /// opens the creation dialog (T3). The repo's rule (AC21, the same one
    /// the sidebar's `nav.visible(key)` applies) extends to the primary
    /// action: a principal without `purchases.create` renders no page action
    /// and no creation dialog at all — the choosing is what creates (AC7),
    /// so a principal that cannot create must not be offered even the
    /// dialog; a principal holding it sees the "New purchase" button whose
    /// onclick opens `#new-purchase-dialog`.
    #[tokio::test]
    async fn s4_purchases_page_offers_the_new_purchase_action_only_when_the_principal_can_create() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let (status, html) = get_html_as(app.clone(), "/purchases", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("data-page-action") && !html.contains("id=\"new-purchase-dialog\""),
            "a principal without purchases.create must not be offered the primary \
             action or the creation dialog it could not submit: {html:.600}"
        );

        // The same page for a principal holding BOTH codes: the action is a
        // dialog-opening button and the dialog is back.
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "purchases.create"],
        )
        .await
        .unwrap();
        let (status, html) =
            get_html_as(app, "/purchases", Some(&test_support::cookie_for(&holder))).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("data-page-action") && html.contains("id=\"new-purchase-dialog\""),
            "a principal that may create must see the primary action and the dialog: {html:.600}"
        );
        assert!(
            html.contains("onclick=\"document.getElementById('new-purchase-dialog').showModal()\"")
                && html.contains("New purchase"),
            "the action must open the creation dialog: {html:.600}"
        );
    }

    /// AC1: the shared page action carries the `.btn-primary` component — the
    /// rest of the system is mint (`roya ◆`, the active nav entry, the success
    /// notice), and neither green survives white text (mint on white is about
    /// 1.4:1), so the label rides the background token. The colour itself is
    /// asserted by the visual net (`e2e/tests/test_visual_baseline.py`), which
    /// records the computed `background-color` of this very element on every
    /// run; this guard pins the COMPONENT, so a future edit cannot quietly
    /// swap the action to a different variant. Asserted on the action's own
    /// opening tag: the component is shared, so a wrongly-variant action on
    /// ANY page is a defect this catches at the source.
    #[tokio::test]
    async fn purchases_page_action_is_mint_with_a_dark_label() {
        let state = test_state().await;
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "purchases.create"],
        )
        .await
        .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, html) =
            get_html_as(app, "/purchases", Some(&test_support::cookie_for(&holder))).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let action = element_tag_containing(&html, "data-page-action");
        assert!(
            action.contains("btn-primary"),
            "the page action must carry the primary button component (the colour itself is asserted by the visual net): {action}"
        );
        assert!(
            action.contains("text-bg"),
            "the label must read the background token — no green survives white text: {action}"
        );
        assert!(
            !action.contains("bg-accent2"),
            "the primary action must not be blue any more: {action}"
        );
        assert!(
            !action.contains("text-white"),
            "the label must not stay white on mint: {action}"
        );
    }

    // -- S3: the purchases list opens the read-only document peek -------------

    /// The peek shell lives on `/purchases` and mirrors the documents drawer:
    /// a fixed right panel whose body the row swaps the detail fragment into.
    #[tokio::test]
    async fn s3_purchase_list_shell_the_purchases_page_renders_the_read_only_peek() {
        let state = test_state().await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("id=\"purchase-drawer\""),
            "the page must render the peek shell: {html:.600}"
        );
        assert!(
            html.contains("id=\"purchase-drawer-body\""),
            "the shell must render the swap target: {html:.600}"
        );
        assert!(
            html.contains("closePurchaseDrawer()"),
            "the shell's close button must be wired: {html:.600}"
        );
    }

    /// The whole list row is the trigger: it carries the peek's `hx-get` over
    /// its existing `href` (the no-JavaScript fallback), and the old `Open`
    /// anchor is gone — an `<a>` inside an `<a>` is invalid HTML.
    #[tokio::test]
    async fn s3_the_purchase_list_row_opens_the_peek_instead_of_navigating() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let id = fixture.purchase_id;
        assert!(
            html.contains(&format!("href=\"/purchases/{id}\"")),
            "the row keeps its no-JavaScript fallback: {html:.800}"
        );
        assert!(
            html.contains(&format!("hx-get=\"/web/documents/detail/purchase/{id}\"")),
            "the row opens the peek over HTMX: {html:.800}"
        );
        assert!(
            html.contains("hx-target=\"#purchase-drawer-body\""),
            "the peek fragment lands in the drawer body: {html:.800}"
        );
        assert!(
            !html.contains(">Open</a>"),
            "the row is the trigger; the Open anchor must not render: {html:.800}"
        );
    }

    /// A principal holding ONLY `purchases.read` opens the reads and is
    /// refused every web mutation, each in the shape its caller reads and
    /// naming its own code: the draft lifecycle `purchases.create`, paying
    /// `purchases.create` (the payment is a purchase-side movement), and
    /// cancelling `purchases.cancel`.
    #[tokio::test]
    async fn ac10_a_purchases_read_only_principal_is_refused_the_web_mutations() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The reads the probe is allowed: the page, the record and the fragments.
        for uri in [
            "/purchases".to_string(),
            format!("/purchases/{}", fixture.purchase_id),
            "/web/purchases".to_string(),
            format!("/web/purchases/{}", fixture.purchase_id),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {html:.200}");
        }

        // The creation page is deleted (T3, AC2): without `purchases.create`
        // the list renders no action and no dialog, and the deleted route
        // would answer 404 even with the right gate.
        let (status, body) = get_html_as(app.clone(), "/purchases", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{body:.200}");
        assert!(
            !body.contains("data-page-action") && !body.contains("id=\"new-purchase-dialog\""),
            "a read-only principal must not see the creation action or the dialog: {body:.400}"
        );

        // Creating a purchase over HTMX: JSON naming the recording gate.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("purchases.create"),
            "the HTMX refusal must name purchases.create: {body}"
        );

        // The same create as a plain browser post: the HTML refusal card.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Action not permitted") && body.contains("purchases.create"),
            "the refusal must use the English fallback and name the gate: {body:.400}"
        );

        // Seeding from a suggestion creates a purchase: purchases.create.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases/from-suggestion",
            "product_id=1&payment_type=Cash",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("purchases.create"), "{body}");

        // The record actions: lines and header are the recording gate.
        for (uri, body) in [
            (
                format!("/web/purchases/{}/lines", fixture.purchase_id),
                format!("product_id={}&qty=1", fixture.product_id),
            ),
            (
                format!("/web/purchases/{}/header", fixture.purchase_id),
                "notes=hacked".to_string(),
            ),
            (
                format!("/web/purchases/{}/confirm", fixture.purchase_id),
                "method_id=".to_string(),
            ),
            (
                format!("/web/purchases/{}/payments", fixture.purchase_id),
                format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
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
                body.contains("purchases.create"),
                "{uri} must name purchases.create: {body}"
            );
        }

        // Line edit and removal, each in its own method (the web routes
        // register POST for the edit, DELETE for the removal).
        for (method, uri, body) in [
            (
                "POST",
                format!(
                    "/web/purchases/{}/lines/{}",
                    fixture.purchase_id, fixture.line_id
                ),
                "qty=9&unit_cost=9",
            ),
            (
                "DELETE",
                format!(
                    "/web/purchases/{}/lines/{}",
                    fixture.purchase_id, fixture.line_id
                ),
                "",
            ),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(&uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .header("cookie", &cookie)
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
            let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("purchases.create"),
                "{method} {uri} must name purchases.create: {text}"
            );
        }

        // The cancel is its own tier.
        let (status, body) = post_form_as(
            app,
            &format!("/web/purchases/{}/cancel", fixture.purchase_id),
            "reason=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("purchases.cancel"), "{body}");
    }

    /// The old collection endpoints (id in the body) are separate gated
    /// boundaries: the adapter declares its own gate and the delegated call is
    /// a plain function call through the ungated `*_impl` body, so removing
    /// the ADAPTER's gate is exactly what this test pins (the path endpoints
    /// pin the inner handlers above).
    #[tokio::test]
    async fn the_purchases_collection_adapters_carry_their_own_gate() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "suppliers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        let cases = [
            (
                "/web/purchases/lines",
                format!(
                    "purchase_id={}&product_id={}&qty=1",
                    fixture.purchase_id, fixture.product_id
                ),
                "purchases.create",
            ),
            (
                "/web/purchases/confirm",
                format!("purchase_id={}&method_id=", fixture.purchase_id),
                "purchases.create",
            ),
            (
                "/web/purchases/payments",
                format!(
                    "purchase_id={}&method_id={}&amount=5&date=2024-05-03",
                    fixture.purchase_id, fixture.method_id
                ),
                "purchases.create",
            ),
            (
                "/web/purchases/cancel",
                format!("purchase_id={}&reason=", fixture.purchase_id),
                "purchases.cancel",
            ),
        ];
        for (uri, body, code) in cases {
            let (status, body) = post_form_as(
                app.clone(),
                uri,
                &body,
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(body.contains(code), "{uri} must name {code}: {body}");
        }
    }

    /// The refusal writes nothing: the refused creation leaves the purchases
    /// table where it was, the refused confirmation keeps the draft, and the
    /// refused payment writes no payment row.
    #[tokio::test]
    async fn ac10_the_purchases_web_refusal_writes_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "suppliers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let purchases_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchases")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.200}");
        let purchases_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchases")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            purchases_after, purchases_before,
            "a refused create must write nothing"
        );

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.purchase.status,
            crate::models::PurchaseStatus::Draft,
            "a refused confirm must not flip the status"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let payments_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchase_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app,
            &format!("/web/purchases/{}/payments", fixture.purchase_id),
            &format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let payments_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchase_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            payments_after, payments_before,
            "a refused payment must write nothing"
        );
    }

    /// A principal holding the permissions gets the normal answers: the
    /// creation redirects to the new record, the seeded suggestion creates a
    /// draft, and the record actions answer their fragments.
    #[tokio::test]
    async fn ac10_the_purchases_web_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &[
                "purchases.read",
                "purchases.create",
                "purchases.cancel",
                "suppliers.read",
                "purchases.costs.read",
                "inventory.read",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let app = crate::routes::router(state.clone());

        // Create: the plain form path redirects to the record.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!(
                "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
                fixture.supplier_id
            ),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body:.200}");

        // Confirm, payment and cancel: their normal HTMX fragments.
        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/payments", fixture.purchase_id),
            &format!("method_id={}&amount=10&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, _body) = post_form_as(
            app,
            &format!("/web/purchases/{}/cancel", fixture.purchase_id),
            "reason=wrong order",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The gate order must not change: an anonymous request gets the login
    /// redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (status, _) = get_html_as(app.clone(), "/purchases", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, _) = get_html_as(app, "/web/purchases", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    // -- DELETE /web/purchases/{id}: the documents drawer's draft delete -------

    /// DELETE carries no body; the interesting answer is the status plus the
    /// `HX-Trigger` the documents page listens for.
    async fn send_delete(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let mut builder = Request::builder().method("DELETE").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let trigger = resp
            .headers()
            .get("HX-Trigger")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string(), trigger)
    }

    #[tokio::test]
    async fn web_delete_draft_purchase_answers_200_with_the_purchase_changed_trigger() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        let (status, body, trigger) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            trigger.as_deref(),
            Some("purchase-changed"),
            "the documents page listens for purchase-changed"
        );
        let err = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::NotFound(_)),
            "the deleted draft must be gone: {err:?}"
        );
    }

    #[tokio::test]
    async fn web_delete_draft_refuses_a_confirmed_purchase_with_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, _) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .is_ok());
    }

    /// A discarded purchase (cancelled before confirm, number still NULL)
    /// deletes through the same route: empty 200, `purchase-changed` trigger,
    /// detail then 404s — the acceptance path for never-confirmed cancelled
    /// rows.
    #[tokio::test]
    async fn web_delete_draft_discarded_cancelled_purchase_answers_200_and_is_gone() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .cancel(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, trigger) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            trigger.as_deref(),
            Some("purchase-changed"),
            "the documents page listens for purchase-changed"
        );
        let err = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::NotFound(_)),
            "the deleted discarded purchase must be gone: {err:?}"
        );
    }

    /// Confirmed-then-cancelled keeps the protection at the route: 400 and
    /// the row survives.
    #[tokio::test]
    async fn web_delete_draft_refuses_a_confirmed_then_cancelled_purchase_with_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .cancel(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, _) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .is_ok());
    }

    /// T3: the record page offers Delete — with the native confirm — ONLY for
    /// a discarded (never-confirmed, number still NULL) cancelled purchase.
    /// A confirmed-then-cancelled record carries its number and must show no
    /// delete at all.
    #[tokio::test]
    async fn web_purchase_record_offers_delete_only_for_a_discarded_cancelled_purchase() {
        let state = test_state().await;
        let discarded = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .cancel(audit_actor(&state).await, discarded.purchase_id, None)
            .await
            .unwrap();

        let annulled = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                annulled.purchase_id,
                Some(annulled.method_id),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .cancel(
                audit_actor(&state).await,
                annulled.purchase_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();

        let app = crate::routes::router(state);

        let (status, html) = get_html(
            app.clone(),
            &format!("/purchases/{}", discarded.purchase_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let needle = format!("hx-delete=\"/web/purchases/{}\"", discarded.purchase_id);
        assert!(
            html.contains(&needle),
            "a discarded purchase must offer delete: {html:.400}"
        );
        assert!(
            element_tag_containing(&html, &needle).contains("hx-confirm"),
            "deleting must ask first"
        );

        let (status, html) = get_html(app, &format!("/purchases/{}", annulled.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains(&format!(
                "hx-delete=\"/web/purchases/{}\"",
                annulled.purchase_id
            )),
            "a confirmed-then-cancelled record must offer no delete: {html:.400}"
        );
    }

    #[tokio::test]
    async fn web_delete_draft_requires_purchases_create() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();

        let (status, _, _) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .is_ok());
    }
}
