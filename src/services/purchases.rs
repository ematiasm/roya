// M3 purchases orchestrator (mirror of M2 SalesService).
//
// PurchasesService calls InventoryService for stock In (reason Purchase) on
// confirm and stock Out (reason Purchase-return) on cancel, TransactionService
// for the Cash Expense / payment Expenses / cancel Income refunds with
// reference = purchase_number, SupplierService for the satellite cost update on
// confirm, and PaymentMethodService for the method-ownership check (the cash
// account is derived from the method, so an invalid combination is impossible
// by construction).
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
use std::collections::{BTreeMap, HashSet};

use crate::error::{AppError, AppResult};
use crate::models::{
    format_purchase_number, MovementReason, MovementType, NewMovement, NewPurchase, PaymentType,
    Purchase, PurchaseDetail, PurchaseLine, PurchaseLineView, PurchaseListFilter, PurchasePayment,
    PurchasePaymentView, PurchaseRecord, PurchaseStatus, PurchaseSuggestion,
    PurchaseSuggestionWithoutSupplier, PurchaseSuggestions, StaleLineCostView, Supplier,
    UpdatePurchaseDraft,
};

/// What `add_or_increment_line` did with the request. The distinction matters
/// because the web route announces a merge with a visible notice, while an
/// ordinary add keeps the existing silent success path.
#[derive(Debug)]
pub enum LineAddOutcome {
    /// The product had no line in this purchase: a new one was created.
    Added(PurchaseLine),
    /// The product already had a line at the same resolved cost: its quantity
    /// was incremented through the update path, so exactly one line remains.
    /// `product_name` feeds the merge notice the route renders.
    Merged {
        line: PurchaseLine,
        product_name: String,
    },
}

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
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
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
        product_name: &str,
        exclude_line: Option<i64>,
    ) -> AppResult<()> {
        let duplicated = lines
            .iter()
            .any(|l| l.product_id == product_id && Some(l.id) != exclude_line);
        if duplicated {
            return Err(AppError::Validation(format!(
                "product {product_name} already has a line in this purchase; record a different price in a separate purchase"
            )));
        }
        Ok(())
    }

    /// The supplier of the most recently created purchase (T3): the
    /// creation dialog pre-fills its picker with it as the DEFAULT — a real,
    /// visible, editable value, never a silent guess (the feature doc's
    /// hazard: the supplier resolves every line's default cost, so a wrong
    /// default would record the lines at the wrong supplier's cost and, at
    /// confirm, overwrite that supplier's real price).
    ///
    /// Empty (None): no purchase has ever been created — an empty database
    /// has no default, so the dialog opens with an empty supplier field and
    /// the operator must choose.
    pub async fn last_used_supplier(&self) -> AppResult<Option<Supplier>> {
        let Some(supplier_id) = self.purchases.last_used_supplier_id().await? else {
            return Ok(None);
        };
        let supplier = self.suppliers.get_supplier(supplier_id).await?;
        Ok(Some(supplier))
    }

    // -- Draft -----------------------------------------------------------------

    /// Create a Draft purchase. `actor` is the acting user's id the route
    /// resolves from its `Principal`; it becomes the row's `created_by` and
    /// nothing the request itself can supply names it.
    pub async fn create_draft(&self, actor: i64, input: NewPurchase) -> AppResult<Purchase> {
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
            .create_purchase(
                actor,
                &NewPurchase {
                    supplier_id: input.supplier_id,
                    payment_type: input.payment_type,
                    purchase_date: input.purchase_date,
                    due_date: input.due_date,
                    supplier_invoice_no: invoice,
                    notes: Some(notes),
                },
            )
            .await
    }

    pub async fn update_draft(
        &self,
        actor: i64,
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
            supplier_invoice_no: patch
                .supplier_invoice_no
                .map(|opt| opt.map(|s| s.trim().to_string())),
            notes: patch.notes.map(|s| s.trim().to_string()),
        };
        self.purchases.update_draft(id, actor, &norm).await
    }

    pub async fn add_line(
        &self,
        actor: i64,
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
        Self::ensure_unique_product(&lines, product_id, &product.name, None)?;
        let cost = self
            .resolve_line_cost(
                product_id,
                product.cost_price,
                purchase.supplier_id,
                unit_cost,
            )
            .await?;
        let line = self
            .purchases
            .create_line(purchase_id, product_id, qty, cost)
            .await?;
        // The line inherits the purchase's actor (no columns of its own), but
        // the document was just edited: the line change stamps the draft's
        // `updated_by` with this request's actor.
        self.purchases.touch_draft(purchase_id, actor).await?;
        Ok(line)
    }

    /// Cost resolution shared by the strict and the merging line-add: an
    /// explicit cost wins (and cannot be negative); an empty cost uses this
    /// supplier's satellite cost, and only falls back to the product column
    /// when no satellite row exists (the fallback purchases never write).
    async fn resolve_line_cost(
        &self,
        product_id: i64,
        product_cost_price: Decimal,
        supplier_id: i64,
        unit_cost: Option<Decimal>,
    ) -> AppResult<Decimal> {
        match unit_cost {
            Some(c) => {
                if c < Decimal::ZERO {
                    return Err(AppError::Validation("unit_cost cannot be negative".into()));
                }
                Ok(c)
            }
            None => Ok(
                match self.suppliers.find_cost(product_id, supplier_id).await? {
                    Some(row) => row.current_cost,
                    None => product_cost_price,
                },
            ),
        }
    }

    /// The web scan path (S5b): a repeat product whose resolved cost equals the
    /// existing line's cost increments that line instead of failing. The
    /// domain rule stays — a product appears at most once per purchase, so one
    /// product can never carry two prices — but merging the SAME price loses
    /// nothing, while merging a DIFFERENT price would silently discard one of
    /// them, so that case keeps the strict 400 unchanged. The JSON API keeps
    /// the strict `add_line` contract: a machine client should use the
    /// line-update endpoint rather than have its request reinterpreted.
    pub async fn add_or_increment_line(
        &self,
        actor: i64,
        purchase_id: i64,
        product_id: i64,
        qty: Decimal,
        unit_cost: Option<Decimal>,
    ) -> AppResult<LineAddOutcome> {
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
        // Cost resolution runs BEFORE the uniqueness check, the one point
        // where this method and `add_line` diverge: the merge decision needs
        // the resolved cost. A visible consequence: a repeat product submitted
        // with an invalid explicit cost (negative) answers
        // "unit_cost cannot be negative", not the duplicate-product 400
        // `add_line` would have answered first. Both are 400 and only invalid
        // input is affected.
        let cost = self
            .resolve_line_cost(
                product_id,
                product.cost_price,
                purchase.supplier_id,
                unit_cost,
            )
            .await?;
        let lines = self.purchases.list_lines(purchase_id).await?;
        match lines.iter().find(|l| l.product_id == product_id) {
            None => {
                let line = self
                    .purchases
                    .create_line(purchase_id, product_id, qty, cost)
                    .await?;
                self.purchases.touch_draft(purchase_id, actor).await?;
                Ok(LineAddOutcome::Added(line))
            }
            // Same product at the same price: sum the quantities through the
            // existing update path (which re-runs the draft and validation
            // guards), keeping exactly one line for the product.
            Some(existing) if existing.unit_cost == cost => {
                let line = self
                    .update_line(actor, existing.id, existing.qty + qty, cost)
                    .await?;
                Ok(LineAddOutcome::Merged {
                    line,
                    product_name: product.name,
                })
            }
            // Same product at a DIFFERENT price: exactly the case the rule
            // exists to catch. The rejection is the same 400
            // `ensure_unique_product` produces, so message and status do not
            // change; the merge would discard one of the two prices.
            Some(_) => {
                Self::ensure_unique_product(&lines, product_id, &product.name, None)?;
                unreachable!("the product's line exists, so ensure_unique_product rejects")
            }
        }
    }

    pub async fn update_line(
        &self,
        actor: i64,
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
        let product = self.inventory.get_product(line.product_id).await?;
        Self::ensure_unique_product(&lines, line.product_id, &product.name, Some(line_id))?;
        let line = self.purchases.update_line(line_id, qty, unit_cost).await?;
        self.purchases.touch_draft(line.purchase_id, actor).await?;
        Ok(line)
    }

    pub async fn remove_line(&self, actor: i64, line_id: i64) -> AppResult<()> {
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
        self.purchases.touch_draft(line.purchase_id, actor).await?;
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

    /// One purchase payment by id — the documents drawer's per-payment read,
    /// so the route never touches a repository. An unknown id is the standard
    /// `NotFound`, naming the family.
    pub async fn find_payment(&self, id: i64) -> AppResult<PurchasePayment> {
        self.purchases
            .find_payment(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase payment {id} not found")))
    }

    /// Record-page view for `/purchases/{id}`: resolves supplier, product,
    /// account and method names through the existing read paths, so the route
    /// never runs SQL of its own and never prints an internal key.
    pub async fn get_record(&self, purchase_id: i64) -> AppResult<PurchaseRecord> {
        let detail = self.get_detail(purchase_id).await?;
        self.record_from_detail(detail).await
    }

    async fn record_from_detail(&self, detail: PurchaseDetail) -> AppResult<PurchaseRecord> {
        let supplier_name = self
            .suppliers
            .get_supplier(detail.purchase.supplier_id)
            .await?
            .name;

        // The status is read before the loop consumes `detail.lines`: the
        // purchase itself is moved into the record at the end, so reading it
        // inside the loop would touch a partially moved value.
        let status = detail.purchase.status;
        let mut lines = Vec::with_capacity(detail.lines.len());
        // Receiving-desk T2: the units the stock flows will move, summed from
        // the same per-line `tracks_stock` flags below — never recomputed
        // elsewhere (a second predicate could drift from confirm/cancel).
        let mut tracked_units = Decimal::ZERO;
        for line in detail.lines {
            let product = self.inventory.get_product(line.product_id).await?;
            // The same predicate confirm and cancel use to decide whether a
            // line moves stock; resolved from the product this read already
            // fetched for the display names, so a preview built from the view
            // cannot drift from what those flows will do.
            let tracks_stock =
                product.kind == crate::models::ProductKind::Product && product.track_stock;
            // Stale-cost flag, derived from the same product read (no extra
            // query): `Some` only on a CONFIRMED purchase whose line cost is
            // strictly higher than a real stored cost. Zero means "no cost
            // recorded yet" (the column is NOT NULL DEFAULT '0'), and equal or
            // lower is not what this warning is about — the drawer badge
            // covers any disagreement.
            //
            // A draft is deliberately NOT flagged: a draft line's cost is
            // provisional — the line can still be edited or deleted, and the
            // purchase may never be confirmed at all — so the comparison would
            // assert something the domain does not yet know. Confirming is
            // the moment the cost becomes a fact, because that is when the
            // document starts to exist; it is also why the apply action
            // belongs here and not in the editable phase. A purchase that was
            // confirmed and later cancelled is a historical document: it shows
            // nothing, and its costs must not feed a product update. (The
            // gate lives here in Rust, not in the template: this is a domain
            // rule, and the template must not be able to re-enable or silence
            // it.)
            let stale_cost = if status == PurchaseStatus::Confirmed
                && line.unit_cost > product.cost_price
                && product.cost_price != Decimal::ZERO
            {
                Some(StaleLineCostView {
                    line_cost: line.unit_cost,
                    stored_cost: product.cost_price,
                })
            } else {
                None
            };
            if tracks_stock {
                tracked_units += line.qty;
            }
            lines.push(PurchaseLineView {
                id: line.id,
                product_name: product.name,
                product_sku: product.sku,
                product_id: line.product_id,
                qty: line.qty,
                unit_cost: line.unit_cost,
                subtotal: line.subtotal(),
                tracks_stock,
                stale_cost,
            });
        }

        let account_names: BTreeMap<i64, String> = self
            .transactions
            .accounts
            .list()
            .await?
            .into_iter()
            .map(|account| (account.id, account.name))
            .collect();
        let method_names: BTreeMap<i64, String> = self
            .payment_methods
            .list()
            .await?
            .into_iter()
            .map(|method| (method.id, method.name))
            .collect();

        let payments = detail
            .payments
            .into_iter()
            .map(|payment| PurchasePaymentView {
                id: payment.id,
                account_name: account_names
                    .get(&payment.account_id)
                    .cloned()
                    .unwrap_or_else(|| "Unknown account".to_string()),
                method_name: method_names
                    .get(&payment.method_id)
                    .cloned()
                    .unwrap_or_else(|| "Unknown method".to_string()),
                amount: payment.amount,
                date: payment.date,
            })
            .collect();

        Ok(PurchaseRecord {
            purchase: detail.purchase,
            supplier_name,
            lines,
            payments,
            total: detail.total,
            paid: detail.paid,
            due: detail.due,
            payment_status: detail.payment_status,
            tracked_units,
        })
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

    /// The same derived list narrowed by the server-side list filter. The party
    /// name is resolved against the suppliers table (normalized) into ids, and the
    /// repository narrows the document query by those ids, so only matching
    /// documents have their lines and payments loaded.
    pub async fn list_details_filtered(
        &self,
        filter: &PurchaseListFilter,
    ) -> AppResult<Vec<PurchaseDetail>> {
        let mut repo_filter = filter.clone();
        if let Some(name) = &filter.supplier {
            repo_filter.supplier_ids = Some(self.matching_supplier_ids(name).await?);
        }
        let purchases = self.purchases.list_purchases_filtered(&repo_filter).await?;
        let mut out = Vec::with_capacity(purchases.len());
        for purchase in purchases {
            out.push(self.detail_for(purchase).await?);
        }
        Ok(out)
    }

    /// Supplier ids whose current name matches `needle` after normalization. The
    /// suppliers table is small by nature, so the match runs in Rust over the whole
    /// set and the document query stays bounded to the matching ids. If the party
    /// catalogue ever stops being small, this needs a normalized index instead.
    async fn matching_supplier_ids(&self, needle: &str) -> AppResult<Vec<i64>> {
        let needle = crate::models::normalize_search(needle);
        Ok(self
            .suppliers
            .list_suppliers()
            .await?
            .into_iter()
            .filter(|supplier| crate::models::normalize_search(&supplier.name).contains(&needle))
            .map(|supplier| supplier.id)
            .collect())
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
        actor: i64,
        purchase_id: i64,
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
                if purchase.purchase_date < existing.current_cost_date {
                    return Err(AppError::Validation(format!(
                        "purchase_date {} precedes the recorded cost date {} for product {} and supplier {}",
                        purchase.purchase_date,
                        existing.current_cost_date,
                        line.product_id,
                        purchase.supplier_id
                    )));
                }
            }
        }

        let (total, _, _) = Self::totals(&lines, &self.purchases.list_payments(purchase_id).await?);

        // The cash account is derived from the method, which belongs to exactly
        // one account: an invalid combination is impossible by construction.
        let cash_account_id: Option<i64> = match purchase.payment_type {
            PaymentType::Cash => {
                let method_id = cash_method_id.ok_or_else(|| {
                    AppError::Validation("cash purchase requires a payment method".into())
                })?;
                if purchase.due_date.is_some() {
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
                }
                // Ownership resolved before any stock/sequence/finance touch.
                Some(self.payment_methods.resolve_account(method_id).await?)
            }
            PaymentType::Credit => {
                if cash_method_id.is_some() {
                    return Err(AppError::Validation(
                        "credit purchase must not include a payment method".into(),
                    ));
                }
                None
            }
        };

        match purchase.payment_type {
            PaymentType::Cash => {
                let account_id = cash_account_id.expect("resolved above");
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
                // A method here was already rejected above; the account is
                // derived from it, so there is nothing else to refuse.
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

        // Stock In (reason Purchase) for tracked Product lines only. The
        // movement carries the CONFIRMING request's actor — the same argument
        // that stamps the finance rows — never a fresh one (AC18).
        for line in &tracked {
            self.inventory
                .record_movement(
                    actor,
                    NewMovement {
                        product_id: line.product_id,
                        qty: line.qty,
                        movement_type: MovementType::In,
                        reason: MovementReason::Purchase,
                        reference: purchase_number.clone(),
                        date: purchase.purchase_date,
                    },
                )
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
                    actor,
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
                    actor,
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
            .set_confirmed(purchase_id, actor, &purchase_number)
            .await?;

        // AC9: after a successful confirm, record the line cost in the satellite
        // (one row per product/supplier pair). The uniqueness guard above means
        // each product appears on exactly one line.
        for line in &lines {
            self.suppliers
                .record_cost(
                    actor,
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
        actor: i64,
        purchase_id: i64,
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
        // The account is derived from the method's owner (no finance touch yet).
        let account_id = self.payment_methods.resolve_account(method_id).await?;
        let lines = self.purchases.list_lines(purchase_id).await?;
        let payments = self.purchases.list_payments(purchase_id).await?;
        let (total, paid, _) = Self::totals(&lines, &payments);
        if paid + amount > total {
            return Err(AppError::Validation(format!(
                "overpay rejected: paid {paid} + {amount} exceeds total {total}"
            )));
        }
        let purchase_number = purchase.purchase_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed purchase missing purchase_number".into())
        })?;
        // Each payment generates one M0 Expense stamped with reference =
        // purchase_number and linked from the payment row it produced.
        let expense = self
            .transactions
            .create_with_reference(
                actor,
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
                actor,
                purchase_id,
                account_id,
                method_id,
                amount,
                date,
                Some(expense.id),
            )
            .await
    }

    /// Pay a supplier across their outstanding Confirmed purchases, oldest debt
    /// first (mirror of `CustomerReceiptService::collect` without the receipt
    /// grouping document: suppliers have none). The supplier must exist (404)
    /// and the amount must be positive; the account is derived from the
    /// method's owner before any write (400 inactive/unassigned, no side
    /// effect). A payment over the supplier's outstanding debt is a 400 naming
    /// both figures. Returns one payment per covered purchase.
    pub async fn pay_supplier(
        &self,
        actor: i64,
        supplier_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
    ) -> AppResult<Vec<PurchasePayment>> {
        let supplier = self.suppliers.get_supplier(supplier_id).await?;
        if amount <= Decimal::ZERO {
            return Err(AppError::Validation("amount must be > 0".into()));
        }
        // The account is derived from the method's owner before any write
        // (400 inactive/unassigned, no side effect).
        self.payment_methods.resolve_account(method_id).await?;

        // The payable: a payment never exceeds what the supplier is owed, and
        // the outstanding figure is what the allocation is checked against.
        // Oldest first: `due_date`, then `purchase_date`, then id — the same
        // ordering rule `collect` plans with.
        let mut debts: Vec<PurchaseDetail> = self
            .outstanding_payables()
            .await?
            .into_iter()
            .filter(|detail| detail.purchase.supplier_id == supplier_id)
            .collect();
        debts.sort_by(|a, b| {
            a.purchase
                .due_date
                .cmp(&b.purchase.due_date)
                .then_with(|| a.purchase.purchase_date.cmp(&b.purchase.purchase_date))
                .then_with(|| a.purchase.id.cmp(&b.purchase.id))
        });
        let outstanding: Decimal = debts.iter().map(|detail| detail.due).sum();
        if amount > outstanding {
            return Err(AppError::Validation(format!(
                "amount {amount} exceeds the outstanding debt {outstanding} of supplier {} ({supplier_id})",
                supplier.name
            )));
        }

        let mut remaining = amount;
        let mut plan = Vec::new();
        for detail in &debts {
            if remaining <= Decimal::ZERO {
                break;
            }
            if detail.due <= Decimal::ZERO {
                continue;
            }
            let take = detail.due.min(remaining);
            plan.push((detail.purchase.id, take));
            remaining -= take;
        }
        let planned: Decimal = plan.iter().map(|(_, take)| *take).sum();
        if planned != amount {
            return Err(AppError::Internal(format!(
                "payment plan {planned} does not consume the paid amount {amount}"
            )));
        }

        // One payment per covered purchase. `record_payment` revalidates the
        // overpay per document, and no allocation exceeds its due by
        // construction.
        let mut payments = Vec::with_capacity(plan.len());
        for (purchase_id, take) in plan {
            payments.push(
                self.record_payment(actor, purchase_id, method_id, take, date)
                    .await?,
            );
        }
        Ok(payments)
    }

    // -- Cancel / purchase return --------------------------------------------------

    pub async fn cancel(
        &self,
        actor: i64,
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
                .set_cancelled(purchase_id, actor, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        // Confirmed -> Cancelled: goods back to the supplier + refunds.
        let lines = self.purchases.list_lines(purchase_id).await?;
        let payments = self.purchases.list_payments(purchase_id).await?;
        let purchase_number = purchase.purchase_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed purchase missing purchase_number".into())
        })?;

        // Pre-validate products (for the return movement) and refund accounts.
        // Invariant 10: a partially-applied annulment (an earlier attempt failed
        // after posting some refunds) is REFUSED, not doubled — a second pass
        // would write the Purchase-return movements a second time and refund
        // every payment again. There is no aggregate balance guard here by
        // design: the purchase refund is an `Income`, money entering the
        // account, so `create_with_reference` enforces no balance precondition
        // on it and copying the sales check would be dead code.
        let partial = payments
            .iter()
            .filter(|p| p.refund_transaction_id.is_some())
            .count();
        if partial > 0 {
            return Err(AppError::Validation(format!(
                "annulment already partially applied: {partial} of {} payments already linked a refund; refusing to write a second return or duplicate refunds",
                payments.len()
            )));
        }
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

        // Stock Out (reason Purchase-return) for tracked lines. The movement
        // carries the cancelling request's actor, like its refund Income.
        for line in &tracked {
            self.inventory
                .record_movement(
                    actor,
                    NewMovement {
                        product_id: line.product_id,
                        qty: line.qty,
                        movement_type: MovementType::Out,
                        reason: MovementReason::PurchaseReturn,
                        reference: purchase_number.clone(),
                        date: purchase.purchase_date,
                    },
                )
                .await?;
        }

        // Refund Income per paid amount to the originating accounts. A purchase
        // refund is money entering: no negative-balance guard applies. Each refund
        // is linked back from the payment row it refunds.
        for pay in &payments {
            let refund = self
                .transactions
                .create_with_reference(
                    actor,
                    pay.account_id,
                    crate::models::TransactionKind::Income,
                    pay.amount,
                    Some(purchase_number.clone()),
                    Some(purchase_number.clone()),
                    purchase.purchase_date,
                )
                .await?;
            self.purchases
                .set_payment_refund_transaction(actor, pay.id, refund.id)
                .await?;
        }

        let cancelled = self
            .purchases
            .set_cancelled(purchase_id, actor, reason.as_deref())
            .await?;
        self.detail_for(cancelled).await
    }

    /// The documents drawer's draft delete — the mirror of the sale flow,
    /// extended to the discarded sibling. TWO states are deletable, and both
    /// posted nothing: a Draft, and a purchase discarded while still Draft
    /// (status Cancelled with `purchase_number` still NULL) — payments only
    /// exist on a Confirmed document, and the stock movement and the ledger
    /// entry are both created by `confirm`, so nothing dangles when the row
    /// goes; only its own CASCADE children die with it. A confirmed document
    /// — even one cancelled afterwards — is ANULLED through `cancel` instead:
    /// deleting one would strand its ledger entries and stock history, and
    /// its number proves it was confirmed. The refusal is always a Validation
    /// NAMING the state, never silent.
    ///
    /// No `actor` parameter, deliberately: nothing survives to stamp — the
    /// row and its lines are gone — and the control is the route's permission
    /// plus the fact that neither deletable state moved stock, money or debt.
    pub async fn delete_draft(&self, id: i64) -> AppResult<()> {
        let purchase = self
            .purchases
            .find_purchase(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {id} not found")))?;
        let deletable = purchase.status == PurchaseStatus::Draft
            || (purchase.status == PurchaseStatus::Cancelled && purchase.purchase_number.is_none());
        if !deletable {
            return Err(AppError::Validation(format!(
                "purchase {id} is {}: only a draft or a discarded (never-confirmed) cancelled purchase can be deleted",
                purchase.status
            )));
        }
        let deleted = self.purchases.delete_draft(id).await?;
        if !deleted {
            // A concurrent confirm won the race: the document is no longer
            // deletable, so the honest answer is the same refusal as above.
            return Err(AppError::Validation(format!(
                "purchase {id} is no longer deletable: only a draft or a discarded (never-confirmed) cancelled purchase can be deleted"
            )));
        }
        Ok(())
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
                .or_else(|| {
                    costs
                        .iter()
                        .min_by(|a, b| a.current_cost.cmp(&b.current_cost))
                })
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
    use crate::security::test_support;
    use crate::services::{
        InventoryService, PaymentMethodService, SupplierService, TransactionService,
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests below seed their own users instead, because
    /// there the point is telling two actors apart.
    /// Borrow-flexible so owned test services (`let (s, _) = svc().await`)
    /// and borrowed ones (the seed helpers' `s: &Svc`) call it the same way.
    async fn audit_actor(s: impl std::borrow::Borrow<Svc>) -> i64 {
        test_support::audit_actor_id(&s.borrow().transactions.accounts.pool)
            .await
            .unwrap()
    }

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
            .create_product(
                audit_actor(s).await,
                NewProduct {
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
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
    }

    async fn seed_service(s: &Svc, sku: &str, cost: &str) -> crate::models::Product {
        s.inventory
            .create_product(
                audit_actor(s).await,
                NewProduct {
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
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
    }

    async fn seed_stock(s: &Svc, product_id: i64, qty: &str) {
        s.inventory
            .record_movement(
                audit_actor(s).await,
                NewMovement {
                    product_id,
                    qty: dec(qty),
                    movement_type: MovementType::In,
                    reason: MovementReason::Initial,
                    reference: "".into(),
                    date: d(2024, 5, 1),
                },
            )
            .await
            .unwrap();
    }

    async fn seed_supplier(s: &Svc, name: &str) -> crate::models::Supplier {
        s.suppliers
            .create_supplier(
                audit_actor(s).await,
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

    async fn seed_account(s: &Svc, name: &str) -> crate::models::Account {
        s.transactions
            .accounts
            .create(audit_actor(s).await, name)
            .await
            .unwrap()
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
        s.payment_methods
            .methods
            .set_method_account(audit_actor(s).await, method_id, Some(account_id))
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

    async fn draft_cash(s: &Svc, supplier_id: i64) -> Purchase {
        s.create_draft(
            audit_actor(s).await,
            NewPurchase {
                supplier_id,
                payment_type: PaymentType::Cash,
                purchase_date: purchase_date(),
                due_date: None,
                supplier_invoice_no: None,
                notes: None,
            },
        )
        .await
        .unwrap()
    }

    async fn draft_credit(s: &Svc, supplier_id: i64) -> Purchase {
        s.create_draft(
            audit_actor(s).await,
            NewPurchase {
                supplier_id,
                payment_type: PaymentType::Credit,
                purchase_date: purchase_date(),
                due_date: Some(d(2024, 6, 1)),
                supplier_invoice_no: None,
                notes: None,
            },
        )
        .await
        .unwrap()
    }

    // -- Stale line cost (confirmed freshness flag) ---------------------------

    /// The real rendering path: build the record view for a purchase's lines
    /// through `record_from_detail`, never by hand-constructing the view.
    async fn line_views(s: &Svc, purchase_id: i64) -> Vec<PurchaseLineView> {
        let detail = s.get_detail(purchase_id).await.unwrap();
        s.record_from_detail(detail).await.unwrap().lines
    }

    #[tokio::test]
    async fn stale_line_cost_flags_line_cost_higher_than_stored_with_both_values() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-HIGH", "5").await;
        let sup = seed_supplier(&s, "CF HIGH SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("7")),
        )
        .await
        .unwrap();
        // Confirmed: the warning exists only after the document does. A credit
        // purchase confirms with no payment method.
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        let stale = views[0]
            .stale_cost
            .as_ref()
            .expect("a rising cost must be flagged");
        // Both numbers asserted so the fields cannot be swapped silently.
        assert_eq!(stale.line_cost, dec("7"));
        assert_eq!(stale.stored_cost, dec("5"));
    }

    #[tokio::test]
    async fn stale_line_cost_equal_cost_is_not_stale() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-EQ", "5").await;
        let sup = seed_supplier(&s, "CF EQ SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        assert!(views[0].stale_cost.is_none(), "equal is not stale");
    }

    #[tokio::test]
    async fn stale_line_cost_lower_cost_is_not_stale() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-LOW", "5").await;
        let sup = seed_supplier(&s, "CF LOW SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("3")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        assert!(
            views[0].stale_cost.is_none(),
            "a decrease is not what this warns about"
        );
    }

    #[tokio::test]
    async fn stale_line_cost_zero_stored_cost_is_never_stale() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-ZERO", "0").await;
        let sup = seed_supplier(&s, "CF ZERO SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        assert!(
            views[0].stale_cost.is_none(),
            "zero stored cost means none recorded yet"
        );
    }

    #[tokio::test]
    async fn stale_line_cost_mixed_lines_flags_only_qualified_lines() {
        let (s, _pool) = svc().await;
        let rising = seed_product(&s, "CF-MIX-A", "5").await;
        let equal = seed_product(&s, "CF-MIX-B", "5").await;
        let no_cost = seed_product(&s, "CF-MIX-C", "0").await;
        let lowering = seed_product(&s, "CF-MIX-D", "9").await;
        let sup = seed_supplier(&s, "CF MIX SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        let l_rising = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                rising.id,
                dec("1"),
                Some(dec("9")),
            )
            .await
            .unwrap();
        // The other three lines exist only to prove they are NOT flagged, so
        // their ids are not needed: the assertion below pins the flagged set
        // to exactly the rising one.
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            equal.id,
            dec("1"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            no_cost.id,
            dec("1"),
            Some(dec("8")),
        )
        .await
        .unwrap();
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            lowering.id,
            dec("1"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(
            views.len(),
            4,
            "every line renders; only qualifying ones are flagged"
        );
        let flagged: Vec<i64> = views
            .iter()
            .filter(|v| v.stale_cost.is_some())
            .map(|v| v.id)
            .collect();
        assert_eq!(
            flagged,
            vec![l_rising.id],
            "only the rising-cost line is flagged"
        );
        let stale = views
            .iter()
            .find(|v| v.id == l_rising.id)
            .unwrap()
            .stale_cost
            .as_ref()
            .unwrap();
        assert_eq!(stale.line_cost, dec("9"));
        assert_eq!(stale.stored_cost, dec("5"));
    }

    /// The correction that moved the gate: a draft whose line cost ROSE must
    /// NOT flag. The comparison asserts something the domain does not yet
    /// know — the line can still be edited or deleted, and the purchase may
    /// never be confirmed at all — so the cost stays provisional until the
    /// document exists. This is the test that fails if anyone removes the
    /// `Confirmed` gate from `record_from_detail`.
    #[tokio::test]
    async fn stale_line_cost_draft_purchase_does_not_flag_a_rising_line_cost() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-DRAFT", "5").await;
        let sup = seed_supplier(&s, "CF DRAFT SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        // Same shape as the confirmed positive case: line cost 7 over stored 5.
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("7")),
        )
        .await
        .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        assert!(
            views[0].stale_cost.is_none(),
            "a draft line's cost is provisional; only a confirmed purchase flags"
        );
    }

    /// The decided scope is `Confirmed` only. A purchase confirmed and later
    /// cancelled is a historical document: its costs were real when confirmed,
    /// but the document is closed and feeding them into a product update would
    /// be a silent write from a record that no longer moves goods or money, so
    /// it shows nothing and the apply handler refuses it. The positive control
    /// lives in the same test so it proves the cancellation is what removed the
    /// flag, not that the flag never existed — a gate loosened to anything
    /// non-draft (e.g. `!= Draft`) keeps every confirmed and draft test green
    /// while a cancelled purchase silently starts flagging.
    #[tokio::test]
    async fn stale_line_cost_confirmed_then_cancelled_does_not_flag() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "CF-CANCEL", "5").await;
        let sup = seed_supplier(&s, "CF CANCEL SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        // Same shape as the confirmed positive case: line cost 7 over stored 5.
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("7")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        // Positive control: while confirmed, this same purchase DID flag.
        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        let stale = views[0]
            .stale_cost
            .as_ref()
            .expect("a rising cost must be flagged while the purchase is confirmed");
        assert_eq!(stale.line_cost, dec("7"));
        assert_eq!(stale.stored_cost, dec("5"));

        s.cancel(audit_actor(&s).await, purchase.id, Some("devuelvo".into()))
            .await
            .unwrap();

        let views = line_views(&s, purchase.id).await;
        assert_eq!(views.len(), 1);
        assert!(
            views[0].stale_cost.is_none(),
            "a confirmed-then-cancelled purchase is a historical document; it must not flag"
        );
    }

    // -- Receiving desk T2: track_stock enrichment -----------------------------

    /// The record carries `tracked_units`: the sum of `qty` over the lines
    /// that `confirm`/`cancel` will actually move stock — the projection base
    /// for the effects preview. It is derived from the SAME per-line
    /// `tracks_stock` predicate the record already computes (never
    /// recomputed in the template), so it cannot drift from what the flows
    /// will do. The per-line flags are pinned alongside: a stock Product
    /// tracks, a Service does not.
    #[tokio::test]
    async fn record_tracked_units_sums_only_stock_tracking_lines() {
        let (s, _pool) = svc().await;
        let tracked = seed_product(&s, "TU-TRK", "5").await;
        let service = seed_service(&s, "TU-SVC", "8").await;
        let sup = seed_supplier(&s, "TU SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            tracked.id,
            dec("3"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            service.id,
            dec("4"),
            Some(dec("8")),
        )
        .await
        .unwrap();

        let detail = s.get_detail(purchase.id).await.unwrap();
        let record = s.record_from_detail(detail).await.unwrap();
        assert_eq!(
            record.tracked_units,
            dec("3"),
            "only the stock-tracking line counts toward the projection: {:?}",
            record.tracked_units
        );
        let tracked_view = record
            .lines
            .iter()
            .find(|l| l.product_id == tracked.id)
            .expect("the tracked line is in the record");
        let service_view = record
            .lines
            .iter()
            .find(|l| l.product_id == service.id)
            .expect("the service line is in the record");
        assert!(
            tracked_view.tracks_stock,
            "a stock Product line is what confirm/cancel will move"
        );
        assert!(
            !service_view.tracks_stock,
            "a Service line never moves stock, so it cannot feed the projection"
        );
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
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("2"),
                Some(dec("4")),
            )
            .await
            .unwrap();
        let updated = s
            .update_line(audit_actor(&s).await, line.id, dec("3"), dec("4.5"))
            .await
            .unwrap();
        assert_eq!(updated.qty, dec("3"));
        assert_eq!(updated.unit_cost, dec("4.5"));
        let edited = s
            .update_draft(
                audit_actor(&s).await,
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
        s.remove_line(audit_actor(&s).await, line.id).await.unwrap();

        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the initial stock move"
        );
        assert_eq!(tx_count(&pool).await, 0);
        assert!(s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .is_none());
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("3"),
            Some(dec("4")),
        )
        .await
        .unwrap(); // total 12

        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
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
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("12")),
        )
        .await
        .unwrap(); // total 24

        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        assert!(detail.purchase.purchase_number.is_some());
        assert_eq!(detail.total, dec("24"));
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("24"));
        assert_eq!(detail.payment_status, crate::models::PaymentStatus::Unpaid);
        assert_eq!(detail.purchase.due_date, Some(d(2024, 6, 1)));
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("12"));
        assert_eq!(tx_count(&pool).await, 0, "Credit confirm posts no Expense");
        assert!(s
            .purchases
            .list_payments(purchase.id)
            .await
            .unwrap()
            .is_empty());
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("20")),
        )
        .await
        .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        s.record_payment(
            audit_actor(&s).await,
            purchase.id,
            cash,
            dec("15"),
            d(2024, 5, 10),
        )
        .await
        .unwrap();
        let d1 = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(d1.paid, dec("15"));
        assert_eq!(d1.due, dec("25"));
        assert_eq!(d1.payment_status, crate::models::PaymentStatus::Partial);
        assert_eq!(tx_count(&pool).await, 1);

        // Overpay rejected with no extra finance row.
        let err = s
            .record_payment(
                audit_actor(&s).await,
                purchase.id,
                cash,
                dec("30"),
                d(2024, 5, 11),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, 1);

        // Paying the remainder closes the payable.
        s.record_payment(
            audit_actor(&s).await,
            purchase.id,
            cash,
            dec("25"),
            d(2024, 5, 12),
        )
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

    // -- T5: pay_supplier (oldest-first across purchases, no receipt) --------

    async fn draft_credit_due(s: &Svc, supplier_id: i64, due: NaiveDate) -> Purchase {
        s.create_draft(
            audit_actor(s).await,
            NewPurchase {
                supplier_id,
                payment_type: PaymentType::Credit,
                purchase_date: purchase_date(),
                due_date: Some(due),
                supplier_invoice_no: None,
                notes: None,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn t5_pay_supplier_covers_oldest_first_with_exact_dues() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "T5-P", "20").await;
        seed_stock(&s, prod.id, "20").await;
        let sup = seed_supplier(&s, "T5 SUP").await;
        let acc = seed_account(&s, "caja-t5").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // Two Confirmed credit purchases: A due 2024-06-01 (2 × 20 = 40),
        // B due 2024-07-01 (3 × 10 = 30).
        let a = draft_credit_due(&s, sup.id, d(2024, 6, 1)).await;
        s.add_line(
            audit_actor(&s).await,
            a.id,
            prod.id,
            dec("2"),
            Some(dec("20")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, a.id, None).await.unwrap();
        let b = draft_credit_due(&s, sup.id, d(2024, 7, 1)).await;
        s.add_line(
            audit_actor(&s).await,
            b.id,
            prod.id,
            dec("3"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, b.id, None).await.unwrap();

        // Pay 55: A covered exactly, B partial — one payment per purchase.
        let payments = s
            .pay_supplier(
                audit_actor(&s).await,
                sup.id,
                cash,
                dec("55"),
                d(2024, 6, 20),
            )
            .await
            .unwrap();
        assert_eq!(payments.len(), 2);
        assert_eq!(payments[0].purchase_id, a.id);
        assert_eq!(payments[0].amount, dec("40"));
        assert_eq!(payments[1].purchase_id, b.id);
        assert_eq!(payments[1].amount, dec("15"));

        let da = s.get_detail(a.id).await.unwrap();
        assert_eq!(da.due, Decimal::ZERO);
        assert_eq!(da.payment_status, crate::models::PaymentStatus::Paid);
        let db = s.get_detail(b.id).await.unwrap();
        assert_eq!(db.paid, dec("15"));
        assert_eq!(db.due, dec("15"));
        assert_eq!(tx_count(&pool).await, 2);

        // Pay the exact remainder: B closes, nothing more is planned.
        let rest = s
            .pay_supplier(
                audit_actor(&s).await,
                sup.id,
                cash,
                dec("15"),
                d(2024, 6, 21),
            )
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].purchase_id, b.id);
        assert_eq!(rest[0].amount, dec("15"));
        let db = s.get_detail(b.id).await.unwrap();
        assert_eq!(db.due, Decimal::ZERO);
        assert_eq!(db.payment_status, crate::models::PaymentStatus::Paid);
    }

    #[tokio::test]
    async fn t5_pay_supplier_overpay_rejected_with_no_side_effect() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "T5-OV", "20").await;
        seed_stock(&s, prod.id, "20").await;
        let sup = seed_supplier(&s, "T5 OV SUP").await;
        let acc = seed_account(&s, "caja-t5ov").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let a = draft_credit_due(&s, sup.id, d(2024, 6, 1)).await;
        s.add_line(
            audit_actor(&s).await,
            a.id,
            prod.id,
            dec("2"),
            Some(dec("20")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, a.id, None).await.unwrap();
        let b = draft_credit_due(&s, sup.id, d(2024, 7, 1)).await;
        s.add_line(
            audit_actor(&s).await,
            b.id,
            prod.id,
            dec("3"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, b.id, None).await.unwrap();

        // Outstanding is 70: paying 71 is a 400 naming both figures.
        let err = s
            .pay_supplier(
                audit_actor(&s).await,
                sup.id,
                cash,
                dec("71"),
                d(2024, 6, 20),
            )
            .await
            .unwrap_err();
        match &err {
            AppError::Validation(msg) => {
                assert!(msg.contains("71"), "amount must be named: {msg}");
                assert!(msg.contains("70"), "outstanding must be named: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(tx_count(&pool).await, 0, "no finance may be posted");
        let db = s.get_detail(b.id).await.unwrap();
        assert_eq!(db.paid, Decimal::ZERO);
        assert_eq!(db.due, dec("30"), "second purchase untouched");
    }

    #[tokio::test]
    async fn t5_pay_supplier_unknown_supplier_is_404() {
        let (s, _) = svc().await;
        let err = s
            .pay_supplier(
                audit_actor(&s).await,
                999_999,
                1,
                dec("10"),
                purchase_date(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn t5_pay_supplier_unassigned_method_is_400() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "T5-UM", "10").await;
        seed_stock(&s, prod.id, "5").await;
        let sup = seed_supplier(&s, "T5 UM SUP").await;
        // Cash stays unassigned: no account may derive from it.
        let cash = cash_method(&s).await;

        let a = draft_credit(&s, sup.id).await;
        s.add_line(audit_actor(&s).await, a.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, a.id, None).await.unwrap();

        let err = s
            .pay_supplier(
                audit_actor(&s).await,
                sup.id,
                cash,
                dec("5"),
                purchase_date(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, 0, "no finance may be posted");
        let da = s.get_detail(a.id).await.unwrap();
        assert_eq!(da.paid, Decimal::ZERO);
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
            .create_draft(
                audit_actor(&s).await,
                NewPurchase {
                    supplier_id: 999_999,
                    payment_type: PaymentType::Cash,
                    purchase_date: purchase_date(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        let purchase = draft_cash(&s, sup.id).await;
        let err = s
            .update_draft(
                audit_actor(&s).await,
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
            .add_line(audit_actor(&s).await, purchase.id, 999_999, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // qty <= 0 / unit_cost < 0 => 400.
        for bad_qty in [dec("0"), dec("-1")] {
            let err = s
                .add_line(audit_actor(&s).await, purchase.id, prod.id, bad_qty, None)
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        let err = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("1"),
                Some(dec("-0.01")),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // unit_cost = 0 is legal.
        let zero = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("1"),
                Some(Decimal::ZERO),
            )
            .await
            .unwrap();
        assert_eq!(zero.unit_cost, Decimal::ZERO);
        let err = s
            .update_line(audit_actor(&s).await, zero.id, dec("0"), dec("1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .update_line(audit_actor(&s).await, zero.id, dec("1"), dec("-1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Unknown method on Cash confirm => 404.
        let err = s
            .confirm(audit_actor(&s).await, purchase.id, Some(999_999))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown method on payment => 404.
        let cp = draft_credit(&s, sup.id).await;
        s.add_line(audit_actor(&s).await, cp.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, cp.id, None).await.unwrap();
        let err = s
            .record_payment(
                audit_actor(&s).await,
                cp.id,
                999_999,
                dec("5"),
                purchase_date(),
            )
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap();

        let err = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::Validation(_) | AppError::Conflict(_)),
            "got {err:?}"
        );

        let err = s
            .add_line(audit_actor(&s).await, purchase.id, prod.id, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let detail = s.get_detail(purchase.id).await.unwrap();
        let line_id = detail.lines[0].id;
        let err = s
            .update_line(audit_actor(&s).await, line_id, dec("2"), dec("9"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .remove_line(audit_actor(&s).await, line_id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .update_draft(
                audit_actor(&s).await,
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
                audit_actor(&s).await,
                acc.id,
                TransactionKind::Income,
                dec("100"),
                Some("fondo".into()),
                purchase_date(),
            )
            .await
            .unwrap();

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("4"),
            Some(dec("10")),
        )
        .await
        .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("14"));

        // Drain the account to 0 so a guarded refund would fail.
        s.transactions
            .create(
                audit_actor(&s).await,
                acc.id,
                TransactionKind::Expense,
                dec("60"),
                Some("gasto".into()),
                purchase_date(),
            )
            .await
            .unwrap();

        let cancelled = s
            .cancel(
                audit_actor(&s).await,
                purchase.id,
                Some(" devuelvo ".into()),
            )
            .await
            .unwrap();
        assert_eq!(cancelled.purchase.status, PurchaseStatus::Cancelled);
        assert_eq!(
            cancelled.purchase.cancel_reason.as_deref(),
            Some("devuelvo")
        );
        assert!(cancelled.purchase.cancelled_at.is_some());
        assert_eq!(
            cancelled.purchase.purchase_number.as_deref(),
            Some(number.as_str())
        );

        // Goods go back: 14 - 4 = 10, Out reason Purchase-return.
        assert_eq!(s.inventory.stock(prod.id).await.unwrap(), dec("10"));
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
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

    // -- Annulment pre-validation: partial-state detection ------------------------

    /// Invariant 10's "the residual is detected rather than hidden", mirrored from
    /// sales: a purchase whose annulment was PARTIALLY applied by an earlier
    /// attempt (a payment already carries a `refund_transaction_id`) must be
    /// refused, not doubled — a second pass would write the return movement a
    /// second time and refund twice. There is no aggregate balance guard here by
    /// design: the purchase refund is an `Income`, money entering the account, so
    /// `create_with_reference` enforces no balance precondition on it.
    #[tokio::test]
    async fn red_purch_cancel_refuses_a_partially_applied_annulment() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "ANUL-P", "5").await;
        seed_stock(&s, prod.id, "10").await;
        let sup = seed_supplier(&s, "ANUL SUP").await;
        let acc = seed_account(&s, "caja-anul-p").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("4"),
            Some(dec("10")),
        )
        .await
        .unwrap(); // total 40
        s.confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap();
        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
        assert_eq!(payments.len(), 1);

        // Simulate the earlier attempt's residual directly: one refund link on
        // the payment row, pointing at a real transaction (the FK is RESTRICT).
        // The real defect's first pass also left a Purchase-return movement
        // behind — which is what this guard refuses to double.
        let residual = s
            .transactions
            .create(
                audit_actor(&s).await,
                acc.id,
                TransactionKind::Income,
                dec("40"),
                Some("refund residual".into()),
                purchase_date(),
            )
            .await
            .unwrap();
        s.purchases
            .set_payment_refund_transaction(audit_actor(&s).await, payments[0].id, residual.id)
            .await
            .unwrap();
        let movements_before = movement_count(&pool).await;

        let err = s
            .cancel(audit_actor(&s).await, purchase.id, Some("otra vez".into()))
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("already linked"),
            "message must name the partial state: {msg}"
        );
        assert!(
            msg.contains("1 of 1"),
            "message must name how many refunds: {msg}"
        );

        // Nothing new was written: no additional return movement, status intact.
        assert_eq!(
            movement_count(&pool).await,
            movements_before,
            "a refused partial annulment must not write another movement"
        );
        let still = s
            .purchases
            .find_purchase(purchase.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.status, PurchaseStatus::Confirmed);
    }

    // -- AC9: satellite update on confirm + CRITICAL pre-validation ---------------

    #[tokio::test]
    async fn ac9_confirm_updates_satellite_previous_and_current() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "AC9", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("10"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        let purchase = s
            .create_draft(
                audit_actor(&s).await,
                NewPurchase {
                    supplier_id: sup.id,
                    payment_type: PaymentType::Credit,
                    purchase_date: d(2024, 5, 10),
                    due_date: Some(d(2024, 6, 1)),
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("12")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let row = s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_cost, dec("12"));
        assert_eq!(row.current_cost_date, d(2024, 5, 10));
        assert_eq!(row.previous_cost, Some(dec("10")));
        assert_eq!(row.previous_cost_date, Some(d(2024, 5, 1)));
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
            .record_cost(
                audit_actor(&s).await,
                prod_b.id,
                sup.id,
                dec("10"),
                d(2024, 5, 10),
            )
            .await
            .unwrap();

        let purchase = draft_cash(&s, sup.id).await; // 2024-05-02, before 05-10
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod_a.id,
            dec("1"),
            Some(dec("3")),
        )
        .await
        .unwrap();
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod_b.id,
            dec("1"),
            Some(dec("4")),
        )
        .await
        .unwrap();

        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Nothing was applied: no stock, no finance, no number, no satellite row.
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, 0);
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.purchase.status, PurchaseStatus::Draft);
        assert!(detail.purchase.purchase_number.is_none());
        assert!(s
            .suppliers
            .find_cost(prod_a.id, sup.id)
            .await
            .unwrap()
            .is_none());
        assert!(s.sequences.current("PURCH", 2024).await.unwrap().is_none());
        let b_row = s
            .suppliers
            .find_cost(prod_b.id, sup.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b_row.current_cost, dec("10"));
        assert_eq!(b_row.current_cost_date, d(2024, 5, 10));
    }

    // -- AC10: purchase never writes products.cost_price ---------------------------

    #[tokio::test]
    async fn ac10_purchase_never_writes_cost_price_and_satellite_wins() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC10", "5").await;
        let other = seed_product(&s, "AC10-2", "7").await;
        let sup = seed_supplier(&s, "AC10 SUP").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("9.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("12")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();

        let stored = s.inventory.get_product(prod.id).await.unwrap();
        assert_eq!(
            stored.cost_price,
            dec("5"),
            "cost_price must stay untouched"
        );
        // Satellite wins after the confirm recorded the line cost.
        assert_eq!(
            s.suppliers.reference_cost(prod.id).await.unwrap(),
            Some(dec("12"))
        );
        // No satellite rows => None, caller falls back to the column.
        assert_eq!(s.suppliers.reference_cost(other.id).await.unwrap(), None);
        let other_stored = s.inventory.get_product(other.id).await.unwrap();
        assert_eq!(other_stored.cost_price, dec("7"));
        let _ = pool;
    }

    // -- add_line empty-cost default: satellite for THIS supplier wins -------------

    #[tokio::test]
    async fn add_line_empty_cost_records_satellite_cost_for_this_supplier() {
        let (s, _pool) = svc().await;
        // Column says 5; the satellite says this supplier charges 9.50.
        let prod = seed_product(&s, "LINE-DEF", "5").await;
        let sup = seed_supplier(&s, "LINE-DEF SUP").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("9.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        let purchase = draft_credit(&s, sup.id).await;
        let line = s
            .add_line(audit_actor(&s).await, purchase.id, prod.id, dec("1"), None)
            .await
            .unwrap();

        assert_eq!(
            line.unit_cost,
            dec("9.50"),
            "empty cost must default to the supplier's satellite cost"
        );
    }

    #[tokio::test]
    async fn add_line_empty_cost_falls_back_to_product_column_without_satellite_row() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "LINE-FB", "5").await;
        let sup = seed_supplier(&s, "LINE-FB SUP").await;

        let purchase = draft_credit(&s, sup.id).await;
        let line = s
            .add_line(audit_actor(&s).await, purchase.id, prod.id, dec("1"), None)
            .await
            .unwrap();

        assert_eq!(
            line.unit_cost,
            dec("5"),
            "no satellite row => the product column is the fallback"
        );
    }

    #[tokio::test]
    async fn add_line_explicit_cost_wins_over_satellite_row() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "LINE-EXPL", "5").await;
        let sup = seed_supplier(&s, "LINE-EXPL SUP").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("9.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        let purchase = draft_credit(&s, sup.id).await;
        let line = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("1"),
                Some(dec("12")),
            )
            .await
            .unwrap();

        assert_eq!(
            line.unit_cost,
            dec("12"),
            "an explicit cost must not be replaced by the satellite"
        );
    }

    #[tokio::test]
    async fn add_line_empty_cost_does_not_leak_another_suppliers_row() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "LINE-LEAK", "5").await;
        let sup_a = seed_supplier(&s, "LINE-LEAK A").await;
        let sup_b = seed_supplier(&s, "LINE-LEAK B").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup_a.id,
                dec("8"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup_b.id,
                dec("6.25"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        let purchase_b = draft_credit(&s, sup_b.id).await;
        let line_b = s
            .add_line(
                audit_actor(&s).await,
                purchase_b.id,
                prod.id,
                dec("1"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            line_b.unit_cost,
            dec("6.25"),
            "a purchase for B must default to B's cost, not A's"
        );
    }

    // -- add_or_increment_line (S5b): a same-cost repeat merges into the
    //    existing line, a different cost keeps the exact 400 ---------------

    /// Unwraps an `Added` outcome, so each test states which branch it means.
    fn assert_added(outcome: LineAddOutcome) -> PurchaseLine {
        match outcome {
            LineAddOutcome::Added(line) => line,
            LineAddOutcome::Merged { .. } => panic!("expected Added, got Merged"),
        }
    }

    /// Unwraps a `Merged` outcome and checks the notice payload.
    fn assert_merged(outcome: LineAddOutcome, product_name: &str) -> PurchaseLine {
        match outcome {
            LineAddOutcome::Merged {
                line,
                product_name: name,
            } => {
                assert_eq!(name, product_name);
                line
            }
            LineAddOutcome::Added(_) => panic!("expected Merged, got Added"),
        }
    }

    #[tokio::test]
    async fn add_or_increment_line_without_a_line_creates_it() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "MERGE-NEW", "5").await;
        let sup = seed_supplier(&s, "MERGE-NEW SUP").await;
        let purchase = draft_credit(&s, sup.id).await;

        let line = assert_added(
            s.add_or_increment_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("2"),
                Some(dec("5")),
            )
            .await
            .unwrap(),
        );
        assert_eq!(line.product_id, prod.id);
        assert_eq!(line.qty, dec("2"));
        assert_eq!(line.unit_cost, dec("5"));
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.lines.len(), 1);
    }

    #[tokio::test]
    async fn add_or_increment_line_same_cost_merges_and_keeps_one_line() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "MERGE-SAME", "5").await;
        let sup = seed_supplier(&s, "MERGE-SAME SUP").await;
        let actor = audit_actor(&s).await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(actor, purchase.id, prod.id, dec("2"), Some(dec("5")))
            .await
            .unwrap();

        let line = assert_merged(
            s.add_or_increment_line(actor, purchase.id, prod.id, dec("3"), Some(dec("5")))
                .await
                .unwrap(),
            &prod.name,
        );
        assert_eq!(line.product_id, prod.id);
        assert_eq!(line.qty, dec("5"), "the merged quantity is the sum");
        assert_eq!(line.unit_cost, dec("5"), "the stored cost does not move");
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(
            detail.lines.len(),
            1,
            "exactly ONE line stays for the product"
        );
    }

    #[tokio::test]
    async fn add_or_increment_line_different_cost_keeps_the_existing_400() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "MERGE-DIFF", "5").await;
        let sup = seed_supplier(&s, "MERGE-DIFF SUP").await;
        let actor = audit_actor(&s).await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(actor, purchase.id, prod.id, dec("2"), Some(dec("5")))
            .await
            .unwrap();

        let err = s
            .add_or_increment_line(actor, purchase.id, prod.id, dec("1"), Some(dec("7")))
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(msg.contains("already has a line"), "{msg}");
                assert!(msg.contains("separate purchase"), "{msg}");
            }
            other => panic!("expected the same Validation 400, got {other:?}"),
        }
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(
            detail.lines[0].qty,
            dec("2"),
            "the line's quantity is unchanged"
        );
        assert_eq!(
            detail.lines[0].unit_cost,
            dec("5"),
            "the stored cost is unchanged"
        );
    }

    #[tokio::test]
    async fn add_or_increment_line_empty_cost_merges_through_the_satellite_row() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "MERGE-SAT", "5").await;
        let sup = seed_supplier(&s, "MERGE-SAT SUP").await;
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("9.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();
        let actor = audit_actor(&s).await;
        let purchase = draft_credit(&s, sup.id).await;
        // The first scan carries an empty cost: the satellite row prices the line.
        s.add_line(actor, purchase.id, prod.id, dec("1"), None)
            .await
            .unwrap();

        // The case a receiving desk actually hits: the same scan again, still
        // with an empty cost, resolving to the same satellite cost.
        let line = assert_merged(
            s.add_or_increment_line(actor, purchase.id, prod.id, dec("2"), None)
                .await
                .unwrap(),
            &prod.name,
        );
        assert_eq!(line.qty, dec("3"));
        assert_eq!(line.unit_cost, dec("9.50"));
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(
            detail.lines.len(),
            1,
            "exactly ONE line stays for the product"
        );
    }

    /// Ordering pin: `add_or_increment_line` resolves the cost BEFORE the
    /// uniqueness check (the merge decision needs it), so a repeat product
    /// with an INVALID explicit cost answers the cost error, not the
    /// duplicate-product 400 `add_line` would have answered first. Both are
    /// 400 and only invalid input is affected — this test records that
    /// consequence so a reorder cannot change it silently.
    #[tokio::test]
    async fn add_or_increment_line_negative_explicit_cost_reports_the_cost_error() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "MERGE-NEG", "5").await;
        let sup = seed_supplier(&s, "MERGE-NEG SUP").await;
        let actor = audit_actor(&s).await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(actor, purchase.id, prod.id, dec("2"), Some(dec("5")))
            .await
            .unwrap();

        let err = s
            .add_or_increment_line(actor, purchase.id, prod.id, dec("1"), Some(dec("-3")))
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("unit_cost cannot be negative"),
                    "the cost error wins over the duplicate error: {msg}"
                );
                assert!(
                    !msg.contains("already has a line"),
                    "the duplicate message must not appear: {msg}"
                );
            }
            other => panic!("expected the cost Validation 400, got {other:?}"),
        }
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(
            detail.lines[0].qty,
            dec("2"),
            "the line's quantity is unchanged"
        );
        assert_eq!(
            detail.lines[0].unit_cost,
            dec("5"),
            "the stored cost is unchanged"
        );
    }

    // -- AC11: references ---------------------------------------------------------

    #[tokio::test]
    async fn ac11_stock_and_finance_rows_reference_purchase_number() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "AC11", "5").await;
        let sup = seed_supplier(&s, "AC11 SUP").await;
        let purchase = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("6")),
        )
        .await
        .unwrap();
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        let received = moves
            .iter()
            .find(|m| m.reason == MovementReason::Purchase)
            .unwrap();
        assert_eq!(received.reference, number);

        let acc = seed_account(&s, "caja11").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        s.record_payment(
            audit_actor(&s).await,
            purchase.id,
            cash,
            dec("5"),
            d(2024, 5, 20),
        )
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
            .record_cost(
                audit_actor(&s).await,
                pref_prod.id,
                sup_a.id,
                dec("9"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                pref_prod.id,
                sup_b.id,
                dec("7"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();
        s.suppliers
            .set_preferred(audit_actor(&s).await, pref_prod.id, sup_a.id)
            .await
            .unwrap();

        // No preferred for cheap_prod: the cheapest current cost wins.
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                cheap_prod.id,
                sup_a.id,
                dec("6.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();
        s.suppliers
            .record_cost(
                audit_actor(&s).await,
                cheap_prod.id,
                sup_b.id,
                dec("6"),
                d(2024, 5, 1),
            )
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

    // -- AC14: unassigned-method rejection without side effects ------------------

    #[tokio::test]
    async fn ac14_unassigned_method_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "AC14", "10").await;
        let sup = seed_supplier(&s, "AC14 SUP").await;
        let acc = seed_account(&s, "caja14").await;
        let cash = cash_method(&s).await;
        let qr = method_by_name(&s, "QR").await;
        allow(&s, acc.id, cash).await; // QR intentionally unassigned

        // Cash confirm with an unassigned method => 400, nothing applied.
        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(audit_actor(&s).await, purchase.id, Some(qr))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, 0);
        let detail = s.get_detail(purchase.id).await.unwrap();
        assert!(detail.purchase.purchase_number.is_none());
        assert_eq!(detail.purchase.status, PurchaseStatus::Draft);

        // Payment with an unassigned method => 400, no finance row.
        let credit = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            credit.id,
            prod.id,
            dec("2"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, credit.id, None)
            .await
            .unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(
                audit_actor(&s).await,
                credit.id,
                qr,
                dec("5"),
                purchase_date(),
            )
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            svc_prod.id,
            dec("3"),
            Some(dec("20")),
        )
        .await
        .unwrap();
        let before = movement_count(&pool).await;
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
            .await
            .unwrap();
        assert_eq!(detail.total, dec("60"));
        // Services are not stock-tracked: no movement at all.
        assert_eq!(movement_count(&pool).await, before);
        assert_eq!(s.inventory.stock(svc_prod.id).await.unwrap(), dec("0"));
        assert_eq!(tx_count(&pool).await, 1);
        // Satellite cost still recorded for the service.
        let row = s
            .suppliers
            .find_cost(svc_prod.id, sup.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_cost, dec("20"));
    }

    #[tokio::test]
    async fn tri_draft_cancel_is_noop_and_keeps_number_null() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "TRI-DRAFT", "5").await;
        seed_stock(&s, prod.id, "5").await;
        let sup = seed_supplier(&s, "TRI DRAFT SUP").await;
        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;

        let cancelled = s
            .cancel(audit_actor(&s).await, purchase.id, Some("ya no".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.purchase.status, PurchaseStatus::Cancelled);
        assert!(cancelled.purchase.purchase_number.is_none());
        assert_eq!(movement_count(&pool).await, moves_before);
        assert_eq!(tx_count(&pool).await, tx_before);
        assert!(s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .is_none());
        // Cancelling again => 400.
        let err = s
            .cancel(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_numbers_unique_and_immutable_after_cancel() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "TRI-NUM", "5").await;
        let sup = seed_supplier(&s, "TRI NUM SUP").await;
        let a = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            a.id,
            prod.id,
            dec("1"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        let b = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            b.id,
            prod.id,
            dec("1"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        let da = s.confirm(audit_actor(&s).await, a.id, None).await.unwrap();
        let db = s.confirm(audit_actor(&s).await, b.id, None).await.unwrap();
        let na = da.purchase.purchase_number.clone().unwrap();
        let nb = db.purchase.purchase_number.clone().unwrap();
        assert_ne!(na, nb);
        assert_eq!(na, "2024-PURCH-000001");
        assert_eq!(nb, "2024-PURCH-000002");

        // Cancelling keeps the assigned number (immutable).
        let cancelled = s.cancel(audit_actor(&s).await, a.id, None).await.unwrap();
        assert_eq!(
            cancelled.purchase.purchase_number.as_deref(),
            Some(na.as_str())
        );
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("10")),
        )
        .await
        .unwrap(); // total 20, account has 0
        let moves_before = movement_count(&pool).await;
        let err = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("20")),
        )
        .await
        .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();
        s.record_payment(
            audit_actor(&s).await,
            purchase.id,
            cash,
            dec("15"),
            d(2024, 5, 10),
        )
        .await
        .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            purchase.id,
            transfer,
            dec("25"),
            d(2024, 5, 11),
        )
        .await
        .unwrap();

        s.cancel(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("5")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        s.cancel(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(
                audit_actor(&s).await,
                purchase.id,
                cash,
                dec("5"),
                purchase_date(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, tx_before);
        let err = s
            .cancel(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_update_draft_dates_supplier_and_payment_type() {
        let (s, _pool) = svc().await;
        let sup_a = seed_supplier(&s, "TRI-UPD A").await;
        let sup_b = seed_supplier(&s, "TRI-UPD B").await;

        // Cash with a due_date is invalid.
        let err = s
            .create_draft(
                audit_actor(&s).await,
                NewPurchase {
                    supplier_id: sup_a.id,
                    payment_type: PaymentType::Cash,
                    purchase_date: purchase_date(),
                    due_date: Some(d(2024, 6, 1)),
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Credit without due_date is invalid.
        let err = s
            .create_draft(
                audit_actor(&s).await,
                NewPurchase {
                    supplier_id: sup_a.id,
                    payment_type: PaymentType::Credit,
                    purchase_date: purchase_date(),
                    due_date: None,
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let purchase = draft_credit(&s, sup_a.id).await;
        // Switching to Cash must clear the due_date in the same patch.
        let err = s
            .update_draft(
                audit_actor(&s).await,
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
                audit_actor(&s).await,
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("5")),
        )
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("4")),
        )
        .await
        .unwrap();

        // A purchase cannot repeat a product: the satellite holds one cost per
        // (product, supplier), so two different line costs have no defined
        // answer. The second add is rejected and the purchase is unchanged.
        let err = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("2"),
                Some(dec("6")),
            )
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            other.id,
            dec("1"),
            Some(dec("9")),
        )
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
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                prod.id,
                dec("1"),
                Some(dec("4")),
            )
            .await
            .unwrap();
        let ok_line = s
            .add_line(
                audit_actor(&s).await,
                purchase.id,
                other.id,
                dec("1"),
                Some(dec("5")),
            )
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
                .update_line(audit_actor(&s).await, line_id, dec("2"), dec("7"))
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
            .update_line(audit_actor(&s).await, ok_line.id, dec("3"), dec("8"))
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("1"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        s.purchases
            .create_line(purchase.id, prod.id, dec("1"), dec("6"))
            .await
            .unwrap();

        // Defensive: add_line/update_line already reject duplicates, but a
        // confirm must never accept a repeated product either.
        let err = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap_err();
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
        s.add_line(
            audit_actor(&s).await,
            first.id,
            prod.id,
            dec("1"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, first.id, None)
            .await
            .unwrap();

        let second = draft_credit(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            second.id,
            prod.id,
            dec("2"),
            Some(dec("6")),
        )
        .await
        .unwrap();
        let detail = s
            .confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap();

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
            .record_cost(
                audit_actor(&s).await,
                prod.id,
                sup.id,
                dec("4"),
                d(2024, 5, 1),
            )
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
            audit_actor(&s).await,
            purchase.id,
            item.product.id,
            item.suggested_qty,
            Some(item.unit_cost),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
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
        // The widened CHECK accepts the new Purchase-return reason. The audit
        // column (slice S10) is NOT NULL: the write carries the sentinel, the
        // same actor the migration attributed the legacy row to.
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO stock_movements (product_id, qty, type, reason, reference, date, created_by) \
             VALUES (?, '1', 'Out', 'Purchase-return', '2024-PURCH-000001', '2024-01-02', ?)",
        )
        .bind(product_id.0)
        .bind(actor)
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("10")),
        )
        .await
        .unwrap(); // total 20

        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, Some(cash))
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
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("20")),
        )
        .await
        .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let number = detail.purchase.purchase_number.clone().unwrap();

        let paid = s
            .record_payment(
                audit_actor(&s).await,
                purchase.id,
                cash,
                dec("15"),
                d(2024, 5, 10),
            )
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

        s.cancel(audit_actor(&s).await, purchase.id, Some("return".into()))
            .await
            .unwrap();

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

    /// N5 follow-up: the purchase filters run in the repository too, so the details
    /// loaded scale with the matching documents. The repository's test-only read
    /// counter makes the before/after difference deterministic.
    #[tokio::test]
    async fn list_details_filtered_reads_only_the_result_set() {
        let (s, _pool) = svc().await;
        let match_supplier = seed_supplier(&s, "PerfMatch").await;
        let other_supplier = seed_supplier(&s, "PerfOther").await;
        let mut matching = 0;
        for i in 0..20 {
            let supplier_id = if i == 3 {
                match_supplier.id
            } else {
                other_supplier.id
            };
            let purchase = s
                .create_draft(
                    audit_actor(&s).await,
                    NewPurchase {
                        supplier_id,
                        payment_type: PaymentType::Cash,
                        purchase_date: purchase_date(),
                        due_date: None,
                        supplier_invoice_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap();
            if i == 3 {
                matching = purchase.id;
            }
        }

        s.purchases.reset_reads();
        let details = s
            .list_details_filtered(&crate::models::PurchaseListFilter {
                supplier: Some("PerfMatch".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        let reads = s.purchases.read_count();

        assert_eq!(
            details.len(),
            1,
            "the supplier filter narrows to its purchase"
        );
        assert_eq!(details[0].purchase.id, matching);
        assert_eq!(
            reads, 3,
            "one filtered query plus the matching document's lines and payments only, got {reads} reads for 20 purchases"
        );
    }

    /// AC18, the INVENTORY half of the purchase flow: the stock movement a
    /// confirmed purchase produces carries the CONFIRMING request's actor —
    /// the same argument that stamps the flow's finance rows — never a fresh
    /// one, and it stays distinct from the product's own creator.
    #[tokio::test]
    async fn ac18_the_purchase_flow_movement_carries_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "pur-alice", "Alice")
            .await
            .unwrap();
        let operator = test_support::seed_audit_user(&pool, "pur-bob", "Bob")
            .await
            .unwrap();

        let acc = seed_account(&s, "purstock").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let prod = seed_product(&s, "PURFLOW", "5").await;
        // The fixture is seeded by the migration's sentinel: a valid actor,
        // but distinct from BOTH dedicated users, so the movement assertion
        // below can tell all three apart.
        assert_eq!(
            prod.created_by,
            test_support::audit_actor_id(&pool).await.unwrap(),
            "the fixture's actor is the sentinel"
        );
        assert_ne!(prod.created_by, creator);
        assert_ne!(
            prod.created_by, operator,
            "the two actors are distinguishable"
        );
        let sup = seed_supplier(&s, "Pur Flow Sup").await;

        let purchase = draft_cash(&s, sup.id).await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("3"),
            Some(dec("4")),
        )
        .await
        .unwrap();

        let _detail = s.confirm(operator, purchase.id, Some(cash)).await.unwrap();
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        let purchase_move = moves
            .iter()
            .find(|m| m.reason == MovementReason::Purchase)
            .unwrap_or_else(|| panic!("the purchase confirm produced its own movement"));
        assert_eq!(
            purchase_move.created_by, operator,
            "the flow's actor, not a fresh one"
        );
        assert_ne!(
            purchase_move.created_by, prod.created_by,
            "distinct from the product's creator"
        );
        assert_eq!(
            purchase_move.updated_by, None,
            "an append-only movement has no editor"
        );
    }

    // -- AC18 (purchases audit, slice S12): two actors, the flow's payment, and
    //    the satellite cost rows the confirm writes ------------------------------

    /// Alice creates the draft, Bob edits and confirms it (the confirm also
    /// writes the satellite cost rows and, for Cash, the payment), Alice pays
    /// and cancels. Every stored row names exactly the actor of the request
    /// that produced it, never the sentinel and never a fresh one.
    #[tokio::test]
    async fn ac18_the_purchase_records_two_different_actors_and_its_payment_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "purch-alice", "Alice")
            .await
            .unwrap();
        let editor = test_support::seed_audit_user(&pool, "purch-bob", "Bob")
            .await
            .unwrap();

        let prod = seed_product(&s, "PURCH-AUD", "5").await;
        let sup = seed_supplier(&s, "PURCH AUD SUP").await;
        let acc = seed_account(&s, "purch-audit-wallet").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // Alice creates the draft: the row names her and no editor yet.
        let purchase = s
            .create_draft(
                creator,
                NewPurchase {
                    supplier_id: sup.id,
                    payment_type: PaymentType::Credit,
                    purchase_date: purchase_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(purchase.created_by, creator, "the draft's creator");
        assert_eq!(purchase.updated_by, None, "a fresh draft has no editor");

        // A line change edits the draft document: the line inherits the
        // purchase's actor and the parent names the requesting user.
        s.add_line(creator, purchase.id, prod.id, dec("2"), Some(dec("10")))
            .await
            .unwrap();
        let after_line = s.get_detail(purchase.id).await.unwrap().purchase;
        assert_eq!(after_line.created_by, creator);
        assert_eq!(
            after_line.updated_by,
            Some(creator),
            "the line change edits the draft"
        );

        // Bob edits the header: the same document now names its last editor,
        // and the creator is untouched.
        let edited = s
            .update_draft(
                editor,
                purchase.id,
                UpdatePurchaseDraft {
                    notes: Some("edited".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.created_by, creator);
        assert_eq!(edited.updated_by, Some(editor));

        // Bob confirms (Credit: receivable, no cash payment): updated_by stays
        // Bob, and the satellite cost row the confirm writes carries HIS actor
        // — the same argument that stamps the finance and stock rows (AC18).
        let confirmed = s.confirm(editor, purchase.id, None).await.unwrap();
        assert_eq!(confirmed.purchase.created_by, creator);
        assert_eq!(confirmed.purchase.updated_by, Some(editor));
        let cost = s
            .suppliers
            .find_cost(prod.id, sup.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cost.created_by, editor,
            "the confirm's cost row names the confirming actor"
        );
        assert_ne!(
            cost.created_by, creator,
            "distinct from the draft's creator"
        );

        // Alice records a payment: the payment row carries the recording
        // request's actor, not the sale's creator and not a fresh one.
        let payment = s
            .record_payment(creator, purchase.id, cash, dec("10"), purchase_date())
            .await
            .unwrap();
        assert_eq!(payment.created_by, creator, "the flow's actor");
        assert_ne!(
            payment.created_by, editor,
            "distinct from the confirming user"
        );
        assert_eq!(payment.updated_by, None, "a fresh payment has no editor");
        let stored = s.purchases.list_payments(purchase.id).await.unwrap();
        assert_eq!(stored[0].created_by, creator, "the stored row keeps it");

        // Alice cancels: the refund links the payment rows carry HER actor in
        // updated_by, like the refund Income she caused.
        let cancelled = s
            .cancel(creator, purchase.id, Some("audit".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.purchase.created_by, creator);
        assert_eq!(cancelled.purchase.updated_by, Some(creator));
        let payments = s.purchases.list_payments(purchase.id).await.unwrap();
        assert_eq!(
            payments[0].updated_by,
            Some(creator),
            "the refund link names its writer"
        );
        assert_eq!(payments[0].created_by, creator, "the creator never changes");
    }

    /// `find_payment` is the read-by-id the documents drawer uses: found
    /// returns the stored payment, absent is the standard `NotFound` error.
    #[tokio::test]
    async fn find_payment_returns_the_stored_row_or_not_found() {
        let (s, _pool) = svc().await;
        let sup = seed_supplier(&s, "FindPay Supplier").await;
        let purchase = draft_credit(&s, sup.id).await;
        let prod = seed_product(&s, "FINDPAY-P", "4").await;
        s.add_line(
            audit_actor(&s).await,
            purchase.id,
            prod.id,
            dec("2"),
            Some(dec("4")),
        )
        .await
        .unwrap();
        let cash = cash_method(&s).await;
        let wallet = seed_account(&s, "findpay wallet").await;
        allow(&s, wallet.id, cash).await;
        s.confirm(audit_actor(&s).await, purchase.id, None)
            .await
            .unwrap();
        let recorded = s
            .record_payment(
                audit_actor(&s).await,
                purchase.id,
                cash,
                dec("5"),
                purchase_date(),
            )
            .await
            .unwrap();

        let found = s.find_payment(recorded.id).await.unwrap();
        assert_eq!(found.id, recorded.id);
        assert_eq!(found.purchase_id, purchase.id);
        assert_eq!(found.amount, dec("5"));

        let missing = s.find_payment(999_999).await;
        assert!(
            matches!(&missing, Err(AppError::NotFound(msg)) if msg.contains("payment")),
            "an unknown payment must be NotFound naming the family: {missing:?}"
        );
    }

    // -- delete_draft (the documents drawer's draft delete) --------------------

    async fn draft_with_one_tracked_line(s: &Svc, sku: &str) -> crate::models::Purchase {
        draft_with_line_typed(s, sku, PaymentType::Cash).await
    }

    /// The confirmed-refusal test uses CREDIT: a cash confirm demands the
    /// method's account, and the refusal under test is about STATE, not about
    /// payment setup.
    async fn draft_with_line_typed(
        s: &Svc,
        sku: &str,
        payment_type: PaymentType,
    ) -> crate::models::Purchase {
        let actor = audit_actor(s).await;
        let supplier = seed_supplier(s, "Delete Supplier").await;
        let product = seed_product(s, sku, "5").await;
        let purchase = s
            .create_draft(
                actor,
                NewPurchase {
                    supplier_id: supplier.id,
                    payment_type,
                    purchase_date: purchase_date(),
                    // Credit requires a due date; Cash requires none.
                    due_date: match payment_type {
                        PaymentType::Credit => Some(purchase_date()),
                        PaymentType::Cash => None,
                    },
                    supplier_invoice_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(actor, purchase.id, product.id, dec("2"), None)
            .await
            .unwrap();
        purchase
    }

    /// A draft is the one deletable state: the row and its lines go, and a
    /// later read is the standard NotFound.
    #[tokio::test]
    async fn delete_draft_removes_a_draft_and_get_detail_then_404s() {
        let (s, _pool) = svc().await;
        let purchase = draft_with_one_tracked_line(&s, "DEL-P").await;

        s.delete_draft(purchase.id).await.unwrap();
        let err = s.get_detail(purchase.id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::NotFound(msg) if msg.contains("purchase")),
            "the deleted draft must be NotFound naming the family: {err:?}"
        );
    }

    /// The service refuses a confirmed document NAMING the state; the SQL
    /// backstop is proven separately at the repository level.
    #[tokio::test]
    async fn delete_draft_refuses_a_confirmed_purchase_with_a_validation_naming_the_state() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let purchase = draft_with_line_typed(&s, "DEL-C", PaymentType::Credit).await;
        // A credit confirm takes NO method (the payment comes later).
        s.confirm(actor, purchase.id, None).await.unwrap();

        let err = s.delete_draft(purchase.id).await.unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Confirmed"),
                    "the refusal must name the state: {msg}"
                );
                assert!(
                    msg.contains("draft"),
                    "the refusal must say only a draft can be deleted: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // The document survives the refused delete.
        assert!(s.get_detail(purchase.id).await.is_ok());
    }

    /// A discarded purchase (cancelled while still Draft: `purchase_number`
    /// stays NULL) posted nothing, so it IS deletable: the row and its lines
    /// go and a later read is the standard NotFound.
    #[tokio::test]
    async fn delete_draft_removes_a_discarded_cancelled_purchase_and_get_detail_then_404s() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let purchase = draft_with_one_tracked_line(&s, "DEL-X").await;
        s.cancel(actor, purchase.id, None).await.unwrap();

        s.delete_draft(purchase.id).await.unwrap();
        let err = s.get_detail(purchase.id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::NotFound(msg) if msg.contains("purchase")),
            "the deleted discarded purchase must be NotFound naming the family: {err:?}"
        );
    }

    /// Confirmed-then-cancelled: the number proves it was confirmed, so the
    /// refusal is a Validation NAMING the state (never silent) and the row
    /// survives. The SQL backstop is proven separately at the repository level.
    #[tokio::test]
    async fn delete_draft_refuses_a_confirmed_then_cancelled_purchase_naming_the_state() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let purchase = draft_with_line_typed(&s, "DEL-Y", PaymentType::Credit).await;
        s.confirm(actor, purchase.id, None).await.unwrap();
        s.cancel(actor, purchase.id, Some("wrong order".to_string()))
            .await
            .unwrap();

        let err = s.delete_draft(purchase.id).await.unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Cancelled"),
                    "the refusal must name the state: {msg}"
                );
                assert!(
                    msg.contains("draft"),
                    "the refusal must say only a draft can be deleted: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(s.get_detail(purchase.id).await.is_ok());
    }

    #[tokio::test]
    async fn delete_draft_of_an_unknown_purchase_is_not_found() {
        let (s, _pool) = svc().await;
        let err = s.delete_draft(999_999).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    }
}
