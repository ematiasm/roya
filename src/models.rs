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
    Loss,
    Adjust,
    Initial,
}

impl std::fmt::Display for MovementReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Purchase => write!(f, "Purchase"),
            Self::Sale => write!(f, "Sale"),
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
