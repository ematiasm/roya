// M2 sales orchestrator (Odoo-style).
// SalesService calls InventoryService for stock Out (reason Sale) / In
// (reason Sale-return) and TransactionService for Income per payment /
// Expense refund with reference = sale_number. It never SQLs `transactions`
// or `stock_movements` directly (all finance/stock rows go via services).
//
// Numbering: YYYY-SALE-NNNNNN assigned on confirm via `doc_sequences` row
// UPDATE (UPSERT + RETURNING, atomic). Draft touches nothing. Cash confirm
// creates 1 payment (account+method) + Income; Credit confirm creates
// receivable, no Income. Payments carry account+method, N per sale to mixed
// accounts/methods with SUM <= total. Pair validated against
// account_payment_methods allowlist (400) before any stock/sequence/finance
// touch. Overpay rejected. Double confirm / edit Confirmed rejected. Cancel
// from Confirmed re-enters stock + refunds guarded by allow flags. Service /
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
    format_sale_number, Customer, MovementReason, MovementType, NewMovement, NewSale,
    PaymentStatus, PaymentType, ProductKind, Sale, SaleDetail, SaleLine, SalePayment,
    UpdateSaleDraft,
};
use crate::repositories::{
    AccountRepository, BarcodeRepository, CategoryRepository, CustomerRepository,
    DocSequenceRepository, PaymentMethodRepository, ProductRepository, SaleRepository,
    StockMovementRepository, TransactionRepository,
};
use crate::services::CustomerService;

#[derive(Clone)]
pub struct SalesService<SR, DR, C, P, B, S, A, T, PM, CR>
where
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PM: PaymentMethodRepository,
    CR: CustomerRepository,
{
    pub sales: SR,
    pub sequences: DR,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
    pub payment_methods: PM,
    /// Sales reach customers only through this service: the sale rows store a
    /// snapshot, and no sales repository runs SQL against the `customers` table.
    pub customers: CustomerService<CR>,
    /// `ENFORCE_CREDIT_LIMIT`: when false an over-limit credit sale is confirmed
    /// and the interface reports the customer as over limit instead.
    pub enforce_credit_limit: bool,
}

