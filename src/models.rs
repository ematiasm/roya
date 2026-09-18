use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum TransactionKind {
    Income,
    Expense,
}

impl std::fmt::Display for TransactionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Income => write!(f, "Income"),
            Self::Expense => write!(f, "Expense"),
        }
    }
}

impl std::str::FromStr for TransactionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "income" => Ok(Self::Income),
            "expense" => Ok(Self::Expense),
            _ => Err(format!("invalid transaction kind: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// DB entities (what sqlx reads)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Account {
    pub id: i64,
    pub name: String,
    /// Stored as TEXT in SQLite, mapped via rust_decimal db-sqlx feature
    pub cached_balance: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Transaction {
    pub id: i64,
    pub account_id: i64,
    pub kind: TransactionKind,
    pub amount: Decimal,
    pub description: String,
    /// Opaque source reference (document number). NULL for manual transactions.
    pub reference: Option<String>,
    pub date: NaiveDate,
    pub created_at: chrono::NaiveDateTime,
}

impl Transaction {
    pub fn is_income(&self) -> bool {
        self.kind == TransactionKind::Income
    }
    pub fn is_expense(&self) -> bool {
        self.kind == TransactionKind::Expense
    }
}

impl AccountWithBalance {
    pub fn is_negative(&self) -> bool {
        self.balance.is_sign_negative()
    }
}

// ---------------------------------------------------------------------------
// DTOs / API payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateTransactionRequest {
    pub account_id: i64,
    #[serde(rename = "type")]
    pub kind: TransactionKind,
    pub amount: Decimal,
    pub description: Option<String>,
    /// Optional opaque source reference; manual transactions omit it.
    pub reference: Option<String>,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTransactionRequest {
    #[serde(rename = "type")]
    pub kind: Option<TransactionKind>,
    pub amount: Option<Decimal>,
    pub description: Option<String>,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Deserialize)]
pub struct TransactionFilter {
    pub account_id: Option<i64>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

// API responses --------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AccountWithBalance {
    pub id: i64,
    pub name: String,
    pub balance: Decimal,
    pub cached_balance: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Serialize)]
pub struct AccountDetail {
    pub id: i64,
    pub name: String,
    pub balance: Decimal,
    pub created_at: chrono::NaiveDateTime,
    pub transactions: Vec<Transaction>,
}

#[derive(Debug, Serialize)]
pub struct DashboardData {
    pub total_balance: Decimal,
    pub accounts: Vec<AccountWithBalance>,
}

// ---------------------------------------------------------------------------
// M1 inventory domain (DB entities + inputs + views, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum ProductKind {
    Product,
    Service,
}

impl std::fmt::Display for ProductKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Product => write!(f, "Product"),
            Self::Service => write!(f, "Service"),
        }
    }
}

impl std::str::FromStr for ProductKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "product" => Ok(Self::Product),
            "service" => Ok(Self::Service),
            _ => Err(format!("invalid product kind: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum MovementType {
    In,
    Out,
    Adjust,
}

impl std::fmt::Display for MovementType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::In => write!(f, "In"),
            Self::Out => write!(f, "Out"),
            Self::Adjust => write!(f, "Adjust"),
        }
    }
}

impl std::str::FromStr for MovementType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            "adjust" => Ok(Self::Adjust),
            _ => Err(format!("invalid movement type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum MovementReason {
    Purchase,
    Sale,
    #[serde(rename = "Sale-return", alias = "SaleReturn", alias = "sale_return", alias = "salereturn")]
    #[sqlx(rename = "Sale-return")]
    SaleReturn,
    #[serde(rename = "Purchase-return", alias = "PurchaseReturn", alias = "purchase_return", alias = "purchasereturn")]
    #[sqlx(rename = "Purchase-return")]
    PurchaseReturn,
    Loss,
    Adjust,
    Initial,
}

impl std::fmt::Display for MovementReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Purchase => write!(f, "Purchase"),
            Self::Sale => write!(f, "Sale"),
            Self::SaleReturn => write!(f, "Sale-return"),
            Self::PurchaseReturn => write!(f, "Purchase-return"),
            Self::Loss => write!(f, "Loss"),
            Self::Adjust => write!(f, "Adjust"),
            Self::Initial => write!(f, "Initial"),
        }
    }
}

