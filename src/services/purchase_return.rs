// M-purchase returns (odd/tasks/purchase-returns-and-credit-notes.md).
//
// PurchaseReturnService is the orchestrator for "I bought three and I am sending
// two back", which the annulment path cannot express: `PurchasesService::cancel`
// is all-or-nothing and it discards the document rather than reversing part of
// it. This service calls InventoryService for stock Out (reason
// Purchase-return) on confirm, TransactionService for the refund Incomes on
// confirm and the reversal Expenses on cancel, and the purchase repository for
// the parent it reads. It never SQLs `transactions`, `stock_movements`,
// `accounts` or `payment_methods` for writes, and it writes NOTHING to
// `product_supplier_costs` — see the note on `confirm`.
//
// Numbering: YYYY-PRET-NNNNNN assigned on confirm via the `doc_sequences`
// consumer PRET, the short form, with `year` taken from the RETURN's own date
// rather than the parent's or the clock's. A Draft touches nothing.
//
// Direction, which is the whole difference from a purchase and the reason the
// two families are separate types rather than one parameterised over a sign:
//
//   stock  Out      the goods go back to the supplier
//   money  Income   the refund is money ENTERING, so the overdraft guard never
//                   fires on it — the same reason an annulment refund is an
//                   Income (`PurchasesService::cancel` says so in its own file
//                   header and does not check a balance either)
//
// Atomicity: ONE transaction, opened immediately before the number is taken and
// committed after `set_confirmed`. A return has MORE steps than a purchase — a
// refund to collect as well as goods to send — and every one of them runs in
// the unit, so a failure at any step leaves the row `("Draft", NULL)` with no
// movement, no finance row, no payment and an UNSPENT number. The residue
// tables in `AGENTS.md` and in `purchases.rs` describe what the pre-transaction
// shape left behind; the `confirm_failure_*` tests below assert their absence.
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    format_purchase_return_number, MovementReason, MovementType, NewMovement, PriceRefusal,
    Purchase, PurchaseDetail, PurchaseLine, PurchaseReturn, PurchaseReturnDetail,
    PurchaseReturnLine, PurchaseReturnPayment, PurchaseReturnStatus, RecordMoney,
};
use crate::services::checked_money_sum;

/// A single confirmed refund, resolved BEFORE the unit is opened, so the write
/// phase below is a straight loop over a plan it cannot second-guess.
///
/// `account_id` and `method_id` are the PARENT PAYMENT's, not the return's:
/// each refund lands in the account and by the method the money left the shop
/// through, which is what `PurchasesService::cancel` does when it refunds a
/// purchase per originating account, and the reason the refund cap and this
/// allocation are the same rule seen from two sides.
#[derive(Debug, Clone, Copy)]
struct RefundPlan {
    account_id: i64,
    method_id: i64,
    amount: Decimal,
}

#[derive(Clone)]
pub struct PurchaseReturnService<RR, DR, PR, C, P, B, S, A, T, PL, PY>
where
    RR: crate::repositories::PurchaseReturnRepository,
    DR: crate::repositories::DocSequenceRepository,
    PR: crate::repositories::PurchaseRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PL: crate::repositories::PartyLedgerRepository,
    PY: crate::repositories::PaymentRepository,
{
    pub returns: RR,
    pub sequences: DR,
    pub purchases: PR,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
    /// The supplier's signed journal (T2), mirroring the customer side: a
    /// confirmed return appends the `Return` that cancels part of the purchase's
    /// `Charge`, in the same unit that moves the stock back.
    pub party_ledger: PL,
    /// The `payments` family (T3d): a cancelled purchase return moves the money back
    /// out as a delivery, so this side needs the same document.
    pub payments: PY,
}

impl<RR, DR, PR, C, P, B, S, A, T, PL, PY>
    PurchaseReturnService<RR, DR, PR, C, P, B, S, A, T, PL, PY>
