// M2 sales orchestrator (Odoo-style).
// SalesService calls InventoryService for stock Out (reason Sale) / In
// (reason Sale-return) and TransactionService for Income per payment /
// Expense refund with reference = sale_number. It never SQLs `transactions`
// or `stock_movements` directly (all finance/stock rows go via services).
//
// Numbering: YYYY-SALE-NNNNNN assigned on confirm via `doc_sequences` row
// UPDATE (UPSERT + RETURNING, atomic). Draft touches nothing. Cash confirm
// creates 1 payment + Income; Credit confirm creates receivable, no Income.
// Overpay rejected. Double confirm / edit Confirmed rejected. Cancel from
// Confirmed re-enters stock + refunds guarded by allow flags. Service /
// untracked lines sellable without stock moves. Decimal-as-TEXT via repos.
//
// Atomicity note: true shared SQLite tx across services would require
// changing finance/inventory services (forbidden here). Instead we
// pre-validate (accounts, stock availability, balances) before any mutation,
// then mutate in order sequence -> stock -> finance -> sale row. The only
// expected side effect on failure after validation is a sequence gap
// (abandoned number), which matches ticket reality. Single-user, no races.
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    format_sale_number, MovementReason, MovementType, NewMovement, NewSale, PaymentStatus,
    PaymentType, ProductKind, Sale, SaleDetail, SaleLine, SalePayment, UpdateSaleDraft,
};
use crate::repositories::{
    AccountRepository, BarcodeRepository, CategoryRepository, DocSequenceRepository,
    ProductRepository, SaleRepository, StockMovementRepository, TransactionRepository,
};

#[derive(Clone)]
pub struct SalesService<SR, DR, C, P, B, S, A, T>
where
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
{
    pub sales: SR,
    pub sequences: DR,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
}