impl std::str::FromStr for MovementReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "purchase" => Ok(Self::Purchase),
            "sale" => Ok(Self::Sale),
            "sale-return" | "sale_return" | "salereturn" | "sale return" => {
                Ok(Self::SaleReturn)
            }
            "purchase-return" | "purchase_return" | "purchasereturn" | "purchase return" => {
                Ok(Self::PurchaseReturn)
            }
            "loss" => Ok(Self::Loss),
            "adjust" => Ok(Self::Adjust),
            "initial" => Ok(Self::Initial),
            _ => Err(format!("invalid movement reason: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: i64,
    pub sku: String,
    pub name: String,
    pub kind: ProductKind,
    pub category_id: Option<i64>,
    pub unit: String,
    pub sale_price: Decimal,
    pub cost_price: Decimal,
    pub track_stock: bool,
    pub min_stock: Option<Decimal>,
    pub max_stock: Option<Decimal>,
    pub location: Option<String>,
    pub notes: Option<String>,
    pub is_active: bool,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl Product {
    /// Template helper for the drawer's category select: `true` when this product
    /// belongs to the given category. Askama binds `Some`/match arms by reference
    /// and cannot deref or build `Some(...)` in expressions, so the comparison
    /// lives here instead of in the template.
    pub fn category_is(&self, id: &i64) -> bool {
        self.category_id == Some(*id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductBarcode {
    pub id: i64,
    pub product_id: i64,
    pub code: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StockMovement {
    pub id: i64,
    pub product_id: i64,
    /// Stored magnitude as TEXT; for `Adjust` it may already be signed.
    pub qty: Decimal,
    pub movement_type: MovementType,
    pub reason: MovementReason,
    pub reference: String,
    pub date: NaiveDate,
    pub created_at: chrono::NaiveDateTime,
}

impl StockMovement {
    /// Signed contribution of this movement to derived stock.
    pub fn signed_qty(&self) -> Decimal {
        match self.movement_type {
            MovementType::In => self.qty,
            MovementType::Out => -self.qty,
            MovementType::Adjust => self.qty,
        }
    }
}

/// Service-level input for product creation (mirrors finance request DTOs).
#[derive(Debug, Clone)]
pub struct NewProduct {
    pub sku: String,
    pub name: String,
    pub kind: ProductKind,
    pub category_id: Option<i64>,
    pub unit: String,
    pub sale_price: Decimal,
    pub cost_price: Decimal,
    pub track_stock: bool,
    pub min_stock: Option<Decimal>,
    pub max_stock: Option<Decimal>,
    pub location: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for product edits. `None` means "leave unchanged"; the web
/// form always sends every field, so it builds a full patch from the form.
/// `Option<Option<T>>` fields distinguish "leave unchanged" (`None`) from
/// "clear" (`Some(None)`), like `UpdateSupplier` / `UpdateCustomer`.
#[derive(Debug, Clone, Default)]
pub struct UpdateProduct {
    pub sku: Option<String>,
    pub name: Option<String>,
    pub kind: Option<ProductKind>,
    pub category_id: Option<Option<i64>>,
    pub unit: Option<String>,
    pub sale_price: Option<Decimal>,
    pub cost_price: Option<Decimal>,
    pub track_stock: Option<bool>,
    pub min_stock: Option<Option<Decimal>>,
    pub max_stock: Option<Option<Decimal>>,
    pub location: Option<Option<String>>,
    pub notes: Option<Option<String>>,
}

/// Service-level input for stock movements.
#[derive(Debug, Clone)]
pub struct NewMovement {
    pub product_id: i64,
    pub qty: Decimal,
    pub movement_type: MovementType,
    pub reason: MovementReason,
    pub reference: String,
    pub date: NaiveDate,
}

/// Derived stock view (never stored as source of truth).
#[derive(Debug, Clone, Serialize)]
pub struct ProductStock {
    pub product: Product,
    pub stock: Decimal,
    /// `max_stock - stock` when `stock <= min_stock`, else `None`.
    pub suggested: Option<Decimal>,
}

// ---------------------------------------------------------------------------
// M2 sales domain (orchestrator, Odoo-style). Decimal-as-TEXT like finance.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum SaleStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for SaleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for SaleStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid sale status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PaymentType {
    Cash,
    Credit,
}

impl std::fmt::Display for PaymentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cash => write!(f, "Cash"),
            Self::Credit => write!(f, "Credit"),
        }
    }
}

impl std::str::FromStr for PaymentType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cash" => Ok(Self::Cash),
            "credit" => Ok(Self::Credit),
            _ => Err(format!("invalid payment type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PaymentStatus {
    Paid,
    Partial,
    Unpaid,
}

impl std::fmt::Display for PaymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paid => write!(f, "Paid"),
            Self::Partial => write!(f, "Partial"),
            Self::Unpaid => write!(f, "Unpaid"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sale {
    pub id: i64,
    pub sale_number: Option<String>,
    pub status: SaleStatus,
    pub payment_type: PaymentType,
    /// The owning customer; the seeded walk-in for anonymous cash sales.
    pub customer_id: i64,
    /// Frozen snapshot of the customer's name at creation time, so correcting the
    /// customer never rewrites history.
    pub customer_name: String,
    pub sale_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub receipt_no: Option<String>,
    pub notes: String,
    pub cancel_reason: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaleLine {
    pub id: i64,
    pub sale_id: i64,
    pub product_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_price >= 0, frozen at confirm, stored as TEXT.
    pub unit_price: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl SaleLine {
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_price
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SalePayment {
    pub id: i64,
    pub sale_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this payment created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when the sale was cancelled, if any.
    pub refund_transaction_id: Option<i64>,
    /// Customer receipt that groups this payment, when a lump-sum collection
    /// produced it; NULL for a direct payment on a single sale.
    pub receipt_id: Option<i64>,
    /// The owning sale's document number, resolved by the receipt-allocation read
    /// so a receipt names the sale the way the user does; `None` in other reads.
    pub sale_number: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

// ---------------------------------------------------------------------------
// M0 payment methods (finance-owned, account-owned 1:N). Seeded Cash/Transfer/
// Debit/CreditCard/QR, no Other. Each method belongs to at most one account
// (`account_id`, NULL = unassigned and unusable); UNIQUE(account_id, name) lets
// two accounts each own a same-named method as separate rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMethod {
    pub id: i64,
    pub name: String,
    /// The owning account; NULL means unassigned and unusable for payments.
    pub account_id: Option<i64>,
    pub is_active: bool,
    pub created_at: chrono::NaiveDateTime,
}

/// One method with its owning account resolved for display, so method-only
/// selects render `"Name — AccountName"` without SQL in a route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMethodWithAccount {
    pub id: i64,
    pub name: String,
    pub account_id: Option<i64>,
    pub account_name: Option<String>,
    pub is_active: bool,
}

impl PaymentMethodWithAccount {
    /// Owning account name, or `unassigned` for methods no account owns yet.
    pub fn account_label(&self) -> String {
        self.account_name
            .clone()
            .unwrap_or_else(|| "unassigned".to_string())
    }

    /// Select label: `"Transfer — Bank"`.
    pub fn label(&self) -> String {
        format!("{} — {}", self.name, self.account_label())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSequence {
    pub doc_type: String,
    pub year: i32,
    pub last_number: i64,
}

/// Service-level input for sale creation (Draft). The service resolves the
/// customer through `CustomerService` and freezes `customer_name` from it.
#[derive(Debug, Clone)]
pub struct NewSale {
    pub customer_id: i64,
    pub payment_type: PaymentType,
    pub sale_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub receipt_no: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for Draft header edits. The customer (and therefore the
/// name snapshot) is fixed at creation; only dates, receipt and notes are edited.
#[derive(Debug, Clone, Default)]
pub struct UpdateSaleDraft {
    pub sale_date: Option<NaiveDate>,
    pub due_date: Option<Option<NaiveDate>>,
    pub receipt_no: Option<Option<String>>,
    pub notes: Option<String>,
}

/// Aggregated sale view with derived totals (never stored as truth).
#[derive(Debug, Clone, Serialize)]
pub struct SaleDetail {
    pub sale: Sale,
    pub lines: Vec<SaleLine>,
    pub payments: Vec<SalePayment>,
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

impl SaleDetail {
    pub fn payment_status_for(total: Decimal, paid: Decimal) -> PaymentStatus {
        let due = total - paid;
        if due <= Decimal::ZERO {
            PaymentStatus::Paid
        } else if paid > Decimal::ZERO {
            PaymentStatus::Partial
        } else {
            PaymentStatus::Unpaid
        }
    }
}

/// Format `YYYY-SALE-NNNNNN` with zero-padded 6-digit sequence.
pub fn format_sale_number(year: i32, seq: i64) -> String {
    format!("{year}-SALE-{seq:06}")
}


/// Fold a search string to its comparable ASCII form: Unicode lowercase plus the
/// Spanish and Latin-1 diacritics mapped to their base letters. Both sides of every
/// party and catalogue match go through this one function, so `Perez` finds
/// `Pérez`, `CAFE` finds `Café` and `Ñandú` finds `ñandú`.
pub fn normalize_search(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars().flat_map(char::to_lowercase) {
        match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => out.push('a'),
            'æ' => out.push_str("ae"),
            'ç' | 'ć' | 'č' => out.push('c'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' => out.push('i'),
            'ð' => out.push('d'),
            'ñ' | 'ń' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => out.push('o'),
            'œ' => out.push_str("oe"),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'þ' => out.push_str("th"),
            'ß' => out.push_str("ss"),
            other => out.push(other),
        }
    }
    out
}

/// Server-side filter for the sales list (redesign-interface N5). Every field is
/// optional and an absent field adds no constraint, so an empty filter returns the
/// whole list and a filter matching nothing returns an empty list rather than an
/// error. `customer` is the typed party name; the service resolves it against the
/// customers table (normalized) into `customer_ids`, and the repository narrows the
/// document query by those ids. `number` matches the document number partially.
#[derive(Debug, Clone, Default)]
pub struct SaleListFilter {
    pub status: Option<SaleStatus>,
    /// The typed party name, resolved by the service into `customer_ids`.
    pub customer: Option<String>,
    /// Matching customer ids, set by the service; `Some(empty)` matches nothing.
    pub customer_ids: Option<Vec<i64>>,
    pub number: Option<String>,
    /// Inclusive lower bound on `sale_date`.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on `sale_date`.
    pub to: Option<NaiveDate>,
}


// ---------------------------------------------------------------------------
// Sale record page (redesign-interface N2)
//
// The persisted line only carries `product_id` and the payment only carries
// `account_id`/`method_id`. These views carry the display names the record page
// shows, resolved by the service through the existing inventory and finance
// read paths, never by SQL in a route.
// ---------------------------------------------------------------------------

/// One sale line resolved for `/sales/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct SaleLineView {
    pub id: i64,
    pub product_name: String,
    pub product_sku: String,
    pub qty: Decimal,
    pub unit_price: Decimal,
    pub subtotal: Decimal,
}

/// One sale payment resolved for `/sales/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct SalePaymentView {
    pub id: i64,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
    pub date: NaiveDate,
}

/// The sale record page payload: the stored document plus every child with its
/// internal keys replaced by display names. Totals stay derived.
#[derive(Debug, Clone, Serialize)]
pub struct SaleRecord {
    pub sale: Sale,
    pub lines: Vec<SaleLineView>,
    pub payments: Vec<SalePaymentView>,
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

/// The sales page's debt banner: a summary, not the full receivable. `total` and
/// `count` are exact (decimal sums in Rust) and `oldest` is the first few unpaid
/// documents by due date, so the banner renders a bounded number of rows. The full
/// receivable list stays a filtered read, never an always-rendered panel.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DebtSummary {
    pub total: Decimal,
    pub count: usize,
    pub oldest: Vec<SaleDetail>,
}

// ---------------------------------------------------------------------------
// M3 purchases: suppliers + product/supplier cost satellite (Slice E).
// Decimal-as-TEXT like finance/inventory. The price alert is derived from
// previous vs current, never stored; `products.cost_price` stays as the
// fallback for products without satellite rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Supplier {
    pub id: i64,
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
    pub is_active: bool,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSupplierCost {
    pub id: i64,
    pub product_id: i64,
    pub supplier_id: i64,
    /// Decimal >= 0, stored as TEXT.
    pub current_cost: Decimal,
    pub current_cost_updated_at: NaiveDate,
    /// Decimal >= 0 or NULL when there is no older recorded price.
    pub previous_cost: Option<Decimal>,
    pub previous_cost_updated_at: Option<NaiveDate>,
    pub is_preferred: bool,
    /// The supplier's own code for this product, stored as TEXT.
    pub supplier_sku: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

impl ProductSupplierCost {
    /// Derived alert: compare the recorded previous price against the current one.
    pub fn price_alert(&self) -> PriceAlert {
        PriceAlert::compare(self.previous_cost, self.current_cost)
    }
}

/// Derived price movement between the previous and current satellite costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PriceAlert {
    Raised,
    Lowered,
    Unchanged,
}

impl PriceAlert {
    /// `None` previous means no movement to compare yet => Unchanged.
    pub fn compare(previous: Option<Decimal>, current: Decimal) -> Self {
        match previous {
            Some(p) if current > p => Self::Raised,
            Some(p) if current < p => Self::Lowered,
            _ => Self::Unchanged,
        }
    }
}

impl std::fmt::Display for PriceAlert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raised => write!(f, "Raised"),
            Self::Lowered => write!(f, "Lowered"),
            Self::Unchanged => write!(f, "Unchanged"),
        }
    }
}

/// Service-level input for supplier creation.
#[derive(Debug, Clone, Deserialize)]
pub struct NewSupplier {
    pub name: String,
    pub phone: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for supplier edits. `Option<Option<T>>` distinguishes
/// "leave unchanged" (`None`) from "clear" (`Some(None)`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSupplier {
    pub name: Option<String>,
    pub phone: Option<Option<String>>,
    pub notes: Option<Option<String>>,
}

// ---------------------------------------------------------------------------
// M3 purchases domain (mirror orchestrator of M2 sales). Decimal-as-TEXT like
// finance/inventory. The purchase Draft is the pedido: it touches no stock, no
// finance and no satellite cost until confirmed.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
#[sqlx(rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum PurchaseStatus {
    Draft,
    Confirmed,
    Cancelled,
}

impl std::fmt::Display for PurchaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "Draft"),
            Self::Confirmed => write!(f, "Confirmed"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl std::str::FromStr for PurchaseStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "confirmed" => Ok(Self::Confirmed),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid purchase status: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Purchase {
    pub id: i64,
    /// `YYYY-PURCH-NNNNNN`, NULL only while Draft, immutable once assigned.
    pub purchase_number: Option<String>,
    pub supplier_id: i64,
    pub status: PurchaseStatus,
    pub payment_type: PaymentType,
    pub purchase_date: NaiveDate,
    /// Required when `payment_type` is Credit, NULL for Cash.
    pub due_date: Option<NaiveDate>,
    pub supplier_invoice_no: Option<String>,
    pub notes: String,
    pub cancel_reason: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub confirmed_at: Option<chrono::NaiveDateTime>,
    pub cancelled_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseLine {
    pub id: i64,
    pub purchase_id: i64,
    pub product_id: i64,
    /// Decimal qty > 0, stored as TEXT.
    pub qty: Decimal,
    /// Decimal unit_cost >= 0, frozen at confirm, stored as TEXT.
    pub unit_cost: Decimal,
    pub created_at: chrono::NaiveDateTime,
}

impl PurchaseLine {
    pub fn subtotal(&self) -> Decimal {
        self.qty * self.unit_cost
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchasePayment {
    pub id: i64,
    pub purchase_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    /// Decimal amount > 0, stored as TEXT.
    pub amount: Decimal,
    pub date: NaiveDate,
    /// Finance transaction this payment created (NULL for historical rows).
    pub transaction_id: Option<i64>,
    /// Refund transaction created when the purchase was cancelled, if any.
    pub refund_transaction_id: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
}

/// Service-level input for purchase creation (Draft).
#[derive(Debug, Clone)]
pub struct NewPurchase {
    pub supplier_id: i64,
    pub payment_type: PaymentType,
    pub purchase_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub supplier_invoice_no: Option<String>,
    pub notes: Option<String>,
}

/// Service-level patch for Draft header edits.
#[derive(Debug, Clone, Default)]
pub struct UpdatePurchaseDraft {
    pub supplier_id: Option<i64>,
    pub payment_type: Option<PaymentType>,
    pub purchase_date: Option<NaiveDate>,
    pub due_date: Option<Option<NaiveDate>>,
    pub supplier_invoice_no: Option<Option<String>>,
    pub notes: Option<String>,
}

/// Aggregated purchase view with derived totals (never stored as truth).
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseDetail {
    pub purchase: Purchase,
    pub lines: Vec<PurchaseLine>,
    pub payments: Vec<PurchasePayment>,
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

impl PurchaseDetail {
    pub fn payment_status_for(total: Decimal, paid: Decimal) -> PaymentStatus {
        SaleDetail::payment_status_for(total, paid)
    }
}

/// One purchase line resolved for `/purchases/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseLineView {
    pub id: i64,
    pub product_name: String,
    pub product_sku: String,
    pub qty: Decimal,
    pub unit_cost: Decimal,
    pub subtotal: Decimal,
}

/// One purchase payment resolved for `/purchases/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct PurchasePaymentView {
    pub id: i64,
    pub account_name: String,
    pub method_name: String,
    pub amount: Decimal,
    pub date: NaiveDate,
}

/// The purchase record page payload: the stored document plus every child with
/// its internal keys replaced by display names (`products.cost_price` stays the
/// fallback the service already applies for an empty line cost). Totals stay
/// derived.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseRecord {
    pub purchase: Purchase,
    /// Purchases store only the supplier id; the name is resolved for display.
    pub supplier_name: String,
    pub lines: Vec<PurchaseLineView>,
    pub payments: Vec<PurchasePaymentView>,
    pub total: Decimal,
    pub paid: Decimal,
    pub due: Decimal,
    pub payment_status: PaymentStatus,
}

/// Format `YYYY-PURCH-NNNNNN` with zero-padded 6-digit sequence.
pub fn format_purchase_number(year: i32, seq: i64) -> String {
    format!("{year}-PURCH-{seq:06}")
}

/// Server-side filter for the purchases list (redesign-interface N5). The same
/// shape as `SaleListFilter`; `supplier` is the typed party name, resolved by the
/// service against the suppliers table (normalized) into `supplier_ids`, and the
/// repository narrows the document query by those ids. `number` matches partially.
#[derive(Debug, Clone, Default)]
pub struct PurchaseListFilter {
    pub status: Option<PurchaseStatus>,
    /// The typed party name, resolved by the service into `supplier_ids`.
    pub supplier: Option<String>,
    /// Matching supplier ids, set by the service; `Some(empty)` matches nothing.
    pub supplier_ids: Option<Vec<i64>>,
    pub number: Option<String>,
    /// Inclusive lower bound on `purchase_date`.
    pub from: Option<NaiveDate>,
    /// Inclusive upper bound on `purchase_date`.
    pub to: Option<NaiveDate>,
}


/// One low-stock product with a chosen supplier from the cost satellite.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseSuggestion {
    pub product: Product,
    pub stock: Decimal,
    pub suggested_qty: Decimal,
    pub supplier_id: i64,
    pub supplier_name: String,
    pub unit_cost: Decimal,
    pub subtotal: Decimal,
}

/// Low-stock product with no satellite row: never silently dropped, returned
/// in the `without_supplier` list so the user can pick a one-off supplier.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseSuggestionWithoutSupplier {
    pub product: Product,
    pub stock: Decimal,
    pub suggested_qty: Decimal,
}

/// The pedido suggestion: costed low-stock lines plus the unsourced ones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PurchaseSuggestions {
    pub suggestions: Vec<PurchaseSuggestion>,
    pub without_supplier: Vec<PurchaseSuggestionWithoutSupplier>,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice K1). Customer CRUD only: the sales link and the derived
// balance/ageing arrive in a later slice. Decimal-as-TEXT like the rest of the
// project. A name is not unique on purpose; duplicates are reported as a
// warning instead of blocking. is_walkin marks the single seeded cash default
// ("Consumidor final"), which can never be deleted or deactivated.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Customer {
    pub id: i64,
    pub name: String,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub tax_id: Option<String>,
    pub notes: Option<String>,
    /// The seeded cash default. Exactly one row has this set.
    pub is_walkin: bool,
    pub is_active: bool,
    /// Decimal >= 0 stored as TEXT; NULL means no limit.
    pub credit_limit: Option<Decimal>,
    /// Default credit term in days; NULL means no default term.
    pub payment_days: Option<i64>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Service-level input for customer creation. `is_walkin` is accepted only when
/// no walk-in exists yet, which after the seed means never.
#[derive(Debug, Clone, Deserialize)]
pub struct NewCustomer {
    pub name: String,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub tax_id: Option<String>,
    pub notes: Option<String>,
    #[serde(default)]
    pub is_walkin: bool,
    /// None means no limit.
    pub credit_limit: Option<Decimal>,
    /// None means no default term.
    pub payment_days: Option<i64>,
}

/// Service-level patch for customer edits. `Option<Option<T>>` distinguishes
/// "leave unchanged" (`None`) from "clear" (`Some(None)`). `is_walkin` is not
/// editable.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateCustomer {
    pub name: Option<String>,
    pub phone: Option<Option<String>>,
    pub address: Option<Option<String>>,
    pub tax_id: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub credit_limit: Option<Option<Decimal>>,
    pub payment_days: Option<Option<i64>>,
}

/// Outcome of creating a customer: the new row plus any customers that already
/// had that exact name, so the interface can warn without blocking (AC15).
#[derive(Debug, Clone, Serialize)]
pub struct CustomerCreateResult {
    pub customer: Customer,
    pub name_matches: Vec<Customer>,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice K3). The receivable is derived from sales and payments,
// so these reads live in `SalesService`: customers sits above sales, and the
// reverse would be circular. Decimal-as-TEXT like the rest of the project, so
// the buckets and the running balance are summed in Rust, never with SQL SUM.
// ---------------------------------------------------------------------------

/// Ageing of a derived receivable against an explicit `as_of` date. Each sale
/// with `due > 0` falls in exactly one bucket by how many days late it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Ageing {
    /// Not yet due, due today, or no due date at all.
    pub current: Decimal,
    /// 1 to 30 days past the due date.
    pub overdue_1_30: Decimal,
    /// 31 to 60 days past the due date.
    pub overdue_31_60: Decimal,
    /// More than 60 days past the due date.
    pub overdue_61_plus: Decimal,
}

impl Ageing {
    /// Sum of the four buckets: the receivable they were computed from.
    pub fn total(&self) -> Decimal {
        self.current + self.overdue_1_30 + self.overdue_31_60 + self.overdue_61_plus
    }
}

/// One row of the receivables view: a customer with a non-zero derived balance
/// and the ageing of that balance as of the requested date. Names stay with
/// `CustomerService`; routes compose the two reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomerAgeing {
    pub customer_id: i64,
    pub balance: Decimal,
    pub ageing: Ageing,
}

/// What produced a statement entry: a confirmed credit sale (a debit) or a
/// payment received on one of those sales (a credit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum StatementEntryKind {
    Sale,
    Payment,
}

