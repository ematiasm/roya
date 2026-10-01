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
    NewPurchase, PaymentType, PurchaseListFilter, PurchaseRecord, PurchaseStatus,
    PurchaseSuggestions, UpdateProduct,
};
use crate::routes::{localized_refusal_error, AppState};
use crate::services::purchase_cost::{self, CostBasis};
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
    /// The document's own fields, flattened, plus the refusal already in the
    /// operator's language.
    ///
    /// `money` is `None` exactly when `total_refusal_message` is not empty, so the
    /// template has one thing to render in place of the figures — and no figure at
    /// all to mistake for a real zero.
    pub purchase: crate::models::Purchase,
    pub line_count: usize,
    pub money: Option<crate::models::RecordMoney>,
    pub total_refusal_message: String,
    pub supplier_name: String,
    /// The payment state, from the document's money. It is a money claim, so it
    /// carries no value for a refused document: the template shows the refusal in
    /// its place and never reads this.
    pub payment_state: PurchasePaymentState,
    /// Whether money was received against the document, from its money. Same
    /// rule: read only when `money` is `Some`.
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
/// The one word a row's money resolves to. The document and its money are passed
/// separately because a LIST row carries them separately — the document is
/// always there and the money is there when the arithmetic carried it.
fn purchase_payment_state_of(
    purchase: &crate::models::Purchase,
    money: crate::models::RecordMoney,
    today: NaiveDate,
) -> PurchasePaymentState {
    if money.due <= Decimal::ZERO {
        PurchasePaymentState::Paid
    } else if purchase.due_date.map(|d| d < today) == Some(true) {
        PurchasePaymentState::Overdue
    } else {
        PurchasePaymentState::Due
    }
}

/// One reorder row with its level resolved: the quantity, or the sentence in its
/// place. The reorder panel is a list of set sums — a level per product — so the
/// product whose movements cannot be added up keeps its row, its name, its SKU
/// and its seed action, and states the rule where the quantity was.
#[derive(Clone)]
pub struct SuggestionView {
    pub product: crate::models::Product,
    /// The suggestion and its subtotal, already formatted — and EMPTY for a
    /// refused level, which has no suggestion to format. A `0` there would read
    /// as "reorder nothing".
    pub suggested_qty: String,
    pub subtotal: String,
    pub supplier_name: String,
    pub unit_cost: String,
    pub stock: String,
    pub stock_message: String,
}

/// A reorder row with no supplier cost yet: the same resolved level, and it keeps
/// its own place in the "without a cost" list rather than being dropped.
#[derive(Clone)]
pub struct SuggestionWithoutSupplierView {
    pub product: crate::models::Product,
    pub suggested_qty: String,
    pub stock: String,
    pub stock_message: String,
}

/// The panel's rows, resolved. `has_suggestions` is the caller's: it is about
/// whether there is anything to show, and the count is unchanged by a refusal.
pub struct SuggestionFigures {
    pub suggestions: Vec<SuggestionView>,
    pub without_supplier: Vec<SuggestionWithoutSupplierView>,
}

fn suggestion_figures(
    suggestions: PurchaseSuggestions,
    localization: &LocalizationContext,
) -> SuggestionFigures {
    let level = |stock: crate::models::SetMoney| match stock.amount {
        Some(level) => (localization.format_quantity(level), String::new()),
        None => (
            String::new(),
            stock
                .refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                .unwrap_or_default(),
        ),
    };
    SuggestionFigures {
        suggestions: suggestions
            .suggestions
            .into_iter()
            .map(|s| {
                let (stock, stock_message) = level(s.stock);
                SuggestionView {
                    product: s.product,
                    suggested_qty: s
                        .suggested_qty
                        .map(|qty| localization.format_quantity(qty))
                        .unwrap_or_default(),
                    subtotal: s
                        .subtotal
                        .map(|total| localization.format_money(total))
                        .unwrap_or_default(),
                    supplier_name: s.supplier_name,
                    // `format_money`, not `format_currency`: the unit cost and
                    // the subtotal above it are `unit_cost` and
                    // `suggested_qty * unit_cost` — a product of what the
                    // operator typed and what the reorder solve produced, at
                    // whatever scale each of those arrived at. Neither is
                    // rounded on the way here, so the panel is where a scale-0
                    // and a scale-2 reading of the SAME cost sit side by side.
                    // Money scale, no arithmetic: see `format_money`.
                    unit_cost: localization.format_money(s.unit_cost),
                    stock,
                    stock_message,
                }
            })
            .collect(),
        without_supplier: suggestions
            .without_supplier
            .into_iter()
            .map(|w| {
                let (stock, stock_message) = level(w.stock);
                SuggestionWithoutSupplierView {
                    product: w.product,
                    suggested_qty: w
                        .suggested_qty
                        .map(|qty| localization.format_quantity(qty))
                        .unwrap_or_default(),
                    stock,
                    stock_message,
                }
            })
            .collect(),
    }
}

#[derive(Template)]
#[template(path = "purchases.html")]
struct PurchasesTemplate {
    title: String,
    localization: LocalizationContext,
    purchases: Vec<PurchaseView>,
    suggestions: SuggestionFigures,
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
    /// The document-total refusal in the operator's language, empty when the
    /// document totals exactly. The page includes the same record partial the
    /// action responses render, so the sentence travels with it.
    total_refusal_message: String,
    tracked_units_message: String,
    /// Today's date, ISO — the `return_date` the record body's "Return goods"
    /// action posts. See `PurchaseRecordContext::return_date`.
    return_date: String,
    /// Whether the acting principal holds `purchases.create`. See
    /// `PurchaseRecordContext::can_return`.
    can_return: bool,
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
    /// The document-total refusal, already in the operator's language, or empty
    /// when the document totals exactly. Resolved HERE, through the one shared
    /// `price_refusal_key` mapping, so the purchase record page cannot word the
    /// rule differently from the sale record page or the index.
    total_refusal_message: String,
    tracked_units_message: String,
    /// T4: the draft's inline header posts the existing header route, and
    /// the picker's `include` names the three sibling field ids so every
    /// post (Enter, Save, a clicked result) carries the same field set —
    /// the header route maps invoice and notes unconditionally, so an
    /// absent field would arrive as empty and wipe the stored value.
    /// Computed in the wiring layer: Askama 0.12 has no string
    /// concatenation and the codebase passes such strings from Rust.
    header_action: String,
    header_include: &'static str,
    /// Today's date, ISO — what the "Return goods" action posts as the return's
    /// own date. See `PurchaseRecordContext::return_date`.
    return_date: String,
    /// Whether the acting principal holds `purchases.create`. See
    /// `PurchaseRecordContext::can_return`.
    can_return: bool,
}