where
    RR: crate::repositories::PurchaseReturnRepository,
    DR: crate::repositories::DocSequenceRepository,
    PR: crate::repositories::PurchaseRepository,
    C: crate::repositories::CategoryRepository,
    P: crate::repositories::ProductRepository,
    B: crate::repositories::BarcodeRepository,
    S: crate::repositories::StockMovementRepository,
    A: crate::repositories::AccountRepository,
    T: crate::repositories::TransactionRepository,
    PL: crate::repositories::PartyLedgerRepository,
    PY: crate::repositories::PaymentRepository,
{
    pub fn new(
        returns: RR,
        sequences: DR,
        purchases: PR,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
        party_ledger: PL,
        payments: PY,
    ) -> Self {
        Self {
            returns,
            sequences,
            purchases,
            inventory,
            transactions,
            party_ledger,
            payments,
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

    /// The whole document-level money as ONE value, or the rule that refused it.
    /// The purchase twin's reason for asking, deliberately: `total`, `paid` and
    /// `due` are one fact and travel together, so no caller can publish a
    /// refundable figure derived from a total that does not exist.
    ///
    /// `tax_total` is ZERO and is not a forgotten figure. A return line freezes
    /// no tax — the column does not exist on the table — so `net_subtotal` and
    /// `total` are the same number and there is nothing to part out. It is
    /// stated here so a reader does not go looking for the missing split.
    ///
    /// Every fold is `checked_money_sum`, so a document of two `5e28` lines is
    /// refused rather than panicking on `Decimal`'s raw `+`: each line's subtotal
    /// is a bounded multiplication, and the sum of a set of them is not. The
    /// refusal is `DocumentTotalTooLarge` because no single line is at fault.
    fn document_money(
        lines: &[PurchaseReturnLine],
        payments: &[PurchaseReturnPayment],
    ) -> Result<RecordMoney, PriceRefusal> {
        let subtotals: Vec<Decimal> = lines.iter().map(|l| l.subtotal()).collect();
        let net_subtotal = checked_money_sum(subtotals.iter())?;
        let total = net_subtotal;
        let paid = checked_money_sum(payments.iter().map(|p| &p.amount))?;
        let due = total
            .checked_sub(paid)
            .ok_or(PriceRefusal::DocumentTotalTooLarge)?;
        Ok(RecordMoney {
            net_subtotal,
            tax_total: Decimal::ZERO,
            total,
            paid,
            due,
            // `PurchaseReturnDetail` has no `payment_status_for` of its own and
            // adding one would be a model change, so the document-level rule is
            // the one `PurchaseDetail` and `SaleDetail` already share: due <= 0
            // is Paid, some money is Partial, none is Unpaid.
            payment_status: PurchaseDetail::payment_status_for(total, paid),
        })
    }

    async fn detail_for(&self, purchase_return: PurchaseReturn) -> AppResult<PurchaseReturnDetail> {
        let lines = self.returns.list_lines(purchase_return.id).await?;
        let payments = self.returns.list_payments(purchase_return.id).await?;
        let money = Self::document_money(&lines, &payments).map_err(AppError::PriceRefused)?;
        Ok(PurchaseReturnDetail {
            purchase_return,
            lines,
            payments,
            net_subtotal: money.net_subtotal,
            total: money.total,
            paid: money.paid,
            due: money.due,
            payment_status: money.payment_status,
        })
    }

    fn ensure_draft(purchase_return: &PurchaseReturn) -> AppResult<()> {
        if purchase_return.status != PurchaseReturnStatus::Draft {
            return Err(AppError::Validation(format!(
                "purchase return {} is not editable (status {})",
                purchase_return.id, purchase_return.status
            )));
        }
        Ok(())
    }

    /// The parent of a return is CONFIRMED history, never a Draft and never a
    /// cancelled document: a return is evidence ABOUT a document that exists,
    /// and the number it will carry is derived from the parent's price. A
    /// Cancelled parent is refused for the sharper reason that its goods went
    /// back, so a return on it would return goods that already left.
    async fn confirmed_parent(&self, purchase_id: i64) -> AppResult<Purchase> {
        let purchase = self
            .purchases
            .find_purchase(purchase_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase {purchase_id} not found")))?;
        if purchase.status != crate::models::PurchaseStatus::Confirmed {
            return Err(AppError::Validation(format!(
                "purchase {purchase_id} is {}: only a Confirmed purchase can be returned",
                purchase.status
            )));
        }
        Ok(purchase)
    }

    /// The parent line a return line names, or `NotFound`. The service holds no
    /// `SqlitePool` and SQLs nothing, so this read goes through the purchase
    /// repository exactly as every other read here does.
    async fn parent_line(&self, purchase_line_id: i64) -> AppResult<PurchaseLine> {
        self.purchases
            .find_line(purchase_line_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("purchase line {purchase_line_id} not found"))
            })
    }

    /// **THE RULE, IN FULL: a return line may claim at most what the parent line
    /// bought MINUS what already-CONFIRMED returns of that same parent line
    /// took.**
    ///
    /// Both terms are now real. The second was not computable before this
    /// repository gained `confirmed_qty_taken_by_purchase_line`: no method on
    /// [`crate::repositories::PurchaseReturnRepository`] looked OUTWARD from a
    /// parent line — `list_lines` is keyed by a RETURN id and `find_line` by a
    /// LINE id, so every read looked the wrong way — and a service that reached
    /// for `pool()` to ask itself would be the one thing the layering rule exists
    /// to prevent. With the hole open, buying five units, returning them, and
    /// returning the same five again refunded the supplier twice; that is the
    /// RED in `a_second_confirmed_return_of_the_same_parent_line_is_refused_because_the_allowance_is_spent`.
    ///
    /// **THE ALLOWANCE IS THE PARENT'S OWN FIGURE MINUS AN AGGREGATE, and the
    /// subtraction is CHECKED.** `parent.qty - taken` is a `-` on two
    /// operator-supplied decimals, and a raw one panics on overflow exactly as a
    /// raw `+` does. `checked_sub` is what makes that a property of the code
    /// rather than a fact about the caller. `AggregateTooLarge` is the honest
    /// refusal: both operands are quantities read out of stored TEXT, and this is
    /// the layer that already answers `AggregateTooLarge` for a fold over such
    /// rows.
    ///
    /// **A DRAFT RESERVES NOTHING, and the aggregate is what decides that.** The
    /// read filters `status = 'Confirmed'`, so a draft's own lines are not in the
    /// figure the draft is measured against — a return being edited still sees
    /// its OWN full allowance, which is what lets an operator take a line from 2
    /// up to 5 on a draft without being refused by the draft's earlier self. This
    /// is the decision, and it is deliberate rather than incidental: a draft has
    /// moved no goods and no money, so there is nothing to reserve. The
    /// consequence is that two drafts may each claim the whole line and the
    /// SECOND CONFIRM is what refuses — see
    /// `a_draft_reserves_nothing_so_a_second_draft_still_writes_and_only_the_first_confirm_wins`.
    /// The alternative (reserving at draft time) would need a reservation concept
    /// this app has no notion of, and would make a second draft die silently
    /// rather than be refused for a stated reason.
    ///
    /// `taken_already` is passed in rather than read here because all three call
    /// sites must read it ONCE PER LINE and the confirm path reads them in a
    /// loop; reading it inside would make that shape invisible.
    fn ensure_within_parent(
        parent: &PurchaseLine,
        taken_already: Decimal,
        qty: Decimal,
    ) -> AppResult<()> {
        if qty <= Decimal::ZERO {
            return Err(AppError::Validation("qty must be > 0".into()));
        }
        let remaining = parent
            .qty
            .checked_sub(taken_already)
            .ok_or(AppError::PriceRefused(PriceRefusal::AggregateTooLarge))?;
        if qty > remaining {
            return Err(AppError::Validation(format!(
                "cannot return {qty} of a purchase line of {}: the line bought {} and \
                 {taken_already} of it is already returned by a confirmed return, \
                 leaving {} returnable",
                parent.id, parent.qty, remaining
            )));
        }
        Ok(())
    }

    // -- Draft -----------------------------------------------------------------

    /// Create a Draft return against a CONFIRMED purchase. `supplier_id` is
    /// COPIED off the parent rather than supplied by the caller: a return names
    /// the supplier the purchase names, because the money comes back from the
    /// purchase, and a second place for the same fact to be wrong is one too
    /// many. `return_date` is the day the return is MADE — the goods leave
    /// today and the money arrives today.
    ///
    /// `actor` is the acting user's id the route resolves from its `Principal`;
    /// it becomes the row's `created_by` and nothing the request itself can
    /// supply names it.
    pub async fn create_draft(
        &self,
        actor: i64,
        purchase_id: i64,
        return_date: NaiveDate,
        notes: &Option<String>,
    ) -> AppResult<PurchaseReturn> {
        let purchase = self.confirmed_parent(purchase_id).await?;
        let notes = Self::clean_notes(notes)?;
        self.returns
            .create_return(
                actor,
                purchase.supplier_id,
                purchase_id,
                return_date,
                &notes,
            )
            .await
    }

    pub async fn update_draft(
        &self,
        actor: i64,
        id: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<PurchaseReturn> {
        let purchase_return = self.return_or_404(id).await?;
        Self::ensure_draft(&purchase_return)?;
        if notes.chars().count() > 512 {
            return Err(AppError::Validation("notes must be <= 512 chars".into()));
        }
        self.returns
            .update_draft(id, actor, return_date, notes.trim())
            .await
    }

    async fn return_or_404(&self, id: i64) -> AppResult<PurchaseReturn> {
        self.returns
            .find_return(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("purchase return {id} not found")))
    }

    /// Add one line naming the PARENT LINE it returns, at the parent's frozen
    /// cost, with no price argument anywhere in the signature.
    ///
    /// **There is no price parameter and there cannot be one.** `create_line` is
    /// the only method that writes `unit_cost`, it takes the figure as an
    /// argument, and this service hands it the parent line's own. That is
    /// decision 1 of the design expressed as a signature: a return is the
    /// absence of goods the business still owns, not a renegotiation, and
    /// `product_supplier_costs` holds one price per (product, supplier) so a
    /// return at a different price has no defined answer.
    pub async fn add_line(
        &self,
        actor: i64,
        return_id: i64,
        purchase_line_id: i64,
        qty: Decimal,
    ) -> AppResult<PurchaseReturnLine> {
        let purchase_return = self.return_or_404(return_id).await?;
        Self::ensure_draft(&purchase_return)?;
        let parent = self.parent_line(purchase_line_id).await?;
        let taken = self
            .returns
            .confirmed_qty_taken_by_purchase_line(purchase_line_id)
            .await?;
        Self::ensure_within_parent(&parent, taken, qty)?;
        let line = self
            .returns
            .create_line(return_id, purchase_line_id, qty, parent.unit_cost)
            .await?;
        self.touch(return_id, actor).await?;
        Ok(line)
    }

    /// Edit a return line's QUANTITY. There is deliberately no price argument:
    /// the repository's `update_line` takes none, so the frozen `unit_cost` is
    /// not a thing an operator may move even if a caller wanted to.
    pub async fn update_line(
        &self,
        actor: i64,
        line_id: i64,
        qty: Decimal,
    ) -> AppResult<PurchaseReturnLine> {
        let line = self.returns.find_line(line_id).await?.ok_or_else(|| {
            AppError::NotFound(format!("purchase return line {line_id} not found"))
        })?;
        let purchase_return = self.return_or_404(line.return_id).await?;
        Self::ensure_draft(&purchase_return)?;
        let parent = self.parent_line(line.purchase_line_id).await?;
        // A DRAFT reserves nothing, so the figure here excludes this line's own
        // document — which is what lets a draft be edited UP to the parent's full
        // quantity rather than being refused by its own earlier self.
        let taken = self
            .returns
            .confirmed_qty_taken_by_purchase_line(line.purchase_line_id)
            .await?;
        Self::ensure_within_parent(&parent, taken, qty)?;
        let line = self.returns.update_line(line_id, qty).await?;
        self.touch(line.return_id, actor).await?;
        Ok(line)
    }

    pub async fn remove_line(&self, actor: i64, line_id: i64) -> AppResult<()> {
        let line = self.returns.find_line(line_id).await?.ok_or_else(|| {
            AppError::NotFound(format!("purchase return line {line_id} not found"))
        })?;
        let purchase_return = self.return_or_404(line.return_id).await?;
        Self::ensure_draft(&purchase_return)?;
        self.returns.delete_line(line_id).await?;
        self.touch(line.return_id, actor).await?;
        Ok(())
    }

    /// A line write is an edit of the document, so it stamps the draft's
    /// `updated_by` with this request's actor. `update_draft` is the statement
    /// that does it and it takes the same two arguments a purchase's
    /// `touch_draft` does.
    async fn touch(&self, return_id: i64, actor: i64) -> AppResult<()> {
        let current = self.return_or_404(return_id).await?;
        self.returns
            .update_draft(return_id, actor, current.return_date, &current.notes)
            .await?;
        Ok(())
    }

    pub async fn get_detail(&self, return_id: i64) -> AppResult<PurchaseReturnDetail> {
        let purchase_return = self.return_or_404(return_id).await?;
        self.detail_for(purchase_return).await
    }

    // -- List ------------------------------------------------------------------

    /// Every return the filter selects, each with its lines, its payments and
    /// its derived money. Mirrors `PurchasesService::list_details_filtered`
    /// method for method: the filter goes to the repository, and each document
    /// that comes back is expanded by the same `detail_for` that `get_detail`
    /// uses, so a list row and a detail can never disagree about one document's
    /// money.
    ///
    /// There is no tolerant `list_rows` twin, and the reason is that it would
    /// need a model: `PurchasesService` has `PurchaseListRow` and this family has
    /// no such type, and inventing one is a model change this change does not
    /// own. A `PriceRefused` from one document therefore fails the whole read
    /// rather than degrading that document to an unpayable row — the STRICT
    /// twin's behaviour, which is the safe direction for a list a caller may act
    /// on.
    ///
    /// The party name is NOT resolved here, and that is a real limitation rather
    /// than a choice: `PurchaseListFilter` carries a typed `supplier: Option<String>`
    /// that the SERVICE turns into ids, because resolving a name needs the
    /// suppliers table and this service holds no supplier repository — its five
    /// repositories are returns, sequences, purchases, inventory and
    /// transactions. The repository therefore takes the resolved ids only, and a
    /// caller that has a name to resolve has to do it itself. Adding a
    /// `SupplierRepository` here is the natural fix and is a constructor change
    /// the routes will drive.
    pub async fn list(
        &self,
        filter: &crate::repositories::purchase_return_repo::PurchaseReturnListFilter,
    ) -> AppResult<Vec<PurchaseReturnDetail>> {
        let purchase_returns = self.returns.list_returns(filter).await?;
        let mut out = Vec::with_capacity(purchase_returns.len());
        for purchase_return in purchase_returns {
            out.push(self.detail_for(purchase_return).await?);
        }
        Ok(out)
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(&self, actor: i64, return_id: i64) -> AppResult<PurchaseReturnDetail> {
        let purchase_return = self.return_or_404(return_id).await?;
        if purchase_return.status == PurchaseReturnStatus::Confirmed {
            return Err(AppError::Validation(
                "purchase return already confirmed".into(),
            ));
        }
        if purchase_return.status == PurchaseReturnStatus::Cancelled {
            return Err(AppError::Validation(
                "cancelled purchase return cannot be confirmed".into(),
            ));
        }

        let lines = self.returns.list_lines(return_id).await?;
        if lines.is_empty() {
            return Err(AppError::Validation(
                "cannot confirm a purchase return with no lines".into(),
            ));
        }

        // The parent is re-read HERE and not taken from the draft, so a return
        // whose parent has been cancelled since the line was added is refused
        // before a single write. The supplier was copied at creation and is not
        // re-resolved: a deactivated supplier is still the supplier the money
        // comes back from, which is why the column is a copy and not a join.
        let purchase = self.confirmed_parent(purchase_return.purchase_id).await?;

        // Per line: the quantity is real, it fits what is LEFT of the parent
        // line, and the product is resolvable. The returnable ceiling is
        // re-checked on the confirm path as well as on the write path, for the
        // same reason `PurchasesService::confirm` re-checks the duplicate
        // product: a confirm must never accept a line the service's own writes
        // would have refused.
        //
        // **THIS IS THE READ THAT SPENDS THE ALLOWANCE.** A draft reserves
        // nothing, so at this point `confirmed_qty_taken_by_purchase_line` still
        // excludes THIS document's own lines — and it must, or a return could
        // never be confirmed at all: every line would be measured against a pool
        // its own draft had already drawn from. The figure read here is the sum
        // over OTHER confirmed returns, which is precisely what a confirm is
        // allowed to consume and what a draft is allowed to ignore.
        let mut tracked: Vec<(PurchaseReturnLine, PurchaseLine)> = Vec::new();
        for line in &lines {
            let parent = self.parent_line(line.purchase_line_id).await?;
            let taken = self
                .returns
                .confirmed_qty_taken_by_purchase_line(line.purchase_line_id)
                .await?;
            Self::ensure_within_parent(&parent, taken, line.qty)?;
            let product = self.inventory.get_product(parent.product_id).await?;
            if !product.is_active {
                return Err(AppError::Validation(format!(
                    "product {} is inactive",
                    product.id
                )));
            }
            if product.kind == crate::models::ProductKind::Product && product.track_stock {
                tracked.push((line.clone(), parent));
            }
        }

        // The document's money is resolved BEFORE any write below, exactly as on
        // the purchase side: a confirmation that cannot state what the return is
        // worth must refuse with nothing written.
        let money = Self::document_money(&lines, &self.returns.list_payments(return_id).await?)
            .map_err(AppError::PriceRefused)?;
        let total = money.total;

        // ---- THE REFUND CAP -------------------------------------------------
        //
        // A return refunds AT MOST what the parent has actually COLLECTED. The
        // rule exists because a purchase can be confirmed and only partly paid,
        // and refunding more than came in would leave a negative payable the app
        // has no concept to hold.
        //
        // The refunds are allocated PER ORIGINATING ACCOUNT — each one lands in
        // the account and by the method the parent's payment came from — which
        // is exactly what `PurchasesService::cancel` does when it refunds a
        // purchase, and it is the only allocation that can be justified: the
        // return does not know how the purchase was paid, and the parent does.
        //
        // A PARENT THAT COLLECTED NOTHING IS NOT A REFUSAL. It writes no refund
        // rows and the goods still go back, because the cap is about how much
        // MONEY moves, not about whether the goods move. Migration 40 says the
        // same in its own comment: "a return on a purchase that is confirmed but
        // unpaid writes no payment row. The return still exists to return the
        // goods."
        //
        // A PARENT THAT COLLECTED SOMETHING IS A CEILING. A return worth more
        // than came in is REFUSED, with the shortfall named, because this app
        // has no notion of a credit owed BY a supplier: a shop that buys on
        // credit and returns the goods before paying has a legitimate case the
        // cap cannot hold, and it is v1's answer rather than a solution. Do not
        // invent a credit-balance mechanism here to make that case fit; it needs
        // a decision, which is what the feature document still lists as open.
        let parent_payments = self.purchases.list_payments(purchase.id).await?;
        let collected = checked_money_sum(parent_payments.iter().map(|p| &p.amount))
            .map_err(AppError::PriceRefused)?;
        if collected > Decimal::ZERO && total > collected {
            return Err(AppError::Validation(format!(
                "return is worth {total} but purchase {} has only collected {collected} from \
                 supplier {}: a refund cannot exceed what was paid, and this app has no \
                 credit balance to hold the difference. Pay the purchase first, or return less.",
                purchase
                    .purchase_number
                    .as_deref()
                    .unwrap_or("(unnumbered)"),
                purchase.supplier_id
            )));
        }
        let plan = Self::refund_plan(total, &parent_payments)?;

        // ---- THE WRITE UNIT -------------------------------------------------
        //
        // Everything from here to the COMMIT is ONE transaction: the sequence
        // number, one stock movement per tracked line, one Income per planned
        // refund, the `purchase_return_payments` rows, and `set_confirmed`.
        //
        // EVERY repository call inside is an `_in` form. That is not a style
        // choice here: a return has more steps than a purchase — a refund to
        // COLLECT as well as goods to send — and on separate autocommit
        // connections a failure between any two would leave the earlier ones
        // committed. That residue was MEASURED on the purchase family
        // (`purchase_confirm_failure_*` in `purchases.rs`): a burned number, a
        // stock movement whose reference names no document, an orphan finance
        // row, and worst of all a Draft that had already taken the money. Every
        // one of those tests now asserts its absence, and they are mirrored
        // below as `confirm_failure_*`.
        //
        // **NO SATELLITE WRITE, by decision 2 of the design.** A return does not
        // touch `product_supplier_costs`, in any step and in either direction.
        // Only a confirmed purchase changes a product's cost, because only a
        // purchase sets a price; a return at the purchase price sets no price,
        // so `current_cost`, `previous_cost` and their dates are untouched and
        // the derived price-change alert correctly does not fire. There is
        // deliberately no `record_cost` call below and no cost-date movement:
        // if you are reading this method and looking for where the satellite
        // should be written, the answer is that it must not be. A confirmed
        // return leaves the table byte-identical, and
        // `a_confirmed_return_leaves_the_cost_satellite_byte_identical` is the
        // test that says so.
        //
        // The BEGIN goes HERE and not one line earlier, on purpose. Every read
        // above it — the document, the parent, the per-line parent lookup, the
        // tracked predicate, the totals, the refund cap — is a pre-check, and a
        // pre-check buys EARLY refusal with a useful message rather than
        // reachability. Holding a transaction open across them would also pin
        // the only connection for the whole pre-check and buy nothing.
        //
        // ROLLBACK IS THE `?`. There is deliberately no explicit rollback arm
        // and no `unwrap_or` on the way out: every `?` here drops the
        // `Transaction`, sqlx rolls it back, and the `AppError` that caused it
        // propagates UNCHANGED. An explicit arm would be a place to swallow a
        // refusal, and the refusal IS the answer. Do not add one.
        let mut tx = self.returns.pool().begin().await?;

        // 1. The number. `PRET`, not the long form: the short prefix is the same
        //    decision the sale and purchase numbers took, for the same reason.
        let year = purchase_return.return_date.year();
        let seq = self.sequences.next_number_in(&mut tx, "PRET", year).await?;
        let return_number = format_purchase_return_number(year, seq);

        // 2. One stock movement per line: Out, reason Purchase-return. The
        //    movement carries the CONFIRMING request's actor, never a fresh one.
        for (line, parent) in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: parent.product_id,
                        qty: line.qty,
                        movement_type: MovementType::Out,
                        reason: MovementReason::PurchaseReturn,
                        reference: return_number.clone(),
                        date: purchase_return.return_date,
                    },
                )
                .await?;
        }

        // 3 and 4. One Income per planned refund, `reference` = this return's
        //    own number, then the payment row that claims it. An Income is money
        //    ENTERING, so `create_with_reference_in` enforces no balance
        //    precondition and the overdraft guard cannot fire — which is the
        //    single behavioural difference from a credit note's Expense.
        for refund in &plan {
            let income = self
                .transactions
                .create_with_reference_in(
                    &mut tx,
                    actor,
                    refund.account_id,
                    crate::models::TransactionKind::Income,
                    refund.amount,
                    Some(return_number.clone()),
                    Some(return_number.clone()),
                    purchase_return.return_date,
                )
                .await?;
            self.returns
                .create_payment_in(
                    &mut tx,
                    actor,
                    return_id,
                    refund.account_id,
                    refund.method_id,
                    refund.amount,
                    purchase_return.return_date,
                    Some(income.id),
                )
                .await?;

            // One Refund per delivery, located on this return (not the parent
            // payment). Its positive sign is PartyEntryKind's rule; the Income
            // above replays the parent's historical account.
            self.party_ledger
                .insert_in(
                    &mut tx,
                    &crate::models::NewPartyLedgerEntry {
                        party_type: crate::models::PartyType::Supplier,
                        party_id: purchase.supplier_id,
                        kind: crate::models::PartyEntryKind::Refund,
                        amount: crate::models::PartyEntryKind::Refund
                            .signed_amount(refund.amount),
                        document_kind: crate::models::PartyDocumentKind::PurchaseReturn,
                        document_id: return_id,
                        entry_date: purchase_return.return_date,
                        reference: Some(return_number.clone()),
                        created_by: actor,
                    },
                )
                .await?;
        }

        // 5. The supplier's journal, in the SAME unit as the stock movement (T2
        // of odd/tasks/party-ledger.md). A purchase return sends goods back, so it
        // cancels part of what the purchase charged: a `Return` of `−total`, which
        // folds the payable down by what went back.
        //
        // Written even when the refund plan is EMPTY, for the same reason as the
        // customer side: the goods left whether or not money came with them, and a
        // purchase confirmed but unpaid legitimately returns goods the shop has
        // not paid for yet.
        self.party_ledger
            .insert_in(
                &mut tx,
                &crate::models::NewPartyLedgerEntry {
                    party_type: crate::models::PartyType::Supplier,
                    party_id: purchase.supplier_id,
                    kind: crate::models::PartyEntryKind::Return,
                    amount: crate::models::PartyEntryKind::Return.signed_amount(total),
                    document_kind: crate::models::PartyDocumentKind::PurchaseReturn,
                    document_id: return_id,
                    entry_date: purchase_return.return_date,
                    reference: Some(return_number.clone()),
                    created_by: actor,
                },
            )
            .await?;

        // 6. The document exists from here.
        let confirmed = self
            .returns
            .set_confirmed_in(&mut tx, return_id, actor, &return_number)
            .await?;

        tx.commit().await?;

        // ---- AFTER THE COMMIT, DELIBERATELY -------------------------------
        //
        // `detail_for` reads the document's lines and payments, and it stays on
        // the pool on purpose: a pool read beneath an open unit cannot answer on
        // a one-connection pool (30s, then `PoolTimedOut`), and it has no reason
        // to be inside the unit anyway.
        self.detail_for(confirmed).await
    }

    /// Split a return's worth across the parent's payments, oldest first, each
    /// taking no more than that payment carried. A payment whose whole amount is
    /// consumed produces no row; a payment with nothing left to take is skipped,
    /// which is what makes a parent confirmed but unpaid produce an EMPTY plan
    /// rather than zero-amount refunds, and `create_with_reference` refuses an
    /// amount of zero in any case.
    ///
    /// Checked on the running remainder, because `remaining -= take` is a raw
    /// subtraction of an operator-controlled figure against another one. It
    /// cannot underflow by construction — `take` is `min(remaining, ...)` — and
    /// the checked form is what makes that a property of the code rather than a
    /// fact about the caller.
    fn refund_plan(
        total: Decimal,
        parent_payments: &[crate::models::PurchasePayment],
    ) -> AppResult<Vec<RefundPlan>> {
        let mut remaining = total;
        let mut plan = Vec::new();
        for pay in parent_payments {
            if remaining <= Decimal::ZERO {
                break;
            }
            let take = pay.amount.min(remaining);
            if take <= Decimal::ZERO {
                continue;
            }
            remaining = remaining
                .checked_sub(take)
                .ok_or(AppError::PriceRefused(PriceRefusal::DocumentTotalTooLarge))?;
            plan.push(RefundPlan {
                account_id: pay.account_id,
                method_id: pay.method_id,
                amount: take,
            });
        }
        Ok(plan)
    }

    // -- Cancel ----------------------------------------------------------------

    /// Reverse a CONFIRMED return. A refund sent by mistake must be undoable, so
    /// the reversal is as real as the confirm: the stock comes back the other
    /// way, the money goes back the other way, and every reversal finance row
    /// is linked to the refund row it reverses through
    /// `set_payment_refund_transaction`, so a return that has itself been
    /// reversed carries BOTH links on each row.
    ///
    /// This mirrors `PurchasesService::cancel` step for step, including what it
    /// does NOT do. It is not a transaction: the existing annulment path on both
    /// families writes on the pool, and making this one atomic while the other
    /// two are not would be a change to behaviour with no unit to justify it.
    /// It has no aggregate balance pre-check either, for the reason
    /// `PurchasesService::cancel` states about ITS refund — the reversal here is
    /// an EXPENSE, so `create_with_reference`'s own per-row guard is what
    /// protects an account the shop has already spent, and it refuses with the
    /// same message and the same figures the guard always gives.
    ///
    /// **THE MOVEMENT REASON IS `Adjust`, and that is an interpretation, not a
    /// given.** The confirm wrote `Out` with reason `Purchase-return`, so the
    /// reversal must be an `In` — and the reason vocabulary offers `Purchase`
    /// (goods in because we bought them), `Sale-return`, `Initial` and `Adjust`.
    /// Every one of the first three states something this movement is not: no
    /// purchase happened, no sale came back, and no opening balance is being
    /// restated. `Adjust` is the only neutral one, and it is neutral rather than
    /// wrong because the `reference` on the movement still carries the return's
    /// own number, so the pair is traceable. Reported rather than silently
    /// chosen: adding a dedicated "reversal" reason would be a model and
    /// migration change, which is a later decision.
    ///
    /// The refund is the mirror of the confirm's: an Income became an Expense,
    /// so the money goes back to the supplier it came from.
    pub async fn cancel(
        &self,
        actor: i64,
        return_id: i64,
        reason: Option<String>,
    ) -> AppResult<PurchaseReturnDetail> {
        let purchase_return = self.return_or_404(return_id).await?;
        if purchase_return.status == PurchaseReturnStatus::Cancelled {
            return Err(AppError::Validation(
                "purchase return already cancelled".into(),
            ));
        }

        // The document's money is resolved BEFORE any write on this path, for
        // the reason `PurchasesService::cancel` states: without it a reversal
        // would move stock and money and only the read at the end would refuse.
        let lines = self.returns.list_lines(return_id).await?;
        let payments = self.returns.list_payments(return_id).await?;
        Self::document_money(&lines, &payments).map_err(AppError::PriceRefused)?;

        if purchase_return.status == PurchaseReturnStatus::Draft {
            // Draft -> Cancelled: discard, no stock or finance side effect.
            let cancelled = self
                .returns
                .set_cancelled(return_id, actor, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        let return_number = purchase_return.return_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed purchase return missing return_number".into())
        })?;

        // The parent purchase, for the SUPPLIER the reversal is assigned to. Read here
        // rather than passed in, and before any write so a missing parent refuses early.
        let purchase = self.confirmed_parent(purchase_return.purchase_id).await?;

        // A partially-applied reversal is REFUSED, not doubled: a second pass
        // would move the goods back in again and reverse every refund twice.
        let partial = payments
            .iter()
            .filter(|p| p.refund_transaction_id.is_some())
            .count();
        if partial > 0 {
            return Err(AppError::Validation(format!(
                "reversal already partially applied: {partial} of {} refunds already link a reversal; \
                 refusing to return the goods a second time or duplicate the reversal",
                payments.len()
            )));
        }

        let mut tracked: Vec<(PurchaseReturnLine, PurchaseLine)> = Vec::new();
        for line in &lines {
            let parent = self.parent_line(line.purchase_line_id).await?;
            let product = self.inventory.get_product(parent.product_id).await?;
            if product.kind == crate::models::ProductKind::Product && product.track_stock {
                if !product.is_active {
                    return Err(AppError::Validation(format!(
                        "product {} is inactive",
                        product.id
                    )));
                }
                tracked.push((line.clone(), parent));
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

        // ---- THE WRITE UNIT (T3d) -------------------------------------------
        //
        // The stock coming back, the reversal deliveries and the cancellation are ONE
        // unit. Before, each reversal was posted by `create_with_reference` — a unit of
        // its own — and only then was the return flipped to Cancelled, so a failure in
        // between left the money gone and the document still Confirmed.
        let mut tx = self.returns.pool().begin().await?;

        // Stock In: the goods the supplier sent back come back onto the shelf.
        for (line, parent) in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: parent.product_id,
                        qty: line.qty,
                        movement_type: MovementType::In,
                        reason: MovementReason::Adjust,
                        reference: return_number.clone(),
                        date: purchase_return.return_date,
                    },
                )
                .await?;
        }

        // The money leaves again, per originating account, as a delivery that REPLAYS
        // the account it came from (decision 9). This is the one direction in which a
        // return's money is an `Expense`, and it is why a reversal can be refused for
        // want of funds where the confirm never could — the pre-checks above are what
        // refuse it, before anything is written.
        for pay in &payments {
            let reversal_delivery = crate::services::payment_writer::record_delivery_in(
                &self.sequences,
                &self.transactions,
                &self.party_ledger,
                &self.payments,
                &mut tx,
                actor,
                crate::models::PaymentDirection::Out,
                crate::models::PartyType::Supplier,
                purchase.supplier_id,
                (crate::models::PartyDocumentKind::PurchaseReturn, return_id),
                pay.method_id,
                pay.account_id,
                pay.amount,
                purchase_return.return_date,
                Some(format!("cancellation of {return_number}")),
                Some(return_number.clone()),
                None,
                &[],
            )
            .await?;
            let reversal_id = reversal_delivery
                .transaction_id
                .ok_or_else(|| AppError::Internal("reversal delivery has no movement".into()))?;
            self.returns
                .set_payment_refund_transaction_in(&mut tx, actor, pay.id, reversal_id)
                .await?;
        }

        let cancelled = self
            .returns
            .set_cancelled_in(&mut tx, return_id, actor, reason.as_deref())
            .await?;

        tx.commit().await?;

        self.detail_for(cancelled).await
    }

    /// The documents drawer's draft delete. TWO states are deletable, and both
    /// posted nothing: a Draft, and a return discarded while still Draft
    /// (Cancelled with `return_number` still NULL). A confirmed return — even
    /// one cancelled afterwards — is REVERSED through `cancel` instead:
    /// deleting it would strand its movements and its refund transactions, and
    /// its number proves it was confirmed.
    ///
    /// No `actor` parameter, for the reason `PurchasesService::delete_draft` has
    /// none: nothing survives to stamp.
    ///
    /// WHAT THE REPOSITORY PREDICATE DOES NOT ESTABLISH, and what this method
    /// therefore still owes the reader: it answers "may this row be removed",
    /// and it says nothing about whether the row is CLEAN. On the purchase
    /// family that gap was real. Here it is closed by the WRITES rather than by
    /// this clause — `confirm` is one transaction, so a Draft of a return has
    /// no committed payment, movement or finance row behind it — and that is a
    /// property measured by the `confirm_failure_*` tests, not inferred from
    /// SQL.
    pub async fn delete_draft(&self, id: i64) -> AppResult<()> {
        let purchase_return = self.return_or_404(id).await?;
        let deletable = purchase_return.status == PurchaseReturnStatus::Draft
            || (purchase_return.status == PurchaseReturnStatus::Cancelled
                && purchase_return.return_number.is_none());
        if !deletable {
            return Err(AppError::Validation(format!(
                "purchase return {id} is {}: only a draft or a discarded (never-confirmed) cancelled \
                 return can be deleted",
                purchase_return.status
            )));
        }
        let deleted = self.returns.delete_draft(id).await?;
        if !deleted {
            // A concurrent confirm won the race: the document is no longer
            // deletable, so the honest answer is the same refusal as above.
            return Err(AppError::Validation(format!(
                "purchase return {id} is no longer deletable: only a draft or a discarded \
                 (never-confirmed) cancelled return can be deleted"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewProduct, PaymentStatus, ProductKind};
    use crate::repositories::{
        purchase_return_repo::PurchaseReturnListFilter, PurchaseReturnRepository,
        SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteDocSequenceRepository, SqliteProductRepository, SqlitePurchaseRepository,
        SqlitePurchaseReturnRepository, SqliteStockMovementRepository, SqliteTransactionRepository,
    };
    use crate::repositories::{
        PartyLedgerRepository, PaymentRepository, SqlitePartyLedgerRepository,
        SqlitePaymentRepository,
    };
    use crate::security::test_support;
    use crate::services::{InventoryService, TransactionService};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;
    use std::time::Duration;

    type Svc = PurchaseReturnService<
        SqlitePurchaseReturnRepository,
        SqliteDocSequenceRepository,
        SqlitePurchaseRepository,
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
        SqliteAccountRepository,
        SqliteTransactionRepository,
        SqlitePartyLedgerRepository,
        SqlitePaymentRepository,
    >;

    /// `max_connections(1)` is LOAD-BEARING for every test in this module and is
    /// not the fixture's default by accident. While `confirm`'s unit is open it
    /// holds the only connection the pool owns, so a repository call that
    /// reached for the pool instead of joining it could not answer at all — it
    /// would sit on sqlx's 30s acquire timeout and come back `PoolTimedOut`. A
    /// wider pool would hide every `_in` regression in this file behind a second
    /// connection that reads and writes outside the unit. This is the argument
    /// `AGENTS.md` makes about the Phase A tests, and it is why there is one
    /// pool builder here and not a per-test convenience.
    async fn test_pool() -> SqlitePool {
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

    /// `allow_stock = true` and `allow_negative = true` are what make the happy
    /// paths reachable at all on a fresh pool: a strict build would refuse a
    /// return of goods the shop is not holding, and a refund into an account it
    /// has never been funded from. A test that needs the STRICT branch builds
    /// with `svc_with_flags` rather than getting it by accident — the same
    /// trap `AGENTS.md` records for `svc()` in the purchase tests.
    async fn svc_with_flags(allow_stock: bool, allow_balance: bool) -> (Svc, SqlitePool) {
        let pool = test_pool().await;
        let s = PurchaseReturnService::new(
            SqlitePurchaseReturnRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            SqlitePurchaseRepository::new(pool.clone()),
            InventoryService::new(
                SqliteCategoryRepository::new(pool.clone()),
                SqliteProductRepository::new(pool.clone()),
                SqliteBarcodeRepository::new(pool.clone()),
                SqliteStockMovementRepository::new(pool.clone()),
                allow_stock,
            ),
            TransactionService::new(
                SqliteAccountRepository::new(pool.clone()),
                SqliteTransactionRepository::new(pool.clone()),
                allow_balance,
            ),
            SqlitePartyLedgerRepository::new(pool.clone()),
            SqlitePaymentRepository::new(pool.clone()),
        );
        (s, pool)
    }

    async fn svc() -> (Svc, SqlitePool) {
        svc_with_flags(true, true).await
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// The parent's day. Every fixture purchase is bought on it.
    fn purchase_date() -> NaiveDate {
        d(2024, 5, 2)
    }

    /// The return's own day, a MONTH later. A return is dated when it is MADE,
    /// so the number's year and the stock movement's date are not the parent's.
    fn return_date() -> NaiveDate {
        d(2024, 6, 1)
    }

    async fn actor(pool: &SqlitePool) -> i64 {
        test_support::audit_actor_id(pool).await.unwrap()
    }

    /// Everything a return test needs to address: the product whose stock moves,
    /// one line of the confirmed purchase it reverses, and the draft return
    /// standing against it.
    struct Parent {
        product_id: i64,
        purchase_line_id: i64,
        return_id: i64,
    }

    /// A tracked product already holding `stock` units, because a purchase
    /// return takes goods OFF the shelf.
    async fn seed_product(
        s: &Svc,
        pool: &SqlitePool,
        sku: &str,
        stock: &str,
    ) -> crate::models::Product {
        let who = actor(pool).await;
        let product = s
            .inventory
            .create_product(
                who,
                NewProduct {
                    sku: sku.into(),
                    name: format!("prod {sku}"),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec("10"),
                    cost_price: dec("3"),
                    track_stock: true,
                    min_stock: Some(dec("1")),
                    max_stock: Some(dec("99")),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        s.inventory
            .record_movement(
                who,
                NewMovement {
                    product_id: product.id,
                    qty: dec(stock),
                    movement_type: MovementType::In,
                    reason: MovementReason::Initial,
                    reference: "".into(),
                    date: purchase_date(),
                },
            )
            .await
            .unwrap();
        product
    }

    async fn seed_supplier(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO suppliers (name, is_active, created_by) VALUES (?, 1, ?) RETURNING id",
        )
        .bind(name)
        .bind(actor(pool).await)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_account(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id")
            .bind(name)
            .bind(actor(pool).await)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A method the named account OWNS. Migration 44 guards the
    /// (account_id, method_id) pair on the payment row, and the seeded methods
    /// are unassigned on a fresh database, so the fixture builds the pair.
    async fn owned_method(pool: &SqlitePool, account: i64) -> i64 {
        match sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE name = 'wallet cash' AND account_id = ?",
        )
        .bind(account)
        .fetch_optional(pool)
        .await
        .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO payment_methods (name, account_id, created_by)\n                 VALUES ('wallet cash', ?, ?) RETURNING id",
            )
            .bind(account)
            .bind(actor(pool).await)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    /// One CONFIRMED purchase carrying one line, seeded through raw SQL because
    /// the parent's own confirm path is `PurchasesService`'s subject and is
    /// tested there. `number` is a caller-chosen unique purchase number, because
    /// `purchase_number` is UNIQUE and several of these tests need two parents.
    async fn seed_parent(
        pool: &SqlitePool,
        supplier_id: i64,
        product_id: i64,
        qty: &str,
        unit_cost: &str,
        number: &str,
    ) -> (i64, i64) {
        let purchase = sqlx::query_scalar(
            r#"INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, purchase_number, created_by)
               VALUES (?, 'Confirmed', 'Credit', ?, ?, ?) RETURNING id"#,
        )
        .bind(supplier_id)
        .bind(purchase_date())
        .bind(number)
        .bind(actor(pool).await)
        .fetch_one(pool)
        .await
        .unwrap();
        let line = add_parent_line(pool, purchase, product_id, qty, unit_cost).await;
        (purchase, line)
    }

    /// A SECOND line on an existing parent, of a DIFFERENT product — which is
    /// the only shape a purchase can carry, since it refuses a repeated product.
    /// This is what gives a return two stock movements, which is the window the
    /// residue tests need.
    async fn add_parent_line(
        pool: &SqlitePool,
        purchase_id: i64,
        product_id: i64,
        qty: &str,
        unit_cost: &str,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO purchase_lines (purchase_id, product_id, qty, unit_cost) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(purchase_id)
        .bind(product_id)
        .bind(qty)
        .bind(unit_cost)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// What the parent has COLLECTED. Each tuple is `(account_name, amount)` and
    /// produces a real `transactions` row plus the `purchase_payments` row that
    /// claims it, because the refund cap reads the PAYMENTS and the refund
    /// allocation reads their accounts: a payment with no finance behind it is
    /// not a shape the real document path ever produces.
    async fn collect(pool: &SqlitePool, purchase_id: i64, number: &str, amounts: &[(&str, &str)]) {
        let who = actor(pool).await;
        for (account_name, amount) in amounts {
            let account = seed_account(pool, account_name).await;
            let method = owned_method(pool, account).await;
            let tx: i64 = sqlx::query_scalar(
                r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by)
                   VALUES (?, 'Expense', ?, 'paid the supplier', ?, ?, ?) RETURNING id"#,
            )
            .bind(account)
            .bind(dec(amount).to_string())
            .bind(number)
            .bind(purchase_date())
            .bind(who)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query(
                r#"INSERT INTO purchase_payments (purchase_id, account_id, method_id, amount, date, transaction_id, created_by)
                   VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(purchase_id)
            .bind(account)
            .bind(method)
            .bind(dec(amount).to_string())
            .bind(purchase_date())
            .bind(tx)
            .bind(who)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    /// Every happy-path test starts from this: a tracked product holding 20
    /// units, a supplier, a CONFIRMED purchase of `qty` units at `unit_cost` that
    /// collected `collected`, and a DRAFT return of `returned` of them.
    async fn draft_return(
        s: &Svc,
        pool: &SqlitePool,
        sku: &str,
        supplier: &str,
        qty: &str,
        unit_cost: &str,
        collected: &[(&str, &str)],
        returned: &str,
    ) -> Parent {
        let number = format!("2024-PURCH-{sku}");
        let product = seed_product(s, pool, sku, "20").await;
        let supplier_id = seed_supplier(pool, supplier).await;
        let (purchase, line) =
            seed_parent(pool, supplier_id, product.id, qty, unit_cost, &number).await;
        collect(pool, purchase, &number, collected).await;
        let who = actor(pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line, dec(returned))
            .await
            .unwrap();
        Parent {
            product_id: product.id,
            purchase_line_id: line,
            return_id: purchase_return.id,
        }
    }

    /// **T3d on the purchase-return side**, the one direction where a reversal is an
    /// `Expense` and can be refused for want of funds.
    #[tokio::test]
    async fn a_failure_while_cancelling_a_purchase_return_rolls_the_reversals_back() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "T3D-PR-ATOMIC",
            "T3D PR Atomic Supplier",
            "2",
            "5",
            &[("t3d pr till", "10")],
            "2",
        )
        .await;
        s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        let tx_before = tx_count(&pool).await;
        sqlx::raw_sql(
            "CREATE TRIGGER injected_cancel_failure BEFORE UPDATE ON purchase_returns \
             WHEN NEW.status = 'Cancelled' \
             BEGIN SELECT RAISE(ABORT, 'injected failure after the reversals'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .cancel(actor(&pool).await, p.return_id, Some("injected".into()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected failure"),
            "the fixture must be the thing that failed, got {err}"
        );
        assert_eq!(
            tx_count(&pool).await,
            tx_before,
            "the reversal movement must die with the unit"
        );
        let reversals: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM purchase_return_payments WHERE refund_transaction_id IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reversals, 0, "and no row claims one");
    }

    // -- the party ledger (T2) ---------------------------------------------------

    /// The journal rows this return wrote, as the sign rule stored them.
    async fn ledger_rows(pool: &SqlitePool, document_id: i64) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, amount FROM party_ledger_entries \
             WHERE document_kind = 'PurchaseReturn' AND document_id = ? ORDER BY id",
        )
        .bind(document_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A purchase return journals goods as Return and each cash delivery as Refund.
    #[tokio::test]
    async fn a_confirmed_purchase_return_appends_one_return_on_the_supplier() {
        let (s, pool) = svc().await;
        // Two units at 5 is 10, and the whole 10 was paid, so the refund plan is
        // full and the cap is not what this test is about.
        let p = draft_return(
            &s,
            &pool,
            "LEDGER-PR",
            "Ledger PR Supplier",
            "2",
            "5",
            &[("ledger pr till", "10")],
            "2",
        )
        .await;

        s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        assert_eq!(
            ledger_rows(&pool, p.return_id).await,
            vec![
                ("Refund".to_string(), "10".to_string()),
                ("Return".to_string(), "-10".to_string()),
            ],
            "goods reduce the payable and cash returned settles that amount"
        );
    }

    /// A fully paid credit purchase returned in cash has four named ledger
    /// movements and nets to zero, mirroring the customer-side identity.
    #[tokio::test]
    async fn a_fully_paid_purchase_refunded_by_purchase_return_folds_to_zero() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "LEDGER-IDENTITY",
            "Ledger Identity Supplier",
            "2",
            "6",
            &[("identity supplier till", "12")],
            "2",
        )
        .await;
        let (purchase_id, supplier_id): (i64, i64) = sqlx::query_as(
            "SELECT purchase_id, supplier_id FROM purchase_returns WHERE id = ?",
        )
        .bind(p.return_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let ledger = SqlitePartyLedgerRepository::new(pool.clone());
        let who = actor(&pool).await;
        for (kind, amount) in [
            (crate::models::PartyEntryKind::Charge, dec("12")),
            (crate::models::PartyEntryKind::Payment, dec("12")),
        ] {
            ledger
                .insert(&crate::models::NewPartyLedgerEntry {
                    party_type: crate::models::PartyType::Supplier,
                    party_id: supplier_id,
                    kind,
                    amount: kind.signed_amount(amount),
                    document_kind: crate::models::PartyDocumentKind::Purchase,
                    document_id: purchase_id,
                    entry_date: purchase_date(),
                    reference: Some("2024-PURCH-LEDGER-IDENTITY".into()),
                    created_by: who,
                })
                .await
                .unwrap();
        }

        s.confirm(who, p.return_id).await.unwrap();

        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT kind, amount, document_kind, document_id FROM party_ledger_entries \
             WHERE party_type = 'Supplier' AND party_id = ? ORDER BY id",
        )
        .bind(supplier_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Charge".into(), "12".into(), "Purchase".into(), purchase_id),
                ("Payment".into(), "-12".into(), "Purchase".into(), purchase_id),
                (
                    "Refund".into(),
                    "12".into(),
                    "PurchaseReturn".into(),
                    p.return_id,
                ),
                (
                    "Return".into(),
                    "-12".into(),
                    "PurchaseReturn".into(),
                    p.return_id,
                ),
            ],
            "the journal names all four obligation movements on their owning documents"
        );
        let balance = SqlitePartyLedgerRepository::new(pool.clone())
            .balance_for_party(crate::models::PartyType::Supplier, supplier_id)
            .await
            .unwrap();
        assert_eq!(balance, Decimal::ZERO);
    }

    /// THE case the empty refund plan exists for: a purchase confirmed but NOT
    /// paid still returns goods, so the payable comes down while no money moves.
    /// An implementation that wrote the entry only alongside a refund would leave
    /// the shop owing for goods it sent back.
    #[tokio::test]
    async fn an_unpaid_parent_still_returns_the_payable_with_no_refund() {
        let (s, pool) = svc().await;
        // Nothing paid: the fixture's refund plan is empty.
        let p = draft_return(
            &s,
            &pool,
            "LEDGER-PR-UNPAID",
            "Ledger PR Unpaid Supplier",
            "2",
            "5",
            &[],
            "2",
        )
        .await;

        s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        assert_eq!(
            ledger_rows(&pool, p.return_id).await,
            vec![("Return".to_string(), "-10".to_string())],
            "the goods went back, so the payable comes down; the empty refund plan adds no Refund"
        );
        assert_eq!(
            tx_count(&pool).await,
            0,
            "and no cash entry exists: the Return is about the goods, not the money"
        );
        let supplier_id: i64 =
            sqlx::query_scalar("SELECT supplier_id FROM purchase_returns WHERE id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let balance = SqlitePartyLedgerRepository::new(pool.clone())
            .balance_for_party(crate::models::PartyType::Supplier, supplier_id)
            .await
            .unwrap();
        assert_eq!(
            balance,
            dec("-10"),
            "without a Charge or refund, the sole Return is the supplier's balance"
        );
    }

    /// The write joins the caller's unit, so a failure after it takes the entry
    /// with it.
    #[tokio::test]
    async fn a_failed_purchase_return_confirm_rolls_its_return_entry_back() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "LEDGER-PR-RB",
            "Ledger PR Rollback Supplier",
            "2",
            "5",
            &[("ledger pr rb till", "10")],
            "2",
        )
        .await;

        sqlx::raw_sql(
            "CREATE TRIGGER injected_ledger_probe BEFORE UPDATE ON purchase_returns \
             WHEN NEW.status = 'Confirmed' \
             BEGIN SELECT RAISE(ABORT, 'injected failure after the ledger write'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .confirm(actor(&pool).await, p.return_id)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("injected failure"),
            "the fixture must be the thing that failed, got {err}"
        );
        assert!(
            ledger_rows(&pool, p.return_id).await.is_empty(),
            "the entry must die with the unit that wrote it"
        );
    }

    // -- the residue tables, read straight from the database -----------------

    async fn movement_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn movement_reasons(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT reason FROM stock_movements ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn tx_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM transactions")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn tx_kinds(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT kind FROM transactions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn tx_references(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT COALESCE(reference, '') FROM transactions ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn payment_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_return_payments")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    /// What `next_number` actually spent. `None` means the `doc_sequences` row
    /// never existed at all, which is a STRONGER observation than a counter that
    /// went back: a gap can be repaired by hand and nobody would notice, a
    /// missing row cannot.
    async fn pret_sequence_last(pool: &SqlitePool) -> Option<i64> {
        sqlx::query_as::<_, (i64,)>("SELECT last_number FROM doc_sequences WHERE doc_type = 'PRET'")
            .fetch_optional(pool)
            .await
            .unwrap()
            .map(|r| r.0)
    }

    async fn row_state(pool: &SqlitePool, return_id: i64) -> (String, Option<String>) {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, return_number FROM purchase_returns WHERE id = ?",
        )
        .bind(return_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The whole cost satellite as comparable TEXT, one row per line. Byte for
    /// byte is the claim: a confirmed return must leave every column of every row
    /// exactly as it found them — `current_cost`, `previous_cost`, their dates,
    /// the trigger-maintained `updated_at`, the preference flag and the supplier
    /// SKU. Every one of those is in the projection because every one of them is
    /// something an UPDATE would move.
    async fn satellite(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar(
            r#"SELECT product_id || '|' || supplier_id || '|' || current_cost || '|'
                      || current_cost_date || '|'
                      || COALESCE(previous_cost, '-') || '|'
                      || COALESCE(previous_cost_date, '-') || '|'
                      || is_preferred || '|'
                      || COALESCE(supplier_sku, '-') || '|'
                      || created_by || '|' || COALESCE(updated_by, '-') || '|'
                      || created_at || '|' || updated_at
               FROM product_supplier_costs ORDER BY id"#,
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// The house failure-injection technique: a trigger that ABORTs on one
    /// specific statement, so `confirm` fails at exactly the step under test and
    /// nowhere else. `sqlx::AssertSqlSafe` is the wrapper every other
    /// failure-injection test in this crate uses.
    async fn inject(pool: &SqlitePool, ddl: String) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
            .execute(pool)
            .await
            .unwrap();
    }

    // ========================================================================
    // THE RETURN
    // ========================================================================

    /// The headline case, end to end. Two of three units go back, at the price
    /// the PURCHASE froze, the stock leaves the shelf, and the money comes back
    /// into the shop as an INCOME against the account it left through.
    ///
    /// Every claim is read from the database rather than from the view the
    /// service returns, because the view is derived from the same rows and would
    /// agree even if the writes had gone somewhere else.
    #[tokio::test]
    async fn a_return_of_part_of_a_purchase_freezes_the_parents_cost_and_takes_the_money_back_as_an_income(
    ) {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "BASE",
            "Base Supplier",
            "3",
            "4",
            &[("base wallet", "12")],
            "2",
        )
        .await;

        let detail = s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        // The line took the PARENT's frozen cost, not anything the operator
        // could have typed: the API has no price to type into.
        assert_eq!(detail.lines[0].purchase_line_id, p.purchase_line_id);
        assert_eq!(detail.lines[0].qty, dec("2"));
        assert_eq!(
            detail.lines[0].unit_cost,
            dec("4"),
            "a return line is at the parent line's cost, frozen when the line was added"
        );

        // The document total is the RETURNED quantity at that cost, not the
        // parent's whole quantity: 2 x 4, not 3 x 4.
        assert_eq!(detail.total, dec("8"));
        assert_eq!(
            detail.net_subtotal, detail.total,
            "a return line freezes no tax, so there is nothing to part out"
        );
        assert_eq!(detail.paid, dec("8"), "the full value came back");
        assert_eq!(detail.due, Decimal::ZERO);
        assert_eq!(detail.payment_status, PaymentStatus::Paid);

        // The stock left, and the movement says WHY in the vocabulary that
        // already existed for the physical event (decision 3).
        assert_eq!(
            movement_reasons(&pool).await,
            vec!["Initial", "Purchase-return"],
            "the goods went out with reason Purchase-return, which the stock CHECK already accepted"
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            dec("18"),
            "two units left a shelf that held twenty"
        );

        // The money came in as an INCOME, not an Expense: an Income is money
        // ENTERING, which is the whole difference from a credit note.
        assert_eq!(
            tx_kinds(&pool).await,
            vec!["Expense", "Income"],
            "the parent's own payment was an Expense; the refund is an Income"
        );
        assert_eq!(
            tx_references(&pool).await,
            vec!["2024-PURCH-BASE", "2024-PRET-000001"],
            "the refund is stamped with the RETURN's own number, not the parent's"
        );
        assert_eq!(payment_count(&pool).await, 1);
        assert_eq!(
            detail.payments[0].transaction_id.is_some(),
            true,
            "the refund row claims the finance row it produced"
        );

        // The number was taken in the return's own year under the short prefix.
        assert_eq!(
            detail.purchase_return.return_number.as_deref(),
            Some("2024-PRET-000001")
        );
        assert_eq!(
            detail.purchase_return.status,
            PurchaseReturnStatus::Confirmed
        );
        assert_eq!(pret_sequence_last(&pool).await, Some(1));
    }

    /// The refund-table EXEMPTION from migration 44, proved end to end: a
    /// refund does not CHOOSE a pair, it REPLAYS the parent payment's pair
    /// (`RefundPlan` copies pay.account_id / pay.method_id). After the method
    /// is re-pointed to ANOTHER account, the return still confirms and the
    /// money comes back out of the box it went into — the method's current
    /// owner never receives it.
    #[tokio::test]
    async fn a_refund_replays_the_parent_payments_account_even_after_the_method_is_repointed() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "HISTORY",
            "History Supplier",
            "3",
            "4",
            &[("history wallet", "12")],
            "3",
        )
        .await;
        let who = actor(&pool).await;
        // Re-point the collected method to another account: from here on the
        // method's current owner is NOT the box the money went into.
        sqlx::query("INSERT INTO accounts (name, created_by) VALUES ('other box', ?) RETURNING id")
            .bind(who)
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE payment_methods SET account_id = (SELECT id FROM accounts WHERE name = 'other box') WHERE name = 'wallet cash'",
        )
        .execute(&pool)
        .await
        .unwrap();

        s.confirm(who, p.return_id).await.unwrap();

        // The refund row keeps the PARENT payment's account — the historical
        // fact of where the money landed.
        let (wallet_account,): (i64,) =
            sqlx::query_as("SELECT id FROM accounts WHERE name = 'history wallet'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (refund_account,): (i64,) =
            sqlx::query_as("SELECT account_id FROM purchase_return_payments WHERE return_id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            refund_account, wallet_account,
            "the money goes back out of the box it went into"
        );
        // The refund's own finance row lands in that same historical account.
        let (income_account,): (i64,) =
            sqlx::query_as("SELECT account_id FROM transactions WHERE kind = 'Income'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            income_account, wallet_account,
            "the refund Income is stamped with the historical account"
        );
    }

    /// A return is a document about a document that EXISTS. A Draft parent has
    /// no settled price to return against and its lines can still be edited; a
    /// cancelled one already sent its goods back. Both are refused at the parent
    /// read, before a row is written.
    #[tokio::test]
    async fn a_return_of_a_draft_or_cancelled_purchase_is_refused() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "PARENT", "20").await;
        let supplier_id = seed_supplier(&pool, "Parent State Supplier").await;
        let who = actor(&pool).await;

        let draft: i64 = sqlx::query_scalar(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by) \
             VALUES (?, 'Draft', 'Credit', ?, ?) RETURNING id",
        )
        .bind(supplier_id)
        .bind(purchase_date())
        .bind(who)
        .fetch_one(&pool)
        .await
        .unwrap();
        match s
            .create_draft(who, draft, return_date(), &None)
            .await
            .unwrap_err()
        {
            AppError::Validation(msg) => assert!(
                msg.contains("Confirmed") && msg.contains("Draft"),
                "the refusal must name the state the parent rests in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        let cancelled: i64 = sqlx::query_scalar(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, purchase_number, created_by) \
             VALUES (?, 'Cancelled', 'Credit', ?, '2024-PURCH-OLD', ?) RETURNING id",
        )
        .bind(supplier_id)
        .bind(purchase_date())
        .bind(who)
        .fetch_one(&pool)
        .await
        .unwrap();
        add_parent_line(&pool, cancelled, product.id, "3", "4").await;
        match s
            .create_draft(who, cancelled, return_date(), &None)
            .await
            .unwrap_err()
        {
            AppError::Validation(msg) => assert!(
                msg.contains("Cancelled"),
                "a cancelled parent's goods already went back: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        let returns: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_returns")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(returns, 0, "neither refusal wrote a row");
    }

    /// A confirmed return is not editable: its lines are the evidence of WHAT
    /// came back, so a closed document's line is history rather than an editable
    /// row. The repository carries the same predicate in its own WHERE; this
    /// proves the SERVICE reaches it and answers with a Validation naming the
    /// state rather than a bare `Conflict`.
    #[tokio::test]
    async fn a_confirmed_return_refuses_every_line_edit_and_says_why() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "FROZEN",
            "Frozen Supplier",
            "3",
            "4",
            &[("frozen wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let cases: Vec<(&str, Option<AppError>)> = vec![
            (
                "add_line",
                s.add_line(who, p.return_id, p.purchase_line_id, dec("1"))
                    .await
                    .err(),
            ),
            (
                "update_line",
                s.update_line(who, return_line, dec("1")).await.err(),
            ),
            ("remove_line", s.remove_line(who, return_line).await.err()),
            (
                "update_draft",
                s.update_draft(who, p.return_id, return_date(), "edited")
                    .await
                    .err(),
            ),
        ];
        for (what, err) in cases {
            match err {
                Some(AppError::Validation(msg)) => assert!(
                    msg.contains("Confirmed"),
                    "{what} must name the state the return rests in: {msg}"
                ),
                Some(other) => panic!("{what}: expected Validation, got {other:?}"),
                None => panic!("{what} was allowed on a confirmed return"),
            }
        }
    }

    /// A return line's QUANTITY is the only thing an operator may move. The
    /// frozen cost survives an edit, and that is not an accident of the tests:
    /// `update_line` takes no price argument and the repository's `update_line`
    /// takes none either, so there is no value a caller could supply to move it.
    #[tokio::test]
    async fn editing_a_return_line_moves_its_quantity_and_never_its_price() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "QTY",
            "Quantity Supplier",
            "3",
            "4",
            &[("qty wallet", "12")],
            "1",
        )
        .await;
        let who = actor(&pool).await;
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let edited = s.update_line(who, return_line, dec("2")).await.unwrap();
        assert_eq!(edited.qty, dec("2"));
        assert_eq!(
            edited.unit_cost,
            dec("4"),
            "the price is a copy of a frozen value and has no argument that could rewrite it"
        );

        // A zero quantity is refused on the write path, so a line can never be
        // edited into a zero-value row that a total fold would silently ignore.
        match s
            .update_line(who, return_line, Decimal::ZERO)
            .await
            .unwrap_err()
        {
            AppError::Validation(msg) => assert!(msg.contains("qty must be > 0"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    // ========================================================================
    // THE RETURNABLE-QUANTITY RULE
    // ========================================================================

    /// A return line may never claim more units than the purchase line actually
    /// bought. This is the half of the rule this layer can state, and it is a
    /// real bound rather than a placeholder: it closes the over-return an
    /// operator types, before a row or a number exists.
    ///
    /// The comment on `ensure_within_parent` records why the OTHER half — the
    /// subtraction of what earlier CONFIRMED returns already took — is absent,
    /// and this test is the honest shape of the gap: on a fresh purchase the
    /// ceiling is the parent's own quantity, and nothing below this line should
    /// be read as proving that a SECOND return of the same line is bounded.
    #[tokio::test]
    async fn returning_more_units_than_the_purchase_line_bought_is_refused() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "OVER", "20").await;
        let supplier_id = seed_supplier(&pool, "Over Supplier").await;
        let (purchase, line) =
            seed_parent(&pool, supplier_id, product.id, "5", "4", "2024-PURCH-OVER").await;
        collect(&pool, purchase, "2024-PURCH-OVER", &[("over wallet", "20")]).await;
        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();

        match s
            .add_line(who, purchase_return.id, line, dec("6"))
            .await
            .unwrap_err()
        {
            AppError::Validation(msg) => assert!(
                msg.contains('6') && msg.contains('5'),
                "the refusal must state both the figure typed and the figure the line holds: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        // The exact quantity is admitted, and the refusal wrote nothing.
        s.add_line(who, purchase_return.id, line, dec("5"))
            .await
            .unwrap();
        let lines: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_return_lines")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(lines, 1, "the refused line wrote nothing at all");
    }

    /// The same ceiling on the other two write paths, because `update_line` is a
    /// separate entry to the document and `confirm` re-reads its own lines: a
    /// check that lived only on `add_line` would be one an operator walks around
    /// by adding a valid line and then widening it.
    #[tokio::test]
    async fn the_quantity_ceiling_holds_on_update_line_and_on_confirm_too() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "CEIL",
            "Ceiling Supplier",
            "4",
            "2",
            &[("ceiling wallet", "8")],
            "1",
        )
        .await;
        let who = actor(&pool).await;
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let err = s.update_line(who, return_line, dec("9")).await.unwrap_err();
        assert!(
            matches!(&err, AppError::Validation(msg) if msg.contains("9") && msg.contains("4")),
            "widening a return line past the parent's own quantity is refused: {err:?}"
        );
        assert_eq!(
            s.returns.list_lines(p.return_id).await.unwrap()[0].qty,
            dec("1"),
            "the refused edit left the line exactly as it was"
        );

        // A quantity the service itself would refuse is still refused at
        // CONFIRM when it reaches the row by another route, so a confirm can
        // never accept goods the write path would not have accepted.
        sqlx::query("UPDATE purchase_return_lines SET qty = '9' WHERE id = ?")
            .bind(return_line)
            .execute(&pool)
            .await
            .unwrap();
        let err = s.confirm(who, p.return_id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::Validation(msg) if msg.contains("9")),
            "confirm re-checks the ceiling rather than trusting how the line was written: {err:?}"
        );
        assert_eq!(pret_sequence_last(&pool).await, None, "no number was spent");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the fixture's own stock-in survives"
        );
    }

    /// A return's quantity of a parent line is ONE quantity, so the same line
    /// cannot appear twice on one return. `UNIQUE (return_id, purchase_line_id)`
    /// is the schema's backstop and this proves the repository's `Conflict`
    /// reaches a caller as a stated refusal rather than a raw constraint error.
    #[tokio::test]
    async fn the_same_parent_line_cannot_appear_twice_on_one_return() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "DUPL",
            "Duplicate Supplier",
            "3",
            "4",
            &[("dupl wallet", "12")],
            "1",
        )
        .await;
        let err = s
            .returns
            .create_line(p.return_id, p.purchase_line_id, dec("1"), dec("4"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Conflict(msg) if msg.contains("already on this purchase return")),
            "the schema refuses a second line for the same parent line: {err:?}"
        );
        let lines: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_return_lines")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(lines, 1);
    }

    /// THE REPEAT-RETURN HOLE, closed. Two CONFIRMED returns of the same parent
    /// line are the shape that refunds twice: buy 5, return them, return them
    /// again, and the supplier is paid for the same goods twice.
    ///
    /// The first return is legal and spends the whole allowance. The second one
    /// names the same parent line, so the ceiling it must respect is the
    /// parent's own quantity MINUS what the first return already took — and
    /// that is zero.
    ///
    /// **THE REFUSAL LANDS TWICE, and both are asserted here.** `add_line`
    /// refuses it, because the aggregate measures CONFIRMED returns and the first
    /// one is confirmed — an early refusal with the figures the operator needs,
    /// on the same path where typing 9 against a line of 5 is already refused.
    /// Then the line is put on the second draft by RAW SQL and `confirm` refuses
    /// it again, which is the assertion that matters: the write path's guard is
    /// not what makes a confirm safe, and a confirm that trusted how its lines
    /// were written would refund the supplier twice.
    ///
    /// This test is written against the API the service already had when the
    /// hole was open, and it is the RED that proved it: both `add_line` and
    /// `confirm` answered `Ok` for the second document, because
    /// `ensure_within_parent` could see only the parent line's own qty.
    #[tokio::test]
    async fn a_second_confirmed_return_of_the_same_parent_line_is_refused_because_the_allowance_is_spent(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SPENT", "20").await;
        let supplier_id = seed_supplier(&pool, "Spent Allowance Supplier").await;
        let number = "2024-PURCH-SPENT";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "5", "4", number).await;
        collect(&pool, purchase, number, &[("spent wallet", "20")]).await;
        let who = actor(&pool).await;

        let first = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("5")).await.unwrap();
        let first_detail = s.confirm(who, first.id).await.unwrap();
        assert_eq!(
            first_detail.total,
            dec("20"),
            "the first return really did take the whole line: the refund cap is not \
             what limits the second one, this test would pass for the wrong reason \
             otherwise"
        );

        // -- the WRITE path refuses, early and with the figures ----------------
        let second = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        let err = s
            .add_line(who, second.id, line, dec("5"))
            .await
            .expect_err("the allowance is spent: nothing of this line is returnable")
            .to_string();
        assert!(
            err.contains('5') && err.contains("already"),
            "the refusal must state the quantity claimed and that an earlier \
             CONFIRMED return already has it: {err}"
        );
        assert_eq!(
            s.returns.list_lines(second.id).await.unwrap().len(),
            0,
            "the refused line wrote nothing"
        );

        // -- and the CONFIRM path refuses it too, on its own -------------------
        // The line arrives by raw SQL so the confirm cannot be relying on
        // `add_line`'s guard having run.
        sqlx::query(
            r#"INSERT INTO purchase_return_lines (return_id, purchase_line_id, qty, unit_cost)
               VALUES (?, ?, '5', '4')"#,
        )
        .bind(second.id)
        .bind(line)
        .execute(&pool)
        .await
        .unwrap();

        match s.confirm(who, second.id).await.unwrap_err() {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains('5') && msg.contains('5'),
                    "the refusal must state the quantity claimed and the parent's own: {msg}"
                );
                assert!(
                    msg.contains("already"),
                    "and it must say the allowance is spent by earlier CONFIRMED \
                     returns, which is the term the rule subtracts: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // The refusal bought EARLY exit: the second document never became a
        // document, and the shop was not refunded twice.
        assert_eq!(
            row_state(&pool, second.id).await,
            ("Draft".to_string(), None),
            "the refused second return must leave no numbered document behind"
        );
        assert_eq!(
            payment_count(&pool).await,
            1,
            "ONE refund exists: the supplier was paid once, for the one real return"
        );
        assert_eq!(
            tx_count(&pool).await,
            2,
            "the fixture's own payment and the one refund the first return produced"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("15"),
            "five units left the shelf, not ten: the second return moved no stock"
        );
    }

    // ========================================================================
    // THE REFUND CAP
    // ========================================================================

    /// A parent that collected NOTHING is not a refusal. The cap is about how
    /// much MONEY moves, not about whether the goods move: the return goes
    /// through, the goods go back, and no payment row is written because there
    /// was nothing to refund. Migration 40 says this in its own comment.
    #[tokio::test]
    async fn a_return_against_a_confirmed_but_unpaid_purchase_writes_no_payment_row() {
        let (s, pool) = svc().await;
        let p = draft_return(&s, &pool, "UNPAID", "Unpaid Supplier", "3", "4", &[], "2").await;
        let detail = s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        assert_eq!(
            detail.purchase_return.status,
            PurchaseReturnStatus::Confirmed,
            "the goods still went back: an unpaid parent is not a reason to keep them"
        );
        assert!(detail.payments.is_empty(), "there was nothing to refund");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            tx_count(&pool).await,
            0,
            "and no finance row exists: the parent collected nothing, so the return took nothing"
        );
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("8"));
        assert_eq!(
            detail.payment_status,
            PaymentStatus::Unpaid,
            "the return is worth 8 and none of it has come back"
        );
        assert_eq!(
            movement_count(&pool).await,
            2,
            "the stock DID move: initial in, plus the return out"
        );
    }

    /// A parent that collected SOMETHING is a ceiling, and exceeding it is
    /// refused with the shortfall named. This is v1's answer and not a solution:
    /// the app has no notion of a credit owed BY a supplier, so a shop that buys
    /// on credit and returns the goods before paying has nowhere for its case to
    /// live. No credit-balance mechanism is invented here to make it fit.
    #[tokio::test]
    async fn a_return_worth_more_than_the_parent_collected_is_refused_naming_the_shortfall() {
        let (s, pool) = svc().await;
        // Three units at 4 is 12; only 4 was collected.
        let p = draft_return(
            &s,
            &pool,
            "CAP",
            "Cap Supplier",
            "3",
            "4",
            &[("cap wallet", "4")],
            "3",
        )
        .await;
        let err = s
            .confirm(actor(&pool).await, p.return_id)
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("12") && msg.contains('4'),
                    "the refusal must state what the return is worth and what came in: {msg}"
                );
                assert!(
                    msg.contains("credit"),
                    "and it must say why the difference has nowhere to go: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // The refusal bought EARLY exit: no number, no movement, no money.
        assert_eq!(pret_sequence_last(&pool).await, None);
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the fixture's own stock-in"
        );
        assert_eq!(tx_count(&pool).await, 1, "only the fixture's own payment");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None)
        );
    }

    /// The allocation the cap is implemented with, read end to end: a return
    /// worth 10 against payments of 4 and 6 produces TWO refund rows, in the
    /// accounts the parent's payments came from, for 4 and 6 — not one row for
    /// 10 in the first account.
    ///
    /// This is what `PurchasesService::cancel` does when it refunds a purchase,
    /// and it is the only allocation a return can justify: the return does not
    /// know how the purchase was paid, and the parent does.
    #[tokio::test]
    async fn a_refund_is_split_across_the_accounts_the_parents_payments_came_from() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SPLIT", "20").await;
        let supplier_id = seed_supplier(&pool, "Split Supplier").await;
        let number = "2024-PURCH-SPLIT";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "5", "2", number).await;
        collect(
            &pool,
            purchase,
            number,
            &[("split first", "4"), ("split second", "6")],
        )
        .await;
        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line, dec("5"))
            .await
            .unwrap();

        let detail = s.confirm(who, purchase_return.id).await.unwrap();

        assert_eq!(detail.total, dec("10"));
        assert_eq!(
            detail.payments.len(),
            2,
            "one refund per originating account"
        );
        let mut amounts: Vec<Decimal> = detail.payments.iter().map(|p| p.amount).collect();
        amounts.sort();
        assert_eq!(amounts, vec![dec("4"), dec("6")]);
        assert_eq!(
            ledger_rows(&pool, purchase_return.id).await,
            vec![
                ("Refund".to_string(), "4".to_string()),
                ("Refund".to_string(), "6".to_string()),
                ("Return".to_string(), "-10".to_string()),
            ],
            "the split refund plan writes one return-located Refund for each delivery"
        );

        let accounts: Vec<i64> =
            sqlx::query_scalar("SELECT account_id FROM purchase_return_payments ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(accounts.len(), 2);
        assert_ne!(
            accounts[0], accounts[1],
            "two refunds in one account would not be an allocation at all"
        );

        // Each finance row is claimed by exactly the payment row that made it:
        // there is no orphan on either side.
        let orphans: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transactions t \
             WHERE NOT EXISTS (SELECT 1 FROM purchase_payments pp WHERE pp.transaction_id = t.id \
                               OR pp.refund_transaction_id = t.id \
                               OR pp.refund_transaction_id IS NULL AND pp.transaction_id IS NULL) \
             AND NOT EXISTS (SELECT 1 FROM purchase_return_payments rp WHERE rp.transaction_id = t.id \
                               OR rp.refund_transaction_id = t.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            orphans.0, 0,
            "every finance row is claimed by a payment row"
        );
    }

    /// **A confirmed return is NEVER partly refunded, and this test is the
    /// proof.** It is a consequence of the cap taken to its conclusion, and it
    /// is worth stating because `PurchaseReturnDetail` carries a
    /// `payment_status` field a reader will assume can be `Partial`.
    ///
    /// The reachable shapes are exactly three:
    ///
    /// * the parent collected NOTHING — the refund plan is empty, `paid` is 0
    ///   and the status is `Unpaid` (see the unpaid test above);
    /// * the parent collected AT LEAST the return is worth — the plan covers the
    ///   whole value, `paid == total` and the status is `Paid`;
    /// * the parent collected SOMETHING but less than the return is worth — that
    ///   is the refusal the cap exists for, so no document is created at all.
    ///
    /// There is no fourth shape in which a document exists and is part-refunded,
    /// because the only code that writes a refund is the plan inside `confirm`
    /// and the plan either covers the whole value or never runs. A `Partial`
    /// return would need a refund recorded AFTER the document exists, and no
    /// such method is in this service's set — `create_payment` deliberately has
    /// no state gate for exactly that future caller, the same arrangement
    /// `record_payment` has on a purchase.
    ///
    /// So the honest assertion is not "a part-paid return reads Partial" — it is
    /// that the third shape is refused and the status is therefore unreachable.
    #[tokio::test]
    async fn a_return_of_a_part_paid_purchase_is_refused_rather_than_left_partly_refunded() {
        let (s, pool) = svc().await;
        // Two units at 3 is 6; the parent collected 4.
        let p = draft_return(
            &s,
            &pool,
            "PART",
            "Part Supplier",
            "2",
            "3",
            &[("part wallet", "4")],
            "2",
        )
        .await;
        let who = actor(&pool).await;

        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains('6') && msg.contains('4'),
                "the refusal must state what the return is worth and what came in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None),
            "no document was created, so there is nothing whose status could be Partial"
        );

        // And the reachable counterpart: a return worth LESS than what came in
        // is refunded in full and reads Paid, because the plan takes `min` per
        // payment rather than a proportion of it.
        let cheap = draft_return(
            &s,
            &pool,
            "CHEAP",
            "Cheap Supplier",
            "3",
            "1",
            &[("cheap wallet", "26")],
            "2",
        )
        .await;
        let detail = s.confirm(who, cheap.return_id).await.unwrap();
        assert_eq!(detail.total, dec("2"));
        assert_eq!(
            detail.paid,
            dec("2"),
            "the refund is the RETURN's worth, not the parent's payment: the plan \
             takes what the return is worth and stops"
        );
        assert_eq!(detail.payment_status, PaymentStatus::Paid);
        assert_eq!(detail.due, Decimal::ZERO);
    }

    /// The cap is a SUM, not a per-line check. Two CONFIRMED returns of 3
    /// against a parent line of 5: the first takes 3 and leaves 2, so the second
    /// is refused for being ONE unit over what is left.
    ///
    /// This is the shape a per-line check cannot see. `ensure_within_parent`
    /// compared the requested quantity against the parent's OWN qty, and 3 <= 5
    /// passes every time — so the second return was refused by nothing at all.
    /// The two claims are individually legal and jointly impossible, which is
    /// exactly what an aggregate is for.
    #[tokio::test]
    async fn two_confirmed_returns_of_three_against_a_parent_line_of_five_leave_the_second_one_unit_over(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SUMCAP", "20").await;
        let supplier_id = seed_supplier(&pool, "Summed Cap Supplier").await;
        let number = "2024-PURCH-SUMCAP";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "5", "4", number).await;
        collect(&pool, purchase, number, &[("sumcap wallet", "40")]).await;
        let who = actor(&pool).await;

        // 3 then 3, each of which is under the parent's own quantity of 5.
        let first = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("3")).await.unwrap();
        s.confirm(who, first.id).await.unwrap();

        let second = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        // The WRITE is refused, and 2 is the figure in the message: the first
        // return took 3 of 5, so 2 remain and 3 is one unit over.
        let err = s
            .add_line(who, second.id, line, dec("3"))
            .await
            .expect_err("only two units remain returnable after the first return")
            .to_string();
        assert!(
            err.contains('3') && err.contains('5') && err.contains('2'),
            "the refusal must state what was claimed, what the line holds and what \
             is left: {err}"
        );
        assert_eq!(
            s.returns.list_lines(second.id).await.unwrap().len(),
            0,
            "the refused line wrote nothing"
        );

        // And the CONFIRM path refuses it on its own, with the line put there by
        // raw SQL so the confirm cannot be relying on `add_line`'s guard.
        sqlx::query(
            r#"INSERT INTO purchase_return_lines (return_id, purchase_line_id, qty, unit_cost)
               VALUES (?, ?, '3', '4')"#,
        )
        .bind(second.id)
        .bind(line)
        .execute(&pool)
        .await
        .unwrap();
        match s.confirm(who, second.id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains('3') && msg.contains('5') && msg.contains('2'),
                "confirm re-checks the summed allowance rather than trusting how the \
                 line was written: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            row_state(&pool, second.id).await,
            ("Draft".to_string(), None),
            "the over-claim never became a document"
        );
        assert_eq!(
            pret_sequence_last(&pool).await,
            Some(1),
            "no second number was spent"
        );
        assert_eq!(
            payment_count(&pool).await,
            1,
            "only the first return's refund exists"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("17"),
            "three units left the shelf, not six"
        );

        // And the remainder is genuinely returnable: 2 of the 5 goes through,
        // which is the positive half of a cap and is what proves the figure in
        // the refusal message is the real remaining allowance rather than a
        // constant that always refuses.
        let rest = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, rest.id, line, dec("2")).await.unwrap();
        assert_eq!(
            s.confirm(who, rest.id).await.unwrap().total,
            dec("8"),
            "the two units that were left are returnable, so the cap refuses only \
             what it should"
        );
    }

    /// THE SUBTLE ONE. A draft reserves nothing — which has two halves, and only
    /// one of them is obvious.
    ///
    /// The obvious half: a draft of the FULL quantity does not stop a second
    /// draft of the same quantity from being written. Both go in, because neither
    /// has spent anything: goods that have not gone back cannot be gone back
    /// twice.
    ///
    /// The subtle half: CONFIRMING the first must then refuse the second, with
    /// the confirmation — not the write — being the moment the allowance is
    /// spent. So the rule is enforced on a read that happens at confirm time and
    /// not at draft time, and the second draft is NOT invalidated by the first
    /// one's success: it is still a valid draft, and it is refused at its own
    /// confirm for a stated reason.
    ///
    /// What must NOT happen, and is what a reservation design would produce: the
    /// second draft silently dying, or the first confirm overwriting the
    /// allowance so the second confirm fails for no stated reason.
    #[tokio::test]
    async fn a_draft_reserves_nothing_so_a_second_draft_still_writes_and_only_the_first_confirm_wins(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "RESERVE", "20").await;
        let supplier_id = seed_supplier(&pool, "Reservation Supplier").await;
        let number = "2024-PURCH-RESERVE";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "5", "4", number).await;
        collect(&pool, purchase, number, &[("reserve wallet", "20")]).await;
        let who = actor(&pool).await;

        // Two drafts, each claiming the WHOLE line. The parent line holds 5, so
        // each of these is individually within it.
        let first = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("5")).await.unwrap();
        let second = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, second.id, line, dec("5")).await.expect(
            "a draft reserves nothing, so a second draft of the same \
             quantity is still writable",
        );

        // Neither draft has spent the allowance yet, which is the premise the
        // next two assertions depend on.
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_purchase_line(line)
                .await
                .unwrap(),
            Decimal::ZERO,
            "two full drafts together must read as ZERO taken, or the second write \
             above passed for the wrong reason"
        );
        assert_eq!(
            row_state(&pool, second.id).await,
            ("Draft".to_string(), None),
            "the second draft is intact and untouched"
        );

        // The first confirm is what spends it.
        s.confirm(who, first.id).await.unwrap();
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_purchase_line(line)
                .await
                .unwrap(),
            dec("5"),
            "the allowance is spent by the CONFIRM, not by the draft"
        );

        // And now the second draft — still a valid draft, with its line still on
        // it — is refused at its own confirm, for a reason it can state.
        match s.confirm(who, second.id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("already"),
                "the refusal must name the spent allowance: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            row_state(&pool, second.id).await,
            ("Draft".to_string(), None),
            "the refused draft is left as a draft: it is not invalidated by \
             someone else's confirm, it is simply refused when it tries to become real"
        );
        assert_eq!(
            s.returns.list_lines(second.id).await.unwrap()[0].qty,
            dec("5"),
            "and its line survives, so an operator can reduce it rather than start again"
        );
        assert_eq!(
            payment_count(&pool).await,
            1,
            "one refund, from the one document that confirmed"
        );
    }

    /// Cancelling a CONFIRMED return gives its allowance BACK. The goods came
    /// back onto the shelf and the money went back to the supplier, so the same
    /// quantity is returnable again — otherwise the second return would be
    /// refused for a quantity the shop no longer holds.
    ///
    /// This is the state predicate earning its keep: `Confirmed` counts, and
    /// `Cancelled` does not, so the same aggregate answers 5 before the
    /// cancellation and 0 after it.
    #[tokio::test]
    async fn cancelling_a_confirmed_return_returns_its_allowance_so_the_same_quantity_may_be_returned_again(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "GIVEBACK", "20").await;
        let supplier_id = seed_supplier(&pool, "Give Back Supplier").await;
        let number = "2024-PURCH-GIVEBACK";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "5", "4", number).await;
        collect(&pool, purchase, number, &[("giveback wallet", "20")]).await;
        let who = actor(&pool).await;

        let spent = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, spent.id, line, dec("5")).await.unwrap();
        s.confirm(who, spent.id).await.unwrap();
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_purchase_line(line)
                .await
                .unwrap(),
            dec("5")
        );

        s.cancel(who, spent.id, Some("sent the wrong goods".into()))
            .await
            .unwrap();
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_purchase_line(line)
                .await
                .unwrap(),
            Decimal::ZERO,
            "a CANCELLED return gave its quantity back: the aggregate counts only \
             Confirmed documents, and a cancelled one is not one"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("20"),
            "the goods came back onto the shelf, which is why the allowance is free"
        );

        // And the whole quantity is genuinely returnable a second time, through
        // the real path rather than only through the aggregate's number.
        let again = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, again.id, line, dec("5")).await.unwrap();
        let detail = s.confirm(who, again.id).await.unwrap();
        assert_eq!(
            detail.total,
            dec("20"),
            "the full five units went back a second time, and were refunded for"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("15")
        );
    }

    // ========================================================================
    // THE LIST
    // ========================================================================

    /// The collection read, and the filter actually filtering. Mirrors
    /// `PurchasesService::list_details_filtered`: a filter in, whole documents
    /// out, each carrying its derived money rather than a bare row.
    ///
    /// The assertion that matters is the SECOND one. A `list` that ignored its
    /// filter would return all four documents and pass a test that only asked
    /// "are the expected ones present" — so each narrowing is asserted as an
    /// exact set, and one read is checked to return NOTHING where the shape says
    /// it must.
    #[tokio::test]
    async fn the_list_returns_the_documents_the_filter_selects_with_their_derived_money() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "LISTME", "20").await;
        let supplier_id = seed_supplier(&pool, "List Me Supplier").await;
        let number = "2024-PURCH-LISTME";
        let (purchase, line) = seed_parent(&pool, supplier_id, product.id, "3", "4", number).await;
        collect(&pool, purchase, number, &[("listme wallet", "12")]).await;
        let who = actor(&pool).await;

        // Three drafts against the same parent, so the list has something to
        // narrow and the party filter has more than one match.
        let mut ids = Vec::new();
        for (notes, returned) in [
            ("first draft", "1"),
            ("second draft", "2"),
            ("third draft", "1"),
        ] {
            let ret = s
                .create_draft(who, purchase, return_date(), &Some(notes.into()))
                .await
                .unwrap();
            s.add_line(who, ret.id, line, dec(returned)).await.unwrap();
            ids.push(ret.id);
        }
        let [first, second, third] = ids[..] else {
            unreachable!("three drafts were created")
        };
        s.confirm(who, second).await.unwrap();

        // An empty filter is the whole family, and each document carries its
        // OWN derived money rather than a zeroed projection.
        let all = s.list(&PurchaseReturnListFilter::default()).await.unwrap();
        assert_eq!(
            all.iter().map(|d| d.purchase_return.id).collect::<Vec<_>>(),
            vec![first, second, third],
            "the empty filter narrows nothing and the order is the repository's"
        );
        assert_eq!(
            all.iter().map(|d| d.total).collect::<Vec<_>>(),
            vec![dec("4"), dec("8"), dec("4")],
            "each document's total is derived from ITS OWN lines: the confirmed one \
             returns 2 units and the drafts 1 each"
        );
        assert_eq!(
            all[1].payment_status,
            PaymentStatus::Paid,
            "the confirmed document reports itself paid and the drafts report \
             themselves unpaid, so the list is not publishing one document's \
             derived state on another"
        );

        // Status. A filter that does not filter is the defect this catches.
        let drafts = s
            .list(&PurchaseReturnListFilter {
                status: Some(PurchaseReturnStatus::Draft),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            drafts
                .iter()
                .map(|d| d.purchase_return.id)
                .collect::<Vec<_>>(),
            vec![first, third],
            "status narrows the set exactly"
        );
        assert!(
            s.list(&PurchaseReturnListFilter {
                status: Some(PurchaseReturnStatus::Cancelled),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a status no document holds returns nothing"
        );

        // The party ids, and a party that matches nobody.
        let mine = s
            .list(&PurchaseReturnListFilter {
                supplier_ids: Some(vec![supplier_id]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(mine.len(), 3);
        assert!(
            s.list(&PurchaseReturnListFilter {
                supplier_ids: Some(Vec::new()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a party filter matching no supplier returns no document"
        );

        // The document's OWN day, which is a month after the parent's — a date
        // filter on the parent's date would return everything or nothing.
        let in_june = s
            .list(&PurchaseReturnListFilter {
                from: Some(return_date()),
                to: Some(return_date()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(in_june.len(), 3, "all three are dated the same day");
        assert!(
            s.list(&PurchaseReturnListFilter {
                from: Some(purchase_date()),
                to: Some(purchase_date()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "no return is dated on the PARENT's day, so that range is empty — which \
             is the proof the filter reads return_date and not purchase_date"
        );

        // The number, which only the confirmed document has.
        assert_eq!(
            s.list(&PurchaseReturnListFilter {
                number: Some("PRET-000001".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
            1,
            "the numbered document is found by a fragment of its own number"
        );

        // Composed, which is what a list surface actually depends on.
        assert!(
            s.list(&PurchaseReturnListFilter {
                status: Some(PurchaseReturnStatus::Draft),
                number: Some("PRET-000001".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a Draft has no number, so status and number cannot both hold: a broken \
             AND would return the confirmed document"
        );
    }

    // ========================================================================
    // THE SATELLITE
    // ========================================================================

    /// Decision 2, measured rather than asserted: a CONFIRMED return leaves
    /// `product_supplier_costs` byte for byte as it found it. Every column of
    /// every row, including the update timestamps an UPDATE would move and the
    /// nullable previous-cost pair, is concatenated and compared.
    ///
    /// The fixture gives the satellite a row to be wrong about FIRST, so the
    /// comparison is between two real states. An empty table passes this test
    /// for the wrong reason, and `a_confirmed_return_never_reaches_the_cost_satellite_so_there_is_no_satellite_window`
    /// is the test that closes that hole from the other side.
    #[tokio::test]
    async fn a_confirmed_return_leaves_the_cost_satellite_byte_identical() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SAT", "20").await;
        let supplier_id = seed_supplier(&pool, "Satellite Supplier").await;
        let (purchase, line) =
            seed_parent(&pool, supplier_id, product.id, "3", "4", "2024-PURCH-SAT").await;
        collect(&pool, purchase, "2024-PURCH-SAT", &[("sat wallet", "12")]).await;
        sqlx::query(
            r#"INSERT INTO product_supplier_costs
                 (product_id, supplier_id, current_cost, current_cost_date, is_preferred, created_by)
               VALUES (?, ?, '4', '2024-05-02T00:00:00.000Z', 0, ?)"#,
        )
        .bind(product.id)
        .bind(supplier_id)
        .bind(actor(&pool).await)
        .execute(&pool)
        .await
        .unwrap();

        let before = satellite(&pool).await;
        assert_eq!(
            before.len(),
            1,
            "the satellite has a row a return could damage"
        );

        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line, dec("2"))
            .await
            .unwrap();
        s.confirm(who, purchase_return.id).await.unwrap();

        assert_eq!(
            satellite(&pool).await,
            before,
            "a return sets no price, so no part of the cost satellite may move — \
             not current_cost, not previous_cost, not a timestamp, not the preference flag"
        );
    }

    // ========================================================================
    // CANCEL
    // ========================================================================

    /// A refund sent by mistake must be undoable. The reversal is real in both
    /// directions: the goods come back ON the shelf, the money leaves as an
    /// EXPENSE (the mirror of the confirm's Income), and each reversal is
    /// linked to the refund row it reverses so the pair is traceable.
    ///
    /// The movement reason is `Adjust`, and that is an INTERPRETATION recorded
    /// on `cancel` and here: the vocabulary has no "reversal" entry, and every
    /// other candidate states something this movement is not.
    #[tokio::test]
    async fn cancelling_a_confirmed_return_reverses_the_stock_and_the_money_and_links_the_pair() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "CANCEL",
            "Cancel Supplier",
            "3",
            "4",
            &[("cancel wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            dec("18")
        );

        let detail = s
            .cancel(who, p.return_id, Some("wrong supplier".into()))
            .await
            .unwrap();

        assert_eq!(
            detail.purchase_return.status,
            PurchaseReturnStatus::Cancelled
        );
        assert_eq!(
            detail.purchase_return.cancel_reason.as_deref(),
            Some("wrong supplier")
        );
        assert_eq!(
            detail.purchase_return.return_number.as_deref(),
            Some("2024-PRET-000001"),
            "a cancelled return keeps its number: it was a real document"
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            dec("20"),
            "the goods came back onto the shelf"
        );
        assert_eq!(
            movement_reasons(&pool).await,
            vec!["Initial", "Purchase-return", "Adjust"],
            "the reversal is In with the neutral reason"
        );
        assert_eq!(
            tx_kinds(&pool).await,
            vec!["Expense", "Income", "Expense"],
            "the Income the refund produced is now reversed by an Expense"
        );
        assert!(
            detail.payments[0].refund_transaction_id.is_some(),
            "the refund row carries BOTH links: the one it produced and the one that reverses it"
        );

        // A second cancel is refused rather than doubling the reversal.
        match s.cancel(who, p.return_id, None).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("already cancelled"),
                "the refusal must say the return is already cancelled: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            movement_count(&pool).await,
            3,
            "nothing moved a second time"
        );
    }

    /// A Draft return is discarded, not reversed: it never moved goods or money,
    /// so cancelling it is a status flip and nothing else. The number stays NULL,
    /// which is also what makes the row deletable afterwards.
    #[tokio::test]
    async fn cancelling_a_draft_return_moves_no_stock_and_no_money_and_makes_it_deletable() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "DISCARD",
            "Discard Supplier",
            "3",
            "4",
            &[("discard wallet", "12")],
            "1",
        )
        .await;
        let who = actor(&pool).await;

        let detail = s.cancel(who, p.return_id, None).await.unwrap();
        assert_eq!(
            detail.purchase_return.status,
            PurchaseReturnStatus::Cancelled
        );
        assert_eq!(detail.purchase_return.return_number, None);
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the fixture's own stock-in"
        );
        assert_eq!(tx_count(&pool).await, 1, "only the fixture's own payment");

        s.delete_draft(p.return_id).await.unwrap();
        match s.delete_draft(p.return_id).await.unwrap_err() {
            AppError::NotFound(msg) => assert!(msg.contains("not found"), "{msg}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A return that was confirmed and then reversed keeps its number, so it is
    /// permanent audit trail and `delete_draft` must refuse it — the goods went
    /// back out and the money came in, and both movements reference the return.
    #[tokio::test]
    async fn a_reversed_return_keeps_its_number_and_is_no_longer_deletable() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "PERM",
            "Permanent Supplier",
            "3",
            "4",
            &[("perm wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();
        s.cancel(who, p.return_id, None).await.unwrap();

        match s.delete_draft(p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("Cancelled"),
                "the refusal must name the state the row rests in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        let rows: i64 =
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_returns WHERE id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap()
                .0;
        assert_eq!(rows, 1, "the refused delete removed nothing");
    }

    /// A return cannot be confirmed twice, and a cancelled one cannot be
    /// confirmed at all. The refused duplicate must not burn a second number —
    /// which is the statement's own `AND status = 'Draft'` predicate at work, and
    /// the same backstop the repository's own tests pin.
    #[tokio::test]
    async fn a_return_cannot_be_confirmed_twice_or_confirmed_after_being_cancelled() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "TWICE",
            "Twice Supplier",
            "3",
            "4",
            &[("twice wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();

        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(msg.contains("already confirmed"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            pret_sequence_last(&pool).await,
            Some(1),
            "the refused duplicate did not burn a second number"
        );

        s.cancel(who, p.return_id, None).await.unwrap();
        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("cancelled"),
                "a cancelled return cannot be confirmed either: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// A return with no lines is not a return. Refused before the number, so the
    /// failure is a Draft with nothing behind it rather than a numbered document
    /// with no stock and no money.
    #[tokio::test]
    async fn a_return_with_no_lines_cannot_be_confirmed() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "EMPTY", "20").await;
        let supplier_id = seed_supplier(&pool, "Empty Supplier").await;
        let (purchase, _line) =
            seed_parent(&pool, supplier_id, product.id, "3", "4", "2024-PURCH-EMPTY").await;
        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();

        match s.confirm(who, purchase_return.id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(msg.contains("no lines"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(pret_sequence_last(&pool).await, None);
        assert_eq!(
            row_state(&pool, purchase_return.id).await,
            ("Draft".to_string(), None)
        );
        assert_eq!(
            movement_count(&pool).await,
            1,
            "only the fixture's own stock-in"
        );
    }

    // ========================================================================
    // THE TRANSACTION — the residue windows
    // ========================================================================
    //
    // These are the tests the whole Phase A/B sequence was for. Each injects a
    // failure at a different step of `confirm` and asserts that NOTHING survives:
    // no movement, no finance row, no payment, and above all an UNSPENT number.
    // On the purchase family the same injections left a burned number, a stock
    // movement whose `reference` named no document, an orphan finance row, and a
    // Draft that had already collected the money. These assertions are inverted
    // because the guarantee changed, and they are the empirical proof that a
    // return leaves nothing behind.

    /// WINDOW 1 — between `next_number` and the FIRST stock movement. The
    /// narrowest residue there is, and it is gone: the number is unspent and
    /// `doc_sequences` has no PRET row at all.
    #[tokio::test]
    async fn confirm_failure_between_the_number_and_the_first_movement_leaves_nothing_written() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "W1", "20").await;
        let supplier_id = seed_supplier(&pool, "W1 Supplier").await;
        let (purchase, line) =
            seed_parent(&pool, supplier_id, product.id, "3", "4", "2024-PURCH-W1").await;
        collect(&pool, purchase, "2024-PURCH-W1", &[("w1 wallet", "12")]).await;
        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line, dec("2"))
            .await
            .unwrap();
        let product_id = product.id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER ret_w1 BEFORE INSERT ON stock_movements \
                 WHEN NEW.reason = 'Purchase-return' AND NEW.product_id = {product_id} \
                 BEGIN SELECT RAISE(ABORT, 'injected first-movement failure'); END"
            ),
        )
        .await;

        let err = s.confirm(who, purchase_return.id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected first-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            pret_sequence_last(&pool).await,
            None,
            "the number was never spent: `doc_sequences` has no PRET row at all, \
             because the increment rolled back with the rest of the unit"
        );
        assert_eq!(movement_count(&pool).await, 1, "no goods went back out");
        assert_eq!(tx_count(&pool).await, 1, "finance never started");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, purchase_return.id).await,
            ("Draft".to_string(), None)
        );
    }

    /// WINDOW 2 — on the SECOND movement of a two-line return. This is where a
    /// return has more steps than a purchase could ever show, because a purchase
    /// refuses a repeated product while a return's lines come from a purchase and
    /// therefore cannot repeat one either: two lines means two PARENTS, or one
    /// purchase with two products. The fixture uses the latter, which is the
    /// shape a multi-line return really arrives in.
    ///
    /// Both of the first line's goods went out before the failure, and both must
    /// roll back with it.
    #[tokio::test]
    async fn confirm_failure_on_the_second_movement_rolls_the_first_one_back_too() {
        let (s, pool) = svc().await;
        let first = seed_product(&s, &pool, "W2A", "20").await;
        let second = seed_product(&s, &pool, "W2B", "20").await;
        let supplier_id = seed_supplier(&pool, "W2 Supplier").await;
        let number = "2024-PURCH-W2";
        let (purchase, line_a) = seed_parent(&pool, supplier_id, first.id, "3", "4", number).await;
        let line_b = add_parent_line(&pool, purchase, second.id, "2", "7").await;
        collect(&pool, purchase, number, &[("w2 wallet", "26")]).await;
        let who = actor(&pool).await;
        let purchase_return = s
            .create_draft(who, purchase, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line_a, dec("2"))
            .await
            .unwrap();
        s.add_line(who, purchase_return.id, line_b, dec("1"))
            .await
            .unwrap();
        let second_id = second.id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER ret_w2 BEFORE INSERT ON stock_movements \
                 WHEN NEW.reason = 'Purchase-return' AND NEW.product_id = {second_id} \
                 BEGIN SELECT RAISE(ABORT, 'injected second-movement failure'); END"
            ),
        )
        .await;

        let err = s.confirm(who, purchase_return.id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected second-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            pret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT"
        );
        assert_eq!(
            movement_count(&pool).await,
            2,
            "no goods went back out: the first line's Out rolled back with the \
             second one's failure"
        );
        assert_eq!(
            tx_count(&pool).await,
            1,
            "only the fixture's own payment exists"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, purchase_return.id).await,
            ("Draft".to_string(), None),
            "no goods went out on a Draft nobody can see"
        );
    }

    /// WINDOW 3 — between the refund Income and the payment row that claims it.
    /// This is the residue that used to leave an orphan finance row stamped with
    /// a number no document carried, so an account held money the document never
    /// recorded. There is no orphan now: the Income rolls back with the payment
    /// row that could not be written.
    #[tokio::test]
    async fn confirm_failure_between_the_income_and_the_payment_row_leaves_no_orphan_income() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "W3",
            "W3 Supplier",
            "3",
            "4",
            &[("w3 wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        let return_id = p.return_id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER ret_w3 BEFORE INSERT ON purchase_return_payments \
                 WHEN NEW.return_id = {return_id} \
                 BEGIN SELECT RAISE(ABORT, 'injected payment-row failure'); END"
            ),
        )
        .await;

        let err = s.confirm(who, return_id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected payment-row failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            pret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT"
        );
        assert_eq!(movement_count(&pool).await, 1, "the Out rolled back too");
        assert_eq!(
            tx_count(&pool).await,
            1,
            "the refund Income rolled back with the payment row that could not be written"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, return_id).await,
            ("Draft".to_string(), None)
        );

        // The shape the confirm used to leave, asserted absent by name.
        let orphans: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transactions t \
             WHERE NOT EXISTS (SELECT 1 FROM purchase_return_payments rp WHERE rp.transaction_id = t.id \
                               OR rp.refund_transaction_id = t.id) \
               AND NOT EXISTS (SELECT 1 FROM purchase_payments pp WHERE pp.transaction_id = t.id \
                               OR pp.refund_transaction_id = t.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            orphans.0, 0,
            "no finance row is claimed by nobody: the orphan shape does not exist here"
        );
    }

    /// WINDOW 4 — on `set_confirmed` itself, the LAST step. This is the residue
    /// `AGENTS.md` calls out as the one that bites: the number spent, the stock
    /// gone, the Income in the account AND the payment row committed, while the
    /// document still reads `("Draft", NULL)`. A `delete_draft` that trusted the
    /// status would remove that Draft, cascade the payment away and keep the
    /// money. All of it rolls back now.
    #[tokio::test]
    async fn confirm_failure_on_set_confirmed_leaves_a_clean_draft_that_reports_nothing_received() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "W4",
            "W4 Supplier",
            "3",
            "4",
            &[("w4 wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        let return_id = p.return_id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER ret_w4 BEFORE UPDATE ON purchase_returns WHEN NEW.id = {return_id} \
                 BEGIN SELECT RAISE(ABORT, 'injected set-confirmed failure'); END"
            ),
        )
        .await;

        let err = s.confirm(who, return_id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected set-confirmed failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            pret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT: `doc_sequences` has no PRET row at all"
        );
        assert_eq!(movement_count(&pool).await, 1, "no goods went back out");
        assert_eq!(tx_count(&pool).await, 1, "the refund Income rolled back");
        assert_eq!(
            payment_count(&pool).await,
            0,
            "the payment rolled back: the supplier was NOT refunded and no document says otherwise"
        );
        assert_eq!(
            row_state(&pool, return_id).await,
            ("Draft".to_string(), None)
        );

        let detail = s.get_detail(return_id).await.unwrap();
        assert_eq!(
            detail.payment_status,
            PaymentStatus::Unpaid,
            "the document reports itself UNPAID: `get_detail` reads the payments table, \
             and that table is empty again"
        );
        assert_eq!(
            detail.paid,
            Decimal::ZERO,
            "nothing came back from the supplier"
        );

        // And the Draft is safe to delete precisely because the unit rolled
        // back: there is no committed payment for its CASCADE to take with it
        // and no finance row for anyone to keep.
        s.delete_draft(return_id).await.unwrap();
        let returns: i64 =
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM purchase_return_lines")
                .fetch_one(&pool)
                .await
                .unwrap()
                .0;
        assert_eq!(returns, 0, "the document and its lines are gone together");
    }

    /// WINDOW 5 — the one a return CANNOT have, and the test that says so.
    ///
    /// `PurchasesService::confirm` has a sixth write (`record_cost`) that runs
    /// last, after `set_confirmed` succeeded, and its failure used to leave a
    /// CONFIRMED, numbered, fully paid purchase with only SOME of its lines'
    /// costs recorded — the one residue with no recovery path, because the retry
    /// was refused as a duplicate. A return has no sixth write at all, by
    /// decision 2: there is no `record_cost` call that could fail.
    ///
    /// So the injection is a trigger that ABORTS on ANY insert into the
    /// satellite, and the assertion is that it never fires — a confirm that
    /// wrote costs could not have succeeded at all. A test that proved the
    /// rollback alone would pass on the purchase family too and would not
    /// distinguish the two families; this one is specifically about the ABSENCE
    /// of the write, which is what makes the window not exist rather than merely
    /// be rolled back.
    #[tokio::test]
    async fn a_confirmed_return_never_reaches_the_cost_satellite_so_there_is_no_satellite_window() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "W5",
            "W5 Supplier",
            "3",
            "4",
            &[("w5 wallet", "12")],
            "2",
        )
        .await;

        inject(
            &pool,
            "CREATE TRIGGER ret_w5 BEFORE INSERT ON product_supplier_costs \
             BEGIN SELECT RAISE(ABORT, 'a return must never write the cost satellite'); END"
                .to_string(),
        )
        .await;

        let detail = s
            .confirm(actor(&pool).await, p.return_id)
            .await
            .expect("a confirm that wrote the satellite would have been aborted by the trigger");
        assert_eq!(
            detail.purchase_return.status,
            PurchaseReturnStatus::Confirmed
        );
        assert_eq!(
            satellite(&pool).await.len(),
            0,
            "and the satellite is still empty: the trigger never had to fire"
        );
    }

    /// A failed confirm must not poison the NEXT one. The unit rolled back, so
    /// the draft is still a Draft, its number is still unspent and a retry
    /// re-passes every predicate and succeeds — which is the property the whole
    /// retryability argument rests on and the one `set_confirmed_in`'s own WHERE
    /// exists to allow.
    #[tokio::test]
    async fn a_confirm_that_failed_part_way_can_be_retried_unchanged() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "RETRY",
            "Retry Supplier",
            "3",
            "4",
            &[("retry wallet", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;

        inject(
            &pool,
            "CREATE TRIGGER ret_retry BEFORE INSERT ON purchase_return_payments \
             BEGIN SELECT RAISE(ABORT, 'injected retry failure'); END"
                .to_string(),
        )
        .await;
        assert!(s.confirm(who, p.return_id).await.is_err());

        sqlx::raw_sql("DROP TRIGGER ret_retry")
            .execute(&pool)
            .await
            .unwrap();

        let detail = s.confirm(who, p.return_id).await.unwrap();
        assert_eq!(
            detail.purchase_return.return_number.as_deref(),
            Some("2024-PRET-000001"),
            "the retry took the FIRST number, not a second one: the burned counter rolled back"
        );
        assert_eq!(payment_count(&pool).await, 1);
        assert_eq!(
            tx_count(&pool).await,
            2,
            "the fixture's payment and the one refund"
        );
    }

    // ========================================================================
    // THE SEAMS — the mutation proof, kept as a permanent test
    // ========================================================================

    /// The permanent form of the mutation proof.
    ///
    /// The mutation this file was developed against changed ONE `_in` call
    /// inside `confirm` back to its public twin — `set_confirmed_in` to
    /// `set_confirmed` — which opens its own `pool.begin()` while the caller's
    /// unit still holds the only connection. On this `max_connections(1)` pool
    /// that cannot answer at all: 30 seconds of sqlx acquire timeout, then
    /// `PoolTimedOut`. Reverted, it answers in single-digit milliseconds.
    ///
    /// What is left as a test is the PREMISE, asserted directly rather than
    /// inferred from a suite that happens to pass: while a unit is open the pool
    /// has no spare connection, so a seam that reached for it would stall.
    /// `pool.try_acquire()` answering `None` is the pool stating a fact about
    /// itself at that instant, not this test's patience. `set_confirmed_in` is
    /// the write exercised because it is the one whose read-back and refusal
    /// helper both have to stay on the caller's connection — a regression to
    /// `&SqlitePool` would pass every happy path in the repository's own file
    /// and fail only here.
    #[tokio::test]
    async fn confirm_opens_its_unit_on_the_only_connection_the_pool_owns() {
        let (s, pool) = svc().await;
        let p = draft_return(
            &s,
            &pool,
            "SEAM",
            "Seam Supplier",
            "3",
            "4",
            &[("seam wallet", "12")],
            "2",
        )
        .await;

        let who = actor(&pool).await;
        // Confirmed FIRST, so the seam under test has to take its REFUSAL path.
        // A happy path proves only that the write landed somewhere; the refusal
        // is the path a regression to `&SqlitePool` breaks, because that is the
        // branch whose read-back and `refuse_confirm` have to stay on the
        // caller's connection while the caller holds the only one.
        s.confirm(who, p.return_id).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );
        let before = std::time::Instant::now();
        let refusal = s
            .returns
            .set_confirmed_in(&mut tx, p.return_id, who, "2024-PRET-000002")
            .await
            .expect_err("the return is Confirmed, so the DRAFT predicate must refuse")
            .to_string();
        let elapsed = before.elapsed();
        assert!(
            refusal.contains("Confirmed"),
            "the joined seam must reach its own refusal on the caller's connection, \
             not a PoolTimedOut: {refusal}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the joined seam took {elapsed:?}; that is a nested BEGIN stalling for a \
             connection, not a joined write"
        );
        assert!(
            pool.try_acquire().is_none(),
            "the seam released the connection it was handed, so it was never inside the unit"
        );
        tx.rollback().await.unwrap();

        // And the whole confirm runs on the same pool, which is the property the
        // fixture's single connection exists to protect: every confirm test in
        // this file is standing on this premise, and none of them states it
        // louder than a pool with two connections would.
        let second = draft_return(
            &s,
            &pool,
            "SEAM2",
            "Seam Supplier Two",
            "3",
            "4",
            &[("seam wallet two", "12")],
            "2",
        )
        .await;
        let detail = s.confirm(who, second.return_id).await.unwrap();
        assert_eq!(
            detail.purchase_return.return_number.as_deref(),
            Some("2024-PRET-000002"),
            "the second document took the second number, on a pool with one connection"
        );
    }

    /// Money can never be the sum of documents without a guard. Two return lines
    /// of `5e28` at a cost of `1` are `1e29` together, above `Decimal::MAX`, and
    /// the raw `+` panics — so the fold is `checked_money_sum` and the refusal is
    /// the document-total rule, not a line rule, because every line is fine.
    #[test]
    fn a_document_total_built_from_two_enormous_lines_is_refused_rather_than_panicking() {
        let lines = vec![
            PurchaseReturnLine {
                id: 1,
                return_id: 1,
                purchase_line_id: 1,
                qty: dec("5e28"),
                unit_cost: dec("1"),
                created_at: return_date().and_hms_opt(0, 0, 0).unwrap(),
            },
            PurchaseReturnLine {
                id: 2,
                return_id: 1,
                purchase_line_id: 2,
                qty: dec("5e28"),
                unit_cost: dec("1"),
                created_at: return_date().and_hms_opt(0, 0, 0).unwrap(),
            },
        ];
        match Svc::document_money(&lines, &[]) {
            Err(PriceRefusal::DocumentTotalTooLarge) => {}
            other => panic!("expected DocumentTotalTooLarge, got {other:?}"),
        }

        // One enormous line alone is representable and must NOT be refused: the
        // bound is about the SUM, which is what the code above is actually
        // guarding.
        assert!(matches!(
            Svc::document_money(&lines[..1], &[]),
            Ok(RecordMoney { .. })
        ));
    }
}
