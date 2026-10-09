// M2 sales orchestrator (Odoo-style).
// SalesService calls InventoryService for stock Out (reason Sale) / In
// (reason Sale-return) and TransactionService for Income per payment /
// Expense refund with reference = sale_number. It never SQLs `transactions`
// or `stock_movements` directly (all finance/stock rows go via services).
//
// Numbering: YYYY-SALE-NNNNNN assigned on confirm via `doc_sequences` row
// UPDATE (UPSERT + RETURNING, atomic). Draft touches nothing. Cash confirm
// creates 1 payment (method-derived account) + Income; Credit confirm creates
// receivable, no Income. Payments carry account+method, N per sale to mixed
// methods with SUM <= total. The method must be assigned to an account (400)
// before any stock/sequence/finance touch. Overpay rejected. Double confirm /
// edit Confirmed rejected. Cancel from Confirmed re-enters stock + refunds
// guarded by allow flags. Service / untracked lines sellable without stock
// moves. Decimal-as-TEXT via repos.
//
// Atomicity: there is NO shared transaction. A true one would require changing
// the finance/inventory services (out of scope when this was written). Instead
// we pre-validate (accounts, stock availability, balances) before any
// mutation, then mutate in order sequence -> stock -> finance -> sale row, each
// step on its own autocommit connection. A failure after the pre-validation
// therefore leaves a PARTIAL WRITE, and the residue is measured, not assumed —
// see the failure-window tests in this module's test block, and
// odd/tasks/confirm-failure-injection-and-state-predicates.md. What a partial
// write can leave, all of it observed:
//
//   W1  number -> 1st movement   Draft, number NULL, no rows written
//   W2  2nd of N movements      Draft, number NULL, ONE Out committed (stock
//                               already deducted, reference matches no sale)
//   W3  finance -> payment      Draft, number NULL, an orphan Income carrying
//                               the burned number and the full total
//   W4  set_confirmed           Draft, number NULL, orphan Income AND a
//                               payment row — get_detail reports this Draft
//                               as Paid, paid=total, due=0
//
// W4 is the alarming one: the books show a full collection against a document
// still editable as a Draft. `cancel` cannot release it, because cancel is a
// hard no-op on a Draft (it is gated behind Confirmed), and the residue has no
// parent document to reverse from.
//
// The pre-check reduces the windows but does not close them, and the number is
// spent in every one. The predicate on set_confirmed refuses a duplicate
// submission of an already-confirmed document; it does NOT make a failed
// confirm safe to retry, because a failed attempt leaves the document in Draft
// and the retry therefore re-passes the very same predicate. Only the shared
// transaction removes the residue, and it also removes the sequence gap for
// free, since `UPDATE last_number = last_number + 1` is already a
// transactional write.
use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    format_sale_number, Ageing, Customer, CustomerAgeing, CustomerStatement, DebtSummary,
    LineTaxView, MovementReason, MovementType, NewMovement, NewSale, PaymentStatus, PaymentType,
    PriceRefusal, ProductKind, RecordMoney, Sale, SaleDetail, SaleLine, SaleLineView,
    SaleListFilter, SaleListRow, SalePayment, SalePaymentView, SaleRecord, SetMoney,
    StatementEntry, StatementEntryKind, UpdateSaleDraft,
};
use crate::repositories::payment_repo::DocumentResidualParts;
use crate::repositories::{
    AccountRepository, BarcodeRepository, CategoryRepository, CustomerRepository,
    DocSequenceRepository, PaymentMethodRepository, ProductRepository, SaleRepository,
    StockMovementRepository, TransactionRepository,
};
use crate::services::line_taxes::tax_inclusive_total;
use crate::services::{checked_money_add, checked_money_sum, CustomerService};

/// How many of the oldest unpaid documents the sales page debt banner renders.
pub const DEBT_BANNER_LIMIT: usize = 5;

#[derive(Clone)]
pub struct SalesService<SR, DR, C, P, B, S, A, T, PM, CR, TS, PL, PY>
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
    TS: crate::repositories::TaxSnapshotRepository,
    PL: crate::repositories::PartyLedgerRepository,
    PY: crate::repositories::PaymentRepository,
{
    pub sales: SR,
    pub sequences: DR,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
    pub payment_methods: PM,
    /// Sales reach customers only through this service: the sale rows store a
    /// snapshot, and no sales repository runs SQL against the `customers` table.
    pub customers: CustomerService<CR>,
    /// Read-only access to the frozen line tax breakdowns the record page shows.
    /// The sale repository OWNS the writes; this service only reads them, so a
    /// document's tax history can be displayed without a write seam existing.
    pub tax_snapshots: TS,
    /// `ENFORCE_CREDIT_LIMIT`: when false an over-limit credit sale is confirmed
    /// and the interface reports the customer as over limit instead.
    pub enforce_credit_limit: bool,
    /// The customer's signed journal (T2). `confirm` appends the `Charge` that
    /// makes the sale a debt, inside the SAME unit that writes the document —
    /// which is the whole point of the ledger: the balance is the fold of these
    /// rows, so an entry that committed separately from its document would be a
    /// balance that moved without a document, or a document with no balance.
    pub party_ledger: PL,
    /// The `payments` family (P3): the delivery-of-money document, its shares and
    /// its cap. A second field rather than folding it into `party_ledger`, because
    /// they are two tables with two rules: the journal is append-only and the
    /// balance IS its fold, while a payment's shares are mutable and capped.
    pub payments: PY,
}