/// One line of a customer statement. `balance` is the running balance after
/// applying this entry, so the last entry always lands on the statement total.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatementEntry {
    pub date: NaiveDate,
    pub kind: StatementEntryKind,
    /// The document the entry belongs to (`YYYY-SALE-NNNNNN`). A payment carries
    /// the sale it was applied to, which keeps tied dates orderable.
    pub document_number: Option<String>,
    pub description: String,
    pub debit: Decimal,
    pub credit: Decimal,
    pub balance: Decimal,
}

/// Derived account statement of one customer: the full confirmed-credit ledger
/// with its running balance, plus the ageing of the same receivable as of
/// `as_of`. Cancelled sales contribute nothing to either side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomerStatement {
    pub customer_id: i64,
    pub balance: Decimal,
    pub as_of: NaiveDate,
    pub ageing: Ageing,
    pub entries: Vec<StatementEntry>,
}

// ---------------------------------------------------------------------------
// M4 customers (Slice L). A customer receipt is the document a single handover
// of money produces: it groups one `sale_payments` row per credit sale the
// amount covered, applied oldest debt first. Each grouped payment still belongs
// to its sale and keeps its own finance link, so traceability is untouched; the
// receipt posts no movement of its own. There is NO stored total: the amount
// handed over is derived as SUM(allocations), so an interrupted collection can
// leave fewer payments but never a receipt claiming more than it applied.
// Decimal-as-TEXT like the rest of the project.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerReceipt {
    pub id: i64,
    pub customer_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    pub date: NaiveDate,
    /// Optional free text (trimmed, <= 256 chars), NULL when empty.
    pub notes: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