#[derive(Template)]
#[template(path = "partials/suggestion_list.html")]
struct SuggestionListPartial {
    localization: LocalizationContext,
    suggestions: SuggestionFigures,
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

// ---------------------------------------------------------------------------
// The two sides of one cost, and which one is the input
// ---------------------------------------------------------------------------

/// The two typed figures, already read and validated, and the basis that says
/// which of them the operator typed into last.
struct TypedCost {
    basis: Option<CostBasis>,
    net: Option<Decimal>,
    gross: Option<Decimal>,
}

impl TypedCost {
    /// Read a form's pair. The net is read as OPTIONAL on both paths, because
    /// "is a net required here?" is not this struct's question — it is the
    /// caller's, and the two callers answer it differently on purpose.
    ///
    /// An unparseable figure is a 400 naming the field, and the field is named
    /// as the operator knows it, not as the form does: `unit_cost` is "costo"
    /// on screen and `unit_cost_gross` is the field beside it.
    fn read(
        net: &str,
        gross: &str,
        basis: &str,
        localization: &LocalizationContext,
    ) -> AppResult<TypedCost> {
        Ok(TypedCost {
            basis: CostBasis::parse(basis),
            net: parse_opt_decimal(net, "unit_cost", localization)?,
            gross: parse_opt_decimal(gross, "unit_cost_gross", localization)?,
        })
    }
}

/// THE DIRECTION RULE, applied once for every surface that writes a purchase
/// line cost: the entry row's add, the inline edit, and the entry row's
/// preview all call this, so they cannot disagree about which side of the pair
/// the operator typed into.
///
/// The tax set is read ONLY when the gross is the input, and that is the whole
/// reason this is a function rather than a pair of inline matches: a net that
/// is already on the form costs no read at all, so the common path cannot be
/// slowed down or made failable by the other field's arithmetic.
///
/// # The read skew, accepted on purpose
///
/// The tax set comes from `list_active_for_product` at ROUTE time, while the
/// write resolves it again INSIDE its own transaction
/// (`purchase_repo::active_taxes_for_product`). A tax linked or deactivated
/// between those two moments is a real, narrow hazard, and it is NOT fixed by
/// freezing a `Vec<Tax>` into the write: the in-transaction resolution is what
/// stops a document being written against a rate set that moved under it, and
/// changing that is a different decision about a different feature. The
/// post-write re-render is the truth the operator actually sees, and it is read
/// back from the database rather than from this solve.
async fn typed_line_cost(
    state: &AppState,
    product_id: i64,
    typed: TypedCost,
) -> AppResult<Option<Decimal>> {
    match purchase_cost::cost_ask(typed.basis, typed.net, typed.gross) {
        // No read, no solve, no refusal: the net the operator typed is the
        // stored truth, and a gross sitting in the other field can never make it
        // invalid.
        purchase_cost::CostAsk::Net(net) => Ok(Some(net)),
        purchase_cost::CostAsk::Gross(gross) => {
            let taxes = state
                .tax_service
                .list_active_for_product(product_id)
                .await?;
            purchase_cost::solve_net_cost_from_gross(gross, &taxes)
                .map(Some)
                .map_err(AppError::PriceRefused)
        }
        // Nothing was typed. Create keeps its own fallback; the edit path turns
        // this into its required-field refusal, which is the asymmetry between
        // the two paths and not an accident of where the branch sits.
        purchase_cost::CostAsk::Unstated => Ok(None),
    }
}

/// The rows of a purchases list, with each document's refusal resolved.
///
/// The ROW read, not the detail read: a list must render a document whose total
/// cannot be computed instead of answering an error and taking every other row on
/// the page with it. The refusal is resolved here, through the one shared
/// `price_refusal_key` mapping, so this surface cannot word the rule differently
/// from the record page, the sales list or the index.
async fn purchase_views(
    state: &AppState,
    filter: &PurchaseListFilter,
    today: NaiveDate,
    localization: &LocalizationContext,
) -> AppResult<Vec<PurchaseView>> {
    let rows = state.purchases_service.list_rows_filtered(filter).await?;
    purchase_views_from_rows(state, rows, today, localization).await
}

/// [`purchase_views`] over rows the caller has already read, so a surface that
/// needs the documents AND something derived from them reads once. The supplier
/// drawer is that surface: it renders the rows and sums their dues.
pub(crate) async fn purchase_views_from_rows(
    state: &AppState,
    rows: Vec<crate::models::PurchaseListRow>,
    today: NaiveDate,
    localization: &LocalizationContext,
) -> AppResult<Vec<PurchaseView>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let supplier = state
            .supplier_service
            .get_supplier(row.purchase.supplier_id)
            .await?;
        let (payment_state, partially_paid) = match row.money {
            // A refused document gets the state a DRAFT gets, which the template
            // never reaches: the status gate runs first and the money zone is
            // replaced by the refusal. It is a value, never a claim.
            Some(money) => (
                purchase_payment_state_of(&row.purchase, money, today),
                money.paid > Decimal::ZERO && money.due > Decimal::ZERO,
            ),
            None => (PurchasePaymentState::Paid, false),
        };
        out.push(PurchaseView {
            total_refusal_message: row
                .total_refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                .unwrap_or_default(),
            purchase: row.purchase,
            line_count: row.line_count,
            money: row.money,
            payment_state,
            partially_paid,
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
    /// The document-total refusal in the operator's language, empty when the
    /// document totals exactly. The page states it instead of answering an
    /// error, so an operator can still open a purchase and reduce it.
    total_refusal_message: String,
    /// The refusal for the document's UNIT count, in the same language and
    /// through the same one mapping. It is a separate figure from the money: a
    /// purchase of `4e28` units at no cost has a perfectly ordinary total of `0`
    /// and a unit count that leaves the range, and the effects preview has to say
    /// so rather than print a movement it cannot state.
    tracked_units_message: String,
    /// Today's date in the active locale, ISO form — what the record page's
    /// "Return goods" action posts as the return's own date. The return is made
    /// on the day the operator starts it, not on the purchase's date, and the
    /// field is a hidden input rather than a date picker because there is nothing
    /// to decide at that moment: the return's header form edits it afterwards.
    return_date: String,
    /// Whether this principal holds `purchases.create` — the gate
    /// `POST /web/purchase-returns` declares. The "Return goods" action renders
    /// only when true, so the record page never offers an action the route would
    /// refuse (AC21's rule for an action rather than a page entry). Resolved in
    /// the wiring layer from the request's own principal, on EVERY render path
    /// including the fragment: the fragment is a page too.
    can_return: bool,
}

async fn record_context(
    state: &AppState,
    purchase_id: i64,
    localization: LocalizationContext,
    can_return: bool,
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
    let message = |refusal: Option<crate::models::PriceRefusal>| {
        refusal
            .map(|refusal| crate::routes::price_refusal_message(&refusal, &localization))
            .unwrap_or_default()
    };
    let total_refusal_message = message(record.total_refusal);
    let tracked_units_message = message(record.tracked_units.refusal);
    Ok(PurchaseRecordContext {
        record,
        confirm_due_date,
        method_options,
        // Cloned rather than moved: the same figure is both the entry row's
        // `today` and the return action's `return_date`, and a field the
        // template reads must exist on the context even when it is the same
        // string twice.
        today: today.clone(),
        localization,
        created_by_name,
        updated_by_name,
        total_refusal_message,
        tracked_units_message,
        return_date: today,
        can_return,
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
        total_refusal_message: context.total_refusal_message,
        tracked_units_message: context.tracked_units_message,
        return_date: context.return_date,
        can_return: context.can_return,
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
    can_return: bool,
) -> AppResult<Response> {
    changed_with_notice(state, purchase_id, localization, can_return, false, None).await
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
    can_return: bool,
    entry_row_focus: bool,
    notice_html: Option<String>,
) -> AppResult<Response> {
    let mut html = render_record(
        record_context(state, purchase_id, localization.clone(), can_return).await?,
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
    // A list of documents can carry one whose total cannot be computed, and
    // this page is where the operator goes to FIND it. It refuses the whole
    // page rather than dropping that document silently — a list that quietly
    // omits a stored sale is a lie about the shop's history — and it refuses
    // in the operator's language, through the one shared mapping.
    let purchases = purchase_views(&state, &query.to_filter(), today, &localization).await?;
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
        (suggestion_figures(suggestions, &localization), has)
    } else {
        (
            SuggestionFigures {
                suggestions: vec![],
                without_supplier: vec![],
            },
            false,
        )
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
    let context = record_context(
        &state,
        id,
        localization,
        principal.has_permission::<PurchasesCreate>(),
    )
    .await?;
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
        total_refusal_message: context.total_refusal_message,
        tracked_units_message: context.tracked_units_message,
        return_date: context.return_date,
        can_return: context.can_return,
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
    // A list of documents can carry one whose total cannot be computed, and
    // this page is where the operator goes to FIND it. It refuses the whole
    // page rather than dropping that document silently — a list that quietly
    // omits a stored sale is a lie about the shop's history — and it refuses
    // in the operator's language, through the one shared mapping.
    let view = purchase_views(&state, &query.to_filter(), today, &localization).await?;
    let title = localization
        .tr(crate::localization::MessageKey::PurchasesAll)
        .to_string();
    Ok(render_list(view, &title, localization)?.into_response())
}

async fn web_purchase_detail(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let html = render_record(
        record_context(
            &state,
            id,
            localization,
            principal.has_permission::<PurchasesCreate>(),
        )
        .await?,
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
        suggestions: suggestion_figures(suggestions, &localization),
        localization,
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
    /// The quantity, and it is OPTIONAL because the entry row stopped asking for
    /// one: a bare scan or a bare accept posts no quantity at all, and the
    /// operator's intent there is unambiguous — "one, and tell me about it".
    ///
    /// The default is `None` -> `Decimal::ONE` in the ROUTE, never inside
    /// `add_or_increment_line`, and the reason is a caller this form is not: the
    /// JSON API's `AddLineRequest` requires an explicit quantity, and the
    /// service's `qty > 0` is the guard that refuses a machine client's zero.
    /// A default reached from the service would turn that 400 into a line for
    /// every caller. So the form is looser than the service, on purpose and in
    /// exactly one place.
    #[serde(default)]
    pub qty: Option<String>,
    /// The net cost. Optional here, as it has always been: an empty cost falls
    /// back to the supplier's satellite and then to the product column.
    #[serde(default)]
    pub unit_cost: String,
    /// The supplier's tax-inclusive cost — the SECOND way in to the same figure.
    /// Empty means the operator did not type one, and the net decides on its
    /// own; the pair is resolved by [`typed_line_cost`], never here.
    #[serde(default)]
    pub unit_cost_gross: String,
    /// Which of the two the operator typed into last, as the fixed wire tokens
    /// `net` and `gross`. Absent whenever the page did not say — a browser with
    /// no JavaScript, a REST client — and then the net wins.
    #[serde(default)]
    pub cost_basis: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineForm {
    #[serde(default)]
    pub qty: String,
    /// The net cost. Required on this path, exactly as it has always been: an
    /// edit that states no cost at all is still refused, because there is no
    /// supplier satellite to fall back to on a line that already exists.
    #[serde(default)]
    pub unit_cost: String,
    /// The tax-inclusive cost, which may stand in for the net when the page says
    /// the operator typed it — see [`AddLineForm::unit_cost_gross`].
    #[serde(default)]
    pub unit_cost_gross: String,
    /// See [`AddLineForm::cost_basis`].
    #[serde(default)]
    pub cost_basis: String,
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
    web_add_line_impl(
        state,
        principal.user_id,
        principal.has_permission::<PurchasesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

async fn web_add_line_impl(
    state: AppState,
    actor: i64,
    can_return: bool,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: AddLineForm,
) -> AppResult<Response> {
    let qty = match form.qty.as_deref() {
        Some(raw) => parse_required_decimal(raw, "qty", &localization)?,
        // THE DEFAULT, and the one place it exists. The entry row carries only
        // the product search, so a scan or an accept posts no quantity and
        // arrives here as `None`; "one" answers a request nobody made, and the
        // line is where the operator corrects it if that is wrong.
        //
        // It resolves BEFORE the product, as it always did, and for the same
        // reason: an unreadable quantity is still the sentence the operator sees
        // before anything else is read. A STATED zero is not the absent case and
        // is not softened here — it parses, and the service's `qty > 0` refuses
        // it. See [`AddLineForm::qty`] for why the default cannot live lower
        // down.
        None => Decimal::ONE,
    };
    // An explicit product id (a clicked result) wins over the typed text; a scan
    // or an Enter carries only the value and resolves through inventory. It is
    // resolved BEFORE the cost because the cost's tax set belongs to the
    // product, and the qty parse stays first so an invalid quantity is still
    // the sentence the operator sees before anything else is read.
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
    // Which side of the pair the operator typed into. The RESULT is what goes
    // to `add_or_increment_line`, and it is the same `Option<Decimal>` a typed
    // net has always produced — so the supplier-satellite fallback, the
    // negative-cost guard and the merge comparison are all reached on exactly
    // the terms they were written for, whichever field the figure came from.
    let unit_cost = typed_line_cost(
        &state,
        product_id,
        TypedCost::read(
            &form.unit_cost,
            &form.unit_cost_gross,
            &form.cost_basis,
            &localization,
        )?,
    )
    .await
    // The CONVERSION refuses in its own vocabulary, and `PriceRefused`
    // serializes as the model's English sentence — which is exactly what a
    // non-localized body would carry. So the solve's refusal is rendered here
    // too, through the same one mapping, or the operator reads English on a
    // Spanish page for the one refusal this feature introduces.
    .map_err(|error| localized_refusal_error(error, &localization))?;
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
        .await
        // A line write runs the shared tax contract, which REFUSES an amount or
        // a tax arithmetic it cannot carry instead of panicking, and the cost's
        // own conversion refuses a gross that is the gross of no net. Both
        // answer through the one shared renderer, in the operator's own
        // language; every other error passes through untouched.
        .map_err(|error| localized_refusal_error(error, &localization))?;
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
        return changed_with_notice(&state, id, &localization, can_return, true, notice).await;
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
        principal.has_permission::<PurchasesCreate>(),
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
    // The SAME direction rule the add applies, on the same terms: the net is
    // read first so an unreadable one is still the sentence the operator sees,
    // and a stated gross is solved through the same service call the entry row's
    // preview uses. What is NOT the same is the required-ness, and that is this
    // path's own asymmetry rather than a difference in the rule: an inline edit
    // that states no cost at all is refused here, exactly as it always was,
    // because there is no satellite to fall back to on a line that exists.
    let typed = TypedCost::read(
        &form.unit_cost,
        &form.unit_cost_gross,
        &form.cost_basis,
        &localization,
    )?;
    // The line's product, and only when the gross is the input: the tax set
    // belongs to a product and this is the one moment an edit has to know which.
    let solved = match purchase_cost::cost_ask(typed.basis, typed.net, typed.gross) {
        purchase_cost::CostAsk::Net(_) | purchase_cost::CostAsk::Unstated => {
            typed_line_cost(&state, 0, typed).await
        }
        purchase_cost::CostAsk::Gross(_) => {
            let product_id = state.purchases_service.line_product_id(line_id).await?;
            typed_line_cost(&state, product_id, typed).await
        }
    }
    // Same renderer, same reason as the add: a refused conversion is a sentence
    // in the operator's language or it is a bug.
    .map_err(|error| localized_refusal_error(error, &localization))?;
    // The required net, stated as the same sentence this path has always
    // answered with — the refusal an operator sees for an empty cost has not
    // changed, only the set of inputs that can satisfy it has grown.
    let unit_cost = solved.ok_or_else(|| AppError::Validation("invalid unit_cost".into()))?;
    state
        .purchases_service
        .update_line(principal.user_id, line_id, qty, unit_cost)
        .await
        // Same contract, same renderer, same reason as the add.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(
            &state,
            purchase_id,
            &localization,
            principal.has_permission::<PurchasesCreate>(),
        )
        .await;
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
    changed(
        &state,
        purchase_id,
        &localization,
        principal.has_permission::<PurchasesCreate>(),
    )
    .await
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
        return changed(
            &state,
            purchase_id,
            &localization,
            principal.has_permission::<PurchasesCreate>(),
        )
        .await;
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
    web_confirm_purchase_impl(
        state,
        principal.user_id,
        principal.has_permission::<PurchasesCreate>(),
        headers,
        &localization,
        id,
        form,
    )
    .await
}

async fn web_confirm_purchase_impl(
    state: AppState,
    actor: i64,
    can_return: bool,
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
        return changed(&state, id, &localization, can_return).await;
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
        principal.has_permission::<PurchasesCreate>(),
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
    web_record_payment_impl(
        state,
        principal.user_id,
        principal.has_permission::<PurchasesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

async fn web_record_payment_impl(
    state: AppState,
    actor: i64,
    can_return: bool,
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
        return changed(&state, id, &localization, can_return).await;
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
        principal.has_permission::<PurchasesCreate>(),
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
    web_cancel_purchase_impl(
        state,
        principal.user_id,
        principal.has_permission::<PurchasesCreate>(),
        headers,
        &localization,
        id,
        form,
    )
    .await
}

async fn web_cancel_purchase_impl(
    state: AppState,
    actor: i64,
    can_return: bool,
    headers: HeaderMap,
    localization: &LocalizationContext,
    id: i64,
    form: CancelPurchaseForm,
) -> AppResult<Response> {
    // The annulment measures refunds against the document's payments, so it can
    // answer the document-total refusal too — in the operator's language, the
    // same way every other refusal on this surface answers.
    state
        .purchases_service
        .cancel(actor, id, clean_opt(&form.reason))
        .await
        .map_err(|error| localized_refusal_error(error, localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, can_return).await;
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
        principal.has_permission::<PurchasesCreate>(),
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
        return changed(
            &state,
            id,
            &localization,
            principal.has_permission::<PurchasesCreate>(),
        )
        .await;
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
    // A DECISION read: seeding a line needs a real quantity, so a suggestion with
    // none — which is what a refused level produces — refuses here instead of
    // writing a line of zero.
    let Some(suggested_qty) = item.suggested_qty.filter(|qty| *qty > Decimal::ZERO) else {
        return Err(AppError::Validation(
            "suggested qty must be > 0 to seed a draft".into(),
        ));
    };
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
            suggested_qty,
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
        for id in [
            "id=\"line-qty\"",
            "id=\"line-unit-cost\"",
            "id=\"line-unit-cost-gross\"",
            "id=\"line-cost-basis\"",
            "id=\"line-cost-refusal\"",
        ] {
            assert!(
                !row.contains(id),
                "the entry row is search-only: it must not carry {id}. An operator is asked for a \
                 quantity and a cost on the LINE, after they have decided they want the product: \
                 {row:.600}"
            );
        }
        // What the entry row keeps is the picker island's own contract: the
        // search, the hidden `product_id` the island writes, and the submit that
        // carries a chosen result in one step.
        assert!(
            row.contains("id=\"product-picker\"")
                && row.contains("name=\"product_id\"")
                && row.contains("type=\"submit\""),
            "the search, the island's hidden product id and the submit all stay: {row:.600}"
        );
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
            // Money scale, so the applied cost of a whole number reads as the
            // price it is: `12` and `12.00` are one number, and this cell used
            // to render whichever the storage happened to carry. The applied
            // cost is unchanged — only the places it is SHOWN at.
            row.contains("12.00 USD"),
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
        // The entry row is persistent and search-only: the product field, the
        // island's hidden id and the Add action render without any click, inside
        // the money region adds swap. Quantity and money are asked on the LINE,
        // where the product is already a line the operator can see.
        assert!(
            html.contains("id=\"line-picker\"")
                && html.contains("id=\"product-picker\"")
                && !html.contains("id=\"line-qty\"")
                && !html.contains("id=\"line-unit-cost\""),
            "the entry row renders its search persistent, and asks for no quantity and no cost: \
             {html:.600}"
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
            .money
            .expect("an ordinary document totals")
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
            .money
            .expect("an ordinary document totals")
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
            !picker.contains("name=\"qty\""),
            "the entry row asks for no quantity: a scan and a click both add ONE, resolved by the \
             server, and the operator corrects it on the line: {picker}"
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

    /// THE DEFAULT, and the reason the entry row can lose its quantity input.
    ///
    /// `value="1"` on that input was MARKUP: the server had no default, so the
    /// form posted a quantity the operator never chose. This posts the add form
    /// with no `qty` field AT ALL — the exact body a bare scan produces once the
    /// input is gone — and requires the line to arrive at one unit, priced by
    /// the same resolution an empty cost has always used.
    ///
    /// The default lives at the FORM boundary, in this route, and nowhere else.
    /// `add_or_increment_line` keeps `qty: Decimal` and its `qty > 0` guard
    /// absolutely: that guard is what refuses a machine client's zero-quantity
    /// line, and a default reached from inside the service would quietly turn
    /// that 400 into a line for EVERY caller, JSON API included. The third
    /// assertion below is what holds that line — a web post that states
    /// `qty=0` is still a refusal, so "absent" and "zero" cannot be one thing.
    #[tokio::test]
    async fn a_scan_with_no_quantity_field_at_all_adds_one_unit() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let scanned = seed_extra_product(&state, "SCAN-DEFAULT", Some("7791234567892")).await;
        let app = crate::routes::router(state.clone());

        // A bare scan: the typed value and nothing else. No `qty`, no cost.
        let (status, _, added) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=7791234567892&unit_cost=",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "an absent quantity is one unit, not a 400: {added}"
        );

        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let line = detail
            .lines
            .iter()
            .find(|line| line.product_id == scanned.id)
            .expect("the scan adds its own line");
        assert_eq!(
            line.qty,
            Decimal::ONE,
            "the default IS one: the operator never chose a quantity"
        );
        assert_eq!(
            line.unit_cost,
            Decimal::from(10),
            "and the cost is still resolved, not defaulted: an empty cost is the product column here, \
             because this supplier has no satellite row"
        );

        // The default is for an ABSENT field only. A stated zero is a refusal,
        // and it stays one: this is the assertion that would fail if the
        // default were reached from inside the service instead of from the form.
        let (status, _, refused) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=7791234567892&unit_cost=&qty=0",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "`qty > 0` is the service's invariant and the default must not loosen it: {refused}"
        );
        assert_eq!(
            state
                .purchases_service
                .get_detail(fixture.purchase_id)
                .await
                .unwrap()
                .lines
                .len(),
            2,
            "the refused add stored nothing: only the fixture's own line and the scan's"
        );
    }

    /// AC10 (clicked result): a result is its own add action; the request carries
    /// the quantity on the wire, which is where it has lived since the entry row
    /// stopped rendering a field for it, and the result supplies the product id
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
            html.contains("data-drawer=\"purchase-drawer\""),
            "the peek must opt into the shared drawer controller: {html:.600}"
        );
        // The close button opts into the controller's close instead of calling a
        // per-module function, and it keeps its `aria-label`: the accessible name
        // and the wiring are two separate contracts on the same element.
        assert!(
            html.contains("data-drawer-close aria-label="),
            "the shell's close button must be wired to the shared controller: {html:.600}"
        );
        // A mutation inside the peek makes it stale, so the peek closes on it.
        // This one is the module's actual behaviour and the reason the opt-in
        // exists: five hand-written close pairs collapse into this attribute.
        assert!(
            html.contains("data-drawer-close-on=\"purchase-changed\""),
            "the peek must close when the purchase changes: {html:.600}"
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

    // -----------------------------------------------------------------------
    // Tax-contract overflow (tax contract overflow T1).
    //
    // The purchase twin of the sale assertion, and it is a separate production
    // path with its own repository: `SqlitePurchaseRepository` evaluates
    // `qty * unit_cost` with the raw operator before entering the shared tax
    // contract, so an amount of `1e20 * 1e9` panics here independently of
    // anything the sale path does.
    // -----------------------------------------------------------------------

    /// A product of its own, so the post cannot land on the fixture's product:
    /// a repeat product takes the scan-merge branch, and this test is about the
    /// amount's own arithmetic, not about which branch answered.
    async fn product_with_sku(state: &AppState, sku: &str) -> i64 {
        let product = state
            .inventory_service
            .create_product(
                audit_actor(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Overflow {sku}"),
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
        product.id
    }

    /// The same defect on the purchase path: an unrepresentable line amount is a
    /// refusal carrying a sentence, not a dropped connection. The operator is
    /// typing a received quantity on this screen, so an unbounded `qty` is
    /// theirs to type, exactly like a sale's.
    #[tokio::test]
    async fn a_purchase_line_whose_amount_overflows_is_a_localized_refusal_not_a_panic() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let product_id = product_with_sku(&state, "PUR-OVF-1").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, _, body) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!("product_id={product_id}&qty=100000000000000000000&unit_cost=1000000000"),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an unrepresentable line amount is a refusal, not a dropped connection: {body:.400}"
        );
        let expected =
            localization.tr(crate::localization::MessageKey::PriceRefusalLineAmountTooLarge);
        assert!(
            body.contains(&expected),
            "the operator must read the refusal in their own language: {body:.400}"
        );
        assert!(
            !state
                .purchases_service
                .get_detail(fixture.purchase_id)
                .await
                .unwrap()
                .lines
                .iter()
                .any(|line| line.product_id == product_id),
            "a refused line writes nothing"
        );
    }

    // -----------------------------------------------------------------------
    // Document-level accumulation (tax contract overflow T3).
    //
    // The purchase twin of the sale construction, and the reason the fix cannot
    // be written for one family: `PurchaseService::tax_split` folds the same
    // three accumulations with the same raw operators, and the operator typing
    // a received quantity and a supplier's cost is typing the same unbounded
    // operands.
    //
    // TWO products, not two posts of one: `add_or_increment_line` merges a
    // repeat product at the same price into the existing line, so a second post
    // of the same product would be a `qty + qty` merge and never a second term
    // in the document's sum.
    // -----------------------------------------------------------------------

    /// `4e28`: individually carryable (`Decimal::MAX ≈ 7.92e28`), accepted by
    /// the checked line write, and two of them are `8e28`, which is not.
    const FOUR_E28: &str = "40000000000000000000000000000";

    #[tokio::test]
    async fn a_draft_purchase_whose_lines_cannot_be_added_up_is_still_readable() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let first = product_with_sku(&state, "DOC-TOTAL-PUR-1").await;
        let second = product_with_sku(&state, "DOC-TOTAL-PUR-2").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();

        for product_id in [first, second] {
            let (status, _, body) = post_form_response(
                app.clone(),
                &format!("/web/purchases/{}/lines", fixture.purchase_id),
                &format!("product_id={product_id}&qty=1&unit_cost={FOUR_E28}"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "a line of 4e28 is carryable on its own: {body:.400}"
            );
        }

        // The fixture's own line is a third, ordinary term, so this document
        // carries the same defect as the sale twin: its lines cannot be added up.
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the purchase is READABLE: an operator who cannot open a stored document cannot fix \
             it: {html:.600}"
        );
        assert!(
            html.contains(&expected),
            "and it states the SAME refusal the sale record page states, in the operator's own \
             language, through the one shared mapping: {html:.2000}"
        );
        assert_eq!(
            html.matches("data-document-total-refusal").count(),
            1,
            "in the place the total would be: {html:.2000}"
        );
        assert!(
            html.contains("data-purchase-payment-status=\"refused\""),
            "and no payment status is published for a total that does not exist: {html:.2000}"
        );
        assert_eq!(
            html.matches("id=\"purchase-line-").count(),
            3,
            "every line is still shown, because each of them is representable: {html:.2000}"
        );

        // The service agrees, through its own typed result.
        match state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
        {
            Err(crate::error::AppError::PriceRefused(refusal)) => {
                assert_eq!(refusal, crate::models::PriceRefusal::DocumentTotalTooLarge)
            }
            other => panic!("the purchase total cannot be computed: {other:?}"),
        }
    }

    /// THE LIST PAGE ANSWERS, the sales twin's decision applied to the purchases
    /// family: a purchase whose lines cannot be added up renders IN PLACE, with
    /// no figure and the refusal where the figure was, and every other purchase
    /// on the page renders exactly as it does today.
    /// A product that TRACKS STOCK, which is the only kind whose quantity reaches
    /// the document's `tracked_units` figure. `product_with_sku` deliberately does
    /// not, so the two are separate helpers rather than a flag.
    async fn stock_tracking_product(state: &AppState, sku: &str) -> i64 {
        let product = state
            .inventory_service
            .create_product(
                audit_actor(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Tracked {sku}"),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
                    cost_price: Decimal::from(10),
                    track_stock: true,
                    min_stock: Some(Decimal::ZERO),
                    max_stock: Some(Decimal::from(1_000_000)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        product.id
    }

    /// A CONFIRMED purchase whose lines cannot be added up, plus the counts a test
    /// needs to prove the annulment wrote NOTHING.
    ///
    /// A confirmation refuses an un-totalable document by design, so the second
    /// line is stored the one way nothing in this application is supposed to:
    /// straight through SQL, past the checked write. That is exactly the state a
    /// confirmed purchase can be found in — by a migration, by an import, by a
    /// bug older than this one — and it is the state the reversal has to refuse
    /// BEFORE it touches anything.
    async fn confirmed_untotalable_purchase(
        state: &AppState,
        account: i64,
    ) -> (i64, i64, ReversalCounts) {
        let actor = audit_actor(state).await;
        let supplier = seed_supplier(state, "Untotalable Supplier").await;
        let product = stock_tracking_product(state, "UNTOUCHABLE").await;
        let purchase = state
            .purchases_service
            .create_draft(
                actor,
                crate::models::NewPurchase {
                    supplier_id: supplier.id,
                    // Credit, so the confirmation needs no payment method: the
                    // point of the fixture is the REVERSAL, and a payment is added
                    // afterwards so the refund leg is exercised too.
                    payment_type: crate::models::PaymentType::Credit,
                    purchase_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        // One line of 4e28 totals exactly, so the real flow confirms it.
        state
            .purchases_service
            .add_line(
                actor,
                purchase.id,
                product,
                Decimal::ONE,
                Some(Decimal::from_str(FOUR_E28).unwrap()),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(actor, purchase.id, None)
            .await
            .expect("one line of 4e28 is carryable, so the confirm is an ordinary one");
        // A payment against it, so the reversal has a refund to post: a stock
        // return alone would prove only half of what "wrote nothing" means.
        state
            .payment_method_service
            .ensure_defaults_for_account(actor, account, "Caja")
            .await
            .unwrap();
        let method = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash" && m.account_id == Some(account))
            .expect("Cash is assigned to the account")
            .id;
        state
            .purchases_service
            .record_payment(
                actor,
                purchase.id,
                method,
                Decimal::from(100),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 3).unwrap(),
            )
            .await
            .expect("100 against a 4e28 due is an ordinary payment");
        // The second line, stored directly.
        sqlx::query(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost, tax_total) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(purchase.id)
        .bind(product)
        .bind(Decimal::ONE.to_string())
        .bind(FOUR_E28)
        .execute(&state.pool)
        .await
        .unwrap();
        (purchase.id, product, ReversalCounts::read(state).await)
    }

    /// What an annulment of a purchase would write. Read as COUNTS so the test
    /// asserts the absence of every write, not the presence of one absence.
    #[derive(Debug, Clone, PartialEq)]
    struct ReversalCounts {
        status_cancelled: i64,
        return_movements: i64,
        income_transactions: i64,
        movements: i64,
    }

    impl ReversalCounts {
        async fn read(state: &AppState) -> Self {
            let status_cancelled: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM purchases WHERE status = 'Cancelled'")
                    .fetch_one(&state.pool)
                    .await
                    .unwrap();
            let return_movements: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM stock_movements WHERE reason = 'Purchase-return'",
            )
            .fetch_one(&state.pool)
            .await
            .unwrap();
            let income_transactions: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM transactions WHERE kind = 'Income'")
                    .fetch_one(&state.pool)
                    .await
                    .unwrap();
            let movements: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stock_movements")
                .fetch_one(&state.pool)
                .await
                .unwrap();
            Self {
                status_cancelled,
                return_movements,
                income_transactions,
                movements,
            }
        }
    }

    /// F2, first fold: the record's `tracked_units` is a SET SUM over the
    /// document's stock-tracking lines, and a bound that holds per line says
    /// nothing about their sum. Two lines of `4e28` units at no cost each: the
    /// document's money totals to a clean zero, and its unit count is `8e28`.
    #[tokio::test]
    async fn a_purchase_whose_units_cannot_be_added_up_is_still_readable() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();

        // Two products, because `add_or_increment_line` merges a repeat product
        // at the same price — and that merge is the SECOND fold this test
        // reaches, so the two lines are deliberately different products.
        let first = stock_tracking_product(&state, "UNITS-A").await;
        let second = stock_tracking_product(&state, "UNITS-B").await;
        let purchase =
            draft_purchase(&state, seed_supplier(&state, "Units Supplier").await.id).await;
        for product in [first, second] {
            let (status, _, body) = post_form_response(
                app.clone(),
                &format!("/web/purchases/{}/lines", purchase.id),
                &format!("product_id={product}&qty={FOUR_E28}&unit_cost=0"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body:.400}");
        }

        // The document's MONEY is a real figure — zero — because zero is what two
        // free lines at an enormous quantity add up to.
        let (status, html) = get_html(app.clone(), &format!("/purchases/{}", purchase.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&expected),
            "the record states the rule for the figure it cannot carry: {html:.600}"
        );
        let (status, html) = get_html(app, &format!("/purchases/{}", purchase.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("8E28") && !html.contains("80000000000000000000000000000"),
            "and publishes no unit figure for the document: {html:.600}"
        );
    }

    /// F2, second fold: a repeat product at the same price MERGES, and the merge
    /// is `existing.qty + qty` — a sum of a stored quantity and a REQUESTED one,
    /// with nothing between them but the write bound on the resulting amount.
    #[tokio::test]
    async fn a_repeat_line_whose_merged_quantity_cannot_be_carried_is_a_refusal() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let product = product_with_sku(&state, "MERGE-HUGE").await;
        let supplier = seed_supplier(&state, "Merge Supplier").await;
        let purchase = draft_purchase(&state, supplier.id).await;
        let post = |qty: &'static str| {
            let app = app.clone();
            async move {
                let (status, _, body) = post_form_response(
                    app,
                    &format!("/web/purchases/{}/lines", purchase.id),
                    &format!("product_id={product}&qty={qty}&unit_cost=0"),
                )
                .await;
                (status, body)
            }
        };

        let (status, _) = post(FOUR_E28).await;
        assert_eq!(status, StatusCode::OK);
        // The second add of the SAME product at the same price is the merge.
        let (status, body) = post(FOUR_E28).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a quantity the merge cannot carry is a localized refusal: {body:.400}"
        );
        assert!(
            body.contains(
                localization.tr(crate::localization::MessageKey::PriceRefusalLineAmountTooLarge)
            ),
            "in the operator's language, through the one shared mapping: {body:.400}"
        );

        // And the STORED line is untouched: the refusal preceded the write, so the
        // line still carries exactly the quantity the first add accepted. The
        // record states no document refusal, because there is none to state — one
        // line of `4e28` units at no cost has an amount of `0` and a unit count
        // that carries; it is only the SUM with a second one that would not.
        let record = state
            .purchases_service
            .get_record(purchase.id)
            .await
            .unwrap();
        assert_eq!(
            record.lines[0].qty,
            Decimal::from_str(FOUR_E28).unwrap(),
            "the refused add wrote nothing"
        );
        assert_eq!(record.total_refusal, None, "and the document still totals");
        let (status, html) = get_html(app, &format!("/purchases/{}", purchase.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("data-document-total-refusal"),
            "so the record states no refusal: {html:.600}"
        );
    }

    /// F3: the purchase annulment resolves the document's money BEFORE any write,
    /// exactly as the sale annulment does. Without that, a confirmed purchase
    /// whose lines cannot be added up returns the stock, posts the refunds, flips
    /// the status — and only then refuses at the read, which is a half applied
    /// reversal reported to the operator as a refusal.
    #[tokio::test]
    async fn a_purchase_annulment_that_cannot_carry_the_money_writes_nothing_at_all() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        // A purchase payment leaves the account, so the reversal has something to
        // refund only if the account holds something to pay with.
        let account = state
            .account_service
            .create(audit_actor(&state).await, "Caja")
            .await
            .unwrap();
        state
            .transaction_service
            .create(
                audit_actor(&state).await,
                account.id,
                crate::models::TransactionKind::Income,
                Decimal::from(1000),
                Some("opening".into()),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        let (purchase, _product, before) = confirmed_untotalable_purchase(&state, account.id).await;

        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{purchase}/cancel"),
            "reason=operator+mistake",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the annulment refuses instead of half applying: {body:.400}"
        );

        let after = ReversalCounts::read(&state).await;
        assert_eq!(after, before, "the refusal preceded every write: {after:?}");
        assert!(
            body.contains(&expected),
            "and it reaches the operator in their language: {body:.400}"
        );

        // And the document is untouched and still READABLE, so the operator can
        // reach the lines that have to be fixed.
        let (status, html) = get_html(app, &format!("/purchases/{purchase}")).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains(&expected), "{html:.600}");
        assert!(
            !html.contains("Purchase-return") && !html.contains("Anulado"),
            "and the record is still a confirmed purchase, not a cancelled one: {html:.600}"
        );
    }

    /// F1 (purchases twin): a list renders the document whose total cannot be
    /// computed, in place, with the rule and no figure.
    #[tokio::test]
    async fn the_purchases_list_renders_a_document_whose_total_cannot_be_computed() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        let big = localization.format_currency(Decimal::from_str(FOUR_E28).unwrap());

        // Two ordinary purchases, then the un-totalable one: two products, because
        // `add_or_increment_line` merges a repeat product at the same price.
        let supplier = seed_supplier(&state, "List Supplier").await;
        let mut ordinary = Vec::new();
        for n in 0..2 {
            let product = product_with_sku(&state, &format!("PLIST-ORD-{n}")).await;
            let purchase = draft_purchase(&state, supplier.id).await;
            let (status, _, body) = post_form_response(
                app.clone(),
                &format!("/web/purchases/{}/lines", purchase.id),
                &format!("product_id={product}&qty=2&unit_cost={ORDINARY_COST}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body:.400}");
            ordinary.push(purchase.id);
        }
        let untotalable = draft_purchase(&state, supplier.id).await;
        for n in 0..2 {
            let product = product_with_sku(&state, &format!("PLIST-BAD-{n}")).await;
            let (status, _, body) = post_form_response(
                app.clone(),
                &format!("/web/purchases/{}/lines", untotalable.id),
                &format!("product_id={product}&qty=1&unit_cost={FOUR_E28}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body:.400}");
        }

        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "one purchase that cannot be totaled must not take the page down: {html:.800}"
        );

        let row = row_html(&html, "purchase-", untotalable.id);
        assert!(
            row.contains(&expected),
            "the refused row states the rule in the operator's language: {row:.1200}"
        );
        assert!(
            row.contains(&supplier.name),
            "and keeps its identity: {row:.1200}"
        );
        assert!(!row.contains(&big), "and shows NO amount: {row:.1200}");
        assert!(
            !row.contains("0.00"),
            "and no zero placeholder: {row:.1200}"
        );

        let ordinary_amount = localization.format_currency(Decimal::from_str("2468").unwrap());
        for id in ordinary {
            let row = row_html(&html, "purchase-", id);
            assert!(
                row.contains(&ordinary_amount),
                "an ordinary purchase still shows its own total: {row:.1200}"
            );
        }
    }

    /// The ordinary line's money: `qty 2` at `1234`. A whole number on purpose:
    /// these tests run under `es-AR`, whose form parser takes `,` as the decimal
    /// separator.
    const ORDINARY_COST: &str = "1234";

    /// A supplier of its own, so the list test's rows all name the same supplier
    /// and the identity assertions are about the row, not about the fixture.
    async fn seed_supplier(state: &AppState, name: &str) -> crate::models::Supplier {
        use crate::models::NewSupplier;

        state
            .supplier_service
            .create_supplier(
                audit_actor(state).await,
                NewSupplier {
                    name: name.into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap()
    }

    /// One draft purchase from a supplier, with no lines: the fixture every list
    /// test adds its own lines to.
    async fn draft_purchase(state: &AppState, supplier_id: i64) -> crate::models::Purchase {
        use crate::models::NewPurchase;
        use chrono::NaiveDate;

        state
            .purchases_service
            .create_draft(
                audit_actor(state).await,
                NewPurchase {
                    supplier_id,
                    payment_type: PaymentType::Cash,
                    purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // Bidirectional cost entry (cost-with-taxes T-C)
    //
    // The form carries two figures for one cost: the net and the supplier's
    // tax-inclusive gross. Whichever the operator typed into is the input and
    // the other is solved — on create and on the inline edit. The arithmetic is
    // `purchase_cost`'s, which delegates to the shared inverse and to the one
    // tax contract; nothing here computes money.
    //
    // THE ENTRY ROW'S PREVIEW IS GONE with the entry row's money fields
    // (purchase-search-only-entry T3): it had no caller, so the endpoint, its
    // collection twin and `typed_counterpart` went with it, and the mirror that
    // read its answers went with those. What the operator loses is LIVE
    // typing-time feedback; what they keep is both figures, server-computed, on
    // the line — where the PUT's answer IS the record re-read from storage,
    // which is the stronger server-authoritative shape T-C already chose for
    // that row. The three figures on the wire are unchanged, so the create-path
    // tests below post `unit_cost` and `unit_cost_gross` exactly as they always
    // did, and a REST client is none the worse.
    //
    // Every form value below is written with a `.` decimal point, which is what
    // `parse_decimal` reads under this suite's default locale. The one test that
    // switches to a comma-decimal locale says so where it does it.
    // -----------------------------------------------------------------------

    /// A draft with ONE product, a 21% tax linked to it and a supplier, and
    /// nothing on the document yet: the exact state the entry row starts from.
    /// `line_id` is a second line on a SECOND product for the edit tests, so a
    /// create and an edit never compete for the same row.
    struct CostEntry {
        purchase_id: i64,
        product_id: i64,
        edit_product_id: i64,
        edit_line_id: i64,
        supplier_id: i64,
        tax_id: i64,
    }

    /// The 21% rate every figure below is derived against. A whole percentage
    /// on purpose: the staircase gap the refusal test needs is a real one at
    /// this rate, not an artefact of a long decimal.
    const IVA21: &str = "21";

    async fn cost_entry(state: &AppState) -> CostEntry {
        use crate::models::{NewProduct, ProductKind};
        use chrono::NaiveDate;

        let actor = audit_actor(state).await;
        let supplier = seed_supplier(state, "Cost Entry Supplier").await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "COST-ENTRY-A".into(),
                    name: "Cost entry alpha".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec_web("25"),
                    cost_price: dec_web("10"),
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
        let edit_product = state
            .inventory_service
            .create_product(
                actor,
                NewProduct {
                    sku: "COST-ENTRY-B".into(),
                    name: "Cost entry beta".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec_web("25"),
                    cost_price: dec_web("10"),
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
        let tax = state
            .tax_service
            .create_tax(
                actor,
                crate::models::NewTax {
                    code: format!("IVA{IVA21}"),
                    name: "IVA 21".into(),
                    rate: dec_web(IVA21),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        for product_id in [product.id, edit_product.id] {
            state
                .tax_service
                .link_product_tax(actor, product_id, tax.id)
                .await
                .unwrap();
        }
        let purchase = draft_purchase(state, supplier.id).await;
        let line = state
            .purchases_service
            .add_line(
                actor,
                purchase.id,
                edit_product.id,
                dec_web("2"),
                Some(dec_web("5")),
            )
            .await
            .unwrap();
        CostEntry {
            purchase_id: purchase.id,
            product_id: product.id,
            edit_product_id: edit_product.id,
            edit_line_id: line.id,
            supplier_id: supplier.id,
            tax_id: tax.id,
        }
    }

    /// The stored unit cost of the purchase's only line for `product_id`.
    async fn stored_unit_cost(state: &AppState, purchase_id: i64, product_id: i64) -> Decimal {
        state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap()
            .lines
            .into_iter()
            .find(|line| line.product_id == product_id)
            .map(|line| line.unit_cost)
            .unwrap_or_else(|| panic!("no line for product {product_id}"))
    }

    /// The entry-row add, through the real route. `body` carries the form
    /// fields verbatim so a test can post a net, a gross, both or neither.
    async fn add_cost_line(
        app: axum::Router,
        purchase_id: i64,
        body: &str,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/web/purchases/{purchase_id}/lines"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn edit_cost_line(
        app: axum::Router,
        purchase_id: i64,
        line_id: i64,
        body: &str,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/web/purchases/{purchase_id}/lines/{line_id}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The sentence a refusal body carries, read out of the JSON rather than
    /// matched inside it: `{"error": "…"}` is the whole body, so the value IS
    /// the message the notice box will paint.
    fn refusal_sentence(body: &str) -> String {
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|error| panic!("a refusal answers JSON, got {body} ({error})"));
        json["error"]
            .as_str()
            .unwrap_or_else(|| panic!("the refusal body has no error message: {body}"))
            .to_string()
    }

    /// A product of its own, for a rate set that only one refusal test links.
    /// A separate product per case because the tax set belongs to the product —
    /// sharing one would make the four cases interfere.
    async fn refusal_product(state: &AppState, sku: &str) -> i64 {
        state
            .inventory_service
            .create_product(
                audit_actor(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: sku.into(),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec_web("25"),
                    cost_price: dec_web("10"),
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

    /// THE PREVIEW ROUTE IS GONE (search-only entry T3).
    ///
    /// The entry row's money fields were its only caller, and the fields are
    /// gone, so the endpoint went with them. This is worth a test rather than a
    /// grep because the path is not inert: it was a READ that answered a SOLVED
    /// figure about a product's tax set, and leaving it registered would leave
    /// exactly that reachable from a page whose gate hides it.
    ///
    /// The two URL shapes are gone for DIFFERENT reasons and each is pinned to
    /// its own, because a blanket "not 200" would pass on a handler that is
    /// still there and refusing for some unrelated reason:
    ///
    /// * `/{id}/lines/cost` still MATCHES a route — the line route
    ///   `/{purchase_id}/lines/{line_id}`, whose `line_id` reads "cost" fine and
    ///   which serves no GET. So the answer is 405, and the body is empty. That
    ///   is the real proof here: re-registering `get(web_preview_line_cost)`
    ///   turns this into a 200 carrying a solved figure.
    /// * `/lines/cost` matches nothing at all, so it is the router's own
    ///   fallback: 404 with its distinctive body.
    #[tokio::test]
    async fn the_entry_row_cost_preview_route_is_gone() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state);

        let matched_by_the_line_route = format!("/web/purchases/{}/lines/cost", entry.purchase_id);
        let (status, body) = get_html_as(
            app.clone(),
            &matched_by_the_line_route,
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "no handler may serve a GET on the preview path: {matched_by_the_line_route}: {body:.300}"
        );
        assert!(
            body.is_empty(),
            "a 405 carries the preview's answer in no form: {body:.300}"
        );

        let (status, body) = get_html_as(
            app,
            "/web/purchases/lines/cost",
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the collection twin must be a routing miss: {body:.300}"
        );
        assert_eq!(
            refusal_sentence(&body),
            "route not found",
            "and the router's own fallback body, so this cannot pass on a handler that still \
             answers here for some reason of its own: {body:.300}"
        );
    }

    /// The four cost refusals are all reachable from a real form post, and each
    /// one renders through the ONE shared mapping in the operator's language.
    ///
    /// The rate sets two of them need cannot be created through
    /// `TaxService::create_tax`, which is the point of that rule: an operator
    /// cannot link a rate of -100. They are seeded through the REPOSITORY the
    /// write itself reads, so the route is exercised over HTTP with the same
    /// data the service test pins — and the sentences below are then the real
    /// 400 bodies, not a rendering asserted beside the arithmetic.
    #[tokio::test]
    async fn every_cost_refusal_answers_the_shared_sentence_on_the_write() {
        use crate::models::NewTax;
        use crate::repositories::{SqliteTaxRepository, TaxRepository};

        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let actor = audit_actor(&state).await;
        let repository = SqliteTaxRepository::new(state.pool.clone());

        // A set that grosses a net away: the divisor is not positive, so there
        // is no estimate to search around.
        let un_invertible = repository
            .create(
                actor,
                &NewTax {
                    code: "IVA-NEG100".into(),
                    name: "IVA -100".into(),
                    rate: dec_web("-100"),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        // One linked rate enormous enough that pricing any net overflows.
        let huge = repository
            .create(
                actor,
                &NewTax {
                    code: "IVA-HUGE".into(),
                    name: "IVA huge".into(),
                    rate: dec_web("100000000000000000000"),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        // 100 taxes at -0,99999999999 gross a figure down by exactly 1e-11, so
        // this gross divides back to `Decimal::MAX` and the window's endpoint
        // leaves the range. `parse_decimal` refuses scientific notation, so the
        // figure is written out in full — the same constraint an operator has.
        let mut crushing: Vec<crate::models::Tax> = Vec::with_capacity(100);
        for n in 0..100 {
            let tax = repository
                .create(
                    actor,
                    &NewTax {
                        code: format!("IVA-CRUSH-{n}"),
                        name: format!("IVA crush {n}"),
                        rate: dec_web("-0.99999999999"),
                        is_active: true,
                    },
                )
                .await
                .unwrap();
            crushing.push(tax);
        }

        // Four products, four rate sets, four typed gross figures.
        let mut cases: Vec<(i64, i64, &str, crate::models::PriceRefusal)> = Vec::new();
        let gap = entry.product_id; // 21% already linked: 0.03 is the staircase gap
        cases.push((
            gap,
            entry.purchase_id,
            "0.03",
            crate::models::PriceRefusal::CostUnreachable,
        ));

        let negative_product = refusal_product(&state, "COST-REFUSE-NEG").await;
        state
            .tax_service
            .link_product_tax(actor, negative_product, un_invertible.id)
            .await
            .unwrap();
        cases.push((
            negative_product,
            entry.purchase_id,
            "10.00",
            crate::models::PriceRefusal::CostNotInvertible,
        ));

        let huge_product = refusal_product(&state, "COST-REFUSE-HUGE").await;
        // A second rate cancelling it exactly, so the divisor stays positive and
        // the search is REACHED: the overflow is hidden in the sum, not in one
        // product, which is the only shape that reaches this refusal.
        let cancel = repository
            .create(
                actor,
                &NewTax {
                    code: "IVA-HUGE-NEG".into(),
                    name: "IVA huge negative".into(),
                    rate: dec_web("-100000000000000000000"),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        for tax_id in [huge.id, cancel.id] {
            state
                .tax_service
                .link_product_tax(actor, huge_product, tax_id)
                .await
                .unwrap();
        }
        cases.push((
            huge_product,
            entry.purchase_id,
            "10000000000",
            crate::models::PriceRefusal::TaxRateTooLargeToCost,
        ));

        let ceiling_product = refusal_product(&state, "COST-REFUSE-CEIL").await;
        for tax in &crushing {
            state
                .tax_service
                .link_product_tax(actor, ceiling_product, tax.id)
                .await
                .unwrap();
        }
        cases.push((
            ceiling_product,
            entry.purchase_id,
            "792281625142643375.93543950335",
            crate::models::PriceRefusal::CostNetTooLarge,
        ));

        let app = crate::routes::router(state.clone());
        for (product_id, purchase_id, gross, expected) in cases {
            let (status, body) = add_cost_line(
                app.clone(),
                purchase_id,
                &format!(
                    "product_id={product_id}&qty=1&unit_cost=&unit_cost_gross={gross}&cost_basis=gross"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{gross}: {body:.400}");
            assert_eq!(
                refusal_sentence(&body),
                crate::routes::price_refusal_message(
                    &expected,
                    &crate::localization::LocalizationContext::fallback()
                ),
                "{gross} must answer the shared sentence for {expected:?}"
            );
            assert!(
                !state
                    .purchases_service
                    .get_detail(purchase_id)
                    .await
                    .unwrap()
                    .lines
                    .iter()
                    .any(|line| line.product_id == product_id),
                "{gross} stored a line for a refused cost"
            );
        }
    }

    /// The same sentence in the operator's OWN language, and byte-identical to
    /// the model's English row under the English default.
    ///
    /// Both halves are the requirement: a refusal that renders through a
    /// surface-local wording is a second sentence about one rule, and a
    /// refusal whose English row drifts from `as_str` is an API body that
    /// changed under a client. The switch to `es-AR` is what makes the first
    /// half observable — under the English default both would be the same
    /// string and the test would prove nothing about localization.
    #[tokio::test]
    async fn a_cost_refusal_renders_in_the_active_locale() {
        use crate::models::PriceRefusal;
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, body) = add_cost_line(
            app.clone(),
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=&unit_cost_gross=0.03&cost_basis=gross",
                entry.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:.400}");
        assert_eq!(
            refusal_sentence(&body),
            PriceRefusal::CostUnreachable.as_str(),
            "the English body is the model's own sentence, byte for byte"
        );

        // The locale switch moves the DECIMAL SEPARATOR with it, so the same
        // figure is now written `0,03` — the same constraint an operator in that
        // locale has, and the reason the form parser is locale-aware at all.
        set_locale(&state, "es-AR", "es").await;
        let (status, body) = add_cost_line(
            app,
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=&unit_cost_gross=0%2C03&cost_basis=gross",
                entry.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:.400}");
        assert_eq!(
            refusal_sentence(&body),
            "Ningún costo neto lleva a este costo con los impuestos vinculados.",
            "the same refusal in the operator's language, and a different string from the English \
             one — a surface-local wording could not produce this"
        );
    }

    /// A GROSS on create still goes through `resolve_line_cost`, so the satellite
    /// it would otherwise have used is never reached, and the solved net is
    /// stored instead.
    ///
    /// The fixture is a supplier whose satellite cost is 9,50 — a real, different
    /// figure. A gross path that bypassed the resolution would either store
    /// 9,50 (the fallback) or skip the negative guard; this test can only pass if
    /// the solved net is handed to the SAME resolution the typed net is.
    #[tokio::test]
    async fn a_gross_typed_on_create_reaches_the_same_cost_resolution_a_net_does() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        state
            .supplier_service
            .record_cost(
                audit_actor(&state).await,
                entry.product_id,
                entry.supplier_id,
                dec_web("9.50"),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Neither field typed: the satellite, exactly as before this pair existed.
        let (status, body) = add_cost_line(
            app.clone(),
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=&unit_cost_gross=",
                entry.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.product_id).await,
            dec_web("9.50"),
            "an empty pair keeps the supplier satellite"
        );

        // And the gross on a second product, with the same satellite, must NOT
        // reach it: the solved net is the figure, and it goes through the same
        // resolution.
        let other = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                crate::models::NewProduct {
                    sku: "COST-ENTRY-C".into(),
                    name: "Cost entry gamma".into(),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec_web("25"),
                    cost_price: dec_web("10"),
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
            .tax_service
            .link_product_tax(audit_actor(&state).await, other.id, entry.tax_id)
            .await
            .unwrap();
        state
            .supplier_service
            .record_cost(
                audit_actor(&state).await,
                other.id,
                entry.supplier_id,
                dec_web("9.50"),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = add_cost_line(
            app,
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=&unit_cost_gross=6.05&cost_basis=gross",
                other.id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, other.id).await,
            dec_web("5.00"),
            "the solved net, not the satellite: a gross path that bypassed resolve_line_cost \
             would have stored 9.50"
        );
    }

    /// THE MERGE PROOF. The same product, added twice — once by gross and once
    /// by net — must land on ONE line, because the two figures describe the same
    /// cost and the merge path compares the SOLVED decimals.
    ///
    /// This is the test that pins the exactness of the inverse where it actually
    /// has an observable consequence. `add_or_increment_line` compares
    /// `existing.unit_cost == cost`; a gross-typed 6,05 and a net-typed 5,00
    /// under 21% are only the same number if the inverse is exact. An inexact
    /// one would answer the duplicate-product 400 instead, and the operator
    /// would be told to use the update endpoint for two identical prices.
    #[tokio::test]
    async fn the_same_product_added_by_gross_and_by_net_merges_into_one_line() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());
        let (status, body) = add_cost_line(
            app.clone(),
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=&unit_cost_gross=6.05&cost_basis=gross",
                entry.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");

        let (status, body) = add_cost_line(
            app,
            entry.purchase_id,
            &format!(
                "product_id={}&qty=1&unit_cost=5.00&unit_cost_gross=&cost_basis=net",
                entry.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");

        let lines: Vec<_> = state
            .purchases_service
            .get_detail(entry.purchase_id)
            .await
            .unwrap()
            .lines
            .into_iter()
            .filter(|line| line.product_id == entry.product_id)
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "the two descriptions of one cost must merge, not collide: {lines:?}"
        );
        assert_eq!(lines[0].unit_cost, dec_web("5.00"));
        assert_eq!(lines[0].qty, dec_web("2"), "and the quantities are summed");
    }

    /// A GROSS typed into the inline edit stores the solved net, and the answer
    /// re-renders BOTH inputs from storage: the server is this path's mirror, so
    /// the operator sees the stored pair rather than a value the page guessed.
    #[tokio::test]
    async fn a_gross_typed_into_an_inline_edit_stores_the_solved_net() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, body) = edit_cost_line(
            app,
            entry.purchase_id,
            entry.edit_line_id,
            "qty=3&unit_cost=&unit_cost_gross=6.05&cost_basis=gross",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.edit_product_id).await,
            dec_web("5.00")
        );
        assert!(
            body.contains(&format!("id=\"line-cost-{}\"", entry.edit_line_id)),
            "the answer re-renders the row's net field: {body:.600}"
        );
    }

    /// The inline edit's own asymmetry, unchanged: with NEITHER figure the edit
    /// is still refused, because the net is required on this path. The gross
    /// only takes over when it can be solved, and "cannot" includes "not typed".
    #[tokio::test]
    async fn an_inline_edit_with_neither_cost_is_still_refused() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, body) = edit_cost_line(
            app,
            entry.purchase_id,
            entry.edit_line_id,
            "qty=3&unit_cost=&unit_cost_gross=&cost_basis=",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:.400}");
        assert_eq!(refusal_sentence(&body), "invalid unit_cost");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.edit_product_id).await,
            dec_web("5.00"),
            "a refused edit stores nothing"
        );
    }

    /// THE BASIS, where it actually decides: BOTH figures on the form.
    ///
    /// This is the inline row's real shape. The net field is prefilled from
    /// storage and the gross field beside it is where the supplier's figure
    /// goes, so an edit to the gross arrives with a net already there — and a
    /// rule that ignored the basis would store the untouched net every time, so
    /// typing a gross would silently do nothing.
    ///
    /// It is also the property the no-JavaScript fallback rests on from the other
    /// side: with no basis stated, the SAME pair stores the net. One rule, two
    /// answers, and the only thing that decides between them is a field the page
    /// sets.
    #[tokio::test]
    async fn the_stated_basis_decides_when_both_figures_are_on_the_form() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());

        // The gross is what the operator typed last: the net solves to 5.00 and
        // the typed 4.00 is ignored.
        let (status, body) = edit_cost_line(
            app.clone(),
            entry.purchase_id,
            entry.edit_line_id,
            "qty=2&unit_cost=4.00&unit_cost_gross=6.05&cost_basis=gross",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.edit_product_id).await,
            dec_web("5.00"),
            "the stated basis is the gross, so 6.05 at 21% decides the stored net"
        );

        // The same pair with the net stated: the net is the input, and the gross
        // beside it cannot refuse it.
        let (status, body) = edit_cost_line(
            app.clone(),
            entry.purchase_id,
            entry.edit_line_id,
            "qty=2&unit_cost=4.00&unit_cost_gross=6.05&cost_basis=net",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.edit_product_id).await,
            dec_web("4.00")
        );

        // And with NO basis — a post from a browser with no JavaScript — the net
        // wins, which is what this form did before the pair existed.
        let (status, body) = edit_cost_line(
            app,
            entry.purchase_id,
            entry.edit_line_id,
            "qty=2&unit_cost=4.00&unit_cost_gross=6.05&cost_basis=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
        assert_eq!(
            stored_unit_cost(&state, entry.purchase_id, entry.edit_product_id).await,
            dec_web("4.00")
        );
    }

    /// A refused GROSS edit stores nothing, and the body is the localized
    /// sentence — the same one the preview paints — so the operator is told the
    /// same thing whichever side of the pair they were on.
    #[tokio::test]
    async fn a_refused_gross_edit_stores_nothing_and_answers_the_shared_sentence() {
        let state = test_state().await;
        let entry = cost_entry(&state).await;
        let app = crate::routes::router(state.clone());

        let (status, body) = edit_cost_line(
            app,
            entry.purchase_id,
            entry.edit_line_id,
            "qty=3&unit_cost=&unit_cost_gross=0.03&cost_basis=gross",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:.400}");
        assert_eq!(
            refusal_sentence(&body),
            crate::models::PriceRefusal::CostUnreachable.as_str()
        );
        let detail = state
            .purchases_service
            .get_detail(entry.purchase_id)
            .await
            .unwrap();
        let line = detail
            .lines
            .iter()
            .find(|line| line.id == entry.edit_line_id)
            .unwrap();
        assert_eq!(
            line.unit_cost,
            dec_web("5.00"),
            "the stored cost is untouched"
        );
        assert_eq!(line.qty, dec_web("2"), "and so is the quantity");
    }

    // -- Return goods: the action that starts a purchase return ---------------

    /// **THE TEST THAT WOULD HAVE CAUGHT THE DEAD CREATION FLOW.**
    ///
    /// A purchase return reverses a NAMED document, so its creation post carries
    /// that document's id — and there is no picker anywhere that could fill one
    /// later. The only place the operator is already holding the right document
    /// is its own record page, so the action lives there and the id is rendered
    /// onto it, filled in. A form that renders with an empty id is a form that
    /// cannot work, and the suite said nothing about it because no assertion ever
    /// looked for the id.
    #[tokio::test]
    async fn a_confirmed_purchase_record_offers_return_goods_with_this_purchase_id_already_filled()
    {
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
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        let form = enclosing_form(&html, "/web/purchase-returns");
        assert!(
            form.contains(&format!("value=\"{}\"", fixture.purchase_id)),
            "the action must carry THIS purchase's id, filled in: a return names the \
             document it reverses and nothing else can supply that id: {form:.600}"
        );
        assert!(
            form.contains("Return goods"),
            "the action states in the operator's language what it does: {form:.600}"
        );
    }

    /// The other half of the same gate: the action is `purchases.create`, the
    /// parent's own write code — the one `POST /web/purchase-returns` declares.
    /// A principal that may read a purchase and may not create one must see no
    /// action (an entry the route would refuse is worse than none) and must be
    /// refused the post itself.
    #[tokio::test]
    async fn return_goods_is_gated_on_purchases_create_for_both_sight_and_post() {
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
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        let (status, html) = get_html_as(
            app.clone(),
            &format!("/purchases/{}", fixture.purchase_id),
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("/web/purchase-returns"),
            "a principal without purchases.create must not be shown an action the \
             route refuses: {html:.600}"
        );

        let (status, html) = post_form_as(
            app,
            "/web/purchase-returns",
            &format!("purchase_id={}", fixture.purchase_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.300}");
        assert!(
            html.contains("purchases.create"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// One row of a rendered list, sliced out by its row id.
    fn row_html<'a>(html: &'a str, id_prefix: &str, id: i64) -> &'a str {
        let marker = format!("id=\"{id_prefix}{id}\"");
        let start = html
            .find(&marker)
            .unwrap_or_else(|| panic!("row {id_prefix}{id} is missing from the page"))
            + marker.len();
        let rest = &html[start..];
        let end = rest
            .find(&format!("id=\"{id_prefix}"))
            .unwrap_or(rest.len());
        &rest[..end]
    }
}