impl<SR, DR, C, P, B, S, A, T, PM, CR, TS, PL, PY>
    SalesService<SR, DR, C, P, B, S, A, T, PM, CR, TS, PL, PY>
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
    TS: crate::repositories::TaxSnapshotRepository,
    PL: crate::repositories::PartyLedgerRepository,
    PY: crate::repositories::PaymentRepository,
{
    pub fn new(
        sales: SR,
        sequences: DR,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
        payment_methods: PM,
        customers: CustomerService<CR>,
        tax_snapshots: TS,
        enforce_credit_limit: bool,
        party_ledger: PL,
        payments: PY,
    ) -> Self {
        Self {
            sales,
            sequences,
            inventory,
            transactions,
            payment_methods,
            tax_snapshots,
            customers,
            enforce_credit_limit,
            party_ledger,
            payments,
        }
    }

    async fn resolve_method_account(
        &self,
        method_id: i64,
        stated_account_id: Option<i64>,
    ) -> AppResult<i64> {
        crate::services::finance_methods::resolve_account_for(
            &self.payment_methods,
            method_id,
            stated_account_id,
        )
        .await
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
                        let days = customer.due_days.ok_or_else(|| {
                            AppError::Validation(
                                "due_date is required for Credit when the customer has no payment term"
                                    .into(),
                            )
                        })?;
                        sale_date + chrono::Duration::days(days)
                    }
                };
                if due < sale_date {
                    return Err(AppError::Validation("due_date must be >= sale_date".into()));
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

    /// The document's money, in three parts: the NET subtotal, the tax the
    /// lines' frozen snapshots charge, and the tax-inclusive total.
    ///
    /// The total is the sum of each line's PINNED line total
    /// (`round(qty * price + tax_total)`), not `round(net + tax)` over the whole
    /// document. That is deliberate: the record page shows the line totals, so a
    /// document total that did not equal their sum would be unauditable. The
    /// two differ whenever a line's net part carries a third decimal.
    ///
    /// # Every accumulation is checked, and the signature is what makes that so
    ///
    /// This returns a `Result` rather than a tuple, and every one of the three
    /// folds inside it is `checked_add`. Per-line carryability — the invariant
    /// T1 and T2 established — says NOTHING about a sum: two lines of `4e28` are
    /// each representable, each is stored by the real checked write, and
    /// `4e28 + 4e28 = 8e28` is above `Decimal::MAX`. rust_decimal's raw `+=`
    /// panics on that, this crate has no `catch_unwind`, and the panic happens on
    /// a READ — after the second line's INSERT has already committed, which
    /// leaves a document that no surface can open.
    ///
    /// A `Result` here, rather than a checked helper offered to the callers, is
    /// the same argument the tax contract itself makes: a comment saying "check
    /// this add" rots, and a helper still leaves every raw `+` at every call
    /// site in place. A caller cannot forget the guard here, because the
    /// signature will not compile until they handle it.
    ///
    /// # Why the census could not have caught it, in one sentence
    ///
    /// A wider write bound would not survive a direct SQL insert, a per-line
    /// census cannot see a property of a SET of rows, and the sum is performed
    /// here, so this is the only layer that can make it total. The refusal is
    /// therefore its own rule, [`PriceRefusal::DocumentTotalTooLarge`], and not
    /// either line rule: every line of the document is fine, so a line-amount or
    /// tax-arithmetic sentence would send the operator to fix a number that is
    /// already correct.
    fn tax_split(lines: &[SaleLine]) -> Result<(Decimal, Decimal, Decimal), PriceRefusal> {
        let mut net = Decimal::ZERO;
        let mut tax = Decimal::ZERO;
        let mut total = Decimal::ZERO;
        for l in lines {
            net = net
                .checked_add(l.subtotal())
                .ok_or(PriceRefusal::DocumentTotalTooLarge)?;
            tax = tax
                .checked_add(l.tax_total)
                .ok_or(PriceRefusal::DocumentTotalTooLarge)?;
            total = total
                .checked_add(tax_inclusive_total(l.subtotal(), l.tax_total))
                .ok_or(PriceRefusal::DocumentTotalTooLarge)?;
        }
        Ok((net, tax, total))
    }

    /// The applied amount and amount still owed come from the allocation-based
    /// residual. Both are already checked by the payment repository's shared
    /// residual fold; keep the document total in this signature so an unstated
    /// total still refuses rather than publishing partial money.
    fn paid_and_due(
        total: Decimal,
        residual: &DocumentResidualParts,
    ) -> Result<(Decimal, Decimal), PriceRefusal> {
        if residual.charge == Decimal::ZERO
            && residual.signed_returns == Decimal::ZERO
            && residual.allocated == Decimal::ZERO
        {
            return Ok((Decimal::ZERO, total));
        }
        Ok((residual.allocated, residual.residual))
    }

    /// The whole document-level money as ONE value, or the rule that refused it.
    ///
    /// This is the ONE place a sale's document money is derived: the detail, the
    /// record view and the payment ceiling all read it, so a figure can never be
    /// computed two ways. The refusal travels with the figures because they are
    /// one fact — a document whose lines cannot be added up has no net, no tax
    /// total, no total, no due and no payment status either, and returning them
    /// separately would let a caller publish a due balance derived from a total
    /// that does not exist.
    fn document_money(
        lines: &[SaleLine],
        residual: &DocumentResidualParts,
    ) -> Result<RecordMoney, PriceRefusal> {
        let (net_subtotal, tax_total, total) = Self::tax_split(lines)?;
        let (paid, due) = Self::paid_and_due(total, residual)?;
        Ok(RecordMoney {
            net_subtotal,
            tax_total,
            total,
            paid,
            due,
            payment_status: SaleDetail::payment_status_for(total, paid),
        })
    }

    /// One document as a LIST ROW: identity and non-money facts always, money when
    /// the arithmetic carried it, and the rule when it did not.
    ///
    /// This is what makes a list page total, and it is the tolerant twin of
    /// [`Self::assemble_detail`]: a page must be able to SHOW a document whose
    /// lines cannot be added up, with no figure, instead of answering an error and
    /// taking every other document on the page with it. Both call the same
    /// checked [`Self::document_money`], so they cannot disagree about which
    /// documents are refusable.
    fn row_for(
        sale: Sale,
        lines: &[SaleLine],
        residual: &DocumentResidualParts,
    ) -> SaleListRow {
        let (money, total_refusal) = match Self::document_money(lines, residual) {
            Ok(money) => (Some(money), None),
            Err(refusal) => (None, Some(refusal)),
        };
        SaleListRow {
            sale,
            money,
            total_refusal,
        }
    }

    /// A SET of documents' figures, as one [`SetMoney`].
    ///
    /// ONE refused member means the set has no figure at all: the figures that did
    /// add up are not published, because a sum missing one of its documents is
    /// indistinguishable from a real one. A sum that overflows on its own refuses
    /// the same way, which is the whole reason the accumulation is checked.
    fn set_sum(rows: &[SaleListRow], figure: impl Fn(&RecordMoney) -> Decimal) -> SetMoney {
        let mut sum = Decimal::ZERO;
        for row in rows {
            let Some(money) = row.money else {
                return SetMoney::refused(
                    row.total_refusal
                        .unwrap_or(PriceRefusal::DocumentTotalTooLarge),
                );
            };
            match checked_money_add(sum, figure(&money)) {
                Ok(next) => sum = next,
                Err(refusal) => return SetMoney::refused(refusal),
            }
        }
        SetMoney::amount(sum)
    }

    /// `total` is the tax-inclusive document total, so every payment ceiling,
    /// overpayment refusal, due balance and debt figure derived here is measured
    /// against the money the customer actually owes.
    fn totals(
        lines: &[SaleLine],
        residual: &DocumentResidualParts,
    ) -> Result<(Decimal, Decimal, Decimal), PriceRefusal> {
        let money = Self::document_money(lines, residual)?;
        Ok((money.total, money.paid, money.due))
    }

    /// Fold a sale and its children into the `SaleDetail` shape every derived
    /// read uses, so `total`/`paid`/`due` are computed in exactly one place.
    ///
    /// The refusal PROPAGATES: every consumer of a `SaleDetail` — the record
    /// page's totals, the list pages, the debt figure, the JSON API — is a
    /// consumer of this document's money, and a detail that carried a total
    /// nobody could compute would be a number the rest of the application would
    /// then do arithmetic with. The record page is the one surface that must not
    /// answer an error, and it reads through [`Self::record_from_parts`], which
    /// keeps the document and states the refusal instead of dropping either.
    fn assemble_detail(
        sale: Sale,
        lines: Vec<SaleLine>,
        payments: Vec<SalePayment>,
        residual: &DocumentResidualParts,
    ) -> AppResult<SaleDetail> {
        let money = Self::document_money(&lines, residual).map_err(AppError::PriceRefused)?;
        Ok(SaleDetail {
            sale,
            lines,
            payments,
            net_subtotal: money.net_subtotal,
            tax_total: money.tax_total,
            total: money.total,
            paid: money.paid,
            due: money.due,
            payment_status: money.payment_status,
        })
    }

    async fn residual_for_read(&self, sale: &Sale) -> AppResult<DocumentResidualParts> {
        match self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &[sale.id])
            .await
        {
            Ok(mut residuals) => residuals
                .remove(&sale.id)
                .ok_or_else(|| AppError::Internal("sale residual missing from batch".into())),
            // The record view must remain readable when its own document total
            // refuses; document_money independently states that same refusal.
            Err(AppError::PriceRefused(_)) => Ok(Self::empty_residual()),
            Err(error) => Err(error),
        }
    }

    async fn detail_for(&self, sale: Sale) -> AppResult<SaleDetail> {
        let lines = self.sales.list_lines(sale.id).await?;
        let payments = self.sales.list_payments(sale.id).await?;
        let residual = self.residual_for_read(&sale).await?;
        Self::assemble_detail(sale, lines, payments, &residual)
    }

    fn empty_residual() -> DocumentResidualParts {
        DocumentResidualParts {
            charge: Decimal::ZERO,
            signed_returns: Decimal::ZERO,
            allocated: Decimal::ZERO,
            residual: Decimal::ZERO,
        }
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

    /// Create a Draft sale. `actor` is the acting user's id the route resolves
    /// from its `Principal`; it becomes the row's `created_by` and nothing the
    /// request itself can supply names it.
    pub async fn create_draft(&self, actor: i64, input: NewSale) -> AppResult<Sale> {
        // Unknown customer => 404; the row is never created.
        let customer = self.customers.get_customer(input.customer_id).await?;
        let notes = Self::clean_notes(&input.notes)?;
        let receipt_no = Self::clean_receipt(&input.receipt_no)?;
        let due_date = Self::resolve_due_date(
            input.payment_type,
            input.sale_date,
            input.due_date,
            &customer,
        )?;
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
        self.sales.create_sale(actor, &clean, &customer.name).await
    }

    pub async fn update_draft(
        &self,
        id: i64,
        actor: i64,
        patch: UpdateSaleDraft,
    ) -> AppResult<Sale> {
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
        self.sales.update_draft(id, actor, &norm).await
    }

    pub async fn add_line(
        &self,
        actor: i64,
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
                    return Err(AppError::Validation("unit_price cannot be negative".into()));
                }
                p
            }
            None => product.sale_price,
        };
        let line = self
            .sales
            .create_line(sale_id, product_id, qty, price)
            .await?;
        // AFTER the line write, and only because it succeeded: a refused add
        // must leave the document claiming no editor rather than one. The
        // restriction to drafts is the `ensure_draft` above, not a predicate
        // here — see `SaleRepository::touch_draft` for why that placement is
        // deliberate.
        self.sales.touch_draft(sale_id, actor).await?;
        Ok(line)
    }

    pub async fn update_line(
        &self,
        actor: i64,
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
            return Err(AppError::Validation("unit_price cannot be negative".into()));
        }
        let updated = self.sales.update_line(line_id, qty, unit_price).await?;
        // Same rule and same order as the add: the line write is an edit of
        // THIS document, so the document names whoever requested it.
        self.sales.touch_draft(line.sale_id, actor).await?;
        Ok(updated)
    }

    pub async fn remove_line(&self, actor: i64, line_id: i64) -> AppResult<()> {
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
        // The repository refuses a line whose sale is not a Draft and reports a
        // missing one as `NotFound`, so this call carries the same answers the
        // guards above do — the `ensure_draft` check stays as the early,
        // cheap refusal and the statement is the backstop.
        self.sales.delete_line(line_id).await?;
        // A removal is an edit too: the audit line must not keep claiming the
        // document has no editor after the operator deleted a line from it.
        self.sales.touch_draft(line.sale_id, actor).await?;
        Ok(())
    }

    pub async fn get_detail(&self, sale_id: i64) -> AppResult<SaleDetail> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        if sale.status != crate::models::SaleStatus::Confirmed {
            let lines = self.sales.list_lines(sale.id).await?;
            let payments = self.sales.list_payments(sale.id).await?;
            return Self::assemble_detail(sale, lines, payments, &Self::empty_residual());
        }
        self.detail_for(sale).await
    }

    /// One sale payment by id — the documents drawer's per-payment read, so
    /// the route never touches a repository. An unknown id is the standard
    /// `NotFound`, naming the family.
    pub async fn find_payment(&self, id: i64) -> AppResult<SalePayment> {
        self.sales
            .find_payment(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale payment {id} not found")))
    }

    /// Record-page view for `/sales/{id}`: resolves product, account and method
    /// names through the existing inventory and finance read paths, so the
    /// route never runs SQL of its own and never prints an internal key.
    ///
    /// It reads through [`Self::record_from_parts`] and NOT through
    /// `get_detail`, on purpose. A `SaleDetail` propagates the document-total
    /// refusal, and this view must not lose the document because of it: a
    /// refused detail would make the record page an error page, and an operator
    /// who cannot open a document cannot reduce it.
    pub async fn get_record(&self, sale_id: i64) -> AppResult<SaleRecord> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        let lines = self.sales.list_lines(sale.id).await?;
        let payments = self.sales.list_payments(sale.id).await?;
        let residual = self.residual_for_read(&sale).await?;
        self.record_from_parts(sale, lines, payments, &residual).await
    }

    /// The one read that renders a document whose money cannot be computed.
    ///
    /// Every line is resolved and shown with its own net, tax and tax-inclusive
    /// total, because each of those IS representable — that is the per-line
    /// invariant this whole work unit leans on. What is missing is the document's
    /// own arithmetic, and it is missing as an absence: `money` is `None` and
    /// `total_refusal` names the rule, so the page states the refusal instead of
    /// publishing a number derived from a sum that could not be made, and the
    /// operator can see which lines the document carries and remove one.
    async fn record_from_parts(
        &self,
        sale: Sale,
        stored_lines: Vec<SaleLine>,
        stored_payments: Vec<SalePayment>,
        residual: &DocumentResidualParts,
    ) -> AppResult<SaleRecord> {
        // Resolved BEFORE the children are consumed below, and it is the only
        // thing that can fail: every name, tax snapshot and amount this view
        // shows is a fact about ONE line, and a line's own money is
        // representable.
        let (money, total_refusal) = match Self::document_money(&stored_lines, residual) {
            Ok(money) => (Some(money), None),
            Err(refusal) => (None, Some(refusal)),
        };

        let mut lines = Vec::with_capacity(stored_lines.len());
        for line in stored_lines {
            let product = self.inventory.get_product(line.product_id).await?;
            // The same predicate confirm and cancel use to decide whether a
            // line moves stock; resolved from the product this read already
            // fetched for the display names, so a preview built from the view
            // cannot drift from what those flows will do.
            let tracks_stock = product.kind == ProductKind::Product && product.track_stock;
            // The FROZEN breakdown, read from the snapshot table: a re-rated,
            // renamed or deactivated tax cannot change what this shows.
            let taxes = self
                .tax_snapshots
                .list_sale_line_taxes(line.id)
                .await?
                .iter()
                .map(LineTaxView::from)
                .collect();
            let subtotal = line.subtotal();
            lines.push(SaleLineView {
                id: line.id,
                product_name: product.name,
                product_sku: product.sku,
                product_id: line.product_id,
                qty: line.qty,
                unit_price: line.unit_price,
                tax_total: line.tax_total,
                total: tax_inclusive_total(subtotal, line.tax_total),
                subtotal,
                taxes,
                tracks_stock,
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
            .list_methods()
            .await?
            .into_iter()
            .map(|method| (method.id, method.name))
            .collect();

        let payments = stored_payments
            .into_iter()
            .map(|payment| SalePaymentView {
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

        Ok(SaleRecord {
            sale,
            lines,
            payments,
            money,
            total_refusal,
        })
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

    async fn rows_with_residuals(&self, sales: Vec<Sale>) -> AppResult<Vec<SaleListRow>> {
        let mut documents = Vec::with_capacity(sales.len());
        let mut ids = Vec::new();
        for sale in sales {
            let lines = self.sales.list_lines(sale.id).await?;
            let total_is_readable = Self::tax_split(&lines).is_ok();
            if sale.status == crate::models::SaleStatus::Confirmed && total_is_readable {
                ids.push(sale.id);
            }
            documents.push((sale, lines, total_is_readable));
        }
        let residuals = self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &ids)
            .await?;
        let mut out = Vec::with_capacity(documents.len());
        for (sale, lines, total_is_readable) in documents {
            if !total_is_readable {
                out.push(SaleListRow {
                    sale,
                    money: None,
                    total_refusal: Some(PriceRefusal::DocumentTotalTooLarge),
                });
                continue;
            }
            let empty = DocumentResidualParts {
                charge: Decimal::ZERO,
                signed_returns: Decimal::ZERO,
                allocated: Decimal::ZERO,
                residual: Decimal::ZERO,
            };
            let has_payment_residual = sale.status == crate::models::SaleStatus::Confirmed;
            let residual = if has_payment_residual {
                residuals.get(&sale.id).unwrap_or(&empty)
            } else {
                &empty
            };
            out.push(Self::row_for(sale, &lines, residual));
        }
        Ok(out)
    }

    /// The same documents as [`Self::list_details`], as LIST ROWS for the pages
    /// that must render a document whose total cannot be computed. A refused
    /// document keeps its place in the list; the read never fails because of one.
    pub async fn list_rows(&self) -> AppResult<Vec<SaleListRow>> {
        self.rows_with_residuals(self.sales.list_sales().await?).await
    }

    /// Typed list for decision callers that need every residual, including when
    /// one member's document total refuses. The error remains explicit rather
    /// than silently dropping a refused debt.
    pub async fn customer_ageing_rows(
        &self,
        customer_id: i64,
    ) -> AppResult<Vec<SaleListRow>> {
        self.customer_credit_rows(customer_id).await
    }

    /// The same derived list narrowed by the server-side list filter. The
    /// criteria run inside the repository query, so only matching documents have
    /// their lines and payments loaded; totals stay derived by `detail_for`,
    /// never by a second summation.
    ///
    /// The STRICT twin of [`Self::list_rows_filtered`]: the same documents as
    /// details, so a caller that makes a decision out of a document's money gets
    /// the refusal instead of a missing figure. Kept for the read-bound test that
    /// pins this read's query count, and for any future decision-shaped list;
    /// `allow(dead_code)` because this crate is a binary, where a `pub` method
    /// only the tests call still warns.
    #[allow(dead_code)]
    pub async fn list_details_filtered(
        &self,
        filter: &SaleListFilter,
    ) -> AppResult<Vec<SaleDetail>> {
        let sales = self.filtered_sales(filter).await?;
        let mut out = Vec::with_capacity(sales.len());
        for sale in sales {
            out.push(self.detail_for(sale).await?);
        }
        Ok(out)
    }

    /// The same documents as [`Self::list_details_filtered`], as LIST ROWS.
    pub async fn list_rows_filtered(&self, filter: &SaleListFilter) -> AppResult<Vec<SaleListRow>> {
        self.rows_with_residuals(self.filtered_sales(filter).await?).await
    }

    /// The list filter resolved to documents, so the strict and the tolerant read
    /// cannot drift on WHICH documents a page shows — only on what one document's
    /// money does.
    async fn filtered_sales(&self, filter: &SaleListFilter) -> AppResult<Vec<Sale>> {
        let mut repo_filter = filter.clone();
        if let Some(name) = &filter.customer {
            repo_filter.customer_ids = Some(self.matching_customer_ids(name).await?);
        }
        Ok(self.sales.list_sales_filtered(&repo_filter).await?)
    }

    /// Customer ids whose current name matches `needle` after normalization. The
    /// customers table is small by nature, so the match runs in Rust over the whole
    /// set and the document query stays bounded to the matching ids. If the party
    /// catalogue ever stops being small, this needs a normalized index instead.
    async fn matching_customer_ids(&self, needle: &str) -> AppResult<Vec<i64>> {
        let needle = crate::models::normalize_search(needle);
        Ok(self
            .customers
            .customers
            .list(false)
            .await?
            .into_iter()
            .filter(|customer| crate::models::normalize_search(&customer.name).contains(&needle))
            .map(|customer| customer.id)
            .collect())
    }

    // -- Derived customer receivable (Slice K3) -------------------------------

    /// Confirmed credit sales of one customer folded into the same `SaleDetail`
    /// shape `outstanding_debt` returns. Cancelled sales never appear, cash sales
    /// never appear, and every total comes from `assemble_detail`, so the money is
    /// summed in Rust over the TEXT columns and never with SQL `SUM`.
    ///
    /// A document whose lines cannot be added up refuses here, and the refusal
    /// PROPAGATES: this read exists for the callers that make a DECISION out of a
    /// document's money — the credit-limit projection and a collection's
    /// allocation — and a decision whose input cannot be stated has no answer to
    /// give. The DISPLAYING reads use [`Self::customer_credit_rows`], which keeps
    /// the document in the set and states the refusal in place of its figure.
    async fn customer_credit_details(&self, customer_id: i64) -> AppResult<Vec<SaleDetail>> {
        let sales = self.sales.list_confirmed_credit_sales(customer_id).await?;
        let mut lines_by_sale = Vec::with_capacity(sales.len());
        let mut ids = Vec::new();
        for sale in &sales {
            let lines = self.sales.list_lines(sale.id).await?;
            if Self::tax_split(&lines).is_ok() {
                ids.push(sale.id);
            }
            lines_by_sale.push((sale.id, lines));
        }
        let residuals = self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &ids)
            .await?;
        let mut details = Vec::with_capacity(sales.len());
        for sale in sales {
            let lines = lines_by_sale
                .iter()
                .find(|(id, _)| *id == sale.id)
                .map(|(_, lines)| lines.clone())
                .ok_or_else(|| AppError::Internal("sale lines missing from detail batch".into()))?;
            let legacy_payments = self.sales.list_payments(sale.id).await?;
            let residual = residuals.get(&sale.id).ok_or_else(|| {
                AppError::Internal(format!("sale {} residual missing from batch", sale.id))
            })?;
            details.push(Self::assemble_detail(sale, lines, legacy_payments, residual)?);
        }
        Ok(details)
    }

    /// Confirmed credit sales of one customer as the DISPLAYING reads need them:
    /// identity and non-money facts always, money when the arithmetic carried it.
    ///
    /// This is the tolerant twin of [`Self::customer_credit_details`], and the
    /// difference is the whole of the list decision: a document whose lines cannot
    /// be added up stays IN the set with its place preserved and no figure, so a
    /// statement renders every invoice instead of hiding the customer's whole
    /// ledger behind one of them. Both reads run the same checked derivation, so
    /// they cannot disagree about which documents are refusable.
    async fn customer_credit_rows(&self, customer_id: i64) -> AppResult<Vec<SaleListRow>> {
        self.rows_with_residuals(
            self.sales.list_confirmed_credit_sales(customer_id).await?,
        )
        .await
    }

    /// Money owed by one customer is the checked sum of document residuals minus
    /// unapplied `In` deliveries. Residuals are debt still sitting on documents;
    /// unapplied money has no document to sit on, so a customer credit subtracts
    /// from the balance. Cancelled sales and cash sales do not contribute. Drives
    /// the credit-limit check and the statement balance.
    ///
    /// Both the residual fold and the subtraction are checked: bounded documents
    /// do not imply a bounded customer total, and raw Decimal subtraction can
    /// overflow as well.
    pub async fn customer_balance(&self, customer_id: i64) -> AppResult<Decimal> {
        let sales = self
            .sales
            .list_confirmed_credit_sales(customer_id)
            .await?;
        let ids: Vec<i64> = sales.iter().map(|sale| sale.id).collect();
        let residuals = self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &ids)
            .await?;
        let document_amounts: Vec<Decimal> = sales
            .iter()
            .map(|sale| {
                residuals
                    .get(&sale.id)
                    .map(|parts| parts.residual)
                    .ok_or_else(|| {
                        AppError::Internal("sale residual missing from batch".into())
                    })
            })
            .collect::<AppResult<_>>()?;
        let documents = checked_money_sum(document_amounts.iter()).map_err(AppError::PriceRefused)?;
        let unapplied = self
            .payments
            .unapplied_for_party(crate::models::PartyType::Customer, customer_id)
            .await?;
        documents.checked_sub(unapplied).ok_or_else(|| {
            AppError::PriceRefused(PriceRefusal::DocumentTotalTooLarge)
        })
    }

    /// The customer's outstanding debt sales, oldest first: `due_date`, then
    /// `sale_date`, then id. This is the order `CustomerReceiptService` collects in,
    /// so the oldest invoice is paid before the largest. Only Confirmed credit sales
    /// with `due > 0` appear, which is why a walk-in sale can never be allocated to:
    /// a confirmed credit sale always carries a due date (K2) and never belongs to
    /// the walk-in.
    ///
    /// Rows, not details: a document whose total cannot be computed is still one
    /// of this customer's invoices, and a list that dropped it would understate
    /// the debt. It is filtered in only when its due is unknown-or-positive, which
    /// is every document that is not proven fully paid.
    pub async fn customer_debt_sales(&self, customer_id: i64) -> AppResult<Vec<SaleListRow>> {
        let mut rows: Vec<SaleListRow> = self
            .customer_credit_rows(customer_id)
            .await?
            .into_iter()
            .filter(|row| match row.money {
                Some(money) => money.due > Decimal::ZERO,
                None => true,
            })
            .collect();
        rows.sort_by(|a, b| {
            a.sale
                .due_date
                .cmp(&b.sale.due_date)
                .then_with(|| a.sale.sale_date.cmp(&b.sale.sale_date))
                .then_with(|| a.sale.id.cmp(&b.sale.id))
        });
        Ok(rows)
    }

    /// The same debts as [`Self::customer_debt_sales`], as DETAILS, for the one
    /// caller that makes a DECISION out of them: a collection allocates real money
    /// against each due, so a document whose due cannot be stated has nothing to
    /// allocate against and the read propagates the refusal.
    pub async fn customer_debt_details(&self, customer_id: i64) -> AppResult<Vec<SaleDetail>> {
        let mut details: Vec<SaleDetail> = self
            .customer_credit_details(customer_id)
            .await?
            .into_iter()
            .filter(|detail| detail.due > Decimal::ZERO)
            .collect();
        details.sort_by(|a, b| {
            a.sale
                .due_date
                .cmp(&b.sale.due_date)
                .then_with(|| a.sale.sale_date.cmp(&b.sale.sale_date))
                .then_with(|| a.sale.id.cmp(&b.sale.id))
        });
        Ok(details)
    }

    /// Add one sale's outstanding `due` to the bucket its `due_date` falls into
    /// against `as_of`: due today, not yet due and no due date are current;
    /// 1..=30, 31..=60 and >60 days late fill the other three.
    ///
    /// A document whose total cannot be computed is not bucketed, and it refuses
    /// the WHOLE grid: its due is unknown, so any bucket could be the one that
    /// lost it, and a grid that kept the other three would be publishing sums the
    /// operator would add up to a total nobody can stand behind.
    ///
    /// A bucket that overflows refuses only ITSELF. That is the honest difference
    /// between the two cases: an unknown `due` could have belonged anywhere, while
    /// an overflow is a fact about the documents already in that bucket, and every
    /// other bucket is still the complete sum of its own.
    fn add_to_ageing(ageing: &mut Ageing, row: &SaleListRow, as_of: NaiveDate) {
        let Some(money) = row.money else {
            if let Some(refusal) = row.total_refusal {
                *ageing = Ageing {
                    current: SetMoney::refused(refusal),
                    overdue_1_30: SetMoney::refused(refusal),
                    overdue_31_60: SetMoney::refused(refusal),
                    overdue_61_plus: SetMoney::refused(refusal),
                };
            }
            return;
        };
        if money.due <= Decimal::ZERO {
            return;
        }
        let due_date = row.sale.due_date;
        let days = due_date.map(|due_date| (as_of - due_date).num_days());
        let bucket = match days {
            None => &mut ageing.current,
            Some(days) if days <= 0 => &mut ageing.current,
            Some(days) if days <= 30 => &mut ageing.overdue_1_30,
            Some(days) if days <= 60 => &mut ageing.overdue_31_60,
            Some(_) => &mut ageing.overdue_61_plus,
        };
        match bucket.amount {
            // Already refused: a later document cannot make the sum carryable, and
            // the bucket stays a refusal so nothing can be read out of it.
            None => {}
            Some(current) => {
                *bucket = match checked_money_add(current, money.due) {
                    Ok(sum) => SetMoney::amount(sum),
                    Err(refusal) => SetMoney::refused(refusal),
                }
            }
        }
    }

    fn ageing_of(rows: &[SaleListRow], as_of: NaiveDate) -> Ageing {
        let mut ageing = Ageing::default();
        for row in rows {
            Self::add_to_ageing(&mut ageing, row, as_of);
        }
        ageing
    }

    /// Ageing of document residuals against an explicit `as_of`. Only positive
    /// residuals are bucketed; unapplied credit remains outside document ageing.
    ///
    /// A document that cannot be totaled makes the ageing answer with the rule
    /// rather than with buckets: this is a DISPLAY, and a report that dropped the
    /// document would misstate the receivable.
    ///
    /// The single-customer form of the same read, for a caller that wants one
    /// customer's ageing without their ledger. `allow(dead_code)` because this
    /// crate is a binary and the pages reach the ageing through the statement.
    #[allow(dead_code)]
    pub async fn customer_ageing(&self, customer_id: i64, as_of: NaiveDate) -> AppResult<Ageing> {
        let rows = self.customer_ageing_rows(customer_id).await?;
        Ok(Self::ageing_of(&rows, as_of))
    }

    /// Chronological journal report: confirmed credit sales as debits and each
    /// customer payment delivery as one credit or refund debit, with the running
    /// balance after every entry. Its final amount equals `customer_balance` when
    /// all figures are representable; `as_of` labels the statement and drives ageing.
    pub async fn customer_statement(
        &self,
        customer_id: i64,
        as_of: NaiveDate,
    ) -> AppResult<CustomerStatement> {
        // One batch residual read supplies per-document figures. Delivery credits
        // come from the party's payment documents, at delivery granularity (not a
        // fabricated row per allocation).
        let sales = self.sales.list_confirmed_credit_sales(customer_id).await?;
        let mut document_lines = Vec::with_capacity(sales.len());
        let mut ids = Vec::new();
        for sale in &sales {
            let lines = self.sales.list_lines(sale.id).await?;
            if Self::tax_split(&lines).is_ok() {
                ids.push(sale.id);
            }
            document_lines.push((sale.id, lines));
        }
        let residuals_result = match self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &ids)
            .await
        {
            Ok(parts) => Ok(parts),
            Err(AppError::PriceRefused(refusal)) => Err(refusal),
            Err(error) => return Err(error),
        };
        let deliveries = self
            .payments
            .list_for_party(crate::models::PartyType::Customer, customer_id)
            .await?;
        let mut document_rows: Vec<SaleListRow> = Vec::with_capacity(sales.len());

        // Intermediate rows kept only long enough to order the ledger before the
        // running balance is applied. Ties on the same date stay deterministic:
        // document number, then debits before credits, then source row id.
        //
        // The rows are the READ documents, so a document whose total cannot be
        // computed still appears: its debit is a refusal, and the running balance
        // from that entry on is one too, because a balance cannot be stated past a
        // figure that does not exist.
        struct LedgerRow {
            date: NaiveDate,
            document: Option<String>,
            kind: StatementEntryKind,
            source_id: i64,
            description: &'static str,
            debit: SetMoney,
            credit: Decimal,
        }

        let mut rows: Vec<LedgerRow> = Vec::new();
        for sale in &sales {
            let lines = document_lines
                .iter()
                .find(|(id, _)| *id == sale.id)
                .map(|(_, lines)| lines)
                .ok_or_else(|| {
                    AppError::Internal("sale lines missing from statement batch".into())
                })?;
            let residual = residuals_result
                .as_ref()
                .ok()
                .and_then(|parts| parts.get(&sale.id));
            let row = match residual {
                Some(parts) => Self::row_for(sale.clone(), lines, parts),
                None => SaleListRow {
                    sale: sale.clone(),
                    money: None,
                    total_refusal: Some(
                        residuals_result
                            .as_ref()
                            .err()
                            .copied()
                            .unwrap_or(PriceRefusal::DocumentTotalTooLarge),
                    ),
                },
            };
            let document = row.sale.sale_number.clone();
            let debit = match (residual, row.total_refusal) {
                (Some(parts), None) => checked_money_add(parts.charge, parts.signed_returns)
                    .map(SetMoney::amount)
                    .unwrap_or_else(SetMoney::refused),
                (_, Some(refusal)) => SetMoney::refused(refusal),
                _ => SetMoney::amount(Decimal::ZERO),
            };
            rows.push(LedgerRow {
                date: row.sale.sale_date,
                document,
                kind: StatementEntryKind::Sale,
                source_id: row.sale.id,
                description: "Credit sale",
                debit,
                credit: Decimal::ZERO,
            });
            document_rows.push(row);
        }
        for payment in deliveries {
            let (debit, credit) = match payment.direction {
                crate::models::PaymentDirection::In => {
                    (SetMoney::amount(Decimal::ZERO), payment.amount)
                }
                crate::models::PaymentDirection::Out => {
                    (SetMoney::amount(payment.amount), Decimal::ZERO)
                }
            };
            rows.push(LedgerRow {
                date: payment.date,
                document: Some(payment.number),
                kind: StatementEntryKind::Payment,
                source_id: payment.id,
                description: "Payment",
                debit,
                credit,
            });
        }
        let document_balance = Self::set_sum(&document_rows, |money| money.due);
        let unapplied = self
            .payments
            .unapplied_for_party(crate::models::PartyType::Customer, customer_id)
            .await?;
        let balance = match document_balance.amount {
            Some(amount) => amount
                .checked_sub(unapplied)
                .map(SetMoney::amount)
                .unwrap_or_else(|| SetMoney::refused(PriceRefusal::DocumentTotalTooLarge)),
            None => document_balance,
        };
        let ageing = Self::ageing_of(&document_rows, as_of);

        rows.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then_with(|| a.document.cmp(&b.document))
                .then_with(|| {
                    Self::statement_kind_rank(a.kind).cmp(&Self::statement_kind_rank(b.kind))
                })
                .then_with(|| a.source_id.cmp(&b.source_id))
        });

        // The running balance is checked for the same reason `balance` above is:
        // it walks the same set of documents' figures, and a statement whose
        // running balance stops being representable has no honest last row.
        let mut running = SetMoney::amount(Decimal::ZERO);
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            running = Self::running_balance(running, &row.debit, row.credit);
            entries.push(StatementEntry {
                date: row.date,
                kind: row.kind,
                document_number: row.document,
                description: row.description.to_string(),
                debit: row.debit,
                credit: row.credit,
                balance: running,
            });
        }

        Ok(CustomerStatement {
            customer_id,
            balance,
            as_of,
            ageing,
            entries,
        })
    }

    /// One step of a statement's running balance: the figure it already holds,
    /// this entry's debit — a refusal when the document cannot be totaled — and
    /// the credit being applied.
    ///
    /// A refusal is STICKY: once the balance cannot be stated, no later entry can
    /// restore it, because the figure that broke it is still missing from the sum.
    /// A figure that resumed afterwards would be one this application cannot
    /// stand behind.
    fn running_balance(current: SetMoney, debit: &SetMoney, credit: Decimal) -> SetMoney {
        let (Some(balance), Some(debit)) = (current.amount, debit.amount) else {
            return SetMoney::refused(
                debit
                    .refusal
                    .or(current.refusal)
                    .unwrap_or(PriceRefusal::DocumentTotalTooLarge),
            );
        };
        let Some(movement) = debit.checked_sub(credit) else {
            return SetMoney::refused(PriceRefusal::DocumentTotalTooLarge);
        };
        match checked_money_add(balance, movement) {
            Ok(running) => SetMoney::amount(running),
            Err(refusal) => SetMoney::refused(refusal),
        }
    }

    fn statement_kind_rank(kind: StatementEntryKind) -> u8 {
        match kind {
            StatementEntryKind::Sale => 0,
            StatementEntryKind::Payment => 1,
        }
    }

    /// Receivables view: every customer with non-zero document residuals and the
    /// ageing of those residuals as of `as_of`, ordered by customer id.
    ///
    /// Both figures are sums over a SET of documents, so both are [`SetMoney`]s:
    /// a receivables report that dropped the document it could not total would
    /// understate what the shop is owed, and one that refused to render would
    /// take every other customer's row with it.
    pub async fn ageing_all(&self, as_of: NaiveDate) -> AppResult<Vec<CustomerAgeing>> {
        let all_sales = self.sales.list_sales().await?;
        let receivables: Vec<Sale> = all_sales
            .into_iter()
            .filter(|sale| {
                sale.status == crate::models::SaleStatus::Confirmed
                    && sale.payment_type == PaymentType::Credit
            })
            .collect();
        let rows = self.rows_with_residuals(receivables).await?;
        let customers_with_rows: std::collections::BTreeSet<i64> =
            rows.iter().map(|row| row.sale.customer_id).collect();
        let mut unapplied_by_customer = BTreeMap::new();
        for customer_id in customers_with_rows {
            let amount = self
                .payments
                .unapplied_for_party(crate::models::PartyType::Customer, customer_id)
                .await?;
            unapplied_by_customer.insert(customer_id, amount);
        }
        let mut by_customer: BTreeMap<i64, Vec<SaleListRow>> = BTreeMap::new();
        for row in rows {
            by_customer
                .entry(row.sale.customer_id)
                .or_default()
                .push(row);
        }
        Ok(by_customer
            .into_iter()
            .filter_map(|(customer_id, rows)| {
                let document_balance = Self::set_sum(&rows, |money| money.due);
                let unapplied = unapplied_by_customer
                    .get(&customer_id)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                let balance = match document_balance.amount {
                    Some(amount) => amount
                        .checked_sub(unapplied)
                        .map(SetMoney::amount)
                        .unwrap_or_else(|| SetMoney::refused(PriceRefusal::DocumentTotalTooLarge)),
                    None => document_balance,
                };
                let ageing = Self::ageing_of(&rows, as_of);
                let settled = balance
                    .amount
                    .map(|amount| amount != Decimal::ZERO)
                    .unwrap_or(true);
                settled.then_some(CustomerAgeing {
                    customer_id,
                    balance,
                    ageing,
                })
            })
            .collect())
    }

    /// Outstanding receivables: Confirmed sales with due > 0.
    pub async fn outstanding_debt(&self) -> AppResult<Vec<SaleDetail>> {
        let all = self.list_details().await?;
        Ok(all
            .into_iter()
            .filter(|d| {
                d.sale.status == crate::models::SaleStatus::Confirmed && d.due > Decimal::ZERO
            })
            .collect())
    }

    /// The debt banner's read: the exact total owed and the number of unpaid
    /// documents, plus the oldest few. Residuals are read in one batch; totals are
    /// checked decimal sums in Rust, never SQL `SUM` over TEXT money.
    ///
    /// A document that cannot be totaled keeps its place in `oldest` and makes
    /// `total` a refusal: a banner that quietly omitted it would misstate the
    /// receivable, and a banner that refused to render would empty the panel for
    /// the whole shop.
    pub async fn debt_summary(&self, limit: usize) -> AppResult<DebtSummary> {
        let mut unpaid = self.list_rows().await?;
        unpaid.retain(|row| {
            row.sale.status == crate::models::SaleStatus::Confirmed
                && row.sale.payment_type == PaymentType::Credit
        });
        // A document whose due is unknown is not proven paid, so it stays in
        // the panel: it is one of the shop's unpaid documents as far as anyone
        // can show.
        unpaid.retain(|row| match row.money {
            Some(money) => money.due > Decimal::ZERO,
            None => true,
        });
        let total = Self::set_sum(&unpaid, |money| money.due);
        let count = unpaid.len();
        unpaid.truncate(limit);
        Ok(DebtSummary {
            total,
            count,
            oldest: unpaid,
        })
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(
        &self,
        actor: i64,
        sale_id: i64,
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
                return Err(AppError::Validation("unit_price cannot be negative".into()));
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

        // The document's money is resolved BEFORE any write below — the stock
        // movements, the finance rows, the sequence number. A confirmation that
        // cannot state what the document costs must refuse with nothing written,
        // exactly as it refuses an inactive product here.
        let empty_residual = DocumentResidualParts {
            charge: Decimal::ZERO,
            signed_returns: Decimal::ZERO,
            allocated: Decimal::ZERO,
            residual: Decimal::ZERO,
        };
        let (total, _, _) = Self::totals(&lines, &empty_residual)
            .map_err(AppError::PriceRefused)?;

        // The cash account is derived from the method, which belongs to exactly
        // one account: an invalid combination is impossible by construction.
        let cash_account_id: Option<i64> = match sale.payment_type {
            PaymentType::Cash => {
                let method_id = cash_method_id.ok_or_else(|| {
                    AppError::Validation("cash sale requires a payment method".into())
                })?;
                if sale.due_date.is_some() {
                    return Err(AppError::Validation(
                        "due_date must be NULL for Cash".into(),
                    ));
                }
                // Ownership resolved before any stock/sequence/finance touch.
                // `confirm` is handed a METHOD and nothing else — the pair on the
                // payment row is derived from it — so there is no stated account
                // to disagree with here. The door that DOES take a stated account
                // is `record_payment`, and it checks the pair before writing.
                Some(self.resolve_method_account(method_id, None).await?)
            }
            PaymentType::Credit => {
                if cash_method_id.is_some() {
                    return Err(AppError::Validation(
                        "credit sale must not include a payment method".into(),
                    ));
                }
                None
            }
        };

        match sale.payment_type {
            PaymentType::Cash => {}
            PaymentType::Credit => {
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
                        let debt = self.customer_balance(customer.id).await?;
                        // Checked: the projected debt is the customer's existing
                        // balance plus THIS document, two sums of documents'
                        // figures, and a credit-limit check that cannot state the
                        // projection has no answer to give.
                        let projected =
                            checked_money_add(debt, total).map_err(AppError::PriceRefused)?;
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
            // AGGREGATED PER PRODUCT, not per line. A document may legitimately
            // carry the same product twice (2 units at one price and 1 at
            // another on the same ticket) — unlike a purchase, where
            // `product_supplier_costs` is UNIQUE (product_id, supplier_id) and
            // duplicates are undefined rather than unusual. Nothing is written
            // while this loop runs, so a per-LINE read gives every line of a
            // product the SAME unmutated level: 6 + 6 of a level of 10 passes
            // twice (10 - 6 = 4), the number is then burned, the first movement
            // is committed, and the second is refused downstream — leaving a
            // movement row whose reference names no sale, a burned number, and
            // a Draft that `cancel` will not release. What the document demands
            // is the SUM, so the sum is what the level is compared against, and
            // the level is read once per product rather than once per line.
            //
            // The sum is CHECKED, not `Iterator::sum`: each line qty is a single
            // bounded write, and the sum of a set of them is not bounded — two
            // free lines (unit_price 0, so the document total above stays at 0
            // and never trips) of 5e28 each are 1e29 together, which rust_decimal's
            // `+` panics on. `checked_money_add` is the same refusal the total
            // fold above already uses, and a line qty is bounded by nothing else.
            let mut demanded: BTreeMap<i64, Decimal> = BTreeMap::new();
            for (line, _) in &tracked {
                let sum = demanded
                    .get(&line.product_id)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                *demanded.entry(line.product_id).or_insert(Decimal::ZERO) =
                    checked_money_add(sum, line.qty).map_err(AppError::PriceRefused)?;
            }
            for (product_id, qty) in &demanded {
                // The STRICT level: this is a decision, so it may not proceed on
                // a figure nobody can state.
                let current = self.inventory.stock_for_decision(*product_id).await?;
                if current - *qty < Decimal::ZERO {
                    return Err(AppError::Validation(format!(
                        "insufficient stock: {current} would become {}",
                        current - *qty
                    )));
                }
            }
        }

        // ---- THE WRITE UNIT -----------------------------------------------
        //
        // Everything from here to the COMMIT is ONE transaction: the sequence
        // number, one stock movement per tracked line, the `Income`, the
        // `sale_payments` row, and `set_confirmed`. Before this line each of
        // them was its own autocommit unit, so a failure between any two left
        // the earlier ones committed and the document a Draft — a burned number,
        // stock deducted from a document nobody could see, an orphan `Income`,
        // or a Draft that reported itself Paid with no way to reconcile it to a
        // number. That residue was MEASURED, not assumed
        // (`confirm_failure_*` in this file), and every one of those tests now
        // asserts its absence.
        //
        // The BEGIN goes HERE and not one line earlier, on purpose. Every read
        // above it — the document, its lines, the per-line product lookup, the
        // totals, the payment-method ownership, the credit ledger, the strict
        // stock pre-check — is a pre-check, and a pre-check buys EARLY refusal
        // with a useful message rather than reachability: the repository's fold
        // is the guarantee, and the fold inside the unit below now sees the
        // unit's own writes. Holding a transaction open across them would also
        // pin a connection for the whole pre-check and buy nothing.
        //
        // ROLLBACK IS THE `?`. There is deliberately no explicit rollback arm
        // and no `unwrap_or` on the way out: every `?` here drops the
        // `Transaction`, sqlx rolls it back, and the `AppError` that caused it
        // propagates UNCHANGED. An explicit arm would be a place to swallow a
        // refusal, and the refusal IS the answer. Do not add one.
        let mut tx = self.sales.pool().begin().await?;

        // Assign number atomically via doc_sequences row UPDATE.
        let year = sale.sale_date.year();
        let seq = self.sequences.next_number_in(&mut tx, "SALE", year).await?;
        let sale_number = format_sale_number(year, seq);

        // Stock Out (reason Sale) for tracked Product lines only.
        // Service / untracked lines are sellable without stock moves (AC9).
        // The movement carries the CONFIRMING request's actor — the same
        // argument that stamps the finance rows — never a fresh one (AC18).
        for (line, _) in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: line.product_id,
                        qty: line.qty,
                        movement_type: MovementType::Out,
                        reason: MovementReason::Sale,
                        reference: sale_number.clone(),
                        date: sale.sale_date,
                    },
                )
                .await?;
        }

        // Finance: Cash => 1 payment + Income now; Credit => receivable, no Income.
        // The Income is stamped with reference = sale_number and linked back from
        // the payment row it produced.
        // `cash_account_id`/`cash_method_id` are `Some` exactly when this is a Cash
        // sale with money to move; the pair is unwrapped here and used far below,
        // AFTER the charge, because the journal must read: the shop sold, and then
        // the customer paid.
        let cash_leg = if sale.payment_type == PaymentType::Cash && total > Decimal::ZERO {
            Some((cash_account_id.unwrap(), cash_method_id.unwrap()))
        } else {
            None
        };

        // The customer's journal, in the SAME unit as the document (T2 of
        // odd/tasks/party-ledger.md). A sale is a CHARGE whichever way it is
        // paid: the shop delivered goods worth `total`, so the customer owes
        // `total` from this moment. A cash sale appends the Payment right after
        // (below), and the two rows fold to zero — which is why the cash case
        // needs no branch here and the balance stays the single fold of the
        // journal rather than a special case per payment type.
        //
        // `Charge` and not a signed `±total` computed here: the sign rule lives
        // in `PartyEntryKind::signed_amount`, one place, so no write path holds
        // a second opinion about it.
        if total > Decimal::ZERO {
            self.party_ledger
                .insert_in(
                    &mut tx,
                    &crate::models::NewPartyLedgerEntry {
                        party_type: crate::models::PartyType::Customer,
                        party_id: sale.customer_id,
                        kind: crate::models::PartyEntryKind::Charge,
                        amount: crate::models::PartyEntryKind::Charge.signed_amount(total),
                        document_kind: crate::models::PartyDocumentKind::Sale,
                        document_id: sale_id,
                        entry_date: sale.sale_date,
                        reference: Some(sale_number.clone()),
                        created_by: actor,
                    },
                )
                .await?;
        }

        // The cash tender's own `Payment` entry is NOT written here: it belongs to
        // the DELIVERY, and `record_delivery_in` (called just below, after this
        // charge) is the one writer that produces it. Writing it here as well put two
        // `Payment` rows in the journal for one tender — caught by
        // `a_confirmed_cash_sale_appends_a_charge_and_its_settlement`.
        // The cash tender, and it comes AFTER the charge so an intermediate reader
        // sees a debt and then its settlement, never the reverse. It is a DELIVERY
        // like any other (decision 5) and goes through the ONE writer, which is what
        // keeps `confirm`, `record_payment` and a multi-invoice collection from
        // drifting into three opinions about what a delivery looks like.
        if let Some((account_id, method_id)) = cash_leg {
            let cash_delivery = crate::services::payment_writer::record_delivery_in(
                &self.sequences,
                &self.transactions,
                &self.party_ledger,
                &self.payments,
                &mut tx,
                actor,
                crate::models::PaymentDirection::In,
                crate::models::PartyType::Customer,
                sale.customer_id,
                method_id,
                account_id,
                total,
                sale.sale_date,
                None,
                Some(sale_number.clone()),
                None,
                &[(crate::models::PartyDocumentKind::Sale, sale_id, total)],
            )
            .await?;

            // The legacy row, until P5 moves the reads, in the same unit.
            self.sales
                .create_payment_in(
                    &mut tx,
                    actor,
                    sale_id,
                    account_id,
                    method_id,
                    total,
                    sale.sale_date,
                    cash_delivery.transaction_id,
                    None,
                )
                .await?;
        }

        let confirmed = self
            .sales
            .set_confirmed_in(&mut tx, sale_id, actor, &sale_number)
            .await?;

        tx.commit().await?;

        // ---- AFTER THE COMMIT, DELIBERATELY -------------------------------
        //
        // `detail_for` reads the document's lines and payments, and it stays on
        // the pool on purpose: a pool read beneath an open unit cannot answer
        // on a one-connection pool (30s, then `PoolTimedOut`), and it has no
        // reason to be inside the unit anyway. `confirmed` is a row this
        // transaction wrote, so the value is already durable by the time the
        // reads run.
        self.detail_for(confirmed).await
    }

    /// Link one covered sale to the delivery's SINGLE movement, in the caller's unit.
    ///
    /// This is the legacy `sale_payments` row, kept until P5 moves the reads: the
    /// receipt's own read asks for it, and with decision 8 it stops being the HOME of
    /// the attribution (that is `payment_allocations`) and becomes a pointer that
    /// lets the old read keep answering.
    ///
    /// Every row written this way names the SAME `transaction_id`, which is why the
    /// traceability invariant had to be re-based: a transaction belongs to one
    /// DELIVERY, and N documents may share it inside that delivery.
    pub async fn link_delivery_payment_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        receipt_id: i64,
        sale_id: i64,
        account_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        transaction_id: Option<i64>,
    ) -> AppResult<SalePayment> {
        self.sales
            .create_payment_in(
                tx,
                actor,
                sale_id,
                account_id,
                method_id,
                amount,
                date,
                transaction_id,
                Some(receipt_id),
            )
            .await
    }

    // -- Pay (Credit) ------------------------------------------------------------

    /// Record a payment on one sale, without a receipt: this is a direct payment
    /// on a single sale and keeps working exactly as before.
    pub async fn record_payment(
        &self,
        actor: i64,
        sale_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
    ) -> AppResult<SalePayment> {
        self.record_payment_with_receipt(actor, sale_id, method_id, amount, date, None)
            .await
    }

    /// Record a payment on one sale, ALL of it inside one unit (P3 of
    /// `odd/tasks/payment-allocation.md`).
    ///
    /// **What this fixes.** Before P3 this method posted the `Income` in one unit
    /// and the payment row in a second (the defect the payment-allocation plan
    /// records as flow 5: "a failure between the two leaves cash in the box with no
    /// document behind it"). A live defect, not a hypothetical one. Every write is
    /// an `_in` form now and the unit opens immediately after the pre-checks, so a
    /// failure anywhere leaves neither the money nor the row.
    ///
    /// **What it writes.** The delivery of money is a `payments` document with its
    /// own number (decision 1), the ONE cash movement it produced (decision 5), one
    /// allocation naming the sale it covers, and one `Payment` ledger entry
    /// (decision 6: one entry per payment DOCUMENT, not per invoice). The legacy
    /// `sale_payments` row is still written, because the reads have not moved yet
    /// (that is P5) and dropping it now would break every list that shows a sale's
    /// payments. Two homes for one fact is a P3 cost, named here so P5/P8 settle it.
    ///
    /// The account is DERIVED from the method, never stated, so the (account,
    /// method) pair cannot disagree with itself — migration 44's guard, and from
    /// migration 46 the same guard on `payments`.
    pub async fn record_payment_with_receipt(
        &self,
        actor: i64,
        sale_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        receipt_id: Option<i64>,
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
        // The account is derived from the method's owner (no finance touch yet).
        let account_id = self.resolve_method_account(method_id, None).await?;
        let lines = self.sales.list_lines(sale_id).await?;
        let residual = self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &[sale_id])
            .await?
            .remove(&sale_id)
            .ok_or_else(|| AppError::Internal("sale residual missing from batch".into()))?;
        // The ceiling is measured against the DUE BALANCE, not against
        // `paid + amount`. Both say the same thing while `paid <= total`, and
        // only one of them can be computed at all: `amount` is an operator's
        // unbounded input and `paid + amount` is a raw add, so an operator
        // typing a payment of `4e28` against an ordinary sale would overflow
        // here rather than be refused the overpayment they just typed. The
        // message is unchanged, and the figures in it are stated, never summed.
        let (total, paid, due) = Self::totals(&lines, &residual).map_err(AppError::PriceRefused)?;
        let _ = paid;
        if amount > due {
            return Err(AppError::Validation(format!(
                "overpay rejected: paid {paid} + {amount} exceeds total {total}"
            )));
        }
        let sale_number = sale
            .sale_number
            .clone()
            .ok_or_else(|| AppError::Internal("confirmed sale missing sale_number".into()))?;
        let _ = sale_number;

        // ---- THE WRITE UNIT -------------------------------------------------
        //
        // Every read above is a pre-check: it buys an EARLY refusal with a useful
        // message rather than reachability. The unit opens here, after all of them,
        // and everything from the number to the legacy row commits or rolls back
        // together.
        let mut tx = self.sales.pool().begin().await?;

        // 1-5. The delivery itself: its number, its ONE movement, the share naming
        // this sale, and the ledger entry — through the ONE writer, so this path and
        // a multi-sale collection and a cash confirm cannot drift apart.
        let delivery = crate::services::payment_writer::record_delivery_in(
            &self.sequences,
            &self.transactions,
            &self.party_ledger,
            &self.payments,
            &mut tx,
            actor,
            crate::models::PaymentDirection::In,
            crate::models::PartyType::Customer,
            sale.customer_id,
            method_id,
            account_id,
            amount,
            date,
            None,
            Some(sale_number.clone()),
            // A direct payment on one sale is not grouped under a receipt.
            None,
            // The SHARE is what the sale still owes — never the amount typed. The
            // difference between the two is the customer's credit, and it stays on
            // the payment rather than inside the document.
            &[(
                crate::models::PartyDocumentKind::Sale,
                sale_id,
                amount.min(due),
            )],
        )
        .await?;

        // 6. The legacy row, until P5 moves the reads. Still `_in`, still the same
        // unit, so the two homes of the same fact cannot disagree.
        let legacy = self
            .sales
            .create_payment_in(
                &mut tx,
                actor,
                sale_id,
                account_id,
                method_id,
                amount,
                date,
                delivery.transaction_id,
                receipt_id,
            )
            .await
            .map_err(|e| match e {
                // The database trigger refuses a payment grouped under another
                // customer's receipt. Surfacing it as a Validation keeps the
                // interface's contract a clean 400 even if a future caller passes
                // a receipt id directly; no route offers that path.
                AppError::Database(ref db)
                    if db.to_string().contains("another customer's receipt") =>
                {
                    AppError::Validation(
                        "the receipt belongs to another customer; a payment can only be grouped under a receipt of its own customer"
                            .into(),
                    )
                }
                other => other,
            })?;

        tx.commit().await?;
        Ok(legacy)
    }

    // -- Cancel / Return -----------------------------------------------------------

    pub async fn cancel(
        &self,
        actor: i64,
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

        // The document's money is resolved BEFORE any write on this path, like
        // every other expected rejection here. Without it, a discard would flip
        // the status and only the read at the end would refuse, and an annulment
        // would return the stock and post the refunds before refusing — a half
        // applied reversal reported to the operator as a refusal. The refusal is
        // recoverable instead: the record page is READABLE, so a line can be
        // removed from it, and `delete_draft` removes a never-confirmed document
        // outright without ever reading its money.
        let lines = self.sales.list_lines(sale_id).await?;
        let residual = self
            .payments
            .residuals_for_documents(crate::models::PartyDocumentKind::Sale, &[sale_id])
            .await?
            .remove(&sale_id)
            .ok_or_else(|| AppError::Internal("sale residual missing from batch".into()))?;
        let payments = self.sales.list_payments(sale_id).await?;
        Self::totals(&lines, &residual).map_err(AppError::PriceRefused)?;

        if sale.status == crate::models::SaleStatus::Draft {
            // Draft -> Cancelled: no-op, no stock/finance.
            let cancelled = self
                .sales
                .set_cancelled(sale_id, actor, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        // Confirmed -> Cancelled: re-enter stock + refunds.
        let sale_number = sale
            .sale_number
            .clone()
            .ok_or_else(|| AppError::Internal("confirmed sale missing sale_number".into()))?;

        // Invariant 10: every expected rejection is validated before any write.
        // A partially-applied annulment (an earlier attempt failed after posting
        // some refunds) must be REFUSED, not doubled: running the pass again
        // would write the Sale-return movements a second time and refund every
        // payment again. The residual says the document is in an inconsistent
        // state; naming it beats hiding it.
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
            // The refunds of one annulment hit the same accounts together, so the
            // guard must be evaluated against the AGGREGATE of refunds per
            // account, not per payment against the pre-refund balance. The old
            // per-payment check passed two payments of 60 against a balance of
            // 100 (both saw 100, nothing was written yet) and only the live
            // re-check inside `create_with_reference` caught the second refund —
            // after the stock had already been returned. Here nothing is written
            // until every account's `balance - Σ refunds >= 0` holds.
            let mut refunds_by_account: std::collections::BTreeMap<i64, Decimal> =
                std::collections::BTreeMap::new();
            for pay in &payments {
                if !self.transactions.accounts.exists(pay.account_id).await? {
                    return Err(AppError::NotFound(format!(
                        "account {} not found",
                        pay.account_id
                    )));
                }
                *refunds_by_account
                    .entry(pay.account_id)
                    .or_insert(Decimal::ZERO) += pay.amount;
            }
            for (account_id, refunds) in &refunds_by_account {
                let balance = self
                    .transactions
                    .transactions
                    .balance_for_account(*account_id)
                    .await?;
                if balance - refunds < Decimal::ZERO {
                    return Err(AppError::Validation(format!(
                        "refund would cause negative balance on account {account_id}: balance {balance} would become {} with {refunds} in refunds",
                        balance - refunds
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

        // ---- THE WRITE UNIT -------------------------------------------------
        //
        // Everything from here to the COMMIT is ONE transaction: the stock coming
        // back, one refund DELIVERY per paid amount, the legacy links and the
        // cancellation itself.
        //
        // **This is what T3d fixed, and the direction of the leak is the reason it
        // matters more than it looks.** Before, the stock came back and each refund
        // was posted by `create_with_reference` — a unit of ITS OWN — and only then was
        // the sale flipped to Cancelled. A failure in between left the money actually
        // refunded and the document still showing Confirmed: the operator sees a live
        // debt that has already been paid out. The pre-checks above (the aggregate
        // refund guard, the product checks) are still where they were, on purpose:
        // they buy an early refusal with a useful message rather than reachability.
        let mut tx = self.sales.pool().begin().await?;

        // 1. Stock In (reason Sale-return) for tracked lines. The movement carries the
        // cancelling request's actor, like its refund Expense.
        for line in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: line.product_id,
                        qty: line.qty,
                        movement_type: MovementType::In,
                        reason: MovementReason::SaleReturn,
                        reference: sale_number.clone(),
                        date: sale.sale_date,
                    },
                )
                .await?;
        }

        // 2. One refund DELIVERY per paid amount, to the account the money came in
        // through (decision 9). A refund is a payment with `direction='Out'` that
        // REPLAYS the parent payment's account — which is why it must read the
        // historical account off the payment row and never re-derive it from the
        // method, whose owner may have moved since.
        //
        // One delivery per parent payment rather than one for the whole annulment,
        // because the refunds can land in DIFFERENT accounts: two payments into two
        // boxes have to come back out of their own boxes, and a single delivery has one
        // account.
        for pay in &payments {
            let refund_delivery = crate::services::payment_writer::record_delivery_in(
                &self.sequences,
                &self.transactions,
                &self.party_ledger,
                &self.payments,
                &mut tx,
                actor,
                crate::models::PaymentDirection::Out,
                crate::models::PartyType::Customer,
                sale.customer_id,
                pay.method_id,
                pay.account_id,
                pay.amount,
                sale.sale_date,
                Some(format!("cancellation of {sale_number}")),
                Some(sale_number.clone()),
                None,
                // The refund does not APPLY money to a document; it takes money
                // back out. An allocation is what covers a debt, so there is none:
                // the sale itself is being annulled, not paid.
                &[],
            )
            .await?;
            let refund_id = refund_delivery
                .transaction_id
                .ok_or_else(|| AppError::Internal("refund delivery has no movement".into()))?;
            self.sales
                .set_payment_refund_transaction_in(&mut tx, actor, pay.id, refund_id)
                .await?;
        }

        // 3. The document stops being Confirmed. In the same unit as the money leaving.
        let cancelled = self
            .sales
            .set_cancelled_in(&mut tx, sale_id, actor, reason.as_deref())
            .await?;

        tx.commit().await?;

        // AFTER THE COMMIT, DELIBERATELY: `detail_for` reads through the pool.
        self.detail_for(cancelled).await
    }

    /// The documents drawer's draft delete — and the discarded (never-confirmed)
    /// cancelled sale the repo layer now admits. A draft is the one deletable
    /// state: it never touched stock, money or a customer's debt —
    /// `record_payment` refuses anything not Confirmed, and the stock
    /// movement and the ledger entry are both created by `confirm` — so
    /// nothing dangles when it goes; only its own CASCADE children die with
    /// it. A discarded sale (cancelled while still Draft: `sale_number` stays
    /// NULL) posted nothing either, so it is deletable on the same terms. A
    /// confirmed (or confirmed-then-cancelled) document is ANULLED through
    /// `cancel` instead: deleting one would strand its ledger entries and
    /// stock history, and its non-NULL number proves it was confirmed.
    ///
    /// No `actor` parameter, deliberately: nothing survives to stamp — the
    /// row and its lines are gone — and the control is the route's permission
    /// plus the fact that neither deletable state ever moved stock, money or
    /// debt.
    pub async fn delete_draft(&self, id: i64) -> AppResult<()> {
        let sale = self
            .sales
            .find_sale(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {id} not found")))?;
        let deletable = sale.status == crate::models::SaleStatus::Draft
            || (sale.status == crate::models::SaleStatus::Cancelled && sale.sale_number.is_none());
        if !deletable {
            return Err(AppError::Validation(format!(
                "sale {id} is {}: only a draft or a discarded (never-confirmed) cancelled sale can be deleted",
                sale.status
            )));
        }
        let deleted = self.sales.delete_draft(id).await?;
        if !deleted {
            // A concurrent confirm won the race: the document is no longer a
            // deletable state, so the honest answer is the same refusal as
            // above.
            return Err(AppError::Validation(format!(
                "sale {id} is no longer deletable: only a draft or a discarded (never-confirmed) cancelled sale can be deleted"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewCustomer, NewProduct, ProductKind, UpdateProduct};
    use crate::repositories::{
        CustomerRepository, PaymentMethodRepository, ProductTaxRepository, SqliteAccountRepository,
        SqliteBarcodeRepository, SqliteCategoryRepository, SqliteCustomerRepository,
        SqliteDocSequenceRepository, SqlitePaymentMethodRepository, SqliteProductRepository,
        SqliteSaleRepository, SqliteStockMovementRepository, SqliteTaxSnapshotRepository,
        SqliteTransactionRepository, TaxRepository,
    };
    use crate::repositories::{
        PartyLedgerRepository, PaymentRepository, SqlitePartyLedgerRepository,
        SqlitePaymentRepository,
    };
    use crate::security::test_support;
    use crate::services::{CustomerService, InventoryService, TransactionService};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests below seed their own users instead, because
    /// there the point is telling two actors apart.
    async fn audit_actor(s: &Svc) -> i64 {
        test_support::audit_actor_id(&s.transactions.accounts.pool)
            .await
            .unwrap()
    }

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
        SqliteTaxSnapshotRepository,
        SqlitePartyLedgerRepository,
        SqlitePaymentRepository,
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

    async fn svc_with_flags(allow_stock: bool, allow_balance: bool) -> (Svc, sqlx::SqlitePool) {
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
            .create_customer(
                test_support::audit_actor_id(&pool).await.unwrap(),
                NewCustomer {
                    name: "Credit Customer".into(),
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
            .unwrap();
        let s = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory,
            transactions,
            SqlitePaymentMethodRepository::new(pool.clone()),
            customers,
            SqliteTaxSnapshotRepository::new(pool.clone()),
            enforce_credit_limit,
            SqlitePartyLedgerRepository::new(pool.clone()),
            SqlitePaymentRepository::new(pool.clone()),
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
            .create_product(
                audit_actor(s).await,
                NewProduct {
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
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
    }

    async fn seed_service(s: &Svc, sku: &str) -> crate::models::Product {
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
                    cost_price: dec("0"),
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
                    date: NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                },
            )
            .await
            .unwrap();
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

    /// A method this account owns, created the way the product creates one.
    ///
    /// Migration 45 deleted the history-less seed leftovers, so a test that wants
    /// a second method of its own has to create it instead of looking up a name
    /// that is no longer in the table.
    async fn own_method(s: &Svc, account_id: i64, name: &str) -> i64 {
        s.payment_methods
            .create_in_account(audit_actor(s).await, name, account_id)
            .await
            .unwrap()
            .id
    }

    /// A method that exists but CANNOT be used: owned by the account and
    /// deactivated. Migration 45 made "unowned" unrepresentable, so this is what
    /// "this method cannot pay" means now — and it is the refusal the operator
    /// sees when they untick a method in the account editor.
    async fn inactive_method(s: &Svc, account_id: i64, name: &str) -> i64 {
        let id = own_method(s, account_id, name).await;
        s.payment_methods
            .set_active(audit_actor(s).await, id, false)
            .await
            .unwrap();
        id
    }

    async fn allow(s: &Svc, account_id: i64, method_id: i64) {
        s.payment_methods
            .set_method_account(audit_actor(s).await, method_id, account_id)
            .await
            .unwrap()
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
        due_days: Option<i64>,
    ) -> crate::models::Customer {
        s.customers
            .create_customer(
                audit_actor(s).await,
                NewCustomer {
                    name: name.into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: limit.map(dec),
                    due_days,
                },
            )
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id,
                    payment_type,
                    sale_date: sale_date(),
                    due_date,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, product_id, dec(qty), None)
            .await
            .unwrap();
        sale
    }

    async fn sale_sequence_last(pool: &sqlx::SqlitePool) -> Option<i64> {
        sqlx::query_as::<_, (i64,)>("SELECT last_number FROM doc_sequences WHERE doc_type = 'SALE'")
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

    /// The document's payments, counted in the database rather than through the
    /// detail view: the duplicate-submission tests must see the raw residue,
    /// including a payment a refused second call managed to leave behind.
    async fn payment_count(pool: &sqlx::SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    /// The document's stored `(status, sale_number)`, read straight from the
    /// row. The failure-window tests must see what the database actually kept,
    /// never what a service view would reconstruct for them.
    async fn row_state(pool: &sqlx::SqlitePool, sale_id: i64) -> (String, Option<String>) {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, sale_number FROM sales WHERE id = ?",
        )
        .bind(sale_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    // -- AC1 ------------------------------------------------------------------

    #[tokio::test]
    async fn red_ac1_draft_touches_nothing() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "RED-1", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        assert!(sale.sale_number.is_none());
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("3"), None)
            .await
            .unwrap();

        let detail = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
        let number = detail.sale.sale_number.clone().unwrap();
        assert_eq!(number, "2024-SALE-000001");
        assert_eq!(detail.total, dec("30"));
        assert_eq!(detail.paid, dec("30"));
        assert_eq!(detail.due, Decimal::ZERO);
        assert_eq!(detail.payment_status, PaymentStatus::Paid);
        // Stock deducted: 10 - 3 = 7.
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("7")
        );
        // 1 Income + 0 other.
        assert_eq!(tx_count(&pool).await, 1);
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, crate::models::TransactionKind::Income);
        assert_eq!(rows[0].amount, dec("30"));
        assert_eq!(rows[0].description, number);
        // Stock reference = sale_number, reason Sale, type Out.
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        let out = moves
            .iter()
            .find(|m| m.movement_type == MovementType::Out)
            .unwrap();
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        let detail = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        assert!(detail.sale.sale_number.is_some());
        assert_eq!(detail.total, dec("24"));
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("24"));
        assert_eq!(detail.payment_status, PaymentStatus::Unpaid);
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("8")
        );
        assert_eq!(tx_count(&pool).await, 0);
    }

    /// N2: the record view resolves product, account and method names through
    /// the service read paths, so the route never runs SQL or renders an
    /// internal key. A draft with no payments still resolves its lines.
    #[tokio::test]
    async fn record_view_resolves_product_account_and_method_names() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "REC-NAME", "20").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-record").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "2",
        )
        .await;
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("10"), sale_date())
            .await
            .unwrap();

        let record = s.get_record(sale.id).await.unwrap();
        assert_eq!(record.lines.len(), 1);
        assert_eq!(record.lines[0].product_name, prod.name);
        assert_eq!(record.lines[0].product_sku, prod.sku);
        assert_eq!(record.lines[0].subtotal, dec("40"));
        assert_eq!(record.payments.len(), 1);
        assert_eq!(record.payments[0].account_name, acc.name);
        assert_eq!(record.payments[0].method_name, "Cash");
        let money = record.money.expect("an ordinary document totals");
        assert_eq!(money.total, dec("40"));
        assert_eq!(money.paid, dec("10"));
        assert_eq!(money.due, dec("30"));
        assert_eq!(money.payment_status, PaymentStatus::Partial);

        let draft = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "1",
        )
        .await;
        let draft_record = s.get_record(draft.id).await.unwrap();
        assert!(draft_record.payments.is_empty());
        assert_eq!(draft_record.lines[0].product_name, prod.name);
        assert_eq!(draft_record.lines[0].product_sku, prod.sku);
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 40
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        s.record_payment(
            audit_actor(&s).await,
            sale.id,
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
                audit_actor(&s).await,
                sale.id,
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
            audit_actor(&s).await,
            sale.id,
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let err = s
            .add_line(audit_actor(&s).await, sale.id, 99999, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // qty <= 0 => 400.
        let err = s
            .add_line(audit_actor(&s).await, sale.id, prod.id, dec("0"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .add_line(audit_actor(&s).await, sale.id, prod.id, dec("-1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Unknown method on Cash confirm => 404.
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        let err = s
            .confirm(audit_actor(&s).await, sale.id, Some(999_999))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unknown method on payment => 404.
        let csale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, csale.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, csale.id, None)
            .await
            .unwrap();
        let err = s
            .record_payment(
                audit_actor(&s).await,
                csale.id,
                999_999,
                dec("5"),
                sale_date(),
            )
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();

        // Double confirm => 400/409.
        let err = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::Validation(_) | AppError::Conflict(_)),
            "got {err:?}"
        );

        // Edit Confirmed => 400 (add / update / remove / header).
        let err = s
            .add_line(audit_actor(&s).await, sale.id, prod.id, dec("1"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let detail = s.get_detail(sale.id).await.unwrap();
        let line_id = detail.lines[0].id;
        let err = s
            .update_line(audit_actor(&s).await, line_id, dec("2"), dec("10"))
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
                sale.id,
                audit_actor(&s).await,
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("4"), None)
            .await
            .unwrap(); // total 40
        let confirmed = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
        let number = confirmed.sale.sale_number.clone().unwrap();
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("6")
        );
        assert_eq!(tx_count(&pool).await, 1);

        let cancelled = s
            .cancel(audit_actor(&s).await, sale.id, Some("devuelve".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.sale.status, crate::models::SaleStatus::Cancelled);
        // Stock re-entered: 6 + 4 = 10.
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("10")
        );
        // Refund Expense created: Income + Expense = 2 rows, net 0.
        assert_eq!(tx_count(&pool).await, 2);
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let expense = rows
            .iter()
            .find(|t| t.kind == crate::models::TransactionKind::Expense)
            .unwrap();
        assert_eq!(expense.amount, dec("40"));
        assert_eq!(expense.description, number);
        // In movement reason Sale-return, reference sale_number.
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        let ret = moves
            .iter()
            .find(|m| {
                m.movement_type == MovementType::In && m.reference == number && m.qty == dec("4")
            })
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 20
        s.confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("8")
        );
        // Drain account: Income 20, then Expense 20 => balance 0.
        s.transactions
            .create(
                audit_actor(&s).await,
                acc.id,
                crate::models::TransactionKind::Expense,
                dec("20"),
                Some("gasto".into()),
                sale_date(),
            )
            .await
            .unwrap();
        // Cancel would refund 20 => balance -20, must fail with allow_negative=false.
        let err = s
            .cancel(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // No stock re-entry, no extra refund.
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            dec("8")
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s2.add_line(audit_actor(&s2).await, sale2.id, prod2.id, dec("2"), None)
            .await
            .unwrap();
        s2.confirm(audit_actor(&s2).await, sale2.id, Some(cash2))
            .await
            .unwrap();
        s2.transactions
            .create(
                audit_actor(&s2).await,
                acc2.id,
                crate::models::TransactionKind::Expense,
                dec("20"),
                Some("gasto".into()),
                sale_date(),
            )
            .await
            .unwrap();
        s2.cancel(audit_actor(&s2).await, sale2.id, None)
            .await
            .unwrap();
        assert_eq!(
            s2.inventory.stock_for_decision(prod2.id).await.unwrap(),
            dec("10")
        );
    }

    // -- Annulment pre-validation: aggregate per account + partial detection ------

    /// The audit's scenario: two payments of 60 on one account whose balance is
    /// 100, `allow_negative = false`. The per-payment pre-check saw 100 twice and
    /// passed; the first refund posted (100 → 40) and the second hit
    /// `create_with_reference`'s live guard, leaving the sale Confirmed with the
    /// stock already returned and one refund linked. The annulment must be
    /// pre-validated against the AGGREGATE of refunds per account, before any
    /// write — and a rejection must leave every effect absent, not half of them.
    #[tokio::test]
    async fn red_cancel_refuses_when_aggregate_refunds_exceed_balance() {
        let (s, pool) = svc_with_flags(true, false).await;
        let prod = seed_product(&s, "ANUL-1", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-anul").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        // A Credit sale creates no payment at confirm, so the two payments of 60
        // below are the only ones (the walk-in cannot take credit; the seeded
        // credit customer can).
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(sale_date()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("12"), None)
            .await
            .unwrap(); // total 120
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        // Two payments of 60 (each posts Income 60); then drain 20 so the
        // account balance is 100 while the sale still holds 120 paid.
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("60"), sale_date())
            .await
            .unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("60"), sale_date())
            .await
            .unwrap();
        s.transactions
            .create(
                audit_actor(&s).await,
                acc.id,
                crate::models::TransactionKind::Expense,
                dec("20"),
                Some("gasto".into()),
                sale_date(),
            )
            .await
            .unwrap();
        let balance_before = s
            .transactions
            .transactions
            .balance_for_account(acc.id)
            .await
            .unwrap();
        assert_eq!(balance_before, dec("100"));

        // The aggregate refund (60 + 60 = 120) exceeds the balance (100): refuse
        // with a Validation naming the shortfall, BEFORE writing anything.
        let err = s
            .cancel(audit_actor(&s).await, sale.id, Some("anulo".into()))
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("balance 100"),
            "message must name the balance: {msg}"
        );
        assert!(
            msg.contains("would become"),
            "message must name the result: {msg}"
        );

        // NOTHING was written: the sale is still Confirmed...
        let still = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(still.status, crate::models::SaleStatus::Confirmed);
        // ...no Sale-return movement exists for its number...
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        assert!(
            !moves.iter().any(|m| m.reason == MovementReason::SaleReturn),
            "no Sale-return movement may exist after a refused annulment"
        );
        // ...and neither payment carries a refund link.
        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 2);
        assert!(
            payments.iter().all(|p| p.refund_transaction_id.is_none()),
            "no payment may carry a refund after a refused annulment"
        );
    }

    /// The same shape with `allow_negative = true`: the aggregate guard is
    /// configuration-dependent, not a new rule — the annulment goes through and
    /// both refunds exist.
    #[tokio::test]
    async fn red_cancel_with_allow_negative_skips_the_aggregate_guard() {
        let (s, _) = svc_with_flags(true, true).await;
        let prod = seed_product(&s, "ANUL-2", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-anul2").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(sale_date()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("12"), None)
            .await
            .unwrap(); // total 120
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            sale.id,
            cash,
            dec("120"),
            sale_date(),
        )
        .await
        .unwrap();

        let cancelled = s
            .cancel(audit_actor(&s).await, sale.id, Some("anulo".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.sale.status, crate::models::SaleStatus::Cancelled);
        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 1);
        assert!(payments.iter().all(|p| p.refund_transaction_id.is_some()));
    }

    /// Invariant 10's "the residual is detected rather than hidden": a sale whose
    /// annulment was PARTIALLY applied by an earlier attempt (one payment already
    /// carries a `refund_transaction_id`) must be refused, not doubled — a second
    /// pass would write the return movement a second time and refund twice.
    #[tokio::test]
    async fn red_cancel_refuses_a_partially_applied_annulment() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "ANUL-3", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-anul3").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;

        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("4"), None)
            .await
            .unwrap(); // total 40
        s.confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 1);

        // Simulate the earlier attempt's residual directly: one refund link on
        // the payment row, pointing at a real transaction (the FK is RESTRICT).
        // The real defect's first pass also left a Sale-return movement behind —
        // which is exactly what this guard refuses to double.
        let residual = s
            .transactions
            .create(
                audit_actor(&s).await,
                acc.id,
                crate::models::TransactionKind::Expense,
                dec("40"),
                Some("refund residual".into()),
                sale_date(),
            )
            .await
            .unwrap();
        s.sales
            .set_payment_refund_transaction(audit_actor(&s).await, payments[0].id, residual.id)
            .await
            .unwrap();
        let movements_before = movement_count(&pool).await;

        let err = s
            .cancel(audit_actor(&s).await, sale.id, Some("otra vez".into()))
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

        // Nothing new was written: no additional return movement.
        assert_eq!(
            movement_count(&pool).await,
            movements_before,
            "a refused partial annulment must not write another movement"
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, svc_prod.id, dec("2"), None)
            .await
            .unwrap();
        let before = movement_count(&pool).await;
        let detail = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, a.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        let b = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, b.id, prod.id, dec("1"), None)
            .await
            .unwrap();

        let da = s
            .confirm(audit_actor(&s).await, a.id, Some(cash))
            .await
            .unwrap();
        let db = s
            .confirm(audit_actor(&s).await, b.id, Some(cash))
            .await
            .unwrap();
        assert_ne!(da.sale.sale_number.unwrap(), db.sale.sale_number.unwrap());

        // Draft -> Cancelled is a no-op for stock/finance.
        let c = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, c.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;
        let cancelled = s.cancel(audit_actor(&s).await, c.id, None).await.unwrap();
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("10"), None)
            .await
            .unwrap();
        let err = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Still Draft, no number.
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.sale.status, crate::models::SaleStatus::Draft);
        assert!(d.sale.sale_number.is_none());
    }

    /// Two lines of the SAME tracked product are a legitimate document shape
    /// (two units at one price and one at another on the same ticket), so the
    /// strict pre-check cannot refuse the duplicate: it has to add the demands
    /// up per product. Checked per LINE, both lines read the same unmutated
    /// level (10 - 6 = 4, twice) and both pass, and the document then burns its
    /// number and commits the FIRST movement before the second is refused
    /// downstream — leaving a movement row whose reference names no sale, a
    /// burned number, and a Draft that `cancel` will not release.
    #[tokio::test]
    async fn strict_stock_refuses_duplicate_product_lines_before_moving_anything() {
        // NOT svc(): that builds with allow_stock = true, so the strict branch
        // would never run and this test would pass for the wrong reason.
        let (s, pool) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "DUP-LINES", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-dup").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let walkin = walkin_of(&s).await;
        let sale = draft_with_line(&s, walkin.id, PaymentType::Cash, None, prod.id, "6").await;
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("6"), None)
            .await
            .unwrap();

        let movements_before = movement_count(&pool).await;
        let txs_before = tx_count(&pool).await;
        let sequence_before = sale_sequence_last(&pool).await;

        // The document demands 6 + 6 = 12 of a level of 10.
        let err = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap_err();
        let msg = match &err {
            AppError::Validation(m) => m.clone(),
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("insufficient stock"),
            "message must name the stock rule: {msg}"
        );

        // Nothing below the number is written: the refusal is the PRE-CHECK's,
        // before the document claims anything at all.
        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            detail.sale.status,
            crate::models::SaleStatus::Draft,
            "a refused document must stay Draft"
        );
        assert!(detail.sale.sale_number.is_none());
        assert_eq!(
            movement_count(&pool).await,
            movements_before,
            "a refused document must not have moved stock: the summed demand \
             (12) exceeds the level (10), so the pre-check owns this refusal"
        );
        assert_eq!(tx_count(&pool).await, txs_before);
        assert_eq!(sale_sequence_last(&pool).await, sequence_before);

        // The level in the message is the UNMUTATED one (10 -> -2, i.e. 10 - 12).
        // Were the second movement the thing that refused, it would report the
        // level it found after the first one: 4 -> -2.
        assert!(
            msg.contains("10 would become"),
            "the summed demand must be what the message subtracts: {msg}"
        );
    }

    /// Migration 12 seeded five methods and none of them was owned; migration 45
    /// adopts the head of that order (`Cash`) into the account it also seeds and
    /// deletes the four that had no history to protect. `Other` is still not a
    /// seeded name — the assertion this test has always carried.
    #[tokio::test]
    async fn red_payment_methods_seeded_without_other() {
        let (_s, pool) = svc().await;
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, account_id FROM payment_methods ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let names: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        assert_eq!(
            names,
            vec!["Cash"],
            "only the method the seed owns survives; the history-less leftovers are deleted"
        );
        assert!(!names.iter().any(|n| n == "Other"));
        assert!(
            rows.iter().all(|r| r.1 > 0),
            "and every surviving method has an owner, which is what NOT NULL bought"
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        // Missing method => 400.
        let err = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("requires a payment method"),
            "got {err}"
        );
        // Credit must not carry a method either.
        let credit = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, credit.id, prod.id, dec("1"), None)
            .await
            .unwrap();
        let err = s
            .confirm(audit_actor(&s).await, credit.id, Some(cash))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn methods_inactive_method_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "M-DENY", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "m-deny").await;
        // Owned by the account but DEACTIVATED, which is the only "this method
        // cannot pay" state migration 45 left.
        let cash = inactive_method(&s, acc.id, "Cash").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        let moves_before = movement_count(&pool).await;
        let tx_before = tx_count(&pool).await;
        let err = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
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
        // Each account gets its OWN method: migration 45 has one owner per row,
        // so "Cash on A and Transfer on B" means two rows created here.
        let cash = own_method(&s, acc_a.id, "Cash").await;
        let transfer = own_method(&s, acc_b.id, "Transfer").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 40
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            sale.id,
            cash,
            dec("15"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            sale.id,
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
    async fn methods_record_payment_rejects_an_inactive_method_without_finance_touch() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "M-PAY-DENY", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "m-pay-deny").await;
        let _cash = own_method(&s, acc.id, "Cash").await;
        // Deactivated: the reachable "this method cannot pay" state after
        // migration 45 removed "belongs to no account".
        let qr = inactive_method(&s, acc.id, "QR").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 20
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let tx_before = tx_count(&pool).await;
        let err = s
            .record_payment(audit_actor(&s).await, sale.id, qr, dec("5"), sale_date())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(tx_count(&pool).await, tx_before);
        let d = s.get_detail(sale.id).await.unwrap();
        assert_eq!(d.paid, Decimal::ZERO);
    }

    /// **THE test for T3d: the leak went the dangerous way.**
    ///
    /// Before T3d a cancellation returned the stock and posted each refund with
    /// `create_with_reference` — a unit of its OWN — and only then flipped the sale to
    /// Cancelled. A failure in between left the money actually refunded and the
    /// document still Confirmed: the operator sees a live debt that has already been
    /// paid out, and nothing reconciles the two.
    ///
    /// The failure is injected on the CANCELLATION UPDATE, which is the last write of
    /// the unit, so every refund is already on the connection when it fires.
    #[tokio::test]
    async fn a_failure_while_cancelling_rolls_the_refunds_and_the_stock_back() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "T3D-ATOMIC", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "t3d-atomic").await;
        let cash = own_method(&s, acc.id, "Cash").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 20
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("20"), sale_date())
            .await
            .unwrap();

        let stock_before = s.inventory.stock_for_decision(prod.id).await.unwrap();
        let tx_before = tx_count(&pool).await;

        sqlx::raw_sql(
            "CREATE TRIGGER injected_cancel_failure BEFORE UPDATE ON sales \
             WHEN NEW.status = 'Cancelled' \
             BEGIN SELECT RAISE(ABORT, 'injected failure after the refunds'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .cancel(audit_actor(&s).await, sale.id, Some("injected".into()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected failure"),
            "the fixture must be the thing that failed, got {err}"
        );

        // NOTHING survived: the refund did not leave, the goods did not come back, no
        // delivery was born and the sale is still Confirmed.
        assert_eq!(
            tx_count(&pool).await,
            tx_before,
            "the refund movement must die with the unit"
        );
        assert_eq!(
            s.inventory.stock_for_decision(prod.id).await.unwrap(),
            stock_before,
            "and the goods must not have come back either"
        );
        let out_deliveries: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE direction = 'Out'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(out_deliveries, 0, "no refund document exists");
        let refunds: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sale_payments WHERE refund_transaction_id IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(refunds, 0, "and no legacy row claims one");
        assert_eq!(
            s.sales.find_sale(sale.id).await.unwrap().unwrap().status,
            crate::models::SaleStatus::Confirmed,
            "the sale is still live: the refusal was a refusal, not a half annulment"
        );
    }

    /// A successful cancellation writes the refund as an `Out` delivery that REPLAYS
    /// the parent payment's account, and the ledger records the reversal.
    #[tokio::test]
    async fn a_cancellation_writes_an_out_delivery_replaying_the_parents_account() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "T3D-SHAPE", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "t3d-shape").await;
        let cash = own_method(&s, acc.id, "Cash").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("20"), sale_date())
            .await
            .unwrap();

        s.cancel(audit_actor(&s).await, sale.id, Some("shape".into()))
            .await
            .unwrap();

        // The Out delivery: same account, same amount as what came in.
        let out: (String, String, i64, i64) = sqlx::query_as(
            "SELECT direction, amount, party_id, account_id FROM payments \
              WHERE direction = 'Out' ORDER BY id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(out.0, "Out");
        assert_eq!(out.1, "20");
        assert_eq!(out.2, CREDIT_CUSTOMER_ID);
        assert_eq!(
            out.3, acc.id,
            "the money goes back out of the box it came into"
        );

        // Its movement is the sale's reversal and carries the delivery number.
        let movement: (String, String) = sqlx::query_as(
            "SELECT t.kind, t.amount FROM transactions t JOIN payments p ON p.transaction_id = t.id \
              WHERE p.direction = 'Out'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(movement, ("Expense".to_string(), "20".to_string()));

        // And the ledger: the charge, the payment, and the refund that cancels both.
        let entries: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, amount FROM party_ledger_entries ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            entries,
            vec![
                ("Charge".to_string(), "20".to_string()),
                ("Payment".to_string(), "-20".to_string()),
                ("Refund".to_string(), "20".to_string()),
            ],
            "a cancellation reverses: charge, payment, refund"
        );
    }

    // -- P3: the delivery of money is one unit -----------------------------------

    /// **THE test for P3a: the live atomicity defect.**
    ///
    /// Before P3, `record_payment` posted the `Income` in one unit and the payment
    /// row in a second, so a failure between them left cash in the box with no
    /// document behind it — flow 5 of the payment-allocation plan, a live defect
    /// rather than a hypothetical one. The failure is injected AFTER the movement
    /// and before the document, which is exactly the window that used to leak.
    #[tokio::test]
    async fn a_failure_after_the_movement_rolls_the_whole_delivery_back() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "P3-ATOMIC", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "p3-atomic").await;
        let cash = own_method(&s, acc.id, "Cash").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("3"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let tx_before = tx_count(&pool).await;

        // The window: the money is already written, the document is not.
        sqlx::raw_sql(
            "CREATE TRIGGER injected_payment_document_failure BEFORE INSERT ON payments \
             BEGIN SELECT RAISE(ABORT, 'injected failure between the cash row and the payment'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .record_payment(audit_actor(&s).await, sale.id, cash, dec("10"), sale_date())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected failure"),
            "the fixture must be the thing that failed, got {err}"
        );

        // NOTHING survived: not the movement, not the legacy row, not the document,
        // not the ledger entry, and the sale still shows no payment.
        assert_eq!(
            tx_count(&pool).await,
            tx_before,
            "the Income must die with the unit that wrote it"
        );
        let payments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payments, 0);
        let legacy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(legacy, 0, "the legacy row is in the same unit");
        let entries: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM party_ledger_entries WHERE kind = 'Payment'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entries, 0);
        assert_eq!(
            s.get_detail(sale.id).await.unwrap().paid,
            Decimal::ZERO,
            "and the sale shows nothing paid"
        );
        // The number was not burned either: the sequence row only commits with the
        // unit, so a retry takes the FIRST number.
        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_number FROM doc_sequences WHERE doc_type = 'PAYMENT'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(last, None, "a rolled-back delivery returns its number");
    }

    /// A successful direct payment writes the document, its share, its ledger entry
    /// and the legacy row — all four, and the money moves exactly once.
    #[tokio::test]
    async fn a_direct_payment_writes_the_document_its_share_and_one_movement() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "P3-SHAPE", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "p3-shape").await;
        let cash = own_method(&s, acc.id, "Cash").await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("3"), None)
            .await
            .unwrap(); // total 30
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let tx_before = tx_count(&pool).await;

        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("20"), sale_date())
            .await
            .unwrap();

        // ONE movement for the delivery, not one per document it covers.
        assert_eq!(tx_count(&pool).await, tx_before + 1);

        let payment = s
            .payments
            .list_for_party(crate::models::PartyType::Customer, CREDIT_CUSTOMER_ID)
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("the delivery is a document");
        assert_eq!(payment.amount, dec("20"));
        assert_eq!(payment.direction, crate::models::PaymentDirection::In);
        assert!(
            payment.transaction_id.is_some(),
            "and it names its movement"
        );

        // Its share names the sale, and it is exactly what the payment delivered.
        let shares = s.payments.list_allocations(payment.id).await.unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(
            shares[0].target_kind,
            crate::models::PartyDocumentKind::Sale
        );
        assert_eq!(shares[0].target_id, sale.id);
        assert_eq!(shares[0].amount, dec("20"));
        assert_eq!(
            s.payments.unapplied_for_payment(payment.id).await.unwrap(),
            Decimal::ZERO,
            "the whole delivery was applied"
        );

        // And the sale's residual is what the document says.
        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(detail.paid, dec("20"));
        assert_eq!(detail.due, dec("10"));
    }

    // -- the party ledger (T2) ---------------------------------------------------

    /// The journal rows of one document, oldest first, as the sign rule stored
    /// them. Read through SQL rather than through a service read on purpose: what
    /// this pins is what `confirm` WROTE, and a read path of its own could agree
    /// with the write while both disagree with the schema.
    async fn ledger_rows(pool: &sqlx::SqlitePool, document_id: i64) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, amount FROM party_ledger_entries \
             WHERE document_kind = 'Sale' AND document_id = ? ORDER BY id",
        )
        .bind(document_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A confirmed CREDIT sale is a debt: one `Charge` of `+total`, no cash leg,
    /// and the customer's balance is exactly that figure.
    #[tokio::test]
    async fn a_confirmed_credit_sale_appends_one_charge_for_its_total() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "LEDGER-CREDIT", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let customer = seed_customer(&s, "Ledger Credit Buyer", None, None).await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 20

        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        assert_eq!(
            ledger_rows(&pool, sale.id).await,
            vec![("Charge".to_string(), "20".to_string())],
            "a credit sale owes its total: one Charge, and no cash leg"
        );
        assert_eq!(
            s.party_ledger
                .balance_for_party(crate::models::PartyType::Customer, customer.id)
                .await
                .unwrap(),
            dec("20"),
            "and the balance is the fold of that row"
        );
    }

    /// A confirmed CASH sale is a debt settled on the spot: `+total` and
    /// `−total` in the SAME unit, folding to zero. The two rows are the point —
    /// a single signed row would hide that money moved, and the balance would
    /// stop being the fold of the journal.
    #[tokio::test]
    async fn a_confirmed_cash_sale_appends_a_charge_and_its_settlement() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "LEDGER-CASH", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "ledger-cash").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("3"), None)
            .await
            .unwrap(); // total 30

        s.confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();

        assert_eq!(
            ledger_rows(&pool, sale.id).await,
            vec![
                ("Charge".to_string(), "30".to_string()),
                ("Payment".to_string(), "-30".to_string()),
            ],
            "a cash sale charges and settles in one confirm, in that order"
        );
        assert_eq!(
            s.party_ledger
                .balance_for_party(crate::models::PartyType::Customer, WALKIN_ID)
                .await
                .unwrap(),
            Decimal::ZERO,
            "and the two rows fold to nothing owed"
        );
    }

    /// The write joins the caller's unit, which is the whole reason it is an
    /// `_in` call: a failure AFTER the entry rolls the entry back with the
    /// document. The injected failure is sqlite refusing the confirmation write
    /// — the last write in the unit — so the ledger row is already in the table
    /// when the unit dies.
    #[tokio::test]
    async fn a_failed_confirm_rolls_the_ledger_entry_back_with_the_document() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "LEDGER-ROLLBACK", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let customer = seed_customer(&s, "Ledger Rollback Buyer", None, None).await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();

        sqlx::raw_sql(
            "CREATE TRIGGER injected_ledger_probe BEFORE UPDATE ON sales \
             WHEN NEW.status = 'Confirmed' \
             BEGIN SELECT RAISE(ABORT, 'injected failure after the ledger write'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected failure"),
            "the fixture must be the thing that failed, got {err}"
        );

        assert!(
            ledger_rows(&pool, sale.id).await.is_empty(),
            "the entry must die with the unit that wrote it"
        );
        assert_eq!(
            s.party_ledger
                .balance_for_party(crate::models::PartyType::Customer, customer.id)
                .await
                .unwrap(),
            Decimal::ZERO,
            "and the balance must not have moved"
        );
        assert_eq!(
            s.sales
                .find_sale(sale.id)
                .await
                .unwrap()
                .unwrap()
                .sale_number,
            None,
            "the sale is still a numberless Draft: nothing committed"
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 20

        let detail = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
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
        // P3 (decision 5): the cash row is stamped with the DELIVERY's number.
        // `description` still carries the sale, which is the human label on a
        // statement; the reference is the document the money is traceable to.
        assert!(
            rows[0]
                .reference
                .as_deref()
                .map(|r| r.contains("-PAY-"))
                .unwrap_or(false),
            "the cash confirm's movement is stamped with the delivery it created, got {:?}",
            rows[0].reference
        );
        assert_ne!(
            rows[0].reference.as_deref(),
            Some(number.as_str()),
            "and NOT with the sale number, which is the document's own"
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let number = detail.sale.sale_number.clone().unwrap();

        let paid = s
            .record_payment(
                audit_actor(&s).await,
                sale.id,
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
        // P3 (decision 5): the movement is stamped with the DELIVERY's number, not
        // the sale's. The money arrived once and may cover several documents, so the
        // cash row names the payment; the sale is named by the allocation and by the
        // ledger entry. One movement, one document — the payment.
        let payment_number = s
            .payments
            .list_for_party(crate::models::PartyType::Customer, sale.customer_id)
            .await
            .unwrap()
            .first()
            .expect("the payment document exists")
            .number
            .clone();
        assert_eq!(income.reference.as_deref(), Some(payment_number.as_str()));
        assert_ne!(
            income.reference.as_deref(),
            Some(number.as_str()),
            "the sale number is no longer what the cash row is stamped with"
        );

        s.cancel(audit_actor(&s).await, sale.id, Some("refund".into()))
            .await
            .unwrap();

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
        // T3d: the refund is a DELIVERY of money going out, so its movement is stamped
        // with the delivery's own number — a different document from the sale's, and
        // that is the point: the money leaving is its own citable fact.
        assert!(
            refund
                .reference
                .as_deref()
                .map(|r| r.contains("-PAY-"))
                .unwrap_or(false),
            "got {:?}",
            refund.reference
        );
        assert_ne!(refund.reference.as_deref(), Some(number.as_str()));
        // And the delivery exists as a document with `direction = 'Out'`.
        let out: (String, String) =
            sqlx::query_as("SELECT direction, amount FROM payments WHERE transaction_id = ?")
                .bind(refund_id)
                .fetch_one(s.sales.pool())
                .await
                .unwrap();
        assert_eq!(out, ("Out".to_string(), "15".to_string()));
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap(); // total 40
        let detail = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let number = detail.sale.sale_number.clone().unwrap();

        let first = s
            .record_payment(
                audit_actor(&s).await,
                sale.id,
                cash,
                dec("15"),
                NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
            )
            .await
            .unwrap();
        let second = s
            .record_payment(
                audit_actor(&s).await,
                sale.id,
                cash,
                dec("25"),
                NaiveDate::from_ymd_opt(2024, 5, 11).unwrap(),
            )
            .await
            .unwrap();
        let first_tx = first
            .transaction_id
            .expect("first payment links its Income");
        let second_tx = second
            .transaction_id
            .expect("second payment links its Income");
        assert_ne!(
            first_tx, second_tx,
            "each payment links its own transaction"
        );

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
        // P3 (decision 5): each movement names the DELIVERY it belongs to, so two
        // payments on one sale carry two different references. The sale is the
        // allocation's and the ledger entry's subject, not the cash row's.
        let refs: Vec<Option<String>> = rows.iter().map(|t| t.reference.clone()).collect();
        assert!(
            refs.iter()
                .all(|r| r.as_deref().map(|n| n.contains("-PAY-")).unwrap_or(false)),
            "every payment movement is stamped with a payment number, got {refs:?}"
        );
        assert_ne!(
            refs[0], refs[1],
            "two deliveries are two documents, so their cash rows cannot share a reference"
        );
        let _ = number;
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        let detail = s
            .confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
        let number = detail.sale.sale_number.clone().unwrap();
        let tx_id = s.sales.list_payments(sale.id).await.unwrap()[0]
            .transaction_id
            .unwrap();

        // `description` is editable free text; the document link must not depend
        // on it, so editing it leaves `reference` (and the payment link) intact.
        let updated = s
            .transactions
            .update(
                audit_actor(&s).await,
                tx_id,
                None,
                None,
                Some("edited by hand".into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(updated.description, "edited by hand");
        // P3: the reference names the DELIVERY document, and editing the free-text
        // description must leave that link exactly as it was.
        assert!(
            updated
                .reference
                .as_deref()
                .map(|r| r.contains("-PAY-"))
                .unwrap_or(false),
            "the movement keeps the delivery's number, got {:?}",
            updated.reference
        );
        assert_ne!(
            updated.reference.as_deref(),
            Some(number.as_str()),
            "editing the description must not re-stamp it with the sale"
        );
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, Some(cash))
            .await
            .unwrap();
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: 99999,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        let customer = seed_customer(&s, "Ana", None, None).await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
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
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();

        s.customers
            .update_customer(
                customer.id,
                audit_actor(&s).await,
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

        let err = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap_err();
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
        let first = draft_with_line(&s, customer.id, PaymentType::Credit, None, prod.id, "3").await;
        assert_eq!(
            first.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            "due_date must default to sale_date + due_days (2024-05-02 + 30)"
        );
        s.confirm(audit_actor(&s).await, first.id, None)
            .await
            .unwrap();

        // Projected 30 + 80 = 110 > 100 => 400 with the projection.
        let second =
            draft_with_line(&s, customer.id, PaymentType::Credit, None, prod.id, "8").await;
        let movements_before = movement_count(&pool).await;
        let err = s
            .confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap_err();
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

        let detail = s
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        assert_eq!(detail.sale.status, crate::models::SaleStatus::Confirmed);
        let debt = s.customer_balance(customer.id).await.unwrap();
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
            let detail = s
                .confirm(audit_actor(&s).await, sale.id, None)
                .await
                .unwrap();
            assert_eq!(
                detail.sale.status,
                crate::models::SaleStatus::Confirmed,
                "enforce_credit_limit={enforce} must not block a null limit"
            );
        }
    }

    /// AC7: a credit sale without a due date takes `sale_date + due_days`;
    /// without a term the due date is required. An explicit date still wins.
    #[tokio::test]
    async fn k2_ac7_due_date_defaults_from_due_days_or_400() {
        let (s, _) = svc().await;
        let term_customer = seed_customer(&s, "Term", None, Some(15)).await;
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: term_customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            sale.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 5, 17).unwrap()),
            "due_date must default to sale_date + due_days"
        );

        // An explicit date wins over the term.
        let explicit = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: term_customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 7, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            explicit.due_date,
            Some(NaiveDate::from_ymd_opt(2024, 7, 1).unwrap())
        );

        // No term and no date => 400 at creation.
        let no_term = seed_customer(&s, "No Term", None, None).await;
        let err = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: no_term.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
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
        let first = draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "6").await;
        s.confirm(audit_actor(&s).await, first.id, None)
            .await
            .unwrap();

        // Paying 40 leaves 20 of debt, so another 70 fits (90 <= 100).
        s.record_payment(
            audit_actor(&s).await,
            first.id,
            cash,
            dec("40"),
            sale_date(),
        )
        .await
        .unwrap();
        let second = draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "7").await;
        s.confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap();

        // Cancelling the first sale removes its remaining 20 => debt 70.
        s.cancel(audit_actor(&s).await, first.id, Some("tri".into()))
            .await
            .unwrap();
        assert_eq!(s.customer_balance(customer.id).await.unwrap(), dec("70"));

        // Boundary: projected debt exactly equal to the limit is allowed.
        let boundary =
            draft_with_line(&s, customer.id, PaymentType::Credit, due, prod.id, "3").await;
        let detail = s
            .confirm(audit_actor(&s).await, boundary.id, None)
            .await
            .unwrap();
        assert_eq!(detail.sale.status, crate::models::SaleStatus::Confirmed);
        assert_eq!(s.customer_balance(customer.id).await.unwrap(), dec("100"));
    }

    // -- K3: derived receivable reads (balance, ageing, statement) -------------

    /// Like `draft_with_line`, but with an explicit sale date so the boundary
    /// tests can place sales far enough back to be 60+ days overdue.
    async fn draft_on(
        s: &Svc,
        customer_id: i64,
        payment_type: PaymentType,
        date: NaiveDate,
        due_date: Option<NaiveDate>,
        product_id: i64,
        qty: &str,
    ) -> crate::models::Sale {
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id,
                    payment_type,
                    sale_date: date,
                    due_date,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, product_id, dec(qty), None)
            .await
            .unwrap();
        sale
    }

    /// AC8: the balance is credit sales minus the payments received on them; cash
    /// sales never contribute and a customer without credit history owes zero.
    #[tokio::test]
    async fn k3_ac8_balance_is_credit_sales_minus_payments() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-AC8", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let acc = seed_account(&s, "k3-ac8").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let due = Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());

        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            Decimal::ZERO,
            "a customer with no credit history owes nothing"
        );

        // Two credit sales: 50 + 30 = 80.
        let first = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            due,
            prod.id,
            "5",
        )
        .await;
        let second = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            due,
            prod.id,
            "3",
        )
        .await;
        s.confirm(audit_actor(&s).await, first.id, None)
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap();

        // A cash sale for the same customer never contributes.
        let cash_sale = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Cash,
            None,
            prod.id,
            "7",
        )
        .await;
        s.confirm(audit_actor(&s).await, cash_sale.id, Some(cash))
            .await
            .unwrap();

        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            dec("80")
        );

        // A payment reduces the balance for that customer only.
        s.record_payment(
            audit_actor(&s).await,
            first.id,
            cash,
            dec("20"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            dec("60")
        );

        let other = seed_customer(&s, "Sin deuda", None, None).await;
        assert_eq!(s.customer_balance(other.id).await.unwrap(), Decimal::ZERO);
    }

    /// AC8: a fully paid sale contributes zero, and a cancelled sale stops
    /// counting on either side while its payment rows stay for history.
    #[tokio::test]
    async fn k3_ac8_fully_paid_and_cancelled_sales_leave_the_balance() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-AC8C", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "k3-ac8c").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let due = Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());

        let sale = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            due,
            prod.id,
            "4",
        )
        .await; // 40
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("15"), sale_date())
            .await
            .unwrap();
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            dec("25")
        );

        // Fully paid: the sale contributes zero.
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("25"), sale_date())
            .await
            .unwrap();
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            Decimal::ZERO
        );

        // Cancelled: nothing on either side, and the payments are still on file.
        s.cancel(audit_actor(&s).await, sale.id, Some("k3".into()))
            .await
            .unwrap();
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            Decimal::ZERO
        );
        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(payments.len(), 2, "cancelled sales keep their payment rows");
        assert!(payments.iter().all(|p| p.refund_transaction_id.is_some()));
        let ageing = s
            .customer_ageing(CREDIT_CUSTOMER_ID, sale_date())
            .await
            .unwrap();
        assert_eq!(ageing.total(), SetMoney::amount(Decimal::ZERO));
    }

    /// AC9: every bucket boundary is exact — due today, 1, 30, 31, 60 and 61 days
    /// late — a credit sale with no due date counts as current, and the buckets
    /// sum exactly to the balance.
    #[tokio::test]
    async fn k3_ac9_ageing_bucket_boundaries() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "K3-AC9", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let as_of = NaiveDate::from_ymd_opt(2024, 5, 31).unwrap();
        let base_date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();

        // One unit per sale makes every boundary unambiguous (10 each).
        let due_dates = [
            NaiveDate::from_ymd_opt(2024, 5, 31).unwrap(), // due today -> current
            NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),  // not yet due -> current
            NaiveDate::from_ymd_opt(2024, 5, 30).unwrap(), // 1 day late
            NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),  // 30 days late
            NaiveDate::from_ymd_opt(2024, 4, 30).unwrap(), // 31 days late
            NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(),  // 60 days late
            NaiveDate::from_ymd_opt(2024, 3, 31).unwrap(), // 61 days late
        ];
        for due_date in due_dates {
            let sale = draft_on(
                &s,
                CREDIT_CUSTOMER_ID,
                PaymentType::Credit,
                base_date,
                Some(due_date),
                prod.id,
                "1",
            )
            .await;
            s.confirm(audit_actor(&s).await, sale.id, None)
                .await
                .unwrap();
        }

        // A credit sale with no due date counts as current.
        let (no_due_id,): (i64,) = sqlx::query_as(
            r#"INSERT INTO sales (sale_number, status, payment_type, customer_id, customer_name, sale_date, due_date, created_by)
               VALUES ('2024-SALE-000900', 'Confirmed', 'Credit', ?, 'Credit Customer', '2024-01-01', NULL, ?)
               RETURNING id"#,
        )
        .bind(CREDIT_CUSTOMER_ID)
        .bind(audit_actor(&s).await)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO sale_lines (sale_id, product_id, qty, unit_price)
               VALUES (?, ?, '1', '10')"#,
        )
        .bind(no_due_id)
        .bind(prod.id)
        .execute(&pool)
        .await
        .unwrap();

        let ageing = s.customer_ageing(CREDIT_CUSTOMER_ID, as_of).await.unwrap();
        assert_eq!(
            ageing.current,
            SetMoney::amount(dec("30")),
            "due today, not yet due and no due date"
        );
        assert_eq!(
            ageing.overdue_1_30,
            SetMoney::amount(dec("20")),
            "exactly 1 and 30 days late"
        );
        assert_eq!(
            ageing.overdue_31_60,
            SetMoney::amount(dec("20")),
            "exactly 31 and 60 days late"
        );
        assert_eq!(
            ageing.overdue_61_plus,
            SetMoney::amount(dec("10")),
            "exactly 61 days late"
        );
        assert_eq!(ageing.total().amount, Some(dec("80")));
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            ageing.total().amount.unwrap()
        );
    }

    /// AC9: the aggregate covers exactly the customers with a non-zero balance,
    /// follows partial payments, and its buckets sum to the summed balances.
    #[tokio::test]
    async fn k3_ac9_ageing_all_covers_nonzero_balances() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-ALL", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let acc = seed_account(&s, "k3-all").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let as_of = NaiveDate::from_ymd_opt(2024, 5, 31).unwrap();
        let base_date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();

        let ana = seed_customer(&s, "Ana", None, None).await;
        let bruno = seed_customer(&s, "Bruno", None, None).await;
        let carla = seed_customer(&s, "Carla", None, None).await;

        // Ana: 30 due today and 20 overdue by 21 days after a partial payment.
        let a1 = draft_on(
            &s,
            ana.id,
            PaymentType::Credit,
            base_date,
            Some(NaiveDate::from_ymd_opt(2024, 5, 31).unwrap()),
            prod.id,
            "3",
        )
        .await;
        let a2 = draft_on(
            &s,
            ana.id,
            PaymentType::Credit,
            base_date,
            Some(NaiveDate::from_ymd_opt(2024, 5, 10).unwrap()),
            prod.id,
            "4",
        )
        .await;
        s.confirm(audit_actor(&s).await, a1.id, None).await.unwrap();
        s.confirm(audit_actor(&s).await, a2.id, None).await.unwrap();
        s.record_payment(
            audit_actor(&s).await,
            a2.id,
            cash,
            dec("10"),
            NaiveDate::from_ymd_opt(2024, 5, 20).unwrap(),
        )
        .await
        .unwrap();

        // Bruno: one sale more than 60 days late.
        let b1 = draft_on(
            &s,
            bruno.id,
            PaymentType::Credit,
            base_date,
            Some(NaiveDate::from_ymd_opt(2024, 3, 1).unwrap()),
            prod.id,
            "4",
        )
        .await;
        s.confirm(audit_actor(&s).await, b1.id, None).await.unwrap();

        // Carla: fully paid, so she must not appear.
        let c1 = draft_on(
            &s,
            carla.id,
            PaymentType::Credit,
            base_date,
            Some(NaiveDate::from_ymd_opt(2024, 5, 31).unwrap()),
            prod.id,
            "1",
        )
        .await;
        s.confirm(audit_actor(&s).await, c1.id, None).await.unwrap();
        s.record_payment(audit_actor(&s).await, c1.id, cash, dec("10"), base_date)
            .await
            .unwrap();

        let rows = s.ageing_all(as_of).await.unwrap();
        assert_eq!(rows.len(), 2, "only non-zero balances are listed");
        assert_eq!(rows[0].customer_id, ana.id);
        assert_eq!(rows[0].balance.amount, Some(dec("60")));
        assert_eq!(rows[0].ageing.current, SetMoney::amount(dec("30")));
        assert_eq!(rows[0].ageing.overdue_1_30, SetMoney::amount(dec("30")));
        assert_eq!(rows[0].ageing.total(), rows[0].balance);
        assert_eq!(rows[1].customer_id, bruno.id);
        assert_eq!(rows[1].balance.amount, Some(dec("40")));
        assert_eq!(rows[1].ageing.overdue_61_plus, SetMoney::amount(dec("40")));
        assert!(rows.iter().all(|row| row.customer_id != carla.id));
        assert!(rows.iter().all(|row| row.customer_id != WALKIN_ID));

        let summed: Decimal = rows
            .iter()
            .map(|row| row.ageing.total().amount.unwrap())
            .sum();
        let balances: Decimal = rows.iter().map(|row| row.balance.amount.unwrap()).sum();
        assert_eq!(summed, dec("100"));
        assert_eq!(summed, balances);
        for row in &rows {
            let per_customer = s.customer_ageing(row.customer_id, as_of).await.unwrap();
            assert_eq!(per_customer, row.ageing, "per-customer and aggregate agree");
        }
    }

    /// The statement is chronological, its debits minus its credits equal the
    /// balance, and the final running balance matches `customer_balance`. The journal
    /// shows each payment delivery, including the cancelled sale's refund.
    #[tokio::test]
    async fn k3_statement_balances_out_to_the_customer_balance() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-STMT", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let acc = seed_account(&s, "k3-stmt").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let as_of = NaiveDate::from_ymd_opt(2024, 8, 1).unwrap();

        // 100 confirmed, then 30 + 20 paid on different dates.
        let big = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "10",
        )
        .await;
        s.confirm(audit_actor(&s).await, big.id, None)
            .await
            .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            big.id,
            cash,
            dec("30"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            big.id,
            cash,
            dec("20"),
            NaiveDate::from_ymd_opt(2024, 6, 5).unwrap(),
        )
        .await
        .unwrap();

        // 50 fully paid: its debit and credit stay and cancel out.
        let small = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            NaiveDate::from_ymd_opt(2024, 5, 3).unwrap(),
            Some(NaiveDate::from_ymd_opt(2024, 6, 3).unwrap()),
            prod.id,
            "5",
        )
        .await;
        s.confirm(audit_actor(&s).await, small.id, None)
            .await
            .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            small.id,
            cash,
            dec("50"),
            NaiveDate::from_ymd_opt(2024, 6, 3).unwrap(),
        )
        .await
        .unwrap();

        // A cancelled credit sale never shows up, payment included.
        let gone = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            NaiveDate::from_ymd_opt(2024, 5, 4).unwrap(),
            Some(NaiveDate::from_ymd_opt(2024, 6, 4).unwrap()),
            prod.id,
            "2",
        )
        .await;
        s.confirm(audit_actor(&s).await, gone.id, None)
            .await
            .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            gone.id,
            cash,
            dec("5"),
            NaiveDate::from_ymd_opt(2024, 5, 20).unwrap(),
        )
        .await
        .unwrap();
        s.cancel(audit_actor(&s).await, gone.id, Some("k3".into()))
            .await
            .unwrap();

        let statement = s
            .customer_statement(CREDIT_CUSTOMER_ID, as_of)
            .await
            .unwrap();
        assert_eq!(statement.customer_id, CREDIT_CUSTOMER_ID);
        assert_eq!(statement.as_of, as_of);
        // 150 sales - 100 payments. The figures are `SetMoney`s because they are
        // sums over a SET of documents; on a receivable that totals they are the
        // amounts, unchanged.
        assert_eq!(statement.balance.amount, Some(dec("50")));
        assert_eq!(statement.balance.refusal, None);
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            statement.balance.amount.unwrap()
        );
        assert_eq!(statement.ageing.total(), statement.balance);

        assert_eq!(statement.entries.len(), 7, "2 active sales plus all five payment deliveries; the cancelled sale's incoming and outgoing deliveries offset");
        let debits: Decimal = statement
            .entries
            .iter()
            .map(|entry| entry.debit.amount.unwrap())
            .sum();
        let credits: Decimal = statement.entries.iter().map(|e| e.credit).sum();
        assert_eq!(debits, dec("155"), "the cancelled sale's 5 refund is a debit");
        assert_eq!(credits, dec("105"), "three active and one cancelled incoming delivery");
        assert_eq!(Some(debits - credits), statement.balance.amount);
        assert_eq!(statement.entries.last().unwrap().balance, statement.balance);
        for pair in statement.entries.windows(2) {
            assert!(pair[0].date <= pair[1].date, "entries are chronological");
        }
        assert!(statement
            .entries
            .iter()
            .any(|e| e.kind == StatementEntryKind::Sale && e.document_number.is_some()));
        assert!(statement
            .entries
            .iter()
            .any(|e| e.kind == StatementEntryKind::Payment && e.credit > Decimal::ZERO));
        assert!(statement.entries.iter().all(|entry| {
            entry.credit == Decimal::ZERO || entry.debit.amount == Some(Decimal::ZERO)
        }));
    }

    /// Ties on the same date stay deterministic: document number, then debits
    /// before credits, then source row id.
    #[tokio::test]
    async fn k3_statement_tied_dates_keep_a_reproducible_order() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-TIE", "10").await;
        seed_stock(&s, prod.id, "100").await;
        let acc = seed_account(&s, "k3-tie").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let as_of = NaiveDate::from_ymd_opt(2024, 6, 30).unwrap();
        let day = NaiveDate::from_ymd_opt(2024, 5, 2).unwrap();

        // Two sales on the same date: 000001 (10) and 000002 (20).
        let first = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            day,
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            prod.id,
            "1",
        )
        .await;
        let second = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            day,
            Some(NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
            prod.id,
            "2",
        )
        .await;
        let n1 = s
            .confirm(audit_actor(&s).await, first.id, None)
            .await
            .unwrap()
            .sale
            .sale_number
            .unwrap();
        let n2 = s
            .confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap()
            .sale
            .sale_number
            .unwrap();
        assert!(n1 < n2, "document order follows the generated numbers");

        // Two payments against the first sale on the same date: creation order.
        s.record_payment(
            audit_actor(&s).await,
            first.id,
            cash,
            dec("7"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();
        s.record_payment(
            audit_actor(&s).await,
            first.id,
            cash,
            dec("3"),
            NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
        )
        .await
        .unwrap();

        let statement = s
            .customer_statement(CREDIT_CUSTOMER_ID, as_of)
            .await
            .unwrap();
        let lines: Vec<(StatementEntryKind, Option<String>, Decimal)> = statement
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.kind,
                    entry.document_number.clone(),
                    entry.balance.amount.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                (StatementEntryKind::Sale, Some(n1.clone()), dec("10")),
                (StatementEntryKind::Sale, Some(n2), dec("30")),
                (StatementEntryKind::Payment, Some("2024-PAY-000001".into()), dec("23")),
                (StatementEntryKind::Payment, Some("2024-PAY-000002".into()), dec("20")),
            ]
        );
    }

    /// Triangulation: the same receivable moves between buckets as `as_of`
    /// advances, always in exactly one bucket and always with the same total.
    #[tokio::test]
    async fn k3_tri_ageing_moves_with_as_of_and_stays_deterministic() {
        let (s, _) = svc().await;
        let prod = seed_product(&s, "K3-TRI", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let due = NaiveDate::from_ymd_opt(2024, 5, 31).unwrap();
        let sale = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(),
            Some(due),
            prod.id,
            "1",
        )
        .await;
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        let on_due = s.customer_ageing(CREDIT_CUSTOMER_ID, due).await.unwrap();
        assert_eq!(on_due.current, SetMoney::amount(dec("10")));
        assert_eq!(on_due.total().amount, Some(dec("10")));

        let at_30 = s
            .customer_ageing(
                CREDIT_CUSTOMER_ID,
                NaiveDate::from_ymd_opt(2024, 6, 30).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(at_30.overdue_1_30, SetMoney::amount(dec("10")));

        let at_60 = s
            .customer_ageing(
                CREDIT_CUSTOMER_ID,
                NaiveDate::from_ymd_opt(2024, 7, 30).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(at_60.overdue_31_60, SetMoney::amount(dec("10")));

        let at_61 = s
            .customer_ageing(
                CREDIT_CUSTOMER_ID,
                NaiveDate::from_ymd_opt(2024, 7, 31).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(at_61.overdue_61_plus, SetMoney::amount(dec("10")));

        let repeat = s
            .customer_ageing(
                CREDIT_CUSTOMER_ID,
                NaiveDate::from_ymd_opt(2024, 7, 31).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            repeat, at_61,
            "the same as_of always yields the same buckets"
        );
    }

    /// N5 follow-up: the filters run in the repository, so the details loaded scale
    /// with the matching documents, not the shop's history. The repository's
    /// test-only read counter makes the before/after difference deterministic.
    #[tokio::test]
    async fn list_details_filtered_reads_only_the_result_set() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "PERF-S", "10").await;
        seed_stock(&s, prod.id, "200").await;

        // 20 drafts plus one confirmed: the filter matches exactly one document.
        let mut matching = 0;
        for i in 0..20 {
            let sale = draft_with_line(
                &s,
                CREDIT_CUSTOMER_ID,
                PaymentType::Credit,
                Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                prod.id,
                "1",
            )
            .await;
            if i == 7 {
                matching = sale.id;
            }
        }
        s.confirm(audit_actor(&s).await, matching, None)
            .await
            .unwrap();

        s.sales.reset_reads();
        let details = s
            .list_details_filtered(&crate::models::SaleListFilter {
                status: Some(crate::models::SaleStatus::Confirmed),
                ..Default::default()
            })
            .await
            .unwrap();
        let reads = s.sales.read_count();

        assert_eq!(details.len(), 1, "the filter narrows to the confirmed sale");
        assert_eq!(details[0].sale.id, matching);
        assert_eq!(
            reads, 3,
            "one filtered query plus the matching document's lines and payments only, got {reads} reads for 20 sales"
        );
    }

    /// N6 follow-up: the debt banner must not load every sale's details. This first
    /// step measures the full receivable read, which is O(history).
    #[tokio::test]
    async fn debt_banner_reads_are_bounded() {
        let (s, _pool) = svc().await;
        let prod = seed_product(&s, "DEBT-P", "10").await;
        seed_stock(&s, prod.id, "100").await;
        for _ in 0..20 {
            let sale = draft_with_line(
                &s,
                CREDIT_CUSTOMER_ID,
                PaymentType::Credit,
                Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                prod.id,
                "1",
            )
            .await;
            s.confirm(audit_actor(&s).await, sale.id, None)
                .await
                .unwrap();
        }

        s.sales.reset_reads();
        let full = s.outstanding_debt().await.unwrap();
        let before = s.sales.read_count();
        assert_eq!(full.len(), 20);

        s.sales.reset_reads();
        let summary = s.debt_summary(DEBT_BANNER_LIMIT).await.unwrap();
        let after = s.sales.read_count();

        assert_eq!(summary.count, 20);
        assert_eq!(summary.oldest.len(), DEBT_BANNER_LIMIT);
        assert_eq!(
            summary.total.amount,
            Some(full.iter().map(|detail| detail.due).sum::<Decimal>())
        );
        assert!(
            after < before,
            "the banner must not scale with history: {before} reads for the full list, {after} for the banner"
        );
        assert_eq!(after, 21, "the banner reads one sales set and each displayed document's lines");
    }

    /// AC18, the flow half of the finance audit: the Income a confirmed sale
    /// produces carries the CONFIRMING request's actor, not a fresh one, and
    /// stays distinct from the account's own creator.
    #[tokio::test]
    async fn ac18_the_sale_flow_income_carries_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "audit-alice", "Alice")
            .await
            .unwrap();
        let operator = test_support::seed_audit_user(&pool, "audit-bob", "Bob")
            .await
            .unwrap();

        let acc = s
            .transactions
            .accounts
            .create(creator, "flowwallet")
            .await
            .unwrap();
        assert_eq!(acc.created_by, creator, "the account's creator is Alice");
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let prod = seed_product(&s, "FLOW", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let customer = seed_customer(&s, "flow-walkin", None, None).await;
        let sale = draft_with_line(&s, customer.id, PaymentType::Cash, None, prod.id, "1").await;

        let _detail = s.confirm(operator, sale.id, Some(cash)).await.unwrap();
        let rows = s
            .transactions
            .transactions
            .list_by_account(acc.id)
            .await
            .unwrap();
        let income = rows.iter().find(|tx| tx.is_income()).unwrap();
        assert_eq!(
            income.created_by, operator,
            "the flow's actor, not a fresh one"
        );
        assert_ne!(
            income.created_by, acc.created_by,
            "distinct from the account's creator"
        );
    }

    /// AC18, the INVENTORY half of the sale flow: the stock movement a
    /// confirmed sale produces carries the CONFIRMING request's actor — the
    /// same argument that stamps the flow's finance rows — never a fresh one,
    /// and it stays distinct from the product's own creator.
    #[tokio::test]
    async fn ac18_the_sale_flow_movement_carries_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "stock-alice", "Alice")
            .await
            .unwrap();
        let operator = test_support::seed_audit_user(&pool, "stock-bob", "Bob")
            .await
            .unwrap();

        let acc = s
            .transactions
            .accounts
            .create(creator, "stockwallet")
            .await
            .unwrap();
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let prod = seed_product(&s, "STOCKFLOW", "10").await;
        seed_stock(&s, prod.id, "10").await;
        assert_ne!(
            prod.created_by, operator,
            "the two actors are distinguishable"
        );
        let customer = seed_customer(&s, "stock-walkin", None, None).await;
        let sale = draft_with_line(&s, customer.id, PaymentType::Cash, None, prod.id, "1").await;

        let _detail = s.confirm(operator, sale.id, Some(cash)).await.unwrap();
        let moves = s
            .inventory
            .movements
            .list_by_product(prod.id)
            .await
            .unwrap();
        let sale_move = moves
            .iter()
            .find(|m| m.reason == MovementReason::Sale)
            .unwrap_or_else(|| panic!("the sale confirm produced its own movement"));
        assert_eq!(
            sale_move.created_by, operator,
            "the flow's actor, not a fresh one"
        );
        assert_ne!(
            sale_move.created_by, prod.created_by,
            "distinct from the product's creator"
        );
        assert_eq!(
            sale_move.updated_by, None,
            "an append-only movement has no editor"
        );
    }

    /// AC18 (sales audit, M5 Phase B slice S11): the sale records TWO different
    /// actors — the draft's creator and, after a header edit and the confirm,
    /// the last editor — and the payment row it creates carries the acting
    /// user of the request that recorded it, never a fresh one. A cancel
    /// re-attributes the payment's `updated_by` to the cancelling request.
    #[tokio::test]
    async fn ac18_the_sale_records_two_different_actors_and_its_payment_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "sale-alice", "Alice")
            .await
            .unwrap();
        let editor = test_support::seed_audit_user(&pool, "sale-bob", "Bob")
            .await
            .unwrap();

        let prod = seed_product(&s, "SALE-AUD", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "sale-audit-wallet").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let customer = seed_customer(&s, "sale-audit-customer", None, None).await;

        // Alice creates the draft: the row names her and no editor yet.
        let sale = s
            .create_draft(
                creator,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(sale.created_by, creator, "the draft's creator");
        assert_eq!(sale.updated_by, None, "a fresh draft has no editor");
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();

        // Bob edits the header: the same document now names its last editor,
        // and the creator is untouched.
        let edited = s
            .update_draft(
                sale.id,
                editor,
                UpdateSaleDraft {
                    notes: Some("edited".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.created_by, creator);
        assert_eq!(edited.updated_by, Some(editor));

        // Bob confirms: the same edit tier, so updated_by stays Bob.
        let confirmed = s.confirm(editor, sale.id, None).await.unwrap();
        assert_eq!(confirmed.sale.created_by, creator);
        assert_eq!(confirmed.sale.updated_by, Some(editor));

        // Alice records a payment: the payment row (the S9/S10 twin of the
        // finance/inventory flow tests) carries the recording request's actor,
        // not the sale's creator and not a fresh one.
        let payment = s
            .record_payment(creator, sale.id, cash, dec("10"), sale_date())
            .await
            .unwrap();
        assert_eq!(payment.created_by, creator, "the flow's actor");
        assert_ne!(
            payment.created_by, editor,
            "distinct from the confirming user"
        );
        assert_eq!(payment.updated_by, None, "a fresh payment has no editor");
        let stored = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(stored[0].created_by, creator, "the stored row keeps it");

        // Alice cancels: the refund links the payment rows carry HER actor in
        // updated_by, like the refund Expense she caused.
        let cancelled = s
            .cancel(creator, sale.id, Some("audit".into()))
            .await
            .unwrap();
        assert_eq!(cancelled.sale.created_by, creator);
        assert_eq!(cancelled.sale.updated_by, Some(creator));
        let payments = s.sales.list_payments(sale.id).await.unwrap();
        assert_eq!(
            payments[0].updated_by,
            Some(creator),
            "the refund link names its writer"
        );
        assert_eq!(payments[0].created_by, creator, "the creator never changes");
    }

    // -----------------------------------------------------------------------
    // A LINE WRITE IS AN EDIT OF THE DOCUMENT
    //
    // A `sale_lines` row has no `created_by`/`updated_by` of its own: it
    // inherits its parent's actor, by design. What the parent owes the operator
    // is therefore that the DOCUMENT names whoever just edited it. The purchase
    // family, both return families and the header/confirm/cancel tiers on this
    // family all already do; these are the line tiers this family did not, and
    // the record page's audit line (`templates/partials/sale_detail.html:124`)
    // claims otherwise in a comment.
    // -----------------------------------------------------------------------

    /// Two distinct users, so an assertion about the editor cannot pass by
    /// accident: `creator` opens the draft, `editor` changes its lines. The
    /// sentinel `audit_actor` is deliberately NOT used here — a stamp that
    /// always equals the system actor would satisfy every assertion below.
    async fn two_actors(pool: &sqlx::SqlitePool) -> (i64, i64) {
        let creator = test_support::seed_audit_user(pool, "sale-line-alice", "Alice")
            .await
            .unwrap();
        let editor = test_support::seed_audit_user(pool, "sale-line-bob", "Bob")
            .await
            .unwrap();
        assert_ne!(creator, editor, "the two actors must be different users");
        (creator, editor)
    }

    /// A credit draft owned by `creator` with one line, so a line-tier test
    /// starts from a document that HAS an editor-free history.
    async fn line_tier_draft(
        s: &Svc,
        creator: i64,
        prod: &crate::models::Product,
    ) -> crate::models::Sale {
        let customer = seed_customer(s, "sale-line-customer", None, None).await;
        let sale = s
            .create_draft(
                creator,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: sale_date(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(creator, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        sale
    }

    /// The STORED `updated_by`, read through the repository rather than a
    /// service projection, so a test cannot pass on a derived value.
    async fn stored_editor(s: &Svc, sale_id: i64) -> Option<i64> {
        s.sales
            .find_sale(sale_id)
            .await
            .unwrap()
            .expect("the draft is still there")
            .updated_by
    }

    #[tokio::test]
    async fn add_line_stamps_the_drafts_updated_by_with_the_calling_actor() {
        let (s, pool) = svc().await;
        let (creator, editor) = two_actors(&pool).await;
        let prod = seed_product(&s, "SALE-LINE-ADD", "10").await;
        let sale = line_tier_draft(&s, creator, &prod).await;

        // The opening line was the creator's, so the draft already names her.
        assert_eq!(
            stored_editor(&s, sale.id).await,
            Some(creator),
            "the creating line is an edit too"
        );

        s.add_line(editor, sale.id, prod.id, dec("1"), None)
            .await
            .unwrap();

        assert_eq!(
            stored_editor(&s, sale.id).await,
            Some(editor),
            "a line write is an edit of the document, so the draft names the \
             operator who made it"
        );
        let stored = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(
            stored.created_by, creator,
            "a line edit must never rewrite the document's creator"
        );
    }

    #[tokio::test]
    async fn update_line_stamps_the_drafts_updated_by_with_the_calling_actor() {
        let (s, pool) = svc().await;
        let (creator, editor) = two_actors(&pool).await;
        let prod = seed_product(&s, "SALE-LINE-UPD", "10").await;
        let sale = line_tier_draft(&s, creator, &prod).await;
        let line_id = s.sales.list_lines(sale.id).await.unwrap()[0].id;

        s.update_line(editor, line_id, dec("5"), dec("12"))
            .await
            .unwrap();

        assert_eq!(
            stored_editor(&s, sale.id).await,
            Some(editor),
            "an inline quantity/price edit names its editor"
        );
        let stored = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(stored.created_by, creator, "the creator is never rewritten");
    }

    #[tokio::test]
    async fn remove_line_stamps_the_drafts_updated_by_with_the_calling_actor() {
        let (s, pool) = svc().await;
        let (creator, editor) = two_actors(&pool).await;
        let prod = seed_product(&s, "SALE-LINE-DEL", "10").await;
        let sale = line_tier_draft(&s, creator, &prod).await;
        let line_id = s.sales.list_lines(sale.id).await.unwrap()[0].id;

        s.remove_line(editor, line_id).await.unwrap();

        assert_eq!(
            stored_editor(&s, sale.id).await,
            Some(editor),
            "removing a line is an edit of the document too"
        );
        let stored = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(stored.created_by, creator, "the creator is never rewritten");
    }

    /// The one that is easy to get wrong. A REFUSED line write must leave the
    /// document's editor exactly as it was: stamping first would make a sale
    /// claim an edit that never happened, and the audit line is the last thing
    /// in this interface that should be wrong.
    #[tokio::test]
    async fn a_refused_line_write_leaves_the_drafts_updated_by_untouched() {
        let (s, pool) = svc().await;
        let (creator, editor) = two_actors(&pool).await;
        let prod = seed_product(&s, "SALE-LINE-REFUSE", "10").await;
        let sale = line_tier_draft(&s, creator, &prod).await;
        let line_id = s.sales.list_lines(sale.id).await.unwrap()[0].id;
        let before = stored_editor(&s, sale.id).await;
        assert_eq!(before, Some(creator), "the baseline editor is the creator");

        // Zero and negative quantities, and a negative price: all refused by the
        // service's own guards, before any statement runs.
        for (name, result) in [
            (
                "zero qty",
                s.update_line(editor, line_id, dec("0"), dec("10")).await,
            ),
            (
                "negative qty",
                s.update_line(editor, line_id, dec("-1"), dec("10")).await,
            ),
            (
                "negative price",
                s.update_line(editor, line_id, dec("1"), dec("-1")).await,
            ),
            (
                "zero qty on add",
                s.add_line(editor, sale.id, prod.id, dec("0"), None).await,
            ),
            (
                "unknown product",
                s.add_line(editor, sale.id, 999_999, dec("1"), None).await,
            ),
        ] {
            assert!(result.is_err(), "{name} must be refused");
            assert_eq!(
                stored_editor(&s, sale.id).await,
                before,
                "a refused {name} must not stamp the document"
            );
        }

        // And the removal: a line that does not exist is refused, and so is
        // nothing — the draft must still name the creator, not the editor.
        assert!(s.remove_line(editor, 999_999).await.is_err());
        assert_eq!(
            stored_editor(&s, sale.id).await,
            before,
            "a refused removal must not stamp the document"
        );
    }

    /// The state guard is a refusal like any other: a Confirmed sale refuses
    /// every line write, and refusing must not rewrite its editor. Without this
    /// the guard would be the one hole in the "stamp only after a real write"
    /// rule, because it is the guard — not the statement — that turns a
    /// non-Draft attempt into an error.
    #[tokio::test]
    async fn a_line_write_refused_because_the_sale_is_not_a_draft_leaves_the_editor_untouched() {
        let (s, pool) = svc().await;
        let (creator, editor) = two_actors(&pool).await;
        let prod = seed_product(&s, "SALE-LINE-CONF", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sale = line_tier_draft(&s, creator, &prod).await;
        let line_id = s.sales.list_lines(sale.id).await.unwrap()[0].id;

        // Bob edits the header, so the baseline editor is BOB and not the
        // creator — otherwise a stamp of `creator` would look like "no change".
        s.update_draft(
            sale.id,
            editor,
            UpdateSaleDraft {
                notes: Some("bob was here".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        s.confirm(editor, sale.id, None).await.unwrap();
        let before = stored_editor(&s, sale.id).await;
        assert_eq!(
            before,
            Some(editor),
            "the baseline editor is the last writer"
        );

        assert!(s
            .add_line(editor, sale.id, prod.id, dec("1"), None)
            .await
            .is_err());
        assert!(s
            .update_line(editor, line_id, dec("3"), dec("10"))
            .await
            .is_err());
        assert!(s.remove_line(editor, line_id).await.is_err());

        assert_eq!(
            stored_editor(&s, sale.id).await,
            before,
            "a non-Draft sale refuses every line write, and a refusal is not an edit"
        );
        assert_eq!(
            s.sales
                .find_sale(sale.id)
                .await
                .unwrap()
                .unwrap()
                .created_by,
            creator,
            "the creator survives a refused line write"
        );
    }

    /// `find_payment` is the read-by-id the documents drawer uses: found
    /// returns the stored payment, absent is the standard `NotFound` error.
    #[tokio::test]
    async fn find_payment_returns_the_stored_row_or_not_found() {
        let (s, _pool) = svc().await;
        let cash = cash_method(&s).await;
        let wallet = seed_account(&s, "findpay wallet").await;
        allow(&s, wallet.id, cash).await;
        let prod = seed_product(&s, "FINDPAY", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let sale = draft_with_line(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            Some(sale_date()),
            prod.id,
            "2",
        )
        .await;
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let recorded = s
            .record_payment(audit_actor(&s).await, sale.id, cash, dec("5"), sale_date())
            .await
            .unwrap();

        let found = s.find_payment(recorded.id).await.unwrap();
        assert_eq!(found.id, recorded.id);
        assert_eq!(found.sale_id, sale.id);
        assert_eq!(found.amount, dec("5"));

        let missing = s.find_payment(999_999).await;
        assert!(
            matches!(&missing, Err(AppError::NotFound(msg)) if msg.contains("payment")),
            "an unknown payment must be NotFound naming the family: {missing:?}"
        );
    }

    // -- delete_draft (the documents drawer's draft delete) --------------------

    async fn draft_with_one_tracked_line(s: &Svc, sku: &str) -> crate::models::Sale {
        draft_with_line_typed(s, sku, PaymentType::Cash).await
    }

    /// The confirmed-refusal test uses CREDIT: a cash confirm demands the
    /// method's account, and the refusal under test is about STATE, not about
    /// payment setup.
    async fn draft_with_line_typed(
        s: &Svc,
        sku: &str,
        payment_type: PaymentType,
    ) -> crate::models::Sale {
        let actor = audit_actor(s).await;
        let product = seed_product(s, sku, "10").await;
        let sale = s
            .create_draft(
                actor,
                NewSale {
                    // Credit cannot go to the walk-in: the seeded credit
                    // customer takes those.
                    customer_id: match payment_type {
                        PaymentType::Credit => CREDIT_CUSTOMER_ID,
                        PaymentType::Cash => WALKIN_ID,
                    },
                    payment_type,
                    sale_date: sale_date(),
                    // Credit requires a due date; Cash requires none.
                    due_date: match payment_type {
                        PaymentType::Credit => Some(sale_date()),
                        PaymentType::Cash => None,
                    },
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.add_line(audit_actor(&s).await, sale.id, product.id, dec("2"), None)
            .await
            .unwrap();
        sale
    }

    /// A draft is the one deletable state: the row and its lines go, and a
    /// later read is the standard NotFound.
    #[tokio::test]
    async fn delete_draft_removes_a_draft_and_get_detail_then_404s() {
        let (s, _pool) = svc().await;
        let sale = draft_with_one_tracked_line(&s, "DEL-S").await;

        s.delete_draft(sale.id).await.unwrap();
        let err = s.get_detail(sale.id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::NotFound(msg) if msg.contains("sale")),
            "the deleted draft must be NotFound naming the family: {err:?}"
        );
    }

    /// The service refuses a confirmed document NAMING the state; the SQL
    /// backstop is proven separately at the repository level.
    #[tokio::test]
    async fn delete_draft_refuses_a_confirmed_sale_with_a_validation_naming_the_state() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let sale = draft_with_line_typed(&s, "DEL-C", PaymentType::Credit).await;
        // A credit confirm takes NO method (the payment comes later).
        s.confirm(actor, sale.id, None).await.unwrap();

        let err = s.delete_draft(sale.id).await.unwrap_err();
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
        assert!(s.get_detail(sale.id).await.is_ok());
    }

    /// A discarded sale (cancelled while still Draft: `sale_number` stays
    /// NULL) posted nothing, so it IS deletable: the row and its lines go and
    /// a later read is the standard NotFound.
    #[tokio::test]
    async fn delete_draft_removes_a_discarded_cancelled_sale_and_get_detail_then_404s() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let sale = draft_with_one_tracked_line(&s, "DEL-X").await;
        s.cancel(actor, sale.id, None).await.unwrap();

        s.delete_draft(sale.id).await.unwrap();
        let err = s.get_detail(sale.id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::NotFound(msg) if msg.contains("sale")),
            "the deleted discarded sale must be NotFound naming the family: {err:?}"
        );
    }

    /// Confirmed-then-cancelled: the number proves it was confirmed, so the
    /// refusal is a Validation NAMING the state (never silent) and the row
    /// survives. The SQL backstop is proven separately at the repository
    /// level.
    #[tokio::test]
    async fn delete_draft_refuses_a_confirmed_then_cancelled_sale_naming_the_state() {
        let (s, _pool) = svc().await;
        let actor = audit_actor(&s).await;
        let sale = draft_with_line_typed(&s, "DEL-Y", PaymentType::Credit).await;
        s.confirm(actor, sale.id, None).await.unwrap();
        s.cancel(actor, sale.id, Some("wrong order".to_string()))
            .await
            .unwrap();

        let err = s.delete_draft(sale.id).await.unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Cancelled"),
                    "the refusal must name the state: {msg}"
                );
                assert!(
                    msg.contains("draft") || msg.contains("discarded"),
                    "the refusal must say only a draft or a discarded cancelled sale can be deleted: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(s.get_detail(sale.id).await.is_ok());
    }

    #[tokio::test]
    async fn delete_draft_of_an_unknown_sale_is_not_found() {
        let (s, _pool) = svc().await;
        let err = s.delete_draft(999_999).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    }

    // -- price snapshot immutability (product-markup) -------------------------

    /// The "history is unaffected" claim, pinned: `sale_lines.unit_price`
    /// snapshots the product's price when the line is built instead of
    /// referencing the product, so re-deriving the product's price never
    /// moves a stored document. The product half of the assertions is what
    /// keeps the test honest — if both prices stayed put, a pass would also
    /// mean the derivation had silently stopped working.
    #[tokio::test]
    async fn a_sale_line_keeps_the_price_it_snapshotted_when_the_products_markup_moves() {
        let (s, _pool) = svc().await;
        // cost 5 with a 100% markup derives 10; the line must snapshot exactly
        // that. The manual 999 sale_price is ignored while markup is set.
        let prod = s
            .inventory
            .create_product(
                audit_actor(&s).await,
                NewProduct {
                    sku: "SNAP-1".into(),
                    name: "snap prod".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec("999"),
                    cost_price: dec("5"),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: Some(dec("100")),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            prod.sale_price,
            dec("10"),
            "the derived price must be live before the line is built"
        );

        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: WALKIN_ID,
                    payment_type: PaymentType::Cash,
                    sale_date: sale_date(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        // No explicit unit_price: the line takes the product's derived price.
        s.add_line(audit_actor(&s).await, sale.id, prod.id, dec("2"), None)
            .await
            .unwrap();
        let before = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            before.lines[0].unit_price,
            dec("10"),
            "the line must snapshot the derived price it was built with"
        );

        // Move the derived price: markup 200% on the same cost derives 15.
        let moved = s
            .inventory
            .update_product(
                audit_actor(&s).await,
                prod.id,
                UpdateProduct {
                    markup_pct: Some(Some(dec("200"))),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            moved.sale_price,
            dec("15"),
            "the product's price must move, or the snapshot assertion is vacuous"
        );

        let after = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            after.lines[0].unit_price, before.lines[0].unit_price,
            "the stored line's snapshot price must not move with the product"
        );
        assert_eq!(
            after.lines[0].unit_price,
            dec("10"),
            "the snapshot stays exactly the price at line-build time"
        );
    }

    // -----------------------------------------------------------------------
    // T2: tax-inclusive document money.
    //
    // T1 already persists each line's `tax_total` and its frozen breakdown.
    // What these tests pin is that every DERIVED read CONSUMES it: the net
    // price never moves, so "the totals are unchanged without taxes" is a claim
    // about the tax, not about rounding drift.
    // -----------------------------------------------------------------------

    /// Create a tax and link it to `product_id` through the same repositories the
    /// product drawer drives, so the resolution a line write performs is one an
    /// operator can actually produce. `rate` is a percentage string.
    async fn link_tax(pool: &sqlx::SqlitePool, code: &str, rate: &str, product_id: i64) -> i64 {
        let taxes = crate::repositories::SqliteTaxRepository::new(pool.clone());
        let links = crate::repositories::SqliteProductTaxRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(pool).await.unwrap();
        let tax = taxes
            .create(
                actor,
                &crate::models::NewTax {
                    code: code.into(),
                    name: format!("tax {code}"),
                    rate: dec(rate),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        links.link(actor, product_id, tax.id).await.unwrap();
        tax.id
    }

    /// An empty Draft sale, so a test can link its taxes BEFORE the line write
    /// that has to resolve them.
    async fn draft_sale(s: &Svc, customer_id: i64, payment: PaymentType) -> crate::models::Sale {
        s.create_draft(
            audit_actor(s).await,
            NewSale {
                customer_id,
                payment_type: payment,
                sale_date: sale_date(),
                due_date: if payment == PaymentType::Credit {
                    Some(sale_date())
                } else {
                    None
                },
                receipt_no: None,
                notes: None,
            },
        )
        .await
        .unwrap()
    }

    /// A tracked product at `price` with stock, so a line can be written at all.
    async fn stockable_product(s: &Svc, sku: &str, price: &str) -> crate::models::Product {
        let product = seed_product(s, sku, price).await;
        seed_stock(s, product.id, "100").await;
        product
    }

    /// No linked tax must leave every derived money field exactly as it was
    /// before the feature: a zero tax total, and a total that is the net rounded
    /// to cents and nothing else.
    #[tokio::test]
    async fn tax_totals_without_taxes_keep_the_net_total() {
        let (s, _pool) = svc().await;
        let product = stockable_product(&s, "TAX-NONE", "12.345").await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("2"),
            Some(dec("12.345")),
        )
        .await
        .unwrap();

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(detail.net_subtotal, dec("24.69"), "2 x 12.345 is 24.69 net");
        assert_eq!(detail.tax_total, dec("0"));
        assert_eq!(detail.total, dec("24.69"));
        assert_eq!(detail.due, dec("24.69"));
    }

    /// One linked tax: the contribution is derived from the NET subtotal and
    /// added to it, so the document total is the tax-inclusive one.
    #[tokio::test]
    async fn tax_totals_with_one_tax_add_the_contribution() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-ONE", "50").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("2"),
            Some(dec("50")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, Some(cash_method(&s).await))
            .await
            .unwrap();

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(detail.net_subtotal, dec("100"));
        assert_eq!(detail.tax_total, dec("21"), "21% of the 100 net");
        assert_eq!(detail.total, dec("121"));
        assert_eq!(detail.due, Decimal::ZERO, "the cash delivery fully allocates the total");
    }

    /// Taxes are ADDITIVE: two linked rates on the same net add up, they never
    /// compound. 21% + 10% on 100 is 31, not 132.10.
    #[tokio::test]
    async fn tax_totals_with_two_taxes_are_additive_not_compounded() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-TWO", "100").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        link_tax(&pool, "IIBB10", "10", product.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, Some(cash_method(&s).await))
            .await
            .unwrap();

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(detail.net_subtotal, dec("100"));
        assert_eq!(
            detail.tax_total,
            dec("31"),
            "21 + 10 percent of the same net"
        );
        assert_eq!(
            detail.total,
            dec("131"),
            "additive, so the compounded 132.10 would be wrong"
        );
    }

    /// The half-up rule belongs to the LINE, and the document total is the sum of
    /// the line totals the record page shows. Two lines of 10.005 net each round
    /// to 12.11 with 21% IVA, so the document is 24.22 — while rounding the
    /// summed parts once would say 24.21. The two answers are not the same, and
    /// only the first reconciles with what the operator reads.
    #[tokio::test]
    async fn tax_totals_round_each_line_half_up_and_sum_the_line_totals() {
        let (s, pool) = svc().await;
        let first = stockable_product(&s, "TAX-ROUND-A", "10.005").await;
        let second = stockable_product(&s, "TAX-ROUND-B", "10.005").await;
        link_tax(&pool, "IVA21", "21", first.id).await;
        link_tax(&pool, "IVA21B", "21", second.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            first.id,
            dec("1"),
            Some(dec("10.005")),
        )
        .await
        .unwrap();
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            second.id,
            dec("1"),
            Some(dec("10.005")),
        )
        .await
        .unwrap();

        let record = s.get_record(sale.id).await.unwrap();
        assert_eq!(record.lines.len(), 2);
        assert_eq!(record.lines[0].total, dec("12.11"), "10.005 + 2.10");
        assert_eq!(record.lines[1].total, dec("12.11"));
        let money = record.money.expect("an ordinary document totals");
        assert_eq!(money.net_subtotal, dec("20.01"));
        assert_eq!(money.tax_total, dec("4.20"));
        assert_eq!(
            money.total,
            dec("24.22"),
            "the sum of the two pinned line totals, not round(20.01 + 4.20)"
        );
        let sum: Decimal = record.lines.iter().map(|l| l.total).sum();
        assert_eq!(
            sum, money.total,
            "the shown lines must add up to the shown total"
        );
    }

    /// The payment ceiling is the tax-inclusive total: the exact total is a legal
    /// payment and one cent more is the overpayment the service refuses.
    #[tokio::test]
    async fn tax_totals_bound_the_payment_at_the_tax_inclusive_total() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-LIMIT", "100").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        let sale = draft_sale(&s, CREDIT_CUSTOMER_ID, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(detail.total, dec("121"), "net 100 plus 21% IVA");
        assert_eq!(detail.due, dec("121"));

        let account = seed_account(&s, "Tax limit account").await;
        let method = own_method(&s, account.id, "Transfer").await;

        s.record_payment(
            audit_actor(&s).await,
            sale.id,
            method,
            dec("121"),
            sale_date(),
        )
        .await
        .expect("the exact tax-inclusive total is a legal payment");

        let over = s
            .record_payment(
                audit_actor(&s).await,
                sale.id,
                method,
                dec("0.01"),
                sale_date(),
            )
            .await
            .unwrap_err();
        assert!(
            over.to_string().contains("exceeds total"),
            "the ceiling must be the tax-inclusive total, got: {over}"
        );

        let settled = s.get_detail(sale.id).await.unwrap();
        assert_eq!(settled.paid, dec("121"));
        assert_eq!(settled.due, dec("0"));
    }

    /// Customer debt is the `due` of the receivable, so it inherits the
    /// tax-inclusive total without needing a second money rule of its own.
    #[tokio::test]
    async fn tax_totals_carry_the_tax_into_the_customer_debt() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-DEBT", "100").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        let customer = seed_customer(&s, "Taxed Debtor", None, Some(30)).await;
        let sale = draft_sale(&s, customer.id, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();

        let debts = s.customer_debt_sales(customer.id).await.unwrap();
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].money.unwrap().total, dec("121"));
        assert_eq!(debts[0].money.unwrap().due, dec("121"), "nothing was paid");

        let summary = s.debt_summary(5).await.unwrap();
        assert_eq!(summary.total.amount, Some(dec("121")));
    }

    /// The credit-limit projection is measured against the tax-inclusive total:
    /// a customer whose limit is exactly the gross total may still buy, and one
    /// cent short of it may not.
    #[tokio::test]
    async fn tax_totals_project_the_tax_into_the_credit_limit_check() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-CREDIT", "100").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        // A limit of exactly the tax-inclusive total: the projected debt only
        // fits if the check measures the gross, not the net.
        let customer = seed_customer(&s, "Tight Limit", Some("121"), Some(30)).await;
        let sale = draft_sale(&s, customer.id, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .expect("projected debt 121 is exactly the limit");

        // The same customer now has 121 of debt, so a second gross-total sale
        // projects to 242 and must be refused.
        let second_product = stockable_product(&s, "TAX-CREDIT-2", "100").await;
        link_tax(&pool, "IVA21B", "21", second_product.id).await;
        let second = draft_sale(&s, customer.id, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            second.id,
            second_product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        let refused = s
            .confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("credit limit exceeded"),
            "the limit must be measured against tax-inclusive money, got: {refused}"
        );
    }

    /// A CONFIRMED sale is frozen: re-rating, renaming and deactivating the tax
    /// afterwards must not move a cent of its total, paid, due or breakdown.
    #[tokio::test]
    async fn tax_totals_freeze_a_confirmed_sale_against_later_tax_edits() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-FROZEN", "100").await;
        let tax_id = link_tax(&pool, "IVA21", "21", product.id).await;
        let sale = draft_sale(&s, CREDIT_CUSTOMER_ID, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        let before = s.get_record(sale.id).await.unwrap();
        let before_money = before.money.expect("an ordinary document totals");
        assert_eq!(before_money.total, dec("121"));
        assert_eq!(before.lines[0].taxes[0].code, "IVA21");

        let taxes = crate::repositories::SqliteTaxRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        taxes
            .update(actor, tax_id, "IVA5", "Renamed IVA", dec("5"), true)
            .await
            .unwrap();
        taxes.deactivate(actor, tax_id).await.unwrap();

        let after = s.get_record(sale.id).await.unwrap();
        let after_money = after.money.expect("an ordinary document totals");
        assert_eq!(
            after_money.total, before_money.total,
            "a confirmed total is frozen"
        );
        assert_eq!(after_money.net_subtotal, before_money.net_subtotal);
        assert_eq!(after_money.tax_total, before_money.tax_total);
        assert_eq!(after_money.due, before_money.due);
        assert_eq!(after.lines[0].total, before.lines[0].total);
        assert_eq!(after.lines[0].taxes.len(), 1);
        assert_eq!(
            after.lines[0].taxes[0].code, "IVA21",
            "the snapshot code is frozen"
        );
        assert_eq!(
            after.lines[0].taxes[0].name, "tax IVA21",
            "the snapshot name is frozen"
        );
        assert_eq!(
            after.lines[0].taxes[0].rate,
            dec("21"),
            "the snapshot rate is frozen"
        );
        assert_eq!(after.lines[0].taxes[0].amount, dec("21"));
    }

    /// A DRAFT line is not history: the T1 repository contract re-resolves and
    /// recomputes it, so a re-rated tax shows up on the next line write.
    #[tokio::test]
    async fn tax_totals_recompute_a_draft_after_the_tax_is_re_rated() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-REDRAFT", "100").await;
        let tax_id = link_tax(&pool, "IVA21", "21", product.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        let line = s
            .add_line(
                audit_actor(&s).await,
                sale.id,
                product.id,
                dec("1"),
                Some(dec("100")),
            )
            .await
            .unwrap();
        assert_eq!(s.get_detail(sale.id).await.unwrap().total, dec("121"));

        let taxes = crate::repositories::SqliteTaxRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        taxes
            .update(actor, tax_id, "IVA21", "tax IVA21", dec("10"), true)
            .await
            .unwrap();

        s.update_line(audit_actor(&s).await, line.id, dec("1"), dec("100"))
            .await
            .unwrap();

        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            detail.tax_total,
            dec("10"),
            "the draft re-resolved the new rate"
        );
        assert_eq!(detail.total, dec("110"));
    }

    /// A deactivated tax stops applying to a DRAFT, and the breakdown follows.
    #[tokio::test]
    async fn tax_totals_drop_a_deactivated_tax_from_a_draft_recomputation() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-DEACT", "100").await;
        let tax_id = link_tax(&pool, "IVA21", "21", product.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        let line = s
            .add_line(
                audit_actor(&s).await,
                sale.id,
                product.id,
                dec("1"),
                Some(dec("100")),
            )
            .await
            .unwrap();
        assert_eq!(s.get_detail(sale.id).await.unwrap().total, dec("121"));

        let taxes = crate::repositories::SqliteTaxRepository::new(pool.clone());
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        taxes.deactivate(actor, tax_id).await.unwrap();
        s.update_line(audit_actor(&s).await, line.id, dec("1"), dec("100"))
            .await
            .unwrap();

        let record = s.get_record(sale.id).await.unwrap();
        let money = record.money.expect("an ordinary document totals");
        assert_eq!(money.tax_total, dec("0"));
        assert_eq!(money.total, dec("100"));
        assert!(record.lines[0].taxes.is_empty());
    }

    /// The record page is the audit surface: it must carry the frozen breakdown
    /// and the three money figures, not just one opaque total.
    #[tokio::test]
    async fn tax_totals_expose_the_breakdown_on_the_sale_record() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "TAX-BREAKDOWN", "100").await;
        link_tax(&pool, "IVA21", "21", product.id).await;
        link_tax(&pool, "IIBB10", "10", product.id).await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("100")),
        )
        .await
        .unwrap();

        let record = s.get_record(sale.id).await.unwrap();
        let money = record.money.expect("an ordinary document totals");
        assert_eq!(money.net_subtotal, dec("100"));
        assert_eq!(money.tax_total, dec("31"));
        assert_eq!(money.total, dec("131"));
        let line = &record.lines[0];
        assert_eq!(
            line.subtotal,
            dec("100"),
            "the stored net price is untouched"
        );
        assert_eq!(line.tax_total, dec("31"));
        assert_eq!(line.total, dec("131"));
        assert_eq!(line.taxes.len(), 2);
        assert_eq!(
            line.taxes[0].code, "IIBB10",
            "the breakdown is ordered by code, so the two rows are deterministic"
        );
        assert_eq!(line.taxes[0].rate, dec("10"));
        assert_eq!(line.taxes[0].amount, dec("10"));
        assert_eq!(line.taxes[1].code, "IVA21");
        assert_eq!(line.taxes[1].rate, dec("21"));
        assert_eq!(line.taxes[1].amount, dec("21"));
        let breakdown: Decimal = line.taxes.iter().map(|t| t.amount).sum();
        assert_eq!(
            breakdown, line.tax_total,
            "the shown breakdown must reconcile with the shown tax total"
        );
    }

    /// A line with no linked tax shows an empty breakdown, never a fabricated
    /// zero row.
    #[tokio::test]
    async fn tax_totals_show_no_breakdown_for_a_product_without_taxes() {
        let (s, _pool) = svc().await;
        let product = stockable_product(&s, "TAX-NOBREAK", "10").await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("2"),
            Some(dec("10")),
        )
        .await
        .unwrap();

        let record = s.get_record(sale.id).await.unwrap();
        assert!(record.lines[0].taxes.is_empty());
        assert_eq!(record.lines[0].tax_total, dec("0"));
        assert_eq!(record.lines[0].total, dec("20"));
    }

    /// An over-collected party's available credit reduces the signed balance below
    /// zero while its document residual remains exactly settled.
    #[tokio::test]
    async fn customer_balance_subtracts_unapplied_credit_and_can_be_negative() {
        let (s, _) = svc().await;
        let product = seed_product(&s, "NEGATIVE-CREDIT", "10").await;
        seed_stock(&s, product.id, "10").await;
        let cash = cash_method(&s).await;
        let sale = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            sale_date(),
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            product.id,
            "3",
        )
        .await;
        s.confirm(audit_actor(&s).await, sale.id, None).await.unwrap();
        let actor = audit_actor(&s).await;
        s.record_payment(actor, sale.id, cash, dec("30"), sale_date())
            .await
            .unwrap();
        let method = s
            .payment_methods
            .find_method_by_name("Cash")
            .await
            .unwrap()
            .unwrap();
        let delivery = s
            .payments
            .create(&crate::models::NewPayment {
                number: "2024-PAY-999999".into(),
                direction: crate::models::PaymentDirection::In,
                party_type: crate::models::PartyType::Customer,
                party_id: CREDIT_CUSTOMER_ID,
                method_id: method.id,
                account_id: method.account_id,
                amount: dec("1"),
                date: sale_date(),
                notes: None,
                transaction_id: None,
                receipt_id: None,
                created_by: actor,
            })
            .await
            .unwrap();
        assert_eq!(
            s.payments.unapplied_for_payment(delivery.id).await.unwrap(),
            dec("1"),
            "the extra delivery is unapplied because the sale's residual is already zero"
        );
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            dec("-1"),
            "zero residual debt minus one unapplied delivery credit"
        );
        assert_eq!(
            s.payments
                .residual_for_document(crate::models::PartyDocumentKind::Sale, sale.id)
                .await
                .unwrap(),
            Decimal::ZERO,
            "the prior payment allocation settled the sale residual"
        );
    }

    /// Batch ageing buckets reflect allocation residuals, not the obsolete
    /// payment rows, and a linked customer return reduces that same residual.
    #[tokio::test]
    async fn ageing_all_buckets_the_residual_after_a_payment_and_credit_note() {
        let (s, _) = svc().await;
        let product = seed_product(&s, "RESIDUAL-AGEING", "10").await;
        seed_stock(&s, product.id, "10").await;
        let cash = cash_method(&s).await;
        let sale = draft_on(
            &s,
            CREDIT_CUSTOMER_ID,
            PaymentType::Credit,
            sale_date(),
            Some(NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()),
            product.id,
            "5",
        )
        .await;
        s.confirm(audit_actor(&s).await, sale.id, None).await.unwrap();
        s.record_payment(audit_actor(&s).await, sale.id, cash, dec("20"), sale_date())
            .await
            .unwrap();
        let returned: i64 = sqlx::query_scalar(
            "INSERT INTO customer_returns (customer_id, sale_id, status, return_date, created_by) VALUES (?, ?, 'Confirmed', '2024-05-02', ?) RETURNING id",
        )
        .bind(CREDIT_CUSTOMER_ID)
        .bind(sale.id)
        .bind(audit_actor(&s).await)
        .fetch_one(s.sales.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price) SELECT ?, id, '1', '10' FROM sale_lines WHERE sale_id = ? LIMIT 1",
        )
        .bind(returned)
        .bind(sale.id)
        .execute(s.sales.pool())
        .await
        .unwrap();
        s.party_ledger
            .insert(&crate::models::NewPartyLedgerEntry {
                party_type: crate::models::PartyType::Customer,
                party_id: CREDIT_CUSTOMER_ID,
                kind: crate::models::PartyEntryKind::Return,
                amount: crate::models::PartyEntryKind::Return.signed_amount(dec("10")),
                document_kind: crate::models::PartyDocumentKind::CustomerReturn,
                document_id: returned,
                entry_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                reference: None,
                created_by: audit_actor(&s).await,
            })
            .await
            .unwrap();

        let as_of = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap();
        let all = s.ageing_all(as_of).await.unwrap();
        let row = all.iter().find(|row| row.customer_id == CREDIT_CUSTOMER_ID).unwrap();
        assert_eq!(row.ageing.current, SetMoney::amount(dec("20")));
        assert_eq!(row.balance, SetMoney::amount(dec("20")));
        let per_customer = s.customer_ageing(CREDIT_CUSTOMER_ID, as_of).await.unwrap();
        assert_eq!(per_customer, row.ageing);
    }

    // -----------------------------------------------------------------------
    // Document-level accumulation (tax contract overflow T3), at the service
    // level. The route tests prove what the operator sees; these prove the
    // SHAPE of the answer, including the one invariant a type cannot state.
    // -----------------------------------------------------------------------

    /// `4e28`: an amount an operator can type into a price box, individually
    /// carryable (`Decimal::MAX ≈ 7.92e28`) and stored by the real checked
    /// write. Two of them are `8e28`, which the range does not hold.
    const FOUR_E28: &str = "40000000000000000000000000000";

    /// A document's money and its refusal are ONE fact: the record page renders
    /// a document whose total cannot be computed, so the two must never disagree
    /// — a record with a total AND a refusal would publish money and a warning
    /// about the same figure, and a record with neither would render an empty
    /// total with no explanation.
    #[tokio::test]
    async fn a_record_carries_either_its_money_or_its_refusal_never_both_and_never_neither() {
        let (s, _pool) = svc().await;
        let product = stockable_product(&s, "DOC-TOTAL-SHAPE", "10").await;
        let sale = draft_sale(&s, WALKIN_ID, PaymentType::Cash).await;

        // One line: the document totals, and says nothing about the rule.
        s.add_line(
            audit_actor(&s).await,
            sale.id,
            product.id,
            dec("1"),
            Some(dec("10")),
        )
        .await
        .unwrap();
        let readable = s.get_record(sale.id).await.unwrap();
        assert!(readable.money.is_some());
        assert!(readable.total_refusal.is_none());
        assert_eq!(readable.money.unwrap().total, dec("10"));

        // Two more lines that carry the sum out of range: the document is still
        // returned, with every figure absent and the rule named.
        for _ in 0..2 {
            s.add_line(
                audit_actor(&s).await,
                sale.id,
                product.id,
                dec("1"),
                Some(dec(FOUR_E28)),
            )
            .await
            .unwrap();
        }
        let refused = s.get_record(sale.id).await.unwrap();
        assert!(
            refused.money.is_none(),
            "no figure is published for a sum that does not exist"
        );
        assert_eq!(
            refused.total_refusal,
            Some(PriceRefusal::DocumentTotalTooLarge)
        );
        assert_eq!(
            refused.lines.len(),
            3,
            "and the LINES are all there: each of their own money is representable, and the page \
             is the operator's only way to reduce the document"
        );
        assert_eq!(refused.lines[1].total, dec(FOUR_E28));
    }

    /// The debt figures are a sum over a SET of documents, which is a level
    /// neither the per-line write bound nor the per-document fold can see: each
    /// document here totals exactly, and the customer's balance does not.
    ///
    /// `4e28` per confirmed credit sale: each sale's own total is carryable and
    /// the document is fully readable, and `4e28 + 4e28 = 8e28` is not — so the
    /// balance, the ageing and the statement refuse the same rule rather than
    /// reporting a debt the range cannot hold.
    #[tokio::test]
    async fn a_customer_balance_whose_documents_cannot_be_added_up_is_a_refusal_not_a_panic() {
        let (s, _pool) = svc().await;
        let product = stockable_product(&s, "DEBT-SET", "10").await;
        let as_of = sale_date();

        // The control, asserted in the same test: ONE such document is an
        // ordinary receivable, so a refusal above can only come from the SET.
        let single = draft_sale(&s, CREDIT_CUSTOMER_ID, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            single.id,
            product.id,
            dec("1"),
            Some(dec(FOUR_E28)),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, single.id, None)
            .await
            .unwrap();
        assert_eq!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await.unwrap(),
            dec(FOUR_E28),
            "one document at 4e28 is a receivable this application can state"
        );

        let second = draft_sale(&s, CREDIT_CUSTOMER_ID, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            second.id,
            product.id,
            dec("1"),
            Some(dec(FOUR_E28)),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, second.id, None)
            .await
            .unwrap();

        // Both documents are individually READABLE — that is what makes the sum
        // the only thing that can break.
        assert_eq!(s.get_detail(second.id).await.unwrap().total, dec(FOUR_E28));
        assert!(matches!(
            s.customer_balance(CREDIT_CUSTOMER_ID).await,
            Err(AppError::PriceRefused(PriceRefusal::DocumentTotalTooLarge))
        ));
        // The DISPLAYING reads answer with the refusal instead of an error, and
        // they answer it as an ABSENCE: no figure at all where the sum was.
        let ageing = s.customer_ageing(CREDIT_CUSTOMER_ID, as_of).await.unwrap();
        assert_eq!(ageing.refusal(), Some(PriceRefusal::DocumentTotalTooLarge));
        assert_eq!(
            ageing.total(),
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
            "a refused ageing has no figure at all, so no client can add its buckets back up \
             into a total this application cannot stand behind"
        );
        // The buckets answer INDIVIDUALLY, and the difference is the point: the
        // bucket that holds both documents refuses (its sum does not carry), and a
        // bucket that holds none is a real zero. Neither is the old shape, where a
        // refusal zeroed all four and a client could not tell a refused bucket from
        // a customer who owes nothing.
        let refused_buckets = ageing
            .buckets()
            .iter()
            .filter(|bucket| bucket.refusal.is_some())
            .count();
        assert_eq!(
            refused_buckets, 1,
            "exactly the bucket the overflow happened in: {ageing:?}"
        );
        for bucket in ageing.buckets().iter().filter(|b| b.refusal.is_none()) {
            assert_eq!(
                bucket.amount,
                Some(Decimal::ZERO),
                "and every other bucket is a figure, not a guess: {ageing:?}"
            );
        }

        let summary = s.debt_summary(5).await.unwrap();
        assert_eq!(
            summary.total,
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge)
        );
        assert_eq!(
            summary.count, 2,
            "both documents are still counted as owed: neither is proven paid"
        );
        assert_eq!(
            summary.oldest.len(),
            2,
            "and the panel still lists them, in place"
        );
        assert!(
            summary
                .oldest
                .iter()
                .all(|row| row.money.is_some() && row.total_refusal.is_none()),
            "and each of them keeps its OWN figure: these documents total individually, and it is \
             the set of them that does not. A row that lost its own money because a DIFFERENT \
             document broke the sum would be the partial figure this whole shape exists to avoid"
        );

        // The receivable list itself is per document, so it still answers: the
        // operator can see and act on the documents even while the SET refuses.
        assert_eq!(s.outstanding_debt().await.unwrap().len(), 2);

        // And the statement, which is a third sum over the same set: its balance
        // and its running balance are refusals, while each document's OWN entry
        // keeps its place in the ledger.
        let statement = s
            .customer_statement(CREDIT_CUSTOMER_ID, as_of)
            .await
            .unwrap();
        assert_eq!(
            statement.balance,
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge)
        );
        assert_eq!(statement.entries.len(), 2, "one entry per document");
        assert!(
            statement
                .entries
                .iter()
                .all(|entry| entry.debit.amount.is_some()),
            "each document is individually carryable, so its OWN debit is a real figure"
        );
        assert_eq!(
            statement.entries[0].balance.amount,
            Some(dec(FOUR_E28)),
            "and the running balance is real up to the entry that breaks the set"
        );
        let last = statement.entries.last().unwrap();
        assert_eq!(
            last.balance,
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
            "the running balance is refused from the entry that breaks the set onward, and never \
             recovers: a balance that resumed would be a figure this application cannot stand \
             behind"
        );
        // THE CONTROL on the same read, for another customer entirely: an
        // ordinary receivable states an ordinary balance, so the refusal above can
        // only be THIS customer's set and never the statement itself.
        let other = seed_customer(&s, "Statement Control", None, None).await;
        let control = draft_sale(&s, other.id, PaymentType::Credit).await;
        s.add_line(
            audit_actor(&s).await,
            control.id,
            product.id,
            dec("1"),
            Some(dec("2468")),
        )
        .await
        .unwrap();
        s.confirm(audit_actor(&s).await, control.id, None)
            .await
            .unwrap();
        let control_statement = s.customer_statement(other.id, as_of).await.unwrap();
        assert_eq!(control_statement.balance.amount, Some(dec("2468")));
        assert_eq!(control_statement.balance.refusal, None);
        assert_eq!(control_statement.ageing.refusal(), None);
        assert!(control_statement
            .entries
            .iter()
            .all(|entry| entry.balance.refusal.is_none()));
    }
    /// The other refusal shape on the same read: a document that cannot be totaled
    /// at all, whose `due` is therefore unknown. It cannot be attributed to a
    /// bucket — it could have been in any of the four — so the WHOLE grid refuses,
    /// which is a different answer from an overflow's and for a different reason.
    ///
    /// A confirmation refuses an un-totalable document by design, so the second
    /// line is stored straight through SQL, past the checked write: the state a
    /// confirmed document can be found in, and the one this branch exists for.
    #[tokio::test]
    async fn an_ageing_with_a_document_it_cannot_total_refuses_every_bucket() {
        let (s, pool) = svc().await;
        let product = stockable_product(&s, "XUNKNOWN", "10").await;
        let as_of = sale_date();
        let sale = s
            .create_draft(
                audit_actor(&s).await,
                NewSale {
                    customer_id: CREDIT_CUSTOMER_ID,
                    payment_type: PaymentType::Credit,
                    sale_date: as_of - chrono::Duration::days(20),
                    due_date: Some(as_of - chrono::Duration::days(5)),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let line = s
            .add_line(
                audit_actor(&s).await,
                sale.id,
                product.id,
                dec("1"),
                Some(dec(FOUR_E28)),
            )
            .await
            .unwrap();
        s.update_line(audit_actor(&s).await, line.id, dec("1"), dec(FOUR_E28))
            .await
            .unwrap();
        s.confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(sale.id)
        .bind(product.id)
        .bind(dec("1").to_string())
        .bind(FOUR_E28)
        .execute(&pool)
        .await
        .unwrap();

        let ageing = s.customer_ageing(CREDIT_CUSTOMER_ID, as_of).await.unwrap();
        assert_eq!(ageing.refusal(), Some(PriceRefusal::DocumentTotalTooLarge));
        for bucket in ageing.buckets() {
            assert_eq!(
                bucket,
                SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
                "a grid that kept three sums would be a total the operator adds up to a number \
                 nobody can stand behind: {ageing:?}"
            );
        }
        assert_eq!(
            ageing.total(),
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge)
        );
    }

    /// The cross-bucket half of the same argument, and the half the suite missed:
    /// the ageing's buckets are a PARTITION of one receivable, so a bound that
    /// holds inside each bucket says nothing about the sum ACROSS them. Two
    /// documents of `4e28` that land in DIFFERENT buckets each fit their own, and
    /// `draft_sale` — which gives both the same due date — put them in the SAME
    /// one, where the per-bucket check caught it. `as_of` is what splits them, so
    /// the two due dates are the whole construction.
    #[tokio::test]
    async fn an_ageing_whose_buckets_each_carry_refuses_their_sum() {
        let (s, _pool) = svc().await;
        let product = stockable_product(&s, "XCROSS", "10").await;
        let as_of = sale_date();
        for due in [
            as_of + chrono::Duration::days(10),
            as_of - chrono::Duration::days(10),
        ] {
            let sale = s
                .create_draft(
                    audit_actor(&s).await,
                    NewSale {
                        customer_id: CREDIT_CUSTOMER_ID,
                        payment_type: PaymentType::Credit,
                        sale_date: as_of - chrono::Duration::days(20),
                        due_date: Some(due),
                        receipt_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap();
            let line = s
                .add_line(
                    audit_actor(&s).await,
                    sale.id,
                    product.id,
                    dec("1"),
                    Some(dec(FOUR_E28)),
                )
                .await
                .unwrap();
            s.update_line(audit_actor(&s).await, line.id, dec("1"), dec(FOUR_E28))
                .await
                .unwrap();
            s.confirm(audit_actor(&s).await, sale.id, None)
                .await
                .unwrap();
        }

        let ageing = s.customer_ageing(CREDIT_CUSTOMER_ID, as_of).await.unwrap();
        // Both buckets carry on their own: `0 + 4e28` fits, twice.
        assert_eq!(ageing.current, SetMoney::amount(dec(FOUR_E28)));
        assert_eq!(ageing.overdue_1_30, SetMoney::amount(dec(FOUR_E28)));
        assert_eq!(
            ageing.refusal(),
            None,
            "no bucket refused, so the AGEING itself is not a refusal: {ageing:?}"
        );
        // The sum across the partition is `8e28`, and THAT is what refuses — the
        // raw `+` this replaces was a panic on the customer's own page.
        assert_eq!(
            ageing.total(),
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
            "a total over buckets that each carry is still a set sum"
        );
        assert_eq!(
            s.customer_statement(CREDIT_CUSTOMER_ID, as_of)
                .await
                .unwrap()
                .ageing
                .total(),
            SetMoney::refused(PriceRefusal::DocumentTotalTooLarge),
            "and the statement reads the same ageing the page does"
        );
    }

    // -- Duplicate confirm (T2) ------------------------------------------------

    /// A duplicate confirm SUBMISSION of the same document is refused, and the
    /// refusal writes nothing at all: no second stock movement, no second
    /// finance row, no second payment, and no second number burned.
    ///
    /// This is the end-to-end form of the guarantee. It reaches the service's
    /// own `status == Confirmed` guard at `confirm`'s first read, which is what
    /// refuses it here — the repository's own DRAFT predicate is the backstop
    /// underneath that, proven separately, because a read is not a lock.
    #[tokio::test]
    async fn duplicate_confirm_submission_is_refused_and_writes_nothing() {
        let (s, pool) = svc().await;
        let prod = seed_product(&s, "DUP-CONFIRM", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-dup").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, prod.id, "2").await;
        let actor = audit_actor(&s).await;

        let first = s.confirm(actor, sale.id, Some(cash)).await.unwrap();
        let movements = movement_count(&pool).await;
        let transactions = tx_count(&pool).await;
        let payments = payment_count(&pool).await;
        assert_eq!(
            movements, 2,
            "one seeding movement in, one Out by the confirm"
        );
        assert_eq!(transactions, 1, "a cash confirm posts one Income");
        assert_eq!(payments, 1, "a cash confirm posts one payment");
        assert_eq!(sale_sequence_last(&pool).await, Some(1));

        // The duplicate submission.
        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        match err {
            AppError::Validation(msg) => assert_eq!(
                msg, "sale already confirmed",
                "the refusal keeps its own wording"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        assert_eq!(
            movement_count(&pool).await,
            movements,
            "the refused duplicate must not deduct a second time"
        );
        assert_eq!(
            tx_count(&pool).await,
            transactions,
            "the refused duplicate must not post a second Income"
        );
        assert_eq!(
            payment_count(&pool).await,
            payments,
            "the refused duplicate must not post a second payment"
        );
        assert_eq!(
            sale_sequence_last(&pool).await,
            Some(1),
            "the refused duplicate must not burn a second number"
        );
        assert_eq!(
            first.sale.sale_number.as_deref(),
            Some("2024-SALE-000001"),
            "the one number the first confirm burned is the one that stands"
        );
    }

    /// The SAME duplicate submission, arriving the way a race delivers it: while
    /// this confirm is between its number and its movement, the document has
    /// already been confirmed underneath it. `confirm`'s opening read saw a
    /// Draft, so every guard above the final write passes — and only the
    /// repository's own `AND status = 'Draft'` can still refuse the second
    /// stamp.
    ///
    /// The trigger is the standing technique in this repo for putting a second
    /// writer inside another's statement (`tax_snapshot_tests.rs:747`), used
    /// here to make the interleaving deterministic instead of timed.
    ///
    /// WHAT THIS TEST NO LONGER SIMULATES, stated plainly because the name
    /// overclaims: a trigger runs on the writer's OWN connection, so with the
    /// confirm unit in place the racing UPDATE is a statement INSIDE that unit,
    /// not a second connection beside it. The refusal by the predicate is real
    /// and still proven. The claim that the racing stamp SURVIVES is not, and
    /// the rollback takes that stamp back too. A genuine second connection is
    /// what this test would need to prove the race, and SQLite's single-writer
    /// locking would then decide the outcome, not this code.
    ///
    /// Strict stock is ON (`svc_with_flags(false, false)`), so the refusal is
    /// proven under the production posture and not under a relaxed flag.
    #[tokio::test]
    async fn duplicate_confirm_that_reaches_the_final_write_is_refused_by_the_predicate() {
        let (s, pool) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "RACE-CONFIRM", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-race").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, prod.id, "2").await;
        let actor = audit_actor(&s).await;
        let prod_id = prod.id;
        let sale_id = sale.id;

        // Another writer confirms the document underneath this one, exactly
        // between this confirm's `next_number` and its first movement.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER racing_confirm BEFORE INSERT ON stock_movements \
             WHEN NEW.reason = 'Sale' AND NEW.product_id = {prod_id} \
             BEGIN UPDATE sales SET status = 'Confirmed', sale_number = '2024-SALE-000099', \
               confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = {sale_id}; END"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Confirmed"),
                    "the refusal must name the state the write saw: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // The racing stamp never became durable. It ran on this confirm's own
        // connection, so it is a statement INSIDE the unit rather than a second
        // writer, and the rollback took it back with the rest. Read straight
        // from the row, not through the service.
        let after = s.sales.find_sale(sale.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            crate::models::SaleStatus::Draft,
            "the document is a Draft again: the racing UPDATE was inside the unit"
        );
        assert_eq!(
            after.sale_number.as_deref(),
            None,
            "so neither the racing number nor this confirm's number was stamped"
        );

        // The refusal, and the shape it leaves behind. The trigger's own UPDATE runs
        // on this confirm's own connection, so it is part of the unit and the
        // rollback takes it back too: the simulated "other writer" was never a
        // separate writer, it was a statement inside this transaction. What
        // survives is nothing — not the work that ran before the predicate, and
        // not the racing stamp.
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the seeding movement in — the Out rolled back with the refusal"
        );
        assert_eq!(tx_count(&pool).await, 0, "the Income rolled back");
        assert_eq!(payment_count(&pool).await, 0, "the payment rolled back");
        assert_eq!(
            sale_sequence_last(&pool).await,
            None,
            "and the number is UNSPENT"
        );
        assert_eq!(
            row_state(&pool, sale.id).await,
            ("Draft".to_string(), None),
            "the document is a Draft with no number: the racing stamp was inside \
             the same unit, so it rolled back with everything else"
        );
    }

    // -- confirm failure windows (T3) ----------------------------------------
    //
    // `confirm` writes the sequence, the stock movements, the finance row, the
    // payment and the document inside ONE transaction. A failure at any point in
    // that run rolls the whole unit back, so the tests below assert the ABSENCE
    // of residue, window by window: every count is back to where it was before
    // the confirm, the document is still a Draft, and the sequence is UNSPENT.
    //
    // These assertions were the residue itself before the transaction landed.
    // They were RIGHT about that code — the residue they measured was real, and
    // `doc_sequences` had no way to give a number back. They are inverted here
    // because the guarantee changed, not because they were wrong.
    //
    // Every one of them runs with STRICT STOCK ON (`svc_with_flags(false, false)`),
    // so the injected failure is provably the thing that stopped the confirm and
    // not the stock pre-check above it.

    /// WINDOW 1 — between `next_number` and the FIRST stock movement. This used
    /// to leave the narrowest residue there is: a burned number and nothing else,
    /// and this is the case the atomicity note at the top of this file used to
    /// describe as the ONLY possible one. The number is now unspent.
    #[tokio::test]
    async fn confirm_failure_between_the_number_and_the_first_movement_leaves_nothing_written() {
        let (s, pool) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "T3-W1", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-t3w1").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, prod.id, "2").await;
        let actor = audit_actor(&s).await;
        let prod_id = prod.id;

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER t3_w1 BEFORE INSERT ON stock_movements \
             WHEN NEW.reason = 'Sale' AND NEW.product_id = {prod_id} \
             BEGIN SELECT RAISE(ABORT, 'injected first-movement failure'); END"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        assert!(
            err.to_string().contains("injected first-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sale_sequence_last(&pool).await,
            None,
            "the number was never spent: `doc_sequences` has no row for SALE at all, \
             because the increment rolled back with the rest of the unit"
        );
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the seeding movement in — the Out rolled back with everything else"
        );
        assert_eq!(tx_count(&pool).await, 0, "finance never started");
        assert_eq!(payment_count(&pool).await, 0, "no payment was written");
        assert_eq!(
            row_state(&pool, sale.id).await,
            ("Draft".to_string(), None),
            "the document is still a Draft and was never given a number"
        );
    }

    /// WINDOW 2 — on the SECOND movement of a two-line sale. Two DIFFERENT
    /// products, each with stock to spare, so the strict per-product demand
    /// pre-check above the loop has nothing to say and the confirm reaches the
    /// loop with both lines eligible.
    ///
    /// This used to be the first proof that a partial write is not a sequence
    /// gap: the first line's Out was committed and real while the document
    /// stayed a Draft with no number, so that stock was deducted from a
    /// document nobody could see and `delete_draft` would take the Draft away
    /// without giving the units back. The first line's movement now rolls back
    /// with the second one's failure.
    #[tokio::test]
    async fn confirm_failure_on_the_second_movement_rolls_the_first_one_back_too() {
        let (s, pool) = svc_with_flags(false, false).await;
        let first = seed_product(&s, "T3-W2A", "10").await;
        seed_stock(&s, first.id, "10").await;
        let second = seed_product(&s, "T3-W2B", "10").await;
        seed_stock(&s, second.id, "10").await;
        let acc = seed_account(&s, "caja-t3w2").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, first.id, "2").await;
        // Lines are loaded in id order, so `first`'s Out is written before
        // `second`'s — this is the SECOND movement that fails.
        s.add_line(audit_actor(&s).await, sale.id, second.id, dec("3"), None)
            .await
            .unwrap();
        let actor = audit_actor(&s).await;
        let second_id = second.id;

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER t3_w2 BEFORE INSERT ON stock_movements \
             WHEN NEW.reason = 'Sale' AND NEW.product_id = {second_id} \
             BEGIN SELECT RAISE(ABORT, 'injected second-movement failure'); END"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        assert!(
            err.to_string().contains("injected second-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sale_sequence_last(&pool).await,
            None,
            "the number is UNSPENT: the increment is inside the rolled-back unit"
        );
        assert_eq!(
            movement_count(&pool).await,
            2,
            "only the two seeding movements in — the first line's Out rolled back \
             with the failure on the second"
        );
        assert_eq!(tx_count(&pool).await, 0, "finance never started");
        assert_eq!(payment_count(&pool).await, 0, "no payment was written");
        assert_eq!(
            row_state(&pool, sale.id).await,
            ("Draft".to_string(), None),
            "no stock left the building on a Draft nobody can see"
        );
        let still_sold: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM stock_movements WHERE product_id = ? AND reason = 'Sale'",
        )
        .bind(first.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            still_sold.0, 0,
            "the first line's Out is GONE: it is not in the table at all, so the \
             units never left stock for a document that was never confirmed"
        );
    }

    /// WINDOW 3 — between the Income row and the payment row. This used to leave
    /// the orphan: a `transactions` row stamped with the sale's reference that
    /// no payment claimed, inflating an account while the sale stayed a Draft
    /// and unpaid forever. `check_payment_links_are_traceable` detects that
    /// shape, but only inside its own test module. There is no orphan to detect
    /// now — the Income rolls back with the payment row that failed.
    #[tokio::test]
    async fn confirm_failure_between_the_income_and_the_payment_leaves_no_orphan_income() {
        let (s, pool) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "T3-W3", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-t3w3").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, prod.id, "2").await;
        let actor = audit_actor(&s).await;
        let sale_id = sale.id;

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER t3_w3 BEFORE INSERT ON sale_payments \
             WHEN NEW.sale_id = {sale_id} \
             BEGIN SELECT RAISE(ABORT, 'injected payment-row failure'); END"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        assert!(
            err.to_string().contains("injected payment-row failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sale_sequence_last(&pool).await,
            None,
            "the number is UNSPENT: the increment is inside the rolled-back unit"
        );
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the seeding movement in — the Out rolled back too"
        );
        assert_eq!(
            tx_count(&pool).await,
            0,
            "the Income rolled back with the payment row that could not be written"
        );
        assert_eq!(
            payment_count(&pool).await,
            0,
            "the payment never landed, and there is now no Income for it to orphan"
        );
        assert_eq!(
            row_state(&pool, sale.id).await,
            ("Draft".to_string(), None),
            "the document is still a Draft with no number"
        );

        let orphans: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transactions t \
             WHERE NOT EXISTS (SELECT 1 FROM sale_payments sp WHERE sp.transaction_id = t.id \
                               OR sp.refund_transaction_id = t.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            orphans.0, 0,
            "no finance row is claimed by nobody: the orphan shape the confirm used \
             to leave behind does not exist here"
        );
    }

    /// WINDOW 4 — on `set_confirmed` itself. This is the LAST write, so this used
    /// to be the largest residue: a Draft that had already moved stock, already
    /// posted an Income and already collected a payment. The money was in, the
    /// units were gone, and the document was still editable as a Draft — which
    /// made it the residue with no recovery path, since `delete_draft` admits a
    /// Draft and its CASCADE took the payment while the `Income` survived.
    ///
    /// Now nothing survives. The Draft reads as UNPAID, because `get_detail`
    /// derives the paid/unpaid state from the payments table and that table is
    /// back to empty.
    #[tokio::test]
    async fn confirm_failure_on_set_confirmed_leaves_a_clean_draft_that_reports_unpaid() {
        let (s, pool) = svc_with_flags(false, false).await;
        let prod = seed_product(&s, "T3-W4", "10").await;
        seed_stock(&s, prod.id, "10").await;
        let acc = seed_account(&s, "caja-t3w4").await;
        let cash = cash_method(&s).await;
        allow(&s, acc.id, cash).await;
        let sale = draft_with_line(&s, WALKIN_ID, PaymentType::Cash, None, prod.id, "2").await;
        let actor = audit_actor(&s).await;
        let sale_id = sale.id;

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER t3_w4 BEFORE UPDATE ON sales WHEN NEW.id = {sale_id} \
             BEGIN SELECT RAISE(ABORT, 'injected set-confirmed failure'); END"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s.confirm(actor, sale.id, Some(cash)).await.unwrap_err();
        assert!(
            err.to_string().contains("injected set-confirmed failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sale_sequence_last(&pool).await,
            None,
            "the number is UNSPENT: the increment is inside the rolled-back unit, so \
             `doc_sequences` has no SALE row at all"
        );
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the seeding movement in — the Out rolled back"
        );
        assert_eq!(tx_count(&pool).await, 0, "the Income rolled back");
        assert_eq!(
            payment_count(&pool).await,
            0,
            "the payment rolled back: the shop does NOT have the money and has no document"
        );
        assert_eq!(
            row_state(&pool, sale.id).await,
            ("Draft".to_string(), None),
            "the document is still a Draft and was never given a number"
        );
        let detail = s.get_detail(sale.id).await.unwrap();
        assert_eq!(
            detail.payment_status,
            crate::models::PaymentStatus::Unpaid,
            "the document reports itself UNPAID: `get_detail` reads the payments \
             table, and that table is empty again"
        );
        assert_eq!(
            detail.paid,
            dec("0"),
            "so the shop's books show nothing collected against the Draft"
        );
        assert_eq!(
            detail.due,
            dec("20"),
            "Drafts remain based on their own full total because they have no residual row"
        );
    }
}