impl<SR, DR, C, P, B, S, A, T, PM, CR> SalesService<SR, DR, C, P, B, S, A, T, PM, CR>
where
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PM: PaymentMethodRepository,
    CR: CustomerRepository,
{
    pub fn new(
        sales: SR,
        sequences: DR,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
        payment_methods: PM,
        customers: CustomerService<CR>,
        enforce_credit_limit: bool,
    ) -> Self {
        Self {
            sales,
            sequences,
            inventory,
            transactions,
            payment_methods,
            customers,
            enforce_credit_limit,
        }
    }

    async fn ensure_method_allowed(
        &self,
        account_id: i64,
        method_id: i64,
    ) -> AppResult<()> {
        let method = self
            .payment_methods
            .find_method(method_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("method {method_id} not found")))?;
        if !method.is_active {
            return Err(AppError::Validation(format!(
                "method {} is inactive",
                method.name
            )));
        }
        if !self
            .payment_methods
            .is_allowed(account_id, method_id)
            .await?
        {
            return Err(AppError::Validation(format!(
                "method {} is not allowed for account {account_id}; configure the account's payment methods and try again",
                method.name
            )));
        }
        Ok(())
    }

    // -- validation helpers -------------------------------------------------

    /// Effective due date. An explicit date wins; for a credit sale without one
    /// the customer's payment term supplies the default (`sale_date + days`), and
    /// without a term the due date is required. Cash never carries one.
    fn resolve_due_date(
        payment_type: PaymentType,
        sale_date: NaiveDate,
        requested: Option<NaiveDate>,
        customer: &Customer,
    ) -> AppResult<Option<NaiveDate>> {
        match payment_type {
            PaymentType::Cash => {
                if requested.is_some() {
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
                }
                Ok(None)
            }
            PaymentType::Credit => {
                let due = match requested {
                    Some(date) => date,
                    None => {
                        let days = customer.payment_days.ok_or_else(|| {
                            AppError::Validation(
                                "due_date is required for Credit when the customer has no payment term"
                                    .into(),
                            )
                        })?;
                        sale_date + chrono::Duration::days(days)
                    }
                };
                if due < sale_date {
                    return Err(AppError::Validation(
                        "due_date must be >= sale_date".into(),
                    ));
                }
                Ok(Some(due))
            }
        }
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
        // Unknown customer => 404; the row is never created.
        let customer = self.customers.get_customer(input.customer_id).await?;
        let notes = Self::clean_notes(&input.notes)?;
        let receipt_no = Self::clean_receipt(&input.receipt_no)?;
        let due_date =
            Self::resolve_due_date(input.payment_type, input.sale_date, input.due_date, &customer)?;
        let clean = NewSale {
            customer_id: customer.id,
            payment_type: input.payment_type,
            sale_date: input.sale_date,
            due_date,
            receipt_no,
            notes: Some(notes),
        };
        // The name is a snapshot of the customer as it is today; later corrections
        // to the customer never rewrite this sale.
        self.sales.create_sale(&clean, &customer.name).await
    }

    pub async fn update_draft(&self, id: i64, patch: UpdateSaleDraft) -> AppResult<Sale> {
        let sale = self
            .sales
            .find_sale(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))?;
        Self::ensure_draft(&sale)?;

        // Validate patch fields before delegating.
        if let Some(ref notes) = patch.notes {
            if notes.chars().count() > 512 {
                return Err(AppError::Validation("notes must be <= 512 chars".into()));
            }
        }
        if let Some(ref receipt_opt) = patch.receipt_no {
            Self::clean_receipt(receipt_opt)?;
        }
        // The customer is fixed at creation, so the term used to resolve a cleared
        // due date comes from that same customer.
        let customer = self.customers.get_customer(sale.customer_id).await?;
        let new_sale_date = patch.sale_date.unwrap_or(sale.sale_date);
        let requested_due = match &patch.due_date {
            Some(inner) => *inner,
            None => sale.due_date,
        };
        let new_due_date =
            Self::resolve_due_date(sale.payment_type, new_sale_date, requested_due, &customer)?;

        // Normalize patch (trim notes) before repo update.
        let norm = UpdateSaleDraft {
            sale_date: patch.sale_date,
            due_date: Some(new_due_date),
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

    /// Derived receivable of one customer: confirmed credit sales (lines total)
    /// minus the payments received on them. Cancelled sales never count. The
    /// per-sale math runs in `Decimal` (the columns are TEXT), so the credit-limit
    /// check never passes through floating point.
    async fn customer_debt(&self, customer_id: i64) -> AppResult<Decimal> {
        let mut debt = Decimal::ZERO;
        for sale in self.sales.list_confirmed_credit_sales(customer_id).await? {
            let lines = self.sales.list_lines(sale.id).await?;
            let payments = self.sales.list_payments(sale.id).await?;
            let (total, paid, _) = Self::totals(&lines, &payments);
            debt += total - paid;
        }
        Ok(debt)
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(
        &self,
        sale_id: i64,
        cash_account_id: Option<i64>,
        cash_method_id: Option<i64>,
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
                    AppError::Validation("cash sale requires account and method".into())
                })?;
                let method_id = cash_method_id.ok_or_else(|| {
                    AppError::Validation("cash sale requires account and method".into())
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
                // Allowlist validated before any stock/sequence/finance touch.
                self.ensure_method_allowed(account_id, method_id).await?;
            }
            PaymentType::Credit => {
                if cash_account_id.is_some() || cash_method_id.is_some() {
                    return Err(AppError::Validation(
                        "credit sale must not include cash account".into(),
                    ));
                }
                if sale.due_date.is_none() {
                    return Err(AppError::Validation(
                        "due_date is required for Credit".into(),
                    ));
                }
                // Credit is a real receivable, so it needs a real customer:
                // "Consumidor final" cannot owe money.
                let customer = self.customers.get_customer(sale.customer_id).await?;
                if customer.is_walkin {
                    return Err(AppError::Validation(
                        "cannot sell on credit to the walk-in customer; choose a customer".into(),
                    ));
                }
                // A null limit is unlimited: the flag never acts as a bypass for a
                // customer who never set one.
                if self.enforce_credit_limit {
                    if let Some(limit) = customer.credit_limit {
                        let debt = self.customer_debt(customer.id).await?;
                        let projected = debt + total;
                        if projected > limit {
                            return Err(AppError::Validation(format!(
                                "credit limit exceeded for {}: projected debt {projected} > limit {limit}",
                                customer.name
                            )));
                        }
                    }
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
        // The Income is stamped with reference = sale_number and linked back from
        // the payment row it produced.
        if sale.payment_type == PaymentType::Cash && total > Decimal::ZERO {
            let account_id = cash_account_id.unwrap();
            let method_id = cash_method_id.unwrap();
            let income = self
                .transactions
                .create_with_reference(
                    account_id,
                    crate::models::TransactionKind::Income,
                    total,
                    Some(sale_number.clone()),
                    Some(sale_number.clone()),
                    sale.sale_date,
                )
                .await?;
            self.sales
                .create_payment(
                    sale_id,
                    account_id,
                    method_id,
                    total,
                    sale.sale_date,
                    Some(income.id),
                )
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
        method_id: i64,
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
        // Allowlist validated before any finance touch (no stock here either).
        self.ensure_method_allowed(account_id, method_id).await?;
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
        // Each payment generates one M0 Income stamped with reference =
        // sale_number and linked from the payment row it produced.
        let income = self
            .transactions
            .create_with_reference(
                account_id,
                crate::models::TransactionKind::Income,
                amount,
                Some(sale_number.clone()),
                Some(sale_number),
                date,
            )
            .await?;
        self.sales
            .create_payment(
                sale_id,
                account_id,
                method_id,
                amount,
                date,
                Some(income.id),
            )
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

        // Refund Expense per paid amount to originating accounts, each linked
        // back from the payment row it refunds.
        for pay in &payments {
            let refund = self
                .transactions
                .create_with_reference(
                    pay.account_id,
                    crate::models::TransactionKind::Expense,
                    pay.amount,
                    Some(sale_number.clone()),
                    Some(sale_number.clone()),
                    sale.sale_date,
                )
                .await?;
            self.sales
                .set_payment_refund_transaction(pay.id, refund.id)
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
    use crate::models::{NewCustomer, NewProduct, ProductKind};
    use crate::repositories::{
        CustomerRepository, PaymentMethodRepository, SqliteAccountRepository,
        SqliteBarcodeRepository, SqliteCategoryRepository, SqliteCustomerRepository,
        SqliteDocSequenceRepository, SqlitePaymentMethodRepository, SqliteProductRepository,
        SqliteSaleRepository, SqliteStockMovementRepository, SqliteTransactionRepository,
    };
    use crate::services::{CustomerService, InventoryService, TransactionService};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// The seeded walk-in is the first row in every fresh test database.
    const WALKIN_ID: i64 = 1;
    /// `svc_with_flags` seeds this non-walk-in customer, so the existing tests have
    /// a valid credit customer without extra setup.
    const CREDIT_CUSTOMER_ID: i64 = 2;

    type Svc = SalesService<
        SqliteSaleRepository,
        SqliteDocSequenceRepository,
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
        SqliteAccountRepository,
        SqliteTransactionRepository,
        SqlitePaymentMethodRepository,
        SqliteCustomerRepository,
    >;

    async fn test_pool() -> sqlx::SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool: the walk-in triggers must fire
            // under REPLACE conflict resolution too.
            .pragma("recursive_triggers", "1");
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
        svc_with_credit_flag(allow_stock, allow_balance, true).await
    }

    /// K2: builds the service with `ENFORCE_CREDIT_LIMIT` injected the same way
    /// production does, plus the deterministic non-walk-in credit customer.
    async fn svc_with_credit_flag(
        allow_stock: bool,
        allow_balance: bool,
        enforce_credit_limit: bool,
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
        let customers = CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
        customers
            .create_customer(NewCustomer {
                name: "Credit Customer".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap();
        let s = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory,
            transactions,
            SqlitePaymentMethodRepository::new(pool.clone()),
            customers,
            enforce_credit_limit,
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
        s.transactions.accounts.create(name).await.unwrap()
    }

    async fn cash_method(s: &Svc) -> i64 {
        s.payment_methods
            .find_method_by_name("Cash")
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn method_by_name(s: &Svc, name: &str) -> i64 {
        s.payment_methods
            .find_method_by_name(name)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn allow(s: &Svc, account_id: i64, method_id: i64) {
        s.payment_methods.allow(account_id, method_id).await.unwrap()
    }

    async fn walkin_of(s: &Svc) -> crate::models::Customer {
        s.customers
            .customers
            .find_walkin()
            .await
            .unwrap()
            .expect("the walk-in is seeded by migration 20")
    }

    async fn seed_customer(
        s: &Svc,
        name: &str,
        limit: Option<&str>,
        payment_days: Option<i64>,
    ) -> crate::models::Customer {
        s.customers
            .create_customer(NewCustomer {
                name: name.into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: limit.map(dec),
                payment_days,
            })
            .await
            .unwrap()
            .customer
    }

    async fn draft_with_line(
        s: &Svc,
        customer_id: i64,
        payment_type: PaymentType,
        due_date: Option<NaiveDate>,
        product_id: i64,
        qty: &str,
    ) -> crate::models::Sale {
        let sale = s
            .create_draft(NewSale {
                customer_id,
                payment_type,
                sale_date: sale_date(),
                due_date,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, product_id, dec(qty), None)
            .await
            .unwrap();
        sale
    }

    async fn sale_sequence_last(pool: &sqlx::SqlitePool) -> Option<i64> {
        sqlx::query_as::<_, (i64,)>(
            "SELECT last_number FROM doc_sequences WHERE doc_type = 'SALE'",
        )
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| r.0)
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
                customer_id: WALKIN_ID,
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
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

        let detail = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
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
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        let detail = s.confirm(sale.id, None, None).await.unwrap();
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 40
        s.confirm(sale.id, None, None).await.unwrap();

        s.record_payment(
            sale.id,
            acc.id,
            cash,
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
                cash,
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
            cash,
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // Unknown product on add_line => 404.
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
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
        let err = s.confirm(sale.id, Some(99999), Some(cash)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown account on payment => 404.
        let csale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(csale.id, prod.id, dec("1"), None).await.unwrap();
        s.confirm(csale.id, None, None).await.unwrap();
        let err = s
            .record_payment(csale.id, 99999, cash, dec("5"), sale_date())
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("1"), None).await.unwrap();
        s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();

        // Double confirm => 400/409.
        let err = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap_err();
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
                    notes: Some("Otro".into()),
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("4"), None).await.unwrap(); // total 40
        let confirmed = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 20
        s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
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
        let cash2 = method_by_name(&s2, "Cash").await;
        allow(&s2, acc2.id, cash2).await;
        let sale2 = s2
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s2.add_line(sale2.id, prod2.id, dec("2"), None).await.unwrap();
        s2.confirm(sale2.id, Some(acc2.id), Some(cash2)).await.unwrap();
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
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
        let detail = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let a = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
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
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(b.id, prod.id, dec("1"), None).await.unwrap();

        let da = s.confirm(a.id, Some(acc.id), Some(cash)).await.unwrap();
        let db = s.confirm(b.id, Some(acc.id), Some(cash)).await.unwrap();
        assert_ne!(
            da.sale.sale_number.unwrap(),
            db.sale.sale_number.unwrap()
        );

        // Draft -> Cancelled is a no-op for stock/finance.
        let c = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
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
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("10"), None).await.unwrap();
        let err = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Still Draft, no number.
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.sale.status, crate::models::SaleStatus::Draft);
        assert!(d.sale.sale_number.is_none());
    }

    #[tokio::test]
    async fn red_payment_methods_seeded_without_other() {
        let (_s, pool) = svc().await;
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM payment_methods ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let names: Vec<String> = rows.into_iter().map(|r| r.0).collect();
        assert_eq!(names, vec!["Cash", "CreditCard", "Debit", "QR", "Transfer"]);
    }

    #[tokio::test]
    async fn methods_confirm_cash_requires_method() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "M-REQ", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "m-req").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("1"), None).await.unwrap();
        // Missing method => 400.
        let err = s.confirm(sale.id, Some(acc.id), None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Missing account => 400.
        let err = s.confirm(sale.id, None, Some(cash)).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn methods_disallowed_pair_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "M-DENY", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "m-deny").await;
        let cash = cash_method(&s).await;
        // No allowlist row: Cash not allowed for this account.
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;
        let err = s
            .confirm(sale.id, Some(acc.id), Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // No stock/finance touch, still Draft without number.
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, tx_before);
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.sale.status, crate::models::SaleStatus::Draft);
        assert!(d.sale.sale_number.is_none());
    }

    #[tokio::test]
    async fn methods_mixed_payments_across_accounts() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "M-MIX", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let acc_a = seed_account(&s, "m-mix-a").await;
        let acc_b = seed_account(&s, "m-mix-b").await;
        let cash = cash_method(&s).await;
        let transfer = method_by_name(&s, "Transfer").await;
        allow(&s, acc_a.id, cash).await;
        allow(&s, acc_b.id, transfer).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 40
        s.confirm(sale.id, None, None).await.unwrap();
        s.record_payment(
            sale.id,
            acc_a.id,
            cash,
            dec("15"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        s.record_payment(
            sale.id,
            acc_b.id,
            transfer,
            dec("25"),
            NaiveDate::from_ymd_opt(2024, 5, 11).unwrap(),
        )
        .await
        .unwrap();
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.paid, dec("40"));
        assert_eq!(d.due, Decimal::ZERO);
        assert_eq!(d.payment_status, PaymentStatus::Paid);
        assert_eq!(d.payments.len(), 2);
        assert_eq!(tx_count(&pool).await, 2);
    }

    #[tokio::test]
    async fn methods_record_payment_rejects_disallowed_without_finance_touch() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "M-PAY-DENY", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "m-pay-deny").await;
        let cash = cash_method(&s).await;
        let qr = method_by_name(&s, "QR").await;
        allow(&s, acc.id, cash).await;
        // QR not allowed for this account.
        let sale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 20
        s.confirm(sale.id, None, None).await.unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(sale.id, acc.id, qr, dec("5"), sale_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, tx_before);
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.paid, Decimal::ZERO);
    }

    // -- money traceability: payment <-> transaction links ---------------------

    #[tokio::test]
    async fn link_cash_confirm_payment_carries_its_transaction_and_reference() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "LINK-CASH", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "link-cash").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 20

        let detail = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
        let number = detail.sale.sale_number.clone().unwrap();

        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 1);
        let tx_id = payments[0]
            .transaction_id
            .expect("payment must link the transaction it created");
        assert!(payments[0].refund_transaction_id.is_none());

        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, tx_id);
        assert_eq!(rows[0].reference.as_deref(), Some(number.as_str()));
        assert_eq!(rows[0].description, number);
    }

    #[tokio::test]
    async fn link_credit_payment_and_cancel_refund_keeps_original_transaction() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "LINK-CREDIT", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "link-credit").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 40
        let detail = s.confirm(sale.id, None, None).await.unwrap();
        let number = detail.sale.sale_number.clone().unwrap();

        let paid = s
            .record_payment(
                sale.id,
                acc.id,
                cash,
                dec("15"),
                NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
            )
            .await
            .unwrap();
        let paid_tx_id = paid
            .transaction_id
            .expect("credit payment must link its Income");

        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        let income = rows.iter().find(|t| t.id == paid_tx_id).unwrap();
        assert_eq!(income.kind, crate::models::TransactionKind::Income);
        assert_eq!(income.reference.as_deref(), Some(number.as_str()));

        s.cancel(sale.id, Some("refund".into())).await.unwrap();

        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 1);
        assert_eq!(
            payments[0].transaction_id,
            Some(paid_tx_id),
            "the original link must stay intact after cancel"
        );
        let refund_id = payments[0]
            .refund_transaction_id
            .expect("cancel must link the refund it created");
        assert_ne!(refund_id, paid_tx_id);

        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        let refund = rows.iter().find(|t| t.id == refund_id).unwrap();
        assert_eq!(refund.kind, crate::models::TransactionKind::Expense);
        assert_eq!(refund.amount, dec("15"));
        assert_eq!(refund.reference.as_deref(), Some(number.as_str()));
    }

    #[tokio::test]
    async fn link_two_payments_to_two_distinct_transactions() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "LINK-TWO", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "link-two").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: CREDIT_CUSTOMER_ID,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap(); // total 40
        let detail = s.confirm(sale.id, None, None).await.unwrap();
        let number = detail.sale.sale_number.clone().unwrap();

        let first = s
            .record_payment(
                sale.id,
                acc.id,
                cash,
                dec("15"),
                NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
            )
            .await
            .unwrap();
        let second = s
            .record_payment(
                sale.id,
                acc.id,
                cash,
                dec("25"),
                NaiveDate::from_ymd_opt(2024, 5, 11).unwrap(),
            )
            .await
            .unwrap();
        let first_tx = first.transaction_id.expect("first payment links its Income");
        let second_tx = second.transaction_id.expect("second payment links its Income");
        assert_ne!(first_tx, second_tx, "each payment links its own transaction");

        let payments = s.sales.list_payments(sale.id).await.unwrap();
        let linked: Vec<Option<i64>> = payments.iter().map(|p| p.transaction_id).collect();
        assert_eq!(linked, vec![Some(first_tx), Some(second_tx)]);

        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|t| t.reference.as_deref() == Some(number.as_str())));
    }

    #[tokio::test]
    async fn tri_editing_description_does_not_break_the_reference_link() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "TRI-EDIT-REF", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "tri-edit-ref").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        let detail = s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
        let number = detail.sale.sale_number.clone().unwrap();
        let tx_id = s.sales.list_payments(sale.id).await.unwrap()[0]
            .transaction_id
            .unwrap();

        // `description` is editable free text; the document link must not depend
        // on it, so editing it leaves `reference` (and the payment link) intact.
        let updated = s
            .transactions
            .update(tx_id, None, None, Some("edited by hand".into()), None)
            .await
            .unwrap();
        assert_eq!(updated.description, "edited by hand");
        assert_eq!(updated.reference.as_deref(), Some(number.as_str()));
        assert_eq!(
            s.sales.list_payments(sale.id).await.unwrap()[0].transaction_id,
            Some(tx_id)
        );
    }

    #[tokio::test]
    async fn tri_linked_transaction_cannot_be_deleted_out_from_under_a_payment() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "TRI-RESTRICT", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "tri-restrict").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: WALKIN_ID,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(sale.id, prod.id, dec("2"), None).await.unwrap();
        s.confirm(sale.id, Some(acc.id), Some(cash)).await.unwrap();
        let tx_id = s.sales.list_payments(sale.id).await.unwrap()[0]
            .transaction_id
            .unwrap();

        // RESTRICT FK: the movement that produced a payment cannot be deleted.
        assert!(s.transactions.delete(tx_id).await.is_err());
        assert!(s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap()
            .iter()
            .any(|t| t.id == tx_id));
    }

    // -- K2: mandatory customer, credit rules, ENFORCE_CREDIT_LIMIT ---------

    /// AC2: an unknown customer id is a 404; a known customer is stored with its
    /// name snapshotted at creation time.
    #[tokio::test]
    async fn k2_ac2_unknown_customer_is_404_and_name_is_snapshotted() {
        let (s, _) = svc().await;
        let err = s
            .create_draft(NewSale {
                customer_id: 99999,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        let customer = seed_customer(&s, "Ana", None, None).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: customer.id,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        assert_eq!(sale.customer_id, customer.id);
        assert_eq!(sale.customer_name, "Ana");
    }

    /// Deliverable rule: correcting the customer never rewrites history, so the
    /// snapshot on an existing sale survives a rename.
    #[tokio::test]
    async fn k2_snapshot_name_survives_customer_rename() {
        let (s, _) = svc().await;
        let customer = seed_customer(&s, "Ana", None, None).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: customer.id,
                payment_type: PaymentType::Cash,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();

        s.customers
            .update_customer(
                customer.id,
                crate::models::UpdateCustomer {
                    name: Some("Ana Pérez".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let stored = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(stored.customer_name, "Ana");
        assert_eq!(stored.customer_id, customer.id);
    }

    /// AC3: credit to the walk-in is rejected at confirm time and touches
    /// nothing: no sequence, no stock, no finance, still Draft.
    #[tokio::test]
    async fn k2_ac3_credit_walkin_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "K2-AC3", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let walkin = walkin_of(&s).await;
        let sale = draft_with_line(
            &s,
            walkin.id,
            PaymentType::Credit,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "2",
        )
        .await;

        let movements_before = movement_count(&pool).await;
        let txs_before = tx_count(&pool).await;
        let sequence_before = sale_sequence_last(&pool).await;

        let err = s.confirm(sale.id, None, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            detail.sale.status,
            crate::models::SaleStatus::Draft,
            "a rejected credit sale must stay Draft"
        );
        assert!(detail.sale.sale_number.is_none());
        assert_eq!(movement_count(&pool).await, movements_before);
        assert_eq!(tx_count(&pool).await, txs_before);
        assert_eq!(sale_sequence_last(&pool).await, sequence_before);
    }

    /// AC4/AC7: the limit check uses the projected debt (current debt + sale
    /// total) and rejects with that figure in the message. The credit draft's
    /// due date comes from the customer's payment term (AC7).
    #[tokio::test]
    async fn k2_ac4_credit_limit_blocks_over_limit_with_projected_figure() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "K2-AC4", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let customer = seed_customer(&s, "Limited", Some("100"), Some(30)).await;

        // Debt 30 stays within the limit of 100.
        let first = draft_with_line(
            &s,
            customer.id,
            PaymentType::Credit,
            None,
            prod.id,
            "3",
        )
        .await;
        assert_eq!(
            first.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            "due_date must default to sale_date + payment_days (2024-05-02 + 30)"
        );
        s.confirm(first.id, None, None).await.unwrap();

        // Projected 30 + 80 = 110 > 100 => 400 with the projection.
        let second = draft_with_line(
            &s,
            customer.id,
            PaymentType::Credit,
            None,
            prod.id,
            "8",
        )
        .await;
        let movements_before = movement_count(&pool).await;
        let err = s.confirm(second.id, None, None).await.unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("110"),
                    "the 400 must carry the projected debt: {msg}"
                );
                assert!(msg.contains("100"), "the 400 must carry the limit: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        let detail = s.get_detail(second.id).await.unwrap();
        assert_eq!(detail.sale.status, crate::models::SaleStatus::Draft);
        assert_eq!(
            movement_count(&pool).await,
            movements_before,
            "a blocked confirm must not move stock"
        );
    }

    /// AC5: with the flag off the same over-limit sale confirms, and the derived
    /// debt proves it is over the limit (the over_limit read is a later slice).
    #[tokio::test]
    async fn k2_ac5_flag_off_confirms_over_limit_sale() {
        let (s, _) = svc_with_credit_flag(true, false, false).await;
        let prod = seed_product(&s, "K2-AC5", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let customer = seed_customer(&s, "Flag Off", Some("50"), None).await;
        let sale = draft_with_line(
            &s,
            customer.id,
            PaymentType::Credit,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "6",
        )
        .await;

        let detail = s.confirm(sale.id, None, None).await.unwrap();
        assert_eq!(detail.sale.status, crate::models::SaleStatus::Confirmed);
        let debt = s.customer_debt(customer.id).await.unwrap();
        assert_eq!(debt, dec("60"));
        assert!(debt > dec("50"), "the customer is over the limit: {debt}");
    }

    /// AC6: a null limit is unlimited with the flag on or off.
    #[tokio::test]
    async fn k2_ac6_null_limit_never_blocks() {
        for enforce in [true, false] {
            let (s, _) = svc_with_credit_flag(true, false, enforce).await;
            let prod = seed_product(&s, "K2-AC6", "10").await;
            seed_stock(&s, prod.id, "100").await;
            let customer = seed_customer(&s, "No Limit", None, None).await;
            let sale = draft_with_line(
                &s,
                customer.id,
                PaymentType::Credit,
                Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                prod.id,
                "99",
            )
            .await;
            let detail = s.confirm(sale.id, None, None).await.unwrap();
            assert_eq!(
                detail.sale.status,
                crate::models::SaleStatus::Confirmed,
                "enforce_credit_limit={enforce} must not block a null limit"
            );
        }
    }

    /// AC7: a credit sale without a due date takes `sale_date + payment_days`;
    /// without a term the due date is required. An explicit date still wins.
    #[tokio::test]
    async fn k2_ac7_due_date_defaults_from_payment_days_or_400() {
        let (s, _) = svc().await;
        let term_customer = seed_customer(&s, "Term", None, Some(15)).await;
        let sale = s
            .create_draft(NewSale {
                customer_id: term_customer.id,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        assert_eq!(
            sale.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 5, 17).unwrap()),
            "due_date must default to sale_date + payment_days"
        );

        // An explicit date wins over the term.
        let explicit = s
            .create_draft(NewSale {
                customer_id: term_customer.id,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 7, 1).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        assert_eq!(
            explicit.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 7, 1).unwrap())
        );

        // No term and no date => 400 at creation.
        let no_term = seed_customer(&s, "No Term", None, None).await;
        let err = s
            .create_draft(NewSale {
                customer_id: no_term.id,
                payment_type: PaymentType::Credit,
                sale_date: sale_date(),
                due_date: None,
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    /// Triangulation: payments and cancelled sales move the debt the limit is
    /// checked against, and the boundary (projected == limit) does not block.
    #[tokio::test]
    async fn k2_tri_debt_ignores_cancelled_and_paid_amounts() {
        let (s, _) = svc_with_flags(true, true).await;
        let prod = seed_product(&s, "K2-TRI", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let acc = seed_account(&s, "k2-tri").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let customer = seed_customer(&s, "Tri", Some("100"), None).await;
        let due = Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());

        // Debt 60, within the limit.
        let first =
            draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "6").await;
        s.confirm(first.id, None, None).await.unwrap();

        // Paying 40 leaves 20 of debt, so another 70 fits (90 <= 100).
        s.record_payment(first.id, acc.id, cash, dec("40"), sale_date())
            .await
            .unwrap();
        let second =
            draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "7").await;
        s.confirm(second.id, None, None).await.unwrap();

        // Cancelling the first sale removes its remaining 20 => debt 70.
        s.cancel(first.id, Some("tri".into())).await.unwrap();
        assert_eq!(s.customer_debt(customer.id).await.unwrap(), dec("70"));

        // Boundary: projected debt exactly equal to the limit is allowed.
        let boundary =
            draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "3").await;
        let detail = s.confirm(boundary.id, None, None).await.unwrap();
        assert_eq!(detail.sale.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(s.customer_debt(customer.id).await.unwrap(), dec("100"));
    }
}
