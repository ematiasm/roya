// M3 purchases orchestrator (mirror of M2 SalesService).
//
// PurchasesService calls InventoryService for stock In (reason Purchase) on
// confirm and stock Out (reason Purchase-return) on cancel, TransactionService
// for the Cash Expense / payment Expenses / cancel Income refunds with
// reference = purchase_number, SupplierService for the satellite cost update on
// confirm, and PaymentMethodService for the (account, method) allowlist check.
// It never SQLs `transactions`, `stock_movements`, `accounts` or
// `payment_methods` for writes.
//
// Numbering: YYYY-PURCH-NNNNNN assigned on confirm via `doc_sequences` consumer
// PURCH. Draft touches nothing. Cash confirm creates 1 payment + 1 Expense;
// Credit confirm creates the payable only, no Expense until paid. Confirm
// updates the satellite cost per (product, supplier); the cost date is
// pre-validated for every line before any mutation so a rejected backdated cost
// can never leave stock or finance already applied. Cancelling a Confirmed
// purchase returns stock (Purchase-return) and refunds paid amounts as Income
// per originating account; a refund is money entering, so it is never blocked
// by the negative-balance guard.
//
// Atomicity note: mirrors sales. No shared SQLite tx across services; we
// pre-validate everything, then mutate in order sequence -> stock -> finance ->
// document -> satellite. The only expected side effect on a late failure is an
// abandoned sequence number.
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use std::collections::HashSet;

use crate::error::{AppError, AppResult};
use crate::models::{
    format_purchase_number, MovementReason, MovementType, NewMovement, NewPurchase,
    PaymentType, Purchase, PurchaseDetail, PurchaseLine, PurchasePayment, PurchaseStatus,
    PurchaseSuggestion, PurchaseSuggestionWithoutSupplier, PurchaseSuggestions,
    UpdatePurchaseDraft,
};

#[derive(Clone)]
pub struct PurchasesService<PR, DR, SR, CR, C, P, B, S, A, T, PM>
where
    PR: crate::repositories::PurchaseRepository,
    DR: crate::repositories::DocSequenceRepository,
    SR: crate::repositories::SupplierRepository,
    CR: crate::repositories::ProductSupplierCostRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PM: crate::repositories::PaymentMethodRepository,
{
    pub purchases: PR,
    pub sequences: DR,
    pub suppliers: crate::services::SupplierService<SR, CR>,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
    pub payment_methods: crate::services::PaymentMethodService<PM>,
}