impl<SR, DR, C, P, B, S, A, T> SalesService<SR, DR, C, P, B, S, A, T>
where
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
{
    pub fn new(
        sales: SR,
        sequences: DR,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
    ) -> Self {
        Self {
            sales,
            sequences,
            inventory,
            transactions,
        }
    }

    // -- validation helpers -------------------------------------------------

    fn clean_customer(name: &str) -> AppResult<String> {
        let t = name.trim();
        if t.chars().count() > 128 {
            return Err(AppError::Validation(
                "customer_name must be <= 128 chars".into(),
            ));
        }
        Ok(t.to_string())
    }

    fn clean_notes(notes: &Option<String>) -> AppResult<String> {
        let s = notes.clone().unwrap_or_default();
        if s.chars().count() > 512 {
            return Err(AppError::Validation("notes must be <= 512 chars".into()));
        }
        Ok(s.trim().to_string())
    }

    fn clean_receipt(receipt: &Option<String>) -> AppResult<Option<String>> {
        match receipt {
            None => Ok(None),
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Ok(None)
                } else {
                    if t.chars().count() > 64 {
                        return Err(AppError::Validation(
                            "receipt_no must be <= 64 chars".into(),
                        ));
                    }
                    Ok(Some(t.to_string()))
                }
            }
        }
    }

    fn validate_dates(
        payment_type: PaymentType,
        sale_date: NaiveDate,
        due_date: Option<NaiveDate>,
    ) -> AppResult<()> {
        match payment_type {
            PaymentType::Cash => {
                if due_date.is_some() {
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
                }
            }
            PaymentType::Credit => {
                let due = due_date.ok_or_else(|| {
                    AppError::Validation("due_date is required for Credit".into())
                })?;
                if due < sale_date {
                    return Err(AppError::Validation(
                        "due_date must be >= sale_date".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn totals(lines: &[SaleLine], payments: &[SalePayment]) -> (Decimal, Decimal, Decimal) {
        let mut total = Decimal::ZERO;
        for l in lines {
            total += l.qty * l.unit_price;
        }
        let mut paid = Decimal::ZERO;
        for p in payments {
            paid += p.amount;
        }
        let due = total - paid;
        (total, paid, due)
    }

    async fn detail_for(&self, sale: Sale) -> AppResult<SaleDetail> {
        let lines = self.sales.list_lines(sale.id).await?;
        let payments = self.sales.list_payments(sale.id).await?;
        let (total, paid, due) = Self::totals(&lines, &payments);
        let payment_status = SaleDetail::payment_status_for(total, paid);
        Ok(SaleDetail {
            sale,
            lines,
            payments,
            total,
            paid,
            due,
            payment_status,
        })
    }

    fn ensure_draft(sale: &Sale) -> AppResult<()> {
        if sale.status != crate::models::SaleStatus::Draft {
            return Err(AppError::Validation(format!(
                "sale {} is not editable (status {})",
                sale.id, sale.status
            )));
        }
        Ok(())
    }

    // -- Draft ---------------------------------------------------------------

    pub async fn create_draft(&self, input: NewSale) -> AppResult<Sale> {
        let customer_name = Self::clean_customer(&input.customer_name)?;
        let notes = Self::clean_notes(&input.notes)?;
        let receipt_no = Self::clean_receipt(&input.receipt_no)?;
        Self::validate_dates(input.payment_type, input.sale_date, input.due_date)?;
        let clean = NewSale {
            customer_name,
            payment_type: input.payment_type,
            sale_date: input.sale_date,
            due_date: input.due_date,
            receipt_no,
            notes: Some(notes),
        };
        self.sales.create_sale(&clean).await
    }

    pub async fn update_draft(&self, id: i64, patch: UpdateSaleDraft) -> AppResult<Sale> {
        let sale = self
            .sales
            .find_sale(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))?;
        Self::ensure_draft(&sale)?;

        // Validate patch fields before delegating.
        if let Some(ref name) = patch.customer_name {
            Self::clean_customer(name)?;
        }
        if let Some(ref notes) = patch.notes {
            if notes.chars().count() > 512 {
                return Err(AppError::Validation("notes must be <= 512 chars".into()));
            }
        }
        if let Some(ref receipt_opt) = patch.receipt_no {
            Self::clean_receipt(receipt_opt)?;
        }
        // Compute prospective dates for validation.
        let new_sale_date = patch.sale_date.unwrap_or(sale.sale_date);
        let new_due_date = match &patch.due_date {
            Some(inner) => *inner,
            None => sale.due_date,
        };
        Self::validate_dates(sale.payment_type, new_sale_date, new_due_date)?;

        // Normalize patch (trim customer/notes) before repo update.
        let norm = UpdateSaleDraft {
            customer_name: patch.customer_name.map(|s| s.trim().to_string()),
            sale_date: patch.sale_date,
            due_date: patch.due_date,
            receipt_no: patch.receipt_no.map(|opt| {
                opt.map(|s| {
                    let t = s.trim();
                    if t.is_empty() {
                        String::new()
                    } else {
                        t.to_string()
                    }
                })
            }),
            notes: patch.notes.map(|s| s.trim().to_string()),
        };
        self.sales.update_draft(id, &norm).await
    }

    pub async fn add_line(
        &self,
        sale_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_price: Option<Decimal>,
    ) -> AppResult<SaleLine> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        Self::ensure_draft(&sale)?;
        if qty <= Decimal::ZERO {
            return Err(AppError::Validation("qty must be > 0".into()));
        }
        // 404 on unknown product (AC5).
        let product = self.inventory.get_product(product_id).await?;
        let price = match unit_price {
            Some(p) => {
                if p < Decimal::ZERO {
                    return Err(AppError::Validation(
                        "unit_price cannot be negative".into(),
                    ));
                }
                p
            }
            None => product.sale_price,
        };
        self.sales
            .create_line(sale_id, product_id, qty, price)
            .await
    }

    pub async fn update_line(
        &self,
        line_id: i64,
        qty: Decimal,
        unit_price: Decimal,
    ) -> AppResult<SaleLine> {
        let line = self
            .sales
            .find_line(line_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale line {line_id} not found")))?;
        let sale = self
            .sales
            .find_sale(line.sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {} not found", line.sale_id)))?;
        Self::ensure_draft(&sale)?;
        if qty <= Decimal::ZERO {
            return Err(AppError::Validation("qty must be > 0".into()));
        }
        if unit_price < Decimal::ZERO {
            return Err(AppError::Validation(
                "unit_price cannot be negative".into(),
            ));
        }
        self.sales.update_line(line_id, qty, unit_price).await
    }

    pub async fn remove_line(&self, line_id: i64) -> AppResult<()> {
        let line = self
            .sales
            .find_line(line_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale line {line_id} not found")))?;
        let sale = self
            .sales
            .find_sale(line.sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {} not found", line.sale_id)))?;
        Self::ensure_draft(&sale)?;
        let deleted = self.sales.delete_line(line_id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!(
                "sale line {line_id} not found"
            )));
        }
        Ok(())
    }

    pub async fn get_detail(&self, sale_id: i64) -> AppResult<SaleDetail> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        self.detail_for(sale).await
    }

    // -- Lists (route support, Slice D) ------------------------------------------

    /// All sales with derived totals, oldest first (repository order).
    pub async fn list_details(&self) -> AppResult<Vec<SaleDetail>> {
        let sales = self.sales.list_sales().await?;
        let mut out = Vec::with_capacity(sales.len());
        for sale in sales {
            out.push(self.detail_for(sale).await?);
        }
        Ok(out)
    }

    /// Outstanding receivables: Confirmed sales with due > 0.
    pub async fn outstanding_debt(&self) -> AppResult<Vec<SaleDetail>> {
        let all = self.list_details().await?;
        Ok(all
            .into_iter()
            .filter(|d| {
                d.sale.status == crate::models::SaleStatus::Confirmed
                    && d.due > Decimal::ZERO
            })
            .collect())
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(
        &self,
        sale_id: i64,
        cash_account_id: Option<i64>,
    ) -> AppResult<SaleDetail> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        if sale.status == crate::models::SaleStatus::Confirmed {
            return Err(AppError::Validation("sale already confirmed".into()));
        }
        if sale.status == crate::models::SaleStatus::Cancelled {
            return Err(AppError::Validation(
                "cancelled sale cannot be confirmed".into(),
            ));
        }

        let lines = self.sales.list_lines(sale_id).await?;
        if lines.is_empty() {
            return Err(AppError::Validation(
                "cannot confirm sale with no lines".into(),
            ));
        }

        // Validate products (404 unknown, Validation inactive/bad values).
        // Collect (line, is_tracked) for stock phase.
        let mut tracked: Vec<(SaleLine, i64)> = Vec::new();
        for line in &lines {
            if line.qty <= Decimal::ZERO {
                return Err(AppError::Validation("qty must be > 0".into()));
            }
            if line.unit_price < Decimal::ZERO {
                return Err(AppError::Validation(
                    "unit_price cannot be negative".into(),
                ));
            }
            let product = self.inventory.get_product(line.product_id).await?;
            if !product.is_active {
                return Err(AppError::Validation(format!(
                    "product {} is inactive",
                    product.id
                )));
            }
            if product.kind == ProductKind::Product && product.track_stock {
                tracked.push((line.clone(), product.id));
            }
        }

        let (total, _, _) = Self::totals(
            &lines,
            &self.sales.list_payments(sale_id).await?,
        );

        match sale.payment_type {
            PaymentType::Cash => {
                let account_id = cash_account_id.ok_or_else(|| {
                    AppError::Validation("cash sale requires account".into())
                })?;
                if sale.due_date.is_some() {
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
                }
                if !self.transactions.accounts.exists(account_id).await? {
                    return Err(AppError::NotFound(format!(
                        "account {account_id} not found"
                    )));
                }
            }
            PaymentType::Credit => {
                if cash_account_id.is_some() {
                    return Err(AppError::Validation(
                        "credit sale must not include cash account".into(),
                    ));
                }
                if sale.due_date.is_none() {
                    return Err(AppError::Validation(
                        "due_date is required for Credit".into(),
                    ));
                }
            }
        }

        // Pre-check strict stock to avoid sequence gap + partial moves.
        if !self.inventory.allow_negative_stock {
            for (line, _) in &tracked {
                let current = self.inventory.stock(line.product_id).await?;
                if current - line.qty < Decimal::ZERO {
                    return Err(AppError::Validation(format!(
                        "insufficient stock: {current} would become {}",
                        current - line.qty
                    )));
                }
            }
        }

        // Assign number atomically via doc_sequences row UPDATE.
        let year = sale.sale_date.year();
        let seq = self.sequences.next_number("SALE", year).await?;
        let sale_number = format_sale_number(year, seq);

        // Stock Out (reason Sale) for tracked Product lines only.
        // Service / untracked lines are sellable without stock moves (AC9).
        for (line, _) in &tracked {
            self.inventory
                .record_movement(NewMovement {
                    product_id: line.product_id,
                    qty: line.qty,
                    movement_type: MovementType::Out,
                    reason: MovementReason::Sale,
                    reference: sale_number.clone(),
                    date: sale.sale_date,
                })
                .await?;
        }

        // Finance: Cash => 1 payment + Income now; Credit => receivable, no Income.
        if sale.payment_type == PaymentType::Cash && total > Decimal::ZERO {
            let account_id = cash_account_id.unwrap();
            self.transactions
                .create(
                    account_id,
                    crate::models::TransactionKind::Income,
                    total,
                    Some(sale_number.clone()),
                    sale.sale_date,
                )
                .await?;
            self.sales
                .create_payment(sale_id, account_id, total, sale.sale_date)
                .await?;
        }

        let confirmed = self.sales.set_confirmed(sale_id, &sale_number).await?;
        self.detail_for(confirmed).await
    }

    // -- Pay (Credit) ------------------------------------------------------------

    pub async fn record_payment(
        &self,
        sale_id: i64,
        account_id: i64,
        amount: Decimal,
        date: NaiveDate,
    ) -> AppResult<SalePayment> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        if sale.status != crate::models::SaleStatus::Confirmed {
            return Err(AppError::Validation(
                "payments require a Confirmed sale".into(),
            ));
        }
        if amount <= Decimal::ZERO {
            return Err(AppError::Validation("amount must be > 0".into()));
        }
        if !self.transactions.accounts.exists(account_id).await? {
            return Err(AppError::NotFound(format!(
                "account {account_id} not found"
            )));
        }
        let lines = self.sales.list_lines(sale_id).await?;
        let payments = self.sales.list_payments(sale_id).await?;
        let (total, paid, _) = Self::totals(&lines, &payments);
        if paid + amount > total {
            return Err(AppError::Validation(format!(
                "overpay rejected: paid {paid} + {amount} exceeds total {total}"
            )));
        }
        let sale_number = sale.sale_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed sale missing sale_number".into())
        })?;
        // Each payment generates one M0 Income with reference = sale_number.
        self.transactions
            .create(
                account_id,
                crate::models::TransactionKind::Income,
                amount,
                Some(sale_number),
                date,
            )
            .await?;
        self.sales
            .create_payment(sale_id, account_id, amount, date)
            .await
    }

    // -- Cancel / Return -----------------------------------------------------------

    pub async fn cancel(
        &self,
        sale_id: i64,
        reason: Option<String>,
    ) -> AppResult<SaleDetail> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        if sale.status == crate::models::SaleStatus::Cancelled {
            return Err(AppError::Validation("sale already cancelled".into()));
        }
        if sale.status == crate::models::SaleStatus::Draft {
            // Draft -> Cancelled: no-op, no stock/finance.
            let cancelled = self
                .sales
                .set_cancelled(sale_id, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        // Confirmed -> Cancelled: re-enter stock + refunds.
        let lines = self.sales.list_lines(sale_id).await?;
        let payments = self.sales.list_payments(sale_id).await?;
        let sale_number = sale.sale_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed sale missing sale_number".into())
        })?;

        // Pre-validate products (for In) and refund balances (guard).
        let mut tracked: Vec<SaleLine> = Vec::new();
        for line in &lines {
            let product = self.inventory.get_product(line.product_id).await?;
            if product.kind == ProductKind::Product && product.track_stock {
                if !product.is_active {
                    return Err(AppError::Validation(format!(
                        "product {} is inactive",
                        product.id
                    )));
                }
                tracked.push(line.clone());
            }
        }
        if !self.transactions.allow_negative {
            for pay in &payments {
                if !self.transactions.accounts.exists(pay.account_id).await? {
                    return Err(AppError::NotFound(format!(
                        "account {} not found",
                        pay.account_id
                    )));
                }
                let current = self
                    .transactions
                    .transactions
                    .balance_for_account(pay.account_id)
                    .await?;
                if current - pay.amount < Decimal::ZERO {
                    return Err(AppError::Validation(format!(
                        "refund would cause negative balance: {current} - {}",
                        pay.amount
                    )));
                }
            }
        } else {
            for pay in &payments {
                if !self.transactions.accounts.exists(pay.account_id).await? {
                    return Err(AppError::NotFound(format!(
                        "account {} not found",
                        pay.account_id
                    )));
                }
            }
        }

        // Stock In (reason Sale-return) for tracked lines.
        for line in &tracked {
            self.inventory
                .record_movement(NewMovement {
                    product_id: line.product_id,
                    qty: line.qty,
                    movement_type: MovementType::In,
                    reason: MovementReason::SaleReturn,
                    reference: sale_number.clone(),
                    date: sale.sale_date,
                })
                .await?;
        }

        // Refund Expense per paid amount to originating accounts.
        for pay in &payments {
            self.transactions
                .create(
                    pay.account_id,
                    crate::models::TransactionKind::Expense,
                    pay.amount,
                    Some(sale_number.clone()),
                    sale.sale_date,
                )
                .await?;
        }

        let cancelled = self
            .sales
            .set_cancelled(sale_id, reason.as_deref())
            .await?;
        self.detail_for(cancelled).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewProduct, ProductKind};
    use crate::repositories::{
        SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteDocSequenceRepository, SqliteProductRepository, SqliteSaleRepository,
        SqliteStockMovementRepository, SqliteTransactionRepository,
    };
    use crate::services::{InventoryService, TransactionService};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    type Svc = SalesService<
        SqliteSaleRepository,
        SqliteDocSequenceRepository,
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
        SqliteAccountRepository,
        SqliteTransactionRepository,
    >;

    async fn test_pool() -> sqlx::SqlitePool {
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
        pool
    }

    async fn svc_with_flags(
        allow_stock: bool,
        allow_balance: bool,
    ) -> (Svc, sqlx::SqlitePool) {
        let pool = test_pool().await;
        let inventory = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            allow_stock,
        );
        let transactions = TransactionService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
            allow_balance,
        );
        let s = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory,
            transactions,
        );
        (s, pool)
    }

    async fn svc() -> (Svc, sqlx::SqlitePool) {
        svc_with_flags(true, false).await
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn sale_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 5, 2).unwrap()
    }

    async fn seed_product(s: &Svc, sku: &str, price: &str) -> crate::models::Product {
        s.inventory
            .create_product(NewProduct {
                sku: sku.into(),
                name: format!("prod {sku}"),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: dec(price),
                cost_price: dec("5"),
                track_stock: true,
                min_stock: Some(dec("0")),
                max_stock: Some(dec("100")),
                location: None,
                notes: None,
            })
            .await
            .unwrap()
    }

    async fn seed_service(s: &Svc, sku: &str) -> crate::models::Product {
        s.inventory
            .create_product(NewProduct {
                sku: sku.into(),
                name: format!("svc {sku}"),
                kind: ProductKind::Service,
                category_id: None,
                unit: "hr".into(),
                sale_price: dec("30"),
                cost_price: dec("0"),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
            })
            .await
            .unwrap()
    }

    async fn seed_stock(s: &Svc, product_id: i64, qty: &str) {
        s.inventory
            .record_movement(NewMovement {
                product_id,
                qty: dec(qty),
                movement_type: MovementType::In,
                reason: MovementReason::Initial,
                reference: "".into(),
                date: NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            })
            .await
            .unwrap();
    }

    async fn seed_account(s: &Svc, name: &str) -> crate::models::Account {
        s.transactions
            .accounts
            .create(name)
            .await
            .unwrap()
    }

    async fn tx_count(pool: &sqlx::SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM transactions")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn movement_count(pool: &sqlx::SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    // -- AC1 ------------------------------------------------------------------

    #[tokio::test]
    async fn red_ac1_draft_touches_nothing() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "RED-1", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "Juan".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        assert_eq!(movement_count(&pool).await, 1);
        assert_eq!(tx_count(&pool).await, 0);
    }

    // -- AC2: Confirm Cash ------------------------------------------------------

    #[tokio::test]
    async fn ac2_confirm_cash_assigns_number_deducts_posts_income() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC2", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "Ana".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        assert!(sale.sale_number.is_none());
        s.add_line(sale.id, prod.id, dec("3"), None).await.unwrap();

        let detail = s.confirm(sale.id, Some(acc.id)).await.unwrap();
        let number = detail.sale.sale_number.clone().unwrap();
        assert_eq!(number, "2024-SALE-000001");
        assert_eq!(detail.total, dec("30"));
        assert_eq!(detail.paid, dec("30"));
        assert_eq!(detail.due, Decimal::ZERO);
        assert_eq!(detail.payment_status, PaymentStatus::Paid);
        // Stock deducted: 10 - 3 = 7.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("7"));
        // 1 Income + 0 other.
        assert_eq!(tx_count(&pool).await, 1);
        let rows = s.transactions.transactions.list_by_account(acc.id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, crate::models::TransactionKind::Income);
        assert_eq!(rows[0].amount, dec("30"));
        assert_eq!(rows[0].description, number);
        // Stock reference = sale_number, reason Sale, type Out.
        let moves = s.inventory.movements.list_by_product(prod.id).await.unwrap();
        let out = moves.iter().find(|m| m.movement_type == MovementType::Out).unwrap();
        assert_eq!(out.reference, number);
        assert_eq!(out.reason, MovementReason::Sale);
    }

    // -- AC3: Confirm Credit ----------------------------------------------------

    #[tokio::test]
    async fn ac3_confirm_credit_deducts_no_income_due_total() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC3", "12").await;
        seed_stock(&s, prod.id, "10").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "Cred".into(),
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        let detail = s.confirm(sale.id, None).await.unwrap();
        assert!(detail.sale.sale_number.is_some());
        assert_eq!(detail.total, dec("24"));
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("24"));
        assert_eq!(detail.payment_status, PaymentStatus::Unpaid);
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("8"));
        assert_eq!(tx_count(&pool).await, 0);
    }

    // -- AC4: Credit payments ---------------------------------------------------

    #[tokio::test]
    async fn ac4_credit_payments_create_incomes_overpay_rejected() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC4", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "banco").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "Deudor".into(),
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 40
        s.confirm(sale.id, None).await.unwrap();

        s.record_payment(
            sale.id,
            acc.id,
            dec("15"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.paid, dec("15"));
        assert_eq!(d.due, dec("25"));
        assert_eq!(d.payment_status, PaymentStatus::Partial);
        assert_eq!(tx_count(&pool).await, 1);

        // Overpay rejected (15 + 30 > 40).
        let err = s
            .record_payment(
                sale.id,
                acc.id,
                dec("30"),
                NaiveDate::from_ymd_opt(2024, 5, 11).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, 1);

        // Pay remainder -> Paid.
        s.record_payment(
            sale.id,
            acc.id,
            dec("25"),
            NaiveDate::from_ymd_opt(2024, 5, 12).unwrap(),
        )
        .await
        .unwrap();
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.paid, dec("40"));
        assert_eq!(d.due, Decimal::ZERO);
        assert_eq!(d.payment_status, PaymentStatus::Paid);
        assert_eq!(tx_count(&pool).await, 2);
    }

    // -- AC5: unknown product/account, bad qty -----------------------------------

    #[tokio::test]
    async fn ac5_unknown_product_account_and_bad_qty() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "AC5", "10").await;
        seed_stock(&s, prod.id, "5").await;
        let acc = seed_account(&s, "caja5").await;

        // Unknown product on add_line => 404.
        let sale = s
            .create_draft(NewSale {
                customer_name: "X".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        let err = s.add_line(sale.id, 99999, dec("1"), None).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // qty <= 0 => 400.
        let err = s.add_line(sale.id, prod.id, dec("0"), None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .add_line(sale.id, prod.id, dec("-1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Unknown account on Cash confirm => 404.
        s.add_line(sale.id, prod.id, dec("1"), None).await.unwrap();
        let err = s.confirm(sale.id, Some(99999)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown account on payment => 404.
        let csale = s
            .create_draft(NewSale {
                customer_name: "C".into(),
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(csale.id, prod.id, dec("1"), None).await.unwrap();
        s.confirm(csale.id, None).await.unwrap();
        let err = s
            .record_payment(csale.id, 99999, dec("5"), sale_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        let _ = acc;
    }

    // -- AC6: double confirm + edit Confirmed --------------------------------------

    #[tokio::test]
    async fn ac6_double_confirm_and_edit_confirmed_rejected() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "AC6", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja6").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "E".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("1"), None).await.unwrap();
        s.confirm(sale.id, Some(acc.id)).await.unwrap();

        // Double confirm => 400/409.
        let err = s.confirm(sale.id, Some(acc.id)).await.unwrap_err();
        assert!(
            matches!(err, AppError::Validation(_) | AppError::Conflict(_)),
            "got {err:?}"
        );

        // Edit Confirmed => 400 (add / update / remove / header).
        let err = s.add_line(sale.id, prod.id, dec("1"), None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let detail = s.get_detail(sale.id).await.unwrap();
        let line_id = detail.lines[0].id;
        let err = s.update_line(line_id, dec("2"), dec("10")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s.remove_line(line_id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .update_draft(
                sale.id,
                UpdateSaleDraft {
                    customer_name: Some("Otro".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    // -- AC7: cancel re-enters + refunds, balance guard ------------------------------

    #[tokio::test]
    async fn ac7_cancel_confirmed_reenters_and_refunds() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC7", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja7").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "R".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("4"), None).await.unwrap(); // total 40
        let confirmed = s.confirm(sale.id, Some(acc.id)).await.unwrap();
        let number = confirmed.sale.sale_number.clone().unwrap();
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("6"));
        assert_eq!(tx_count(&pool).await, 1);

        let cancelled = s
            .cancel(sale.id, Some("devuelve".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.sale.status, crate::models::SaleStatus::Cancelled);
        // Stock re-entered: 6 + 4 = 10.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("10"));
        // Refund Expense created: Income + Expense = 2 rows, net 0.
        assert_eq!(tx_count(&pool).await, 2);
        let rows = s.transactions.transactions.list_by_account(acc.id).await.unwrap();
        assert_eq!(rows.len(), 2);
        let expense = rows.iter().find(|t| t.kind == crate::models::TransactionKind::Expense).unwrap();
        assert_eq!(expense.amount, dec("40"));
        assert_eq!(expense.description, number);
        // In movement reason Sale-return, reference sale_number.
        let moves = s.inventory.movements.list_by_product(prod.id).await.unwrap();
        let ret = moves
            .iter()
            .find(|m| m.movement_type == MovementType::In && m.reference == number && m.qty == dec("4"))
            .unwrap();
        assert_eq!(ret.reason, MovementReason::SaleReturn);
    }

    #[tokio::test]
    async fn ac7_cancel_refund_respects_negative_balance_guard() {
        // allow_negative = false: refund that would overdraft must fail.
        let (s, pool) = svc_with_flags(true, false).await;
        let prod = seed_product(&s, "AC7G", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja7g").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "G".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 20
        s.confirm(sale.id, Some(acc.id)).await.unwrap();
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("8"));
        // Drain account: Income 20, then Expense 20 => balance 0.
        s.transactions
            .create(
                acc.id,
                crate::models::TransactionKind::Expense,
                dec("20"),
                Some("gasto".into()),
                sale_date(),
            )
            .await
            .unwrap();
        // Cancel would refund 20 => balance -20, must fail with allow_negative=false.
        let err = s.cancel(sale.id, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // No stock re-entry, no extra refund.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("8"));
        assert_eq!(tx_count(&pool).await, 2); // Income + gasto, no refund

        // With allow_negative=true the same cancel succeeds.
        let (s2, _) = svc_with_flags(true, true).await;
        // Rebuild same scenario in fresh DB for permissive path.
        let prod2 = seed_product(&s2, "AC7G2", "10").await;
        seed_stock(&s2, prod2.id, "10").await;
        let acc2 = seed_account(&s2, "caja7g2").await;
        let sale2 = s2
            .create_draft(NewSale {
                customer_name: "G".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s2.add_line(sale2.id, prod2.id, dec("2"), None).await.unwrap();
        s2.confirm(sale2.id, Some(acc2.id)).await.unwrap();
        s2.transactions
            .create(
                acc2.id,
                crate::models::TransactionKind::Expense,
                dec("20"),
                Some("gasto".into()),
                sale_date(),
            )
            .await
            .unwrap();
        s2.cancel(sale2.id, None).await.unwrap();
        assert_eq!(s2.inventory.stock(prod2.id).await.unwrap(), dec("10"));
    }

    // -- triangulate -----------------------------------------------------------------

    #[tokio::test]
    async fn tri_service_lines_sellable_without_stock() {
        let (s, pool) = svc().await;
        let svc_prod = seed_service(&s, "TRI-SRV").await;
        let acc = seed_account(&s, "caja-tri").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "Serv".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, svc_prod.id, dec("2"), None).await.unwrap();
        let before = movement_count(&pool).await;
        let detail = s.confirm(sale.id, Some(acc.id)).await.unwrap();
        assert_eq!(detail.total, dec("60"));
        // No stock movements for service lines.
        assert_eq!(movement_count(&pool).await, before);
        assert_eq!(tx_count(&pool).await, 1);
    }

    #[tokio::test]
    async fn tri_sale_number_unique_immutable_and_draft_cancel_noop() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "TRI-NUM", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-num").await;

        let a = s
            .create_draft(NewSale {
                customer_name: "A".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(a.id, prod.id, dec("1"), None).await.unwrap();
        let b = s
            .create_draft(NewSale {
                customer_name: "B".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(b.id, prod.id, dec("1"), None).await.unwrap();

        let da = s.confirm(a.id, Some(acc.id)).await.unwrap();
        let db = s.confirm(b.id, Some(acc.id)).await.unwrap();
        assert_ne!(
            da.sale.sale_number.unwrap(),
            db.sale.sale_number.unwrap()
        );

        // Draft -> Cancelled is a no-op for stock/finance.
        let c = s
            .create_draft(NewSale {
                customer_name: "C".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(c.id, prod.id, dec("1"), None).await.unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;
        let cancelled = s.cancel(c.id, None).await.unwrap();
        assert_eq!(cancelled.sale.status, crate::models::SaleStatus::Cancelled);
        assert!(cancelled.sale.sale_number.is_none());
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, tx_before);
    }

    #[tokio::test]
    async fn tri_strict_stock_blocks_oversell_on_confirm() {
        let (s, _) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "TRI-STRICT", "10").await;
        seed_stock(&s, prod.id, "5").await;
        let acc = seed_account(&s, "caja-strict").await;
        let sale = s
            .create_draft(NewSale {
                customer_name: "S".into(),
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("10"), None).await.unwrap();
        let err = s.confirm(sale.id, Some(acc.id)).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Still Draft, no number.
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.sale.status, crate::models::SaleStatus::Draft);
        assert!(d.sale.sale_number.is_none());
    }
}