/// Service-level input for creating a receipt. There is no total field: the
/// collected amount is a plan input, not a stored claim; what the document
/// applied is derived from its payments. There is no account field either: the
/// account is derived from the method, which belongs to exactly one account.
#[derive(Debug, Clone)]
pub struct NewReceipt {
    pub customer_id: i64,
    pub account_id: i64,
    pub method_id: i64,
    pub date: NaiveDate,
    pub notes: Option<String>,
}

/// One receipt with the payments it groups. `allocations` are the
/// `sale_payments` rows carrying the receipt id, one per covered sale and each
/// with its own `transaction_id`.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptDetail {
    pub receipt: CustomerReceipt,
    pub allocations: Vec<SalePayment>,
    /// Derived, never stored: `SUM(allocations.amount)`, i.e. exactly what was
    /// handed over and applied. A stored copy could disagree with the payments;
    /// this one is computed from them.
    pub total: Decimal,
    /// Account name resolved for display through the account read path.
    pub account_name: String,
    /// Payment-method name resolved for display through the finance read path.
    pub method_name: String,
}

impl ReceiptDetail {
    pub fn new(receipt: CustomerReceipt, allocations: Vec<SalePayment>) -> Self {
        let total = allocations.iter().map(|payment| payment.amount).sum();
        Self {
            receipt,
            allocations,
            total,
            account_name: String::new(),
            method_name: String::new(),
        }
    }

    /// Attach the display names the receipt list shows, so the template never
    /// prints the internal account/method keys.
    pub fn with_names(mut self, account_name: String, method_name: String) -> Self {
        self.account_name = account_name;
        self.method_name = method_name;
        self
    }
}