impl<PR, DR, SR, CR, C, P, B, S, A, T, PM> PurchasesService<PR, DR, SR, CR, C, P, B, S, A, T, PM>
where
    PR: crate::repositories::PurchaseRepository,
    DR: crate::repositories::DocSequenceRepository,
    SR: crate::repositories::SupplierRepository,
    CR: crate::repositories::ProductSupplierCostRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PM: crate::repositories::PaymentMethodRepository,
{
    pub fn new(
        purchases: PR,
        sequences: DR,
        suppliers: crate::services::SupplierService<SR, CR>,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
        payment_methods: crate::services::PaymentMethodService<PM>,
    ) -> Self {
        Self {
            purchases,
            sequences,
            suppliers,
            inventory,
            transactions,
            payment_methods,
        }
    }

    // -- validation helpers ---------------------------------------------------

    fn clean_notes(notes: &Option<String>) -> AppResult<String> {
        let s = notes.clone().unwrap_or_default();
        if s.chars().count() > 512 {
            return Err(AppError::Validation("notes must be <= 512 chars".into()));
        }
        Ok(s.trim().to_string())
    }

    fn clean_invoice(invoice: &Option<String>) -> AppResult<Option<String>> {
        match invoice {
            None => Ok(None),
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Ok(None)
                } else if t.chars().count() > 64 {
                    Err(AppError::Validation(
                        "supplier_invoice_no must be <= 64 chars".into(),
                    ))
                } else {
                    Ok(Some(t.to_string()))
                }
            }
        }
    }

    fn validate_dates(
        payment_type: PaymentType,
        purchase_date: NaiveDate,
        due_date: Option<NaiveDate>,
    ) -> AppResult<()> {
        match payment_type {
            PaymentType::Cash => {
                if due_date.is_some() {
                    return Err(AppError::Validation("due_date must be NULL for Cash".into()));
                }
            }
            PaymentType::Credit => {
                let due = due_date.ok_or_else(|| {
                    AppError::Validation("due_date is required for Credit".into())
                })?;
                if due < purchase_date {
                    return Err(AppError::Validation(
                        "due_date must be >= purchase_date".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn totals(lines: &[PurchaseLine], payments: &[PurchasePayment]) -> (Decimal, Decimal, Decimal) {
        let mut total = Decimal::ZERO;
        for l in lines {
            total += l.subtotal();
        }
        let mut paid = Decimal::ZERO;
        for p in payments {
            paid += p.amount;
        }
        let due = total - paid;
        (total, paid, due)
    }

    async fn detail_for(&self, purchase: Purchase) -> AppResult<PurchaseDetail> {
        let lines = self.purchases.list_lines(purchase.id).await?;
        let payments = self.purchases.list_payments(purchase.id).await?;
        let (total, paid, due) = Self::totals(&lines, &payments);
        let payment_status = PurchaseDetail::payment_status_for(total, paid);
        Ok(PurchaseDetail {
            purchase,
            lines,
            payments,
            total,
            paid,
            due,
            payment_status,
        })
    }

    fn ensure_draft(purchase: &Purchase) -> AppResult<()> {
        if purchase.status != PurchaseStatus::Draft {
            return Err(AppError::Validation(format!(
                "purchase {} is not editable (status {})",
                purchase.id, purchase.status
            )));
        }
        Ok(())
    }

    /// A purchase cannot contain the same product twice: the satellite holds
    /// exactly one cost per (product, supplier), so two different line costs for
    /// the same product in one purchase have no defined answer. Buying at two
    /// different prices means two purchases. `exclude_line` ignores the line
    /// being updated so it does not collide with itself.
    fn ensure_unique_product(
        lines: &[PurchaseLine],
        product_id: i64,
        exclude_line: Option<i64>,
    ) -> AppResult<()> {
        let duplicated = lines
            .iter()
            .any(|l| l.product_id == product_id && Some(l.id) != exclude_line);
        if duplicated {
            return Err(AppError::Validation(format!(
                "product {product_id} already has a line in this purchase; record a different price in a separate purchase"
            )));
        }
        Ok(())
    }

    // -- Draft -----------------------------------------------------------------

    pub async fn create_draft(&self, input: NewPurchase) -> AppResult<Purchase> {
        if !self.suppliers.suppliers.exists(input.supplier_id).await? {
            return Err(AppError::NotFound(format!(
                "supplier {} not found",
                input.supplier_id
            )));
        }
        let notes = Self::clean_notes(&input.notes)?;
        let invoice = Self::clean_invoice(&input.supplier_invoice_no)?;
        Self::validate_dates(input.payment_type, input.purchase_date, input.due_date)?;
        self.purchases
            .create_purchase(&NewPurchase {
                supplier_id: input.supplier_id,
                payment_type: input.payment_type,
                purchase_date: input.purchase_date,
                due_date: input.due_date,
                supplier_invoice_no: invoice,
                notes: Some(notes),
            })
            .await
    }

    pub async fn update_draft(
        &self,
        id: i64,
        patch: UpdatePurchaseDraft,
    ) -> AppResult<Purchase> {
        let purchase = self
            .purchases
            .find_purchase(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {id} not found")))?;
        Self::ensure_draft(&purchase)?;

        if let Some(supplier_id) = patch.supplier_id {
            if !self.suppliers.suppliers.exists(supplier_id).await? {
                return Err(AppError::NotFound(format!(
                    "supplier {supplier_id} not found"
                )));
            }
        }
        if let Some(ref notes) = patch.notes {
            if notes.chars().count() > 512 {
                return Err(AppError::Validation("notes must be <= 512 chars".into()));
            }
        }
        if let Some(ref invoice) = patch.supplier_invoice_no {
            Self::clean_invoice(invoice)?;
        }

        // Validate prospective header before delegating.
        let new_payment_type = patch.payment_type.unwrap_or(purchase.payment_type);
        let new_date = patch.purchase_date.unwrap_or(purchase.purchase_date);
        let new_due = match &patch.due_date {
            Some(inner) => *inner,
            None => purchase.due_date,
        };
        Self::validate_dates(new_payment_type, new_date, new_due)?;

        let norm = UpdatePurchaseDraft {
            supplier_id: patch.supplier_id,
            payment_type: patch.payment_type,
            purchase_date: patch.purchase_date,
            due_date: patch.due_date,
            supplier_invoice_no: patch.supplier_invoice_no.map(|opt| {
                opt.map(|s| s.trim().to_string())
            }),
            notes: patch.notes.map(|s| s.trim().to_string()),
        };
        self.purchases.update_draft(id, &norm).await
    }

    pub async fn add_line(
        &self,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Option<Decimal>,
    ) -> AppResult<PurchaseLine> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        Self::ensure_draft(&purchase)?;
        if qty <= Decimal::ZERO {
            return Err(AppError::Validation("qty must be > 0".into()));
        }
        // 404 on unknown product (AC5).
        let product = self.inventory.get_product(product_id).await?;
        // Duplicate product on the same purchase is a 400 before any write.
        let lines = self.purchases.list_lines(purchase_id).await?;
        Self::ensure_unique_product(&lines, product_id, None)?;
        let cost = match unit_cost {
            Some(c) => {
                if c < Decimal::ZERO {
                    return Err(AppError::Validation("unit_cost cannot be negative".into()));
                }
                c
            }
            // Manual line with no supplier cost yet: fall back to the product
            // column, which purchases never write.
            None => product.cost_price,
        };
        self.purchases
            .create_line(purchase_id, product_id, qty, cost)
            .await
    }

    pub async fn update_line(
        &self,
        line_id: i64,
        qty: Decimal,
        unit_cost: Decimal,
    ) -> AppResult<PurchaseLine> {
        let line = self
            .purchases
            .find_line(line_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase line {line_id} not found")))?;
        let purchase = self
            .purchases
            .find_purchase(line.purchase_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("purchase {} not found", line.purchase_id))
            })?;
        Self::ensure_draft(&purchase)?;
        if qty <= Decimal::ZERO {
            return Err(AppError::Validation("qty must be > 0".into()));
        }
        if unit_cost < Decimal::ZERO {
            return Err(AppError::Validation("unit_cost cannot be negative".into()));
        }
        // Same uniqueness rule as add_line; the line's own product is excluded.
        let lines = self.purchases.list_lines(line.purchase_id).await?;
        Self::ensure_unique_product(&lines, line.product_id, Some(line_id))?;
        self.purchases.update_line(line_id, qty, unit_cost).await
    }

    pub async fn remove_line(&self, line_id: i64) -> AppResult<()> {
        let line = self
            .purchases
            .find_line(line_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase line {line_id} not found")))?;
        let purchase = self
            .purchases
            .find_purchase(line.purchase_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("purchase {} not found", line.purchase_id))
            })?;
        Self::ensure_draft(&purchase)?;
        let deleted = self.purchases.delete_line(line_id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!(
                "purchase line {line_id} not found"
            )));
        }
        Ok(())
    }

    pub async fn get_detail(&self, purchase_id: i64) -> AppResult<PurchaseDetail> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        self.detail_for(purchase).await
    }

    /// All purchases with derived totals, oldest first (repository order).
    pub async fn list_details(&self) -> AppResult<Vec<PurchaseDetail>> {
        let purchases = self.purchases.list_purchases().await?;
        let mut out = Vec::with_capacity(purchases.len());
        for purchase in purchases {
            out.push(self.detail_for(purchase).await?);
        }
        Ok(out)
    }

    /// Outstanding payables: Confirmed purchases with due > 0.
    pub async fn outstanding_payables(&self) -> AppResult<Vec<PurchaseDetail>> {
        let all = self.list_details().await?;
        Ok(all
            .into_iter()
            .filter(|d| d.purchase.status == PurchaseStatus::Confirmed && d.due > Decimal::ZERO)
            .collect())
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(
        &self,
        purchase_id: i64,
        cash_account_id: Option<i64>,
        cash_method_id: Option<i64>,
    ) -> AppResult<PurchaseDetail> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        if purchase.status == PurchaseStatus::Confirmed {
            return Err(AppError::Validation("purchase already confirmed".into()));
        }
        if purchase.status == PurchaseStatus::Cancelled {
            return Err(AppError::Validation(
                "cancelled purchase cannot be confirmed".into(),
            ));
        }

        let lines = self.purchases.list_lines(purchase_id).await?;
        if lines.is_empty() {
            return Err(AppError::Validation(
                "cannot confirm purchase with no lines".into(),
            ));
        }

        // Defensive: add_line/update_line already reject a repeated product, but
        // a confirm must never accept one either. The satellite holds exactly
        // one cost per (product, supplier), so two line costs for the same
        // product in one purchase have no defined answer.
        let mut seen_products: HashSet<i64> = HashSet::new();
        for line in &lines {
            if !seen_products.insert(line.product_id) {
                return Err(AppError::Validation(format!(
                    "product {} appears on more than one line; record a different price in a separate purchase",
                    line.product_id
                )));
            }
        }

        // Validate lines and collect the stock-tracked ones (services and
        // untracked products receive no stock movement).
        let mut tracked: Vec<PurchaseLine> = Vec::new();
        for line in &lines {
            if line.qty <= Decimal::ZERO {
                return Err(AppError::Validation("qty must be > 0".into()));
            }
            if line.unit_cost < Decimal::ZERO {
                return Err(AppError::Validation("unit_cost cannot be negative".into()));
            }
            let product = self.inventory.get_product(line.product_id).await?;
            if !product.is_active {
                return Err(AppError::Validation(format!(
                    "product {} is inactive",
                    product.id
                )));
            }
            if product.kind == crate::models::ProductKind::Product && product.track_stock {
                tracked.push(line.clone());
            }
        }

        // CRITICAL: pre-validate the satellite cost date for every line before
        // any mutation. SupplierService::record_cost rejects a date earlier than
        // the satellite's current cost date; failing after stock/finance would
        // leave a half-applied confirm.
        for line in &lines {
            if let Some(existing) = self
                .suppliers
                .find_cost(line.product_id, purchase.supplier_id)
                .await?
            {
                if purchase.purchase_date < existing.current_cost_updated_at {
                    return Err(AppError::Validation(format!(
                        "purchase_date {} precedes the recorded cost date {} for product {} and supplier {}",
                        purchase.purchase_date,
                        existing.current_cost_updated_at,
                        line.product_id,
                        purchase.supplier_id
                    )));
                }
            }
        }

        let (total, _, _) = Self::totals(
            &lines,
            &self.purchases.list_payments(purchase_id).await?,
        );

        match purchase.payment_type {
            PaymentType::Cash => {
                let account_id = cash_account_id.ok_or_else(|| {
                    AppError::Validation("cash purchase requires account and method".into())
                })?;
                let method_id = cash_method_id.ok_or_else(|| {
                    AppError::Validation("cash purchase requires account and method".into())
                })?;
                if purchase.due_date.is_some() {
                    return Err(AppError::Validation("due_date must be NULL for Cash".into()));
                }
                if !self.transactions.accounts.exists(account_id).await? {
                    return Err(AppError::NotFound(format!(
                        "account {account_id} not found"
                    )));
                }
                // Allowlist validated before any stock/sequence/finance touch.
                self.payment_methods
                    .require_allowed(account_id, method_id)
                    .await?;
                // The Cash Expense leaves the account: keep the M0 overdraft
                // guard from failing after stock/finance already applied.
                if !self.transactions.allow_negative && total > Decimal::ZERO {
                    let current = self
                        .transactions
                        .transactions
                        .balance_for_account(account_id)
                        .await?;
                    if current - total < Decimal::ZERO {
                        return Err(AppError::Validation(format!(
                            "insufficient funds: balance {current} would become {}",
                            current - total
                        )));
                    }
                }
            }
            PaymentType::Credit => {
                if cash_account_id.is_some() || cash_method_id.is_some() {
                    return Err(AppError::Validation(
                        "credit purchase must not include cash account".into(),
                    ));
                }
                if purchase.due_date.is_none() {
                    return Err(AppError::Validation(
                        "due_date is required for Credit".into(),
                    ));
                }
            }
        }

        // Assign number atomically via doc_sequences row UPDATE.
        let year = purchase.purchase_date.year();
        let seq = self.sequences.next_number("PURCH", year).await?;
        let purchase_number = format_purchase_number(year, seq);

        // Stock In (reason Purchase) for tracked Product lines only.
        for line in &tracked {
            self.inventory
                .record_movement(NewMovement {
                    product_id: line.product_id,
                    qty: line.qty,
                    movement_type: MovementType::In,
                    reason: MovementReason::Purchase,
                    reference: purchase_number.clone(),
                    date: purchase.purchase_date,
                })
                .await?;
        }

        // Finance: Cash => 1 payment + 1 Expense now; Credit => payable only.
        // The Expense is stamped with reference = purchase_number and linked back
        // from the payment row it produced.
        if purchase.payment_type == PaymentType::Cash && total > Decimal::ZERO {
            let account_id = cash_account_id.expect("validated above");
            let method_id = cash_method_id.expect("validated above");
            let expense = self
                .transactions
                .create_with_reference(
                    account_id,
                    crate::models::TransactionKind::Expense,
                    total,
                    Some(purchase_number.clone()),
                    Some(purchase_number.clone()),
                    purchase.purchase_date,
                )
                .await?;
            self.purchases
                .create_payment(
                    purchase_id,
                    account_id,
                    method_id,
                    total,
                    purchase.purchase_date,
                    Some(expense.id),
                )
                .await?;
        }

        let confirmed = self
            .purchases
            .set_confirmed(purchase_id, &purchase_number)
            .await?;

        // AC9: after a successful confirm, record the line cost in the satellite
        // (one row per product/supplier pair). The uniqueness guard above means
        // each product appears on exactly one line.
        for line in &lines {
            self.suppliers
                .record_cost(
                    line.product_id,
                    purchase.supplier_id,
                    line.unit_cost,
                    purchase.purchase_date,
                )
                .await?;
        }

        self.detail_for(confirmed).await
    }

    // -- Pay (Credit) ------------------------------------------------------------

    pub async fn record_payment(
        &self,
        purchase_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
    ) -> AppResult<PurchasePayment> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        if purchase.status != PurchaseStatus::Confirmed {
            return Err(AppError::Validation(
                "payments require a Confirmed purchase".into(),
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
        self.payment_methods
            .require_allowed(account_id, method_id)
            .await?;
        let lines = self.purchases.list_lines(purchase_id).await?;
        let payments = self.purchases.list_payments(purchase_id).await?;
        let (total, paid, _) = Self::totals(&lines, &payments);
        if paid + amount > total {
            return Err(AppError::Validation(format!(
                "overpay rejected: paid {paid} + {amount} exceeds total {total}"
            )));
        }
        let purchase_number = purchase
            .purchase_number
            .clone()
            .ok_or_else(|| {
                AppError::Internal("confirmed purchase missing purchase_number".into())
            })?;
        // Each payment generates one M0 Expense stamped with reference =
        // purchase_number and linked from the payment row it produced.
        let expense = self
            .transactions
            .create_with_reference(
                account_id,
                crate::models::TransactionKind::Expense,
                amount,
                Some(purchase_number.clone()),
                Some(purchase_number),
                date,
            )
            .await?;
        self.purchases
            .create_payment(
                purchase_id,
                account_id,
                method_id,
                amount,
                date,
                Some(expense.id),
            )
            .await
    }

    // -- Cancel / purchase return --------------------------------------------------

    pub async fn cancel(
        &self,
        purchase_id: i64,
        reason: Option<String>,
    ) -> AppResult<PurchaseDetail> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        if purchase.status == PurchaseStatus::Cancelled {
            return Err(AppError::Validation("purchase already cancelled".into()));
        }
        if purchase.status == PurchaseStatus::Draft {
            // Draft -> Cancelled: discard, no stock/finance/satellite side effect.
            let cancelled = self
                .purchases
                .set_cancelled(purchase_id, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        // Confirmed -> Cancelled: goods back to the supplier + refunds.
        let lines = self.purchases.list_lines(purchase_id).await?;
        let payments = self.purchases.list_payments(purchase_id).await?;
        let purchase_number = purchase
            .purchase_number
            .clone()
            .ok_or_else(|| {
                AppError::Internal("confirmed purchase missing purchase_number".into())
            })?;

        // Pre-validate products (for the return movement) and refund accounts.
        let mut tracked: Vec<PurchaseLine> = Vec::new();
        for line in &lines {
            let product = self.inventory.get_product(line.product_id).await?;
            if product.kind == crate::models::ProductKind::Product && product.track_stock {
                if !product.is_active {
                    return Err(AppError::Validation(format!(
                        "product {} is inactive",
                        product.id
                    )));
                }
                tracked.push(line.clone());
            }
        }
        for pay in &payments {
            if !self.transactions.accounts.exists(pay.account_id).await? {
                return Err(AppError::NotFound(format!(
                    "account {} not found",
                    pay.account_id
                )));
            }
        }

        // Stock Out (reason Purchase-return) for tracked lines.
        for line in &tracked {
            self.inventory
                .record_movement(NewMovement {
                    product_id: line.product_id,
                    qty: line.qty,
                    movement_type: MovementType::Out,
                    reason: MovementReason::PurchaseReturn,
                    reference: purchase_number.clone(),
                    date: purchase.purchase_date,
                })
                .await?;
        }

        // Refund Income per paid amount to the originating accounts. A purchase
        // refund is money entering: no negative-balance guard applies. Each refund
        // is linked back from the payment row it refunds.
        for pay in &payments {
            let refund = self
                .transactions
                .create_with_reference(
                    pay.account_id,
                    crate::models::TransactionKind::Income,
                    pay.amount,
                    Some(purchase_number.clone()),
                    Some(purchase_number.clone()),
                    purchase.purchase_date,
                )
                .await?;
            self.purchases
                .set_payment_refund_transaction(pay.id, refund.id)
                .await?;
        }

        let cancelled = self
            .purchases
            .set_cancelled(purchase_id, reason.as_deref())
            .await?;
        self.detail_for(cancelled).await
    }

    // -- Suggestion builder (the pedido) -------------------------------------------

    /// Low-stock products with their suggested reorder quantity and chosen
    /// supplier. The chosen supplier is the preferred one, else the cheapest
    /// current satellite cost. Products without a satellite row are returned in
    /// `without_supplier`, never silently dropped. Stock is read through
    /// InventoryService, never with raw SQL.
    pub async fn suggestions(&self) -> AppResult<PurchaseSuggestions> {
        let low = self.inventory.low_stock().await?;
        let mut suggestions = Vec::new();
        let mut without_supplier = Vec::new();
        for ps in low {
            let suggested_qty = ps.suggested.unwrap_or(Decimal::ZERO);
            let costs = self.suppliers.list_costs_for_product(ps.product.id).await?;
            if costs.is_empty() {
                without_supplier.push(PurchaseSuggestionWithoutSupplier {
                    product: ps.product,
                    stock: ps.stock,
                    suggested_qty,
                });
                continue;
            }
            let chosen = costs
                .iter()
                .find(|c| c.is_preferred)
                .or_else(|| costs.iter().min_by(|a, b| a.current_cost.cmp(&b.current_cost)))
                .expect("costs is not empty");
            let supplier = self
                .suppliers
                .suppliers
                .find_by_id(chosen.supplier_id)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!(
                        "supplier {} for cost row {} not found",
                        chosen.supplier_id, chosen.id
                    ))
                })?;
            let unit_cost = chosen.current_cost;
            suggestions.push(PurchaseSuggestion {
                product: ps.product,
                stock: ps.stock,
                suggested_qty,
                supplier_id: supplier.id,
                supplier_name: supplier.name,
                unit_cost,
                subtotal: suggested_qty * unit_cost,
            });
        }
        Ok(PurchaseSuggestions {
            suggestions,
            without_supplier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        MovementReason, MovementType, NewMovement, NewProduct, NewSupplier, PriceAlert,
        ProductKind, PurchaseStatus, TransactionKind, UpdatePurchaseDraft,
    };
    use crate::repositories::{
        AccountRepository, DocSequenceRepository, PaymentMethodRepository, PurchaseRepository,
        SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteDocSequenceRepository, SqlitePaymentMethodRepository, SqliteProductRepository,
        SqliteProductSupplierCostRepository, SqlitePurchaseRepository,
        SqliteStockMovementRepository, SqliteSupplierRepository, SqliteTransactionRepository,
        StockMovementRepository, TransactionRepository,
    };
    use crate::services::{
        InventoryService, PaymentMethodService, SupplierService, TransactionService,
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    type Svc = PurchasesService<
        SqlitePurchaseRepository,
        SqliteDocSequenceRepository,
        SqliteSupplierRepository,
        SqliteProductSupplierCostRepository,
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
        SqliteAccountRepository,
        SqliteTransactionRepository,
        SqlitePaymentMethodRepository,
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

    async fn svc_with_flags(allow_stock: bool, allow_balance: bool) -> (Svc, sqlx::SqlitePool) {
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
        let suppliers = SupplierService::new(
            SqliteSupplierRepository::new(pool.clone()),
            SqliteProductSupplierCostRepository::new(pool.clone()),
        );
        let s = PurchasesService::new(
            SqlitePurchaseRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            suppliers,
            inventory,
            transactions,
            PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone())),
        );
        (s, pool)
    }

    async fn svc() -> (Svc, sqlx::SqlitePool) {
        svc_with_flags(true, true).await
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn purchase_date() -> NaiveDate {
        d(2024, 5, 2)
    }

    async fn seed_product(s: &Svc, sku: &str, cost: &str) -> crate::models::Product {
        seed_tracked(s, sku, cost, "5", "50").await
    }

    async fn seed_tracked(
        s: &Svc,
        sku: &str,
        cost: &str,
        min: &str,
        max: &str,
    ) -> crate::models::Product {
        s.inventory
            .create_product(NewProduct {
                sku: sku.into(),
                name: format!("prod {sku}"),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: dec("10"),
                cost_price: dec(cost),
                track_stock: true,
                min_stock: Some(dec(min)),
                max_stock: Some(dec(max)),
                location: None,
                notes: None,
            })
            .await
            .unwrap()
    }

    async fn seed_service(s: &Svc, sku: &str, cost: &str) -> crate::models::Product {
        s.inventory
            .create_product(NewProduct {
                sku: sku.into(),
                name: format!("svc {sku}"),
                kind: ProductKind::Service,
                category_id: None,
                unit: "hr".into(),
                sale_price: dec("30"),
                cost_price: dec(cost),
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
                date: d(2024, 5, 1),
            })
            .await
            .unwrap();
    }

    async fn seed_supplier(s: &Svc, name: &str) -> crate::models::Supplier {
        s.suppliers
            .create_supplier(NewSupplier {
                name: name.into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap()
    }

    async fn seed_account(s: &Svc, name: &str) -> crate::models::Account {
        s.transactions.accounts.create(name).await.unwrap()
    }

    async fn cash_method(s: &Svc) -> i64 {
        s.payment_methods
            .methods
            .find_method_by_name("Cash")
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn method_by_name(s: &Svc, name: &str) -> i64 {
        s.payment_methods
            .methods
            .find_method_by_name(name)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn allow(s: &Svc, account_id: i64, method_id: i64) {
        s.payment_methods.methods.allow(account_id, method_id).await.unwrap()
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

    async fn draft_cash(s: &Svc, supplier_id: i64) -> Purchase {
        s.create_draft(NewPurchase {
            supplier_id,
            payment_type: PaymentType::Cash,
            purchase_date: purchase_date(),
            due_date: None,
            supplier_invoice_no: None,
            notes: None,
        })
        .await
        .unwrap()
    }

    async fn draft_credit(s: &Svc, supplier_id: i64) -> Purchase {
        s.create_draft(NewPurchase {
            supplier_id,
            payment_type: PaymentType::Credit,
            purchase_date: purchase_date(),
            due_date: Some(d(2024, 6, 1)),
            supplier_invoice_no: None,
            notes: None,
        })
        .await
        .unwrap()
    }

    // -- AC1 ------------------------------------------------------------------

    #[tokio::test]
    async fn ac1_draft_touches_no_stock_finance_or_satellite() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC1-P", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC1 SUP").await;

        let purchase = draft_cash(&s, sup.id).await;
        assert!(purchase.purchase_number.is_none());
        let line = s
            .add_line(purchase.id, prod.id, dec("2"), Some(dec("4")))
            .await
            .unwrap();
        let updated = s.update_line(line.id, dec("3"), dec("4.5")).await.unwrap();
        assert_eq!(updated.qty, dec("3"));
        assert_eq!(updated.unit_cost, dec("4.5"));
        let edited = s
            .update_draft(
                purchase.id,
                UpdatePurchaseDraft {
                    notes: Some(" pedido ".into()),
                    supplier_invoice_no: Some(Some(" A-001 ".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.notes, "pedido");
        assert_eq!(edited.supplier_invoice_no.as_deref(), Some("A-001"));
        s.remove_line(line.id).await.unwrap();

        assert_eq!(movement_count(&pool).await, 1, "only the initial stock move");
        assert_eq!(tx_count(&pool).await, 0);
        assert!(s.suppliers.find_cost(prod.id, sup.id).await.unwrap().is_none());
        assert!(
            s.sequences.current("PURCH", 2024).await.unwrap().is_none(),
            "Draft must not consume a document number"
        );
        let stored = s.inventory.get_product(prod.id).await.unwrap();
        assert_eq!(stored.cost_price, dec("5"));
    }

    // -- AC2: Confirm Cash ------------------------------------------------------

    #[tokio::test]
    async fn ac2_confirm_cash_assigns_number_receives_stock_posts_expense() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC2", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC2 SUP").await;
        let acc = seed_account(&s, "caja2").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("3"), Some(dec("4")))
            .await
            .unwrap(); // total 12

        let detail = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();
        assert_eq!(number, "2024-PURCH-000001");
        assert_eq!(detail.purchase.status, PurchaseStatus::Confirmed);
        assert!(detail.purchase.confirmed_at.is_some());
        assert_eq!(detail.total, dec("12"));
        assert_eq!(detail.paid, dec("12"));
        assert_eq!(detail.due, Decimal::ZERO);
        assert_eq!(detail.payment_status, crate::models::PaymentStatus::Paid);

        // Stock In reason Purchase, reference = purchase_number.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("13"));
        let moves = s.inventory.movements.list_by_product(prod.id).await.unwrap();
        let received = moves
            .iter()
            .find(|m| m.reason == MovementReason::Purchase)
            .unwrap();
        assert_eq!(received.movement_type, MovementType::In);
        assert_eq!(received.qty, dec("3"));
        assert_eq!(received.reference, number);

        // 1 payment + 1 Expense.
        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
        assert_eq!(payments.len(), 1);
        assert_eq!(payments[0].amount, dec("12"));
        assert_eq!(payments[0].account_id, acc.id);
        assert_eq!(payments[0].method_id, cash);
        assert_eq!(tx_count(&pool).await, 1);
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, TransactionKind::Expense);
        assert_eq!(rows[0].amount, dec("12"));
        assert_eq!(rows[0].description, number);
    }

    // -- AC3: Confirm Credit ----------------------------------------------------

    #[tokio::test]
    async fn ac3_confirm_credit_receives_stock_without_expense() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC3", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC3 SUP").await;

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("12")))
            .await
            .unwrap(); // total 24

        let detail = s.confirm(purchase.id, None, None).await.unwrap();
        assert!(detail.purchase.purchase_number.is_some());
        assert_eq!(detail.total, dec("24"));
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("24"));
        assert_eq!(detail.payment_status, crate::models::PaymentStatus::Unpaid);
        assert_eq!(detail.purchase.due_date, Some(d(2024, 6, 1)));
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("12"));
        assert_eq!(tx_count(&pool).await, 0, "Credit confirm posts no Expense");
        assert!(s.purchases.list_payments(purchase.id).await.unwrap().is_empty());
    }

    // -- AC4: Credit payments ---------------------------------------------------

    #[tokio::test]
    async fn ac4_credit_payments_post_expenses_overpay_rejected_paid_at_zero_due() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC4", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC4 SUP").await;
        let acc = seed_account(&s, "banco4").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("20")))
            .await
            .unwrap(); // total 40
        let detail = s.confirm(purchase.id, None, None).await.unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        s.record_payment(purchase.id, acc.id, cash, dec("15"), d(2024, 5, 10))
            .await
            .unwrap();
        let d1 = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(d1.paid, dec("15"));
        assert_eq!(d1.due, dec("25"));
        assert_eq!(d1.payment_status, crate::models::PaymentStatus::Partial);
        assert_eq!(tx_count(&pool).await, 1);

        // Overpay rejected with no extra finance row.
        let err = s
            .record_payment(purchase.id, acc.id, cash, dec("30"), d(2024, 5, 11))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, 1);

        // Paying the remainder closes the payable.
        s.record_payment(purchase.id, acc.id, cash, dec("25"), d(2024, 5, 12))
            .await
            .unwrap();
        let d2 = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(d2.paid, dec("40"));
        assert_eq!(d2.due, Decimal::ZERO);
        assert_eq!(d2.payment_status, crate::models::PaymentStatus::Paid);
        assert_eq!(tx_count(&pool).await, 2);
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert!(rows.iter().all(|t| t.kind == TransactionKind::Expense));
        assert!(rows.iter().all(|t| t.description == number));
    }

    // -- AC5: unknown refs + bad values ------------------------------------------

    #[tokio::test]
    async fn ac5_unknown_supplier_product_account_and_bad_values() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "AC5", "10").await;
        seed_stock(&s, prod.id, "5").await;
        let sup = seed_supplier(&s, "AC5 SUP").await;
        let acc = seed_account(&s, "caja5").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // Unknown supplier on create/update => 404.
        let err = s
            .create_draft(NewPurchase {
                supplier_id: 999_999,
                payment_type: PaymentType::Cash,
                purchase_date: purchase_date(),
                due_date: None,
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        let purchase = draft_cash(&s, sup.id).await;
        let err = s
            .update_draft(
                purchase.id,
                UpdatePurchaseDraft {
                    supplier_id: Some(999_999),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown product => 404.
        let err = s
            .add_line(purchase.id, 999_999, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // qty <= 0 / unit_cost < 0 => 400.
        for bad_qty in [dec("0"), dec("-1")] {
            let err = s
                .add_line(purchase.id, prod.id, bad_qty, None)
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        let err = s
            .add_line(purchase.id, prod.id, dec("1"), Some(dec("-0.01")))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // unit_cost = 0 is legal.
        let zero = s
            .add_line(purchase.id, prod.id, dec("1"), Some(Decimal::ZERO))
            .await
            .unwrap();
        assert_eq!(zero.unit_cost, Decimal::ZERO);
        let err = s.update_line(zero.id, dec("0"), dec("1")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s.update_line(zero.id, dec("1"), dec("-1")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Unknown account on Cash confirm => 404.
        let err = s
            .confirm(purchase.id, Some(999_999), Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown account on payment => 404.
        let cp = draft_credit(&s, sup.id).await;
        s.add_line(cp.id, prod.id, dec("1"), None).await.unwrap();
        s.confirm(cp.id, None, None).await.unwrap();
        let err = s
            .record_payment(cp.id, 999_999, cash, dec("5"), purchase_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    // -- AC6: double confirm + edits of Confirmed --------------------------------

    #[tokio::test]
    async fn ac6_double_confirm_and_confirmed_edits_rejected() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "AC6", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC6 SUP").await;
        let acc = seed_account(&s, "caja6").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("10")))
            .await
            .unwrap();
        s.confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap();

        let err = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::Validation(_) | AppError::Conflict(_)),
            "got {err:?}"
        );

        let err = s
            .add_line(purchase.id, prod.id, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let detail = s.get_detail(purchase.id).await.unwrap();
        let line_id = detail.lines[0].id;
        let err = s.update_line(line_id, dec("2"), dec("9")).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s.remove_line(line_id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .update_draft(
                purchase.id,
                UpdatePurchaseDraft {
                    notes: Some("otro".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    // -- AC7: cancel Confirmed returns stock + refunds (no balance guard) ---------

    #[tokio::test]
    async fn ac7_cancel_confirmed_returns_stock_and_refunds_without_balance_guard() {
        // allow_negative = false: the Expense at confirm is guarded, but the
        // Income refund at cancel is money entering and must never be blocked.
        let (s, pool) = svc_with_flags(true, false).await;
        let prod = seed_product(&s, "AC7", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "AC7 SUP").await;
        let acc = seed_account(&s, "caja7").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        s.transactions
            .create(
                acc.id,
                TransactionKind::Income,
                dec("100"),
                Some("fondo".into()),
                purchase_date(),
            )
            .await
            .unwrap();

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("4"), Some(dec("10")))
            .await
            .unwrap(); // total 40
        let detail = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("14"));

        // Drain the account to 0 so a guarded refund would fail.
        s.transactions
            .create(
                acc.id,
                TransactionKind::Expense,
                dec("60"),
                Some("gasto".into()),
                purchase_date(),
            )
            .await
            .unwrap();

        let cancelled = s
            .cancel(purchase.id, Some(" devuelvo ".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.purchase.status, PurchaseStatus::Cancelled);
        assert_eq!(cancelled.purchase.cancel_reason.as_deref(), Some("devuelvo"));
        assert!(cancelled.purchase.cancelled_at.is_some());
        assert_eq!(
            cancelled.purchase.purchase_number.as_deref(),
            Some(number.as_str())
        );

        // Goods go back: 14 - 4 = 10, Out reason Purchase-return.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("10"));
        let moves = s.inventory.movements.list_by_product(prod.id).await.unwrap();
        let back = moves
            .iter()
            .find(|m| m.reason == MovementReason::PurchaseReturn)
            .unwrap();
        assert_eq!(back.movement_type, MovementType::Out);
        assert_eq!(back.qty, dec("4"));
        assert_eq!(back.reference, number);

        // Refund: Income 40 back to the originating account.
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        let refund = rows
            .iter()
            .find(|t| t.kind == TransactionKind::Income && t.description == number)
            .unwrap();
        assert_eq!(refund.amount, dec("40"));
        assert_eq!(
            s.transactions
                .transactions
                .balance_for_account(acc.id)
                .await
                .unwrap(),
            dec("40")
        );
        let _ = pool;
    }

    // -- AC9: satellite update on confirm + CRITICAL pre-validation ---------------

    #[tokio::test]
    async fn ac9_confirm_updates_satellite_previous_and_current() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "AC9", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP").await;
        s.suppliers
            .record_cost(prod.id, sup.id, dec("10"), d(2024, 5, 1))
            .await
            .unwrap();

        let purchase = s
            .create_draft(NewPurchase {
                supplier_id: sup.id,
                payment_type: PaymentType::Credit,
                purchase_date: d(2024, 5, 10),
                due_date: Some(d(2024, 6, 1)),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("12")))
            .await
            .unwrap();
        s.confirm(purchase.id, None, None).await.unwrap();

        let row = s.suppliers.find_cost(prod.id, sup.id).await.unwrap().unwrap();
        assert_eq!(row.current_cost, dec("12"));
        assert_eq!(row.current_cost_updated_at, d(2024, 5, 10));
        assert_eq!(row.previous_cost, Some(dec("10")));
        assert_eq!(row.previous_cost_updated_at, Some(d(2024, 5, 1)));
        assert_eq!(row.price_alert(), PriceAlert::Raised);
    }

    #[tokio::test]
    async fn ac9_backdated_cost_rejected_before_any_stock_finance_or_number() {
        // CRITICAL: the satellite date is validated for every line before any
        // mutation, so a rejected cost update can never leave a partial confirm.
        let (s, pool) = svc().await;
        let prod_a = seed_product(&s, "AC9-A", "5").await;
        let prod_b = seed_product(&s, "AC9-B", "5").await;
        seed_stock(&s, prod_a.id, "1").await;
        seed_stock(&s, prod_b.id, "1").await;
        let sup = seed_supplier(&s, "AC9 BACK").await;
        let acc = seed_account(&s, "caja9").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // Satellite for B is newer than the purchase date; A has no row yet.
        s.suppliers
            .record_cost(prod_b.id, sup.id, dec("10"), d(2024, 5, 10))
            .await
            .unwrap();

        let purchase = draft_cash(&s, sup.id).await; // 2024-05-02, before 05-10
        s.add_line(purchase.id, prod_a.id, dec("1"), Some(dec("3")))
            .await
            .unwrap();
        s.add_line(purchase.id, prod_b.id, dec("1"), Some(dec("4")))
            .await
            .unwrap();

        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Nothing was applied: no stock, no finance, no number, no satellite row.
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, 0);
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.purchase.status, PurchaseStatus::Draft);
        assert!(detail.purchase.purchase_number.is_none());
        assert!(s.suppliers.find_cost(prod_a.id, sup.id).await.unwrap().is_none());
        assert!(s.sequences.current("PURCH", 2024).await.unwrap().is_none());
        let b_row = s.suppliers.find_cost(prod_b.id, sup.id).await.unwrap().unwrap();
        assert_eq!(b_row.current_cost, dec("10"));
        assert_eq!(b_row.current_cost_updated_at, d(2024, 5, 10));
    }

    // -- AC10: purchase never writes products.cost_price ---------------------------

    #[tokio::test]
    async fn ac10_purchase_never_writes_cost_price_and_satellite_wins() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC10", "5").await;
        let other = seed_product(&s, "AC10-2", "7").await;
        let sup = seed_supplier(&s, "AC10 SUP").await;
        s.suppliers
            .record_cost(prod.id, sup.id, dec("9.50"), d(2024, 5, 1))
            .await
            .unwrap();

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("12")))
            .await
            .unwrap();
        s.confirm(purchase.id, None, None).await.unwrap();

        let stored = s.inventory.get_product(prod.id).await.unwrap();
        assert_eq!(stored.cost_price, dec("5"), "cost_price must stay untouched");
        // Satellite wins after the confirm recorded the line cost.
        assert_eq!(s.suppliers.reference_cost(prod.id).await.unwrap(), Some(dec("12")));
        // No satellite rows => None, caller falls back to the column.
        assert_eq!(s.suppliers.reference_cost(other.id).await.unwrap(), None);
        let other_stored = s.inventory.get_product(other.id).await.unwrap();
        assert_eq!(other_stored.cost_price, dec("7"));
        let _ = pool;
    }

    // -- AC11: references ---------------------------------------------------------

    #[tokio::test]
    async fn ac11_stock_and_finance_rows_reference_purchase_number() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "AC11", "5").await;
        let sup = seed_supplier(&s, "AC11 SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("6")))
            .await
            .unwrap();
        let detail = s.confirm(purchase.id, None, None).await.unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        let moves = s.inventory.movements.list_by_product(prod.id).await.unwrap();
        let received = moves
            .iter()
            .find(|m| m.reason == MovementReason::Purchase)
            .unwrap();
        assert_eq!(received.reference, number);

        let acc = seed_account(&s, "caja11").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        s.record_payment(purchase.id, acc.id, cash, dec("5"), d(2024, 5, 20))
            .await
            .unwrap();
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows[0].description, number);
        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
        assert_eq!(payments[0].account_id, acc.id);
        assert_eq!(payments[0].method_id, cash);
    }

    // -- AC12: suggestion builder -------------------------------------------------

    #[tokio::test]
    async fn ac12_suggestions_pick_supplier_cheapest_or_preferred_and_list_without_supplier() {
        let (s, _pool) = svc().await;
        let pref_prod = seed_tracked(&s, "SUG-PREF", "5", "5", "50").await;
        seed_stock(&s, pref_prod.id, "2").await; // suggested 48
        let cheap_prod = seed_tracked(&s, "SUG-CHEAP", "5", "5", "20").await;
        seed_stock(&s, cheap_prod.id, "1").await; // suggested 19
        let no_sup = seed_tracked(&s, "SUG-NONE", "5", "5", "30").await;
        // No movements at all => derived stock 0, suggested 30.
        let fine = seed_tracked(&s, "SUG-FINE", "5", "5", "50").await;
        seed_stock(&s, fine.id, "10").await; // above min: absent
        let svc_prod = seed_service(&s, "SUG-SVC", "3").await;

        let sup_a = seed_supplier(&s, "SUG A").await;
        let sup_b = seed_supplier(&s, "SUG B").await;

        // Preferred beats cheaper for pref_prod.
        s.suppliers
            .record_cost(pref_prod.id, sup_a.id, dec("9"), d(2024, 5, 1))
            .await
            .unwrap();
        s.suppliers
            .record_cost(pref_prod.id, sup_b.id, dec("7"), d(2024, 5, 1))
            .await
            .unwrap();
        s.suppliers.set_preferred(pref_prod.id, sup_a.id).await.unwrap();

        // No preferred for cheap_prod: the cheapest current cost wins.
        s.suppliers
            .record_cost(cheap_prod.id, sup_a.id, dec("6.50"), d(2024, 5, 1))
            .await
            .unwrap();
        s.suppliers
            .record_cost(cheap_prod.id, sup_b.id, dec("6"), d(2024, 5, 1))
            .await
            .unwrap();

        let out = s.suggestions().await.unwrap();
        assert_eq!(out.suggestions.len(), 2, "two costed low-stock products");
        assert_eq!(out.without_supplier.len(), 1);

        let a = out
            .suggestions
            .iter()
            .find(|x| x.product.id == pref_prod.id)
            .unwrap();
        assert_eq!(a.suggested_qty, dec("48"));
        assert_eq!(a.supplier_id, sup_a.id);
        assert_eq!(a.supplier_name, "SUG A");
        assert_eq!(a.unit_cost, dec("9"));
        assert_eq!(a.subtotal, dec("432"));

        let b = out
            .suggestions
            .iter()
            .find(|x| x.product.id == cheap_prod.id)
            .unwrap();
        assert_eq!(b.suggested_qty, dec("19"));
        assert_eq!(b.supplier_id, sup_b.id);
        assert_eq!(b.unit_cost, dec("6"));
        assert_eq!(b.subtotal, dec("114"));

        let c = out
            .without_supplier
            .iter()
            .find(|x| x.product.id == no_sup.id)
            .unwrap();
        assert_eq!(c.suggested_qty, dec("30"));
        assert!(!out.suggestions.iter().any(|x| x.product.id == no_sup.id));
        assert!(!out.suggestions.iter().any(|x| x.product.id == fine.id));
        assert!(!out.suggestions.iter().any(|x| x.product.id == svc_prod.id));
    }

    // -- AC14: allowlist rejection without side effects ----------------------------

    #[tokio::test]
    async fn ac14_disallowed_pair_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC14", "10").await;
        let sup = seed_supplier(&s, "AC14 SUP").await;
        let acc = seed_account(&s, "caja14").await;
        let cash = cash_method(&s).await;
        let qr = method_by_name(&s, "QR").await;
        allow(&s, acc.id, cash).await; // QR intentionally not allowed

        // Cash confirm with a disallowed pair => 400, nothing applied.
        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("10")))
            .await
            .unwrap();
        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(purchase.id, Some(acc.id), Some(qr))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, 0);
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert!(detail.purchase.purchase_number.is_none());
        assert_eq!(detail.purchase.status, PurchaseStatus::Draft);

        // Payment with a disallowed pair => 400, no finance row.
        let credit = draft_credit(&s, sup.id).await;
        s.add_line(credit.id, prod.id, dec("2"), Some(dec("10")))
            .await
            .unwrap();
        s.confirm(credit.id, None, None).await.unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(credit.id, acc.id, qr, dec("5"), purchase_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, tx_before);
        let d = s.get_detail(credit.id).await.unwrap();
        assert_eq!(d.paid, Decimal::ZERO);
        assert!(d.payments.is_empty());
    }

    // -- triangulation -------------------------------------------------------------

    #[tokio::test]
    async fn tri_service_lines_received_without_stock_move_but_cost_updated() {
        let (s, pool) = svc().await;
        let svc_prod = seed_service(&s, "TRI-SRV", "3").await;
        let sup = seed_supplier(&s, "TRI SRV SUP").await;
        let acc = seed_account(&s, "caja-srv").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, svc_prod.id, dec("3"), Some(dec("20")))
            .await
            .unwrap();
        let before = movement_count(&pool).await;
        let detail = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap();
        assert_eq!(detail.total, dec("60"));
        // Services are not stock-tracked: no movement at all.
        assert_eq!(movement_count(&pool).await, before);
        assert_eq!(s.inventory.stock(svc_prod.id).await.unwrap(), dec("0"));
        assert_eq!(tx_count(&pool).await, 1);
        // Satellite cost still recorded for the service.
        let row = s.suppliers.find_cost(svc_prod.id, sup.id).await.unwrap().unwrap();
        assert_eq!(row.current_cost, dec("20"));
    }

    #[tokio::test]
    async fn tri_draft_cancel_is_noop_and_keeps_number_null() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "TRI-DRAFT", "5").await;
        seed_stock(&s, prod.id, "5").await;
        let sup = seed_supplier(&s, "TRI DRAFT SUP").await;
        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("4")))
            .await
            .unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;

        let cancelled = s.cancel(purchase.id, Some("ya no".into())).await.unwrap();
        assert_eq!(cancelled.purchase.status, PurchaseStatus::Cancelled);
        assert!(cancelled.purchase.purchase_number.is_none());
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, tx_before);
        assert!(s.suppliers.find_cost(prod.id, sup.id).await.unwrap().is_none());
        // Cancelling again => 400.
        let err = s.cancel(purchase.id, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_numbers_unique_and_immutable_after_cancel() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-NUM", "5").await;
        let sup = seed_supplier(&s, "TRI NUM SUP").await;
        let a = draft_credit(&s, sup.id).await;
        s.add_line(a.id, prod.id, dec("1"), Some(dec("5"))).await.unwrap();
        let b = draft_credit(&s, sup.id).await;
        s.add_line(b.id, prod.id, dec("1"), Some(dec("5"))).await.unwrap();
        let da = s.confirm(a.id, None, None).await.unwrap();
        let db = s.confirm(b.id, None, None).await.unwrap();
        let na = da.purchase.purchase_number.clone().unwrap();
        let nb = db.purchase.purchase_number.clone().unwrap();
        assert_ne!(na, nb);
        assert_eq!(na, "2024-PURCH-000001");
        assert_eq!(nb, "2024-PURCH-000002");

        // Cancelling keeps the assigned number (immutable).
        let cancelled = s.cancel(a.id, None).await.unwrap();
        assert_eq!(cancelled.purchase.purchase_number.as_deref(), Some(na.as_str()));
    }

    #[tokio::test]
    async fn tri_cash_confirm_balance_guard_blocks_without_side_effects() {
        let (s, pool) = svc_with_flags(true, false).await;
        let prod = seed_product(&s, "TRI-GUARD", "5").await;
        let sup = seed_supplier(&s, "TRI GUARD SUP").await;
        let acc = seed_account(&s, "caja-guard").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("10")))
            .await
            .unwrap(); // total 20, account has 0
        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, 0);
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.purchase.status, PurchaseStatus::Draft);
        assert!(detail.purchase.purchase_number.is_none());
    }

    #[tokio::test]
    async fn tri_cancel_confirmed_refunds_each_originating_account() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-REFUND", "5").await;
        let sup = seed_supplier(&s, "TRI REFUND SUP").await;
        let acc_a = seed_account(&s, "tri-refund-a").await;
        let acc_b = seed_account(&s, "tri-refund-b").await;
        let cash = cash_method(&s).await;
        let transfer = method_by_name(&s, "Transfer").await;
        allow(&s, acc_a.id, cash).await;
        allow(&s, acc_b.id, transfer).await;

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("20")))
            .await
            .unwrap(); // total 40
        let detail = s.confirm(purchase.id, None, None).await.unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();
        s.record_payment(purchase.id, acc_a.id, cash, dec("15"), d(2024, 5, 10))
            .await
            .unwrap();
        s.record_payment(purchase.id, acc_b.id, transfer, dec("25"), d(2024, 5, 11))
            .await
            .unwrap();

        s.cancel(purchase.id, None).await.unwrap();
        let rows_a = s
            .transactions
            .transactions
            .list_by_account(acc_a.id)
            .await
            .unwrap();
        let refund_a = rows_a
            .iter()
            .find(|t| t.kind == TransactionKind::Income)
            .unwrap();
        assert_eq!(refund_a.amount, dec("15"));
        assert_eq!(refund_a.description, number);
        let rows_b = s
            .transactions
            .transactions
            .list_by_account(acc_b.id)
            .await
            .unwrap();
        let refund_b = rows_b
            .iter()
            .find(|t| t.kind == TransactionKind::Income)
            .unwrap();
        assert_eq!(refund_b.amount, dec("25"));
        assert_eq!(refund_b.description, number);
    }

    #[tokio::test]
    async fn tri_cancelled_purchase_rejects_payment_and_second_cancel() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "TRI-CANPAY", "5").await;
        let sup = seed_supplier(&s, "TRI CANPAY SUP").await;
        let acc = seed_account(&s, "caja-canpay").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("5")))
            .await
            .unwrap();
        s.confirm(purchase.id, None, None).await.unwrap();
        s.cancel(purchase.id, None).await.unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(purchase.id, acc.id, cash, dec("5"), purchase_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, tx_before);
        let err = s.cancel(purchase.id, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_update_draft_dates_supplier_and_payment_type() {
        let (s, _pool) = svc().await;
        let sup_a = seed_supplier(&s, "TRI-UPD A").await;
        let sup_b = seed_supplier(&s, "TRI-UPD B").await;

        // Cash with a due_date is invalid.
        let err = s
            .create_draft(NewPurchase {
                supplier_id: sup_a.id,
                payment_type: PaymentType::Cash,
                purchase_date: purchase_date(),
                due_date: Some(d(2024, 6, 1)),
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Credit without due_date is invalid.
        let err = s
            .create_draft(NewPurchase {
                supplier_id: sup_a.id,
                payment_type: PaymentType::Credit,
                purchase_date: purchase_date(),
                due_date: None,
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let purchase = draft_credit(&s, sup_a.id).await;
        // Switching to Cash must clear the due_date in the same patch.
        let err = s
            .update_draft(
                purchase.id,
                UpdatePurchaseDraft {
                    payment_type: Some(PaymentType::Cash),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let switched = s
            .update_draft(
                purchase.id,
                UpdatePurchaseDraft {
                    payment_type: Some(PaymentType::Cash),
                    due_date: Some(None),
                    supplier_id: Some(sup_b.id),
                    purchase_date: Some(d(2024, 5, 4)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(switched.payment_type, PaymentType::Cash);
        assert_eq!(switched.due_date, None);
        assert_eq!(switched.supplier_id, sup_b.id);
        assert_eq!(switched.purchase_date, d(2024, 5, 4));
    }

    #[tokio::test]
    async fn tri_supplier_and_product_delete_blocked_by_purchase_history() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-RESTR", "5").await;
        let sup = seed_supplier(&s, "TRI RESTR SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("5")))
            .await
            .unwrap();

        // Draft history alone is enough for the DB RESTRICT (no cost rows yet).
        let err = s.suppliers.delete_supplier(sup.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.suppliers.get_supplier(sup.id).await.is_ok());
        let err = s.inventory.delete_product(prod.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.inventory.get_product(prod.id).await.is_ok());
    }

    #[tokio::test]
    async fn tri_duplicate_product_on_same_purchase_rejected_unchanged() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-DUP", "5").await;
        let other = seed_product(&s, "TRI-DUP-OK", "5").await;
        let sup = seed_supplier(&s, "TRI DUP SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("4")))
            .await
            .unwrap();

        // A purchase cannot repeat a product: the satellite holds one cost per
        // (product, supplier), so two different line costs have no defined
        // answer. The second add is rejected and the purchase is unchanged.
        let err = s
            .add_line(purchase.id, prod.id, dec("2"), Some(dec("6")))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(detail.lines[0].product_id, prod.id);
        assert_eq!(detail.lines[0].unit_cost, dec("4"));
        assert_eq!(detail.total, dec("4"));
        assert!(s
            .suppliers
            .list_costs_for_product(prod.id)
            .await
            .unwrap()
            .is_empty());

        // A different product still fits on the same purchase.
        s.add_line(purchase.id, other.id, dec("1"), Some(dec("9")))
            .await
            .unwrap();
        assert_eq!(s.get_detail(purchase.id).await.unwrap().lines.len(), 2);
    }

    #[tokio::test]
    async fn tri_update_line_with_duplicate_product_rejected_unchanged() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-UPD-DUP", "5").await;
        let other = seed_product(&s, "TRI-UPD-OK", "5").await;
        let sup = seed_supplier(&s, "TRI UPD DUP SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        let first = s
            .add_line(purchase.id, prod.id, dec("1"), Some(dec("4")))
            .await
            .unwrap();
        let ok_line = s
            .add_line(purchase.id, other.id, dec("1"), Some(dec("5")))
            .await
            .unwrap();
        // Fabricate the duplicate state outside the service (defensive path).
        let dup = s
            .purchases
            .create_line(purchase.id, prod.id, dec("1"), dec("6"))
            .await
            .unwrap();

        for line_id in [first.id, dup.id] {
            let err = s
                .update_line(line_id, dec("2"), dec("7"))
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        let first_row = s.purchases.find_line(first.id).await.unwrap().unwrap();
        assert_eq!(first_row.qty, dec("1"));
        assert_eq!(first_row.unit_cost, dec("4"));
        let dup_row = s.purchases.find_line(dup.id).await.unwrap().unwrap();
        assert_eq!(dup_row.qty, dec("1"));
        assert_eq!(dup_row.unit_cost, dec("6"));

        // The unrelated line still updates normally.
        let updated = s
            .update_line(ok_line.id, dec("3"), dec("8"))
            .await
            .unwrap();
        assert_eq!(updated.qty, dec("3"));
        assert_eq!(updated.unit_cost, dec("8"));
    }

    #[tokio::test]
    async fn tri_confirm_rejects_duplicate_product_lines_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "TRI-CONF-DUP", "5").await;
        let sup = seed_supplier(&s, "TRI CONF DUP SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("1"), Some(dec("4")))
            .await
            .unwrap();
        s.purchases
            .create_line(purchase.id, prod.id, dec("1"), dec("6"))
            .await
            .unwrap();

        // Defensive: add_line/update_line already reject duplicates, but a
        // confirm must never accept a repeated product either.
        let err = s.confirm(purchase.id, None, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let after = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(after.purchase.status, PurchaseStatus::Draft);
        assert_eq!(after.purchase.purchase_number, None);
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), Decimal::ZERO);
        assert_eq!(movement_count(&pool).await, 0);
        assert_eq!(tx_count(&pool).await, 0);
        assert!(s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn tri_same_product_on_two_purchases_updates_satellite_normally() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-DIFF", "5").await;
        let sup = seed_supplier(&s, "TRI DIFF SUP").await;

        let first = draft_credit(&s, sup.id).await;
        s.add_line(first.id, prod.id, dec("1"), Some(dec("4")))
            .await
            .unwrap();
        s.confirm(first.id, None, None).await.unwrap();

        let second = draft_credit(&s, sup.id).await;
        s.add_line(second.id, prod.id, dec("2"), Some(dec("6")))
            .await
            .unwrap();
        let detail = s.confirm(second.id, None, None).await.unwrap();

        assert_eq!(detail.total, dec("12"));
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("3"));
        let cost = s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cost.current_cost, dec("6"));
        assert_eq!(cost.previous_cost, Some(dec("4")));
        assert_eq!(cost.price_alert(), PriceAlert::Raised);
    }

    #[tokio::test]
    async fn tri_pedido_built_from_suggestion_restocks_and_clears_low_stock() {
        let (s, _pool) = svc().await;
        let prod = seed_tracked(&s, "TRI-PEDIDO", "5", "5", "20").await;
        seed_stock(&s, prod.id, "2").await; // suggested 18
        let sup = seed_supplier(&s, "TRI PEDIDO SUP").await;
        s.suppliers
            .record_cost(prod.id, sup.id, dec("4"), d(2024, 5, 1))
            .await
            .unwrap();

        let before = s.suggestions().await.unwrap();
        let item = before
            .suggestions
            .iter()
            .find(|x| x.product.id == prod.id)
            .unwrap();
        assert_eq!(item.suggested_qty, dec("18"));
        assert_eq!(item.supplier_id, sup.id);

        // Draft the pedido from the suggestion and confirm it (Credit).
        let purchase = draft_credit(&s, item.supplier_id).await;
        s.add_line(
            purchase.id,
            item.product.id,
            item.suggested_qty,
            Some(item.unit_cost),
        )
        .await
        .unwrap();
        s.confirm(purchase.id, None, None).await.unwrap();
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("20"));

        let after = s.suggestions().await.unwrap();
        assert!(!after.suggestions.iter().any(|x| x.product.id == prod.id));
        assert!(!after
            .without_supplier
            .iter()
            .any(|x| x.product.id == prod.id));
    }

    #[tokio::test]
    async fn tri_purchase_return_check_expansion_preserves_existing_movements() {
        // Apply migrations up to the Slice E boundary, insert a legacy movement,
        // then finish the migrations: the rebuild must preserve the row and only
        // widen the reason CHECK.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        let migrator = sqlx::migrate!("./migrations");
        migrator.run_to(20240101000014, &pool).await.unwrap();

        let product_id: (i64,) = sqlx::query_as(
            "INSERT INTO products (sku, name, kind, unit, sale_price, cost_price, track_stock) \
             VALUES ('LEGACY-1', 'Legacy', 'Product', 'un', '1', '0', 1) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO stock_movements (product_id, qty, type, reason, reference, date) \
             VALUES (?, '5', 'In', 'Purchase', '', '2024-01-01')",
        )
        .bind(product_id.0)
        .execute(&pool)
        .await
        .unwrap();

        migrator.run(&pool).await.unwrap();

        let kept: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM stock_movements WHERE product_id = ? AND qty = '5' AND reason = 'Purchase'",
        )
        .bind(product_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept.0, 1, "existing movements survive the CHECK rebuild");
        // The widened CHECK accepts the new Purchase-return reason.
        sqlx::query(
            "INSERT INTO stock_movements (product_id, qty, type, reason, reference, date) \
             VALUES (?, '1', 'Out', 'Purchase-return', '2024-PURCH-000001', '2024-01-02')",
        )
        .bind(product_id.0)
        .execute(&pool)
        .await
        .unwrap();
    }

    #[test]
    fn movement_reason_purchase_return_roundtrip() {
        assert_eq!(
            MovementReason::PurchaseReturn.to_string(),
            "Purchase-return"
        );
        assert_eq!(
            "Purchase-return".parse::<MovementReason>().unwrap(),
            MovementReason::PurchaseReturn
        );
        assert_eq!(
            "PurchaseReturn".parse::<MovementReason>().unwrap(),
            MovementReason::PurchaseReturn
        );
        assert_eq!(
            "purchase_return".parse::<MovementReason>().unwrap(),
            MovementReason::PurchaseReturn
        );
    }

    // -- money traceability: payment <-> transaction links ---------------------

    #[tokio::test]
    async fn link_cash_confirm_payment_carries_its_transaction_and_reference() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "LINK-CASH", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "LINK CASH SUP").await;
        let acc = seed_account(&s, "link-cash").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("10")))
            .await
            .unwrap(); // total 20

        let detail = s
            .confirm(purchase.id, Some(acc.id), Some(cash))
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
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
        assert_eq!(rows[0].kind, TransactionKind::Expense);
        assert_eq!(rows[0].reference.as_deref(), Some(number.as_str()));
        assert_eq!(rows[0].description, number);
    }

    #[tokio::test]
    async fn link_credit_payment_and_cancel_refund_keeps_original_transaction() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "LINK-CREDIT", "5").await;
        let sup = seed_supplier(&s, "LINK CREDIT SUP").await;
        let acc = seed_account(&s, "link-credit").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(purchase.id, prod.id, dec("2"), Some(dec("20")))
            .await
            .unwrap(); // total 40
        let detail = s.confirm(purchase.id, None, None).await.unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        let paid = s
            .record_payment(purchase.id, acc.id, cash, dec("15"), d(2024, 5, 10))
            .await
            .unwrap();
        let paid_tx_id = paid
            .transaction_id
            .expect("credit payment must link its Expense");

        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        let expense = rows.iter().find(|t| t.id == paid_tx_id).unwrap();
        assert_eq!(expense.kind, TransactionKind::Expense);
        assert_eq!(expense.reference.as_deref(), Some(number.as_str()));

        s.cancel(purchase.id, Some("return".into())).await.unwrap();

        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
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
        assert_eq!(refund.kind, TransactionKind::Income);
        assert_eq!(refund.amount, dec("15"));
        assert_eq!(refund.reference.as_deref(), Some(number.as_str()));
    }
}
