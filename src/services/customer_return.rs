// M-credit notes (odd/tasks/purchase-returns-and-credit-notes.md).
//
// CustomerReturnService is the orchestrator for "the customer is bringing two of
// the three back", which `SalesService::cancel` cannot express: cancelling a sale
// is all-or-nothing and it discards the document rather than reversing part of
// it. This service calls InventoryService for stock In (reason Sale-return) on
// confirm, TransactionService for the refund Expenses on confirm and the
// reversal Incomes on cancel, and the sale repository for the parent it reads. It
// never SQLs `transactions`, `stock_movements`, `accounts` or `payment_methods`
// for writes, and it writes NOTHING to `product_supplier_costs`.
//
// Numbering: YYYY-SRET-NNNNNN assigned on confirm via the `doc_sequences`
// consumer SRET, the short form, with `year` taken from the RETURN's own date.
// A Draft touches nothing.
//
// THE MIRROR, and the three rows where it breaks. Everything else about this file
// is `purchase_return.rs` with the family's nouns swapped, and that is the
// argument the design makes for building one engine and configuring it twice:
//
//   stock  In       the goods come BACK from the customer
//   money  Expense  the refund is money LEAVING, so the overdraft guard DOES
//                   fire on it — the same guard `SalesService::cancel` names
//                   when it refuses an annulment the shop cannot pay for
//   cost   no write by decision 2, exactly as on a purchase return
//
// The document is `CustomerReturn` rather than `SaleReturn` on purpose:
// `SaleReturn` is the STOCK MOVEMENT REASON, it describes a physical event, and
// it keeps that name (decision 3). A document and a movement must not share a
// word in code either, since the ambiguity is exactly what the naming decision
// removed. So the number column is `credit_note_number`, not `return_number`,
// and this is the one field that is deliberately NOT the purchase return's
// mirror.
//
// Atomicity: ONE transaction, opened immediately before the number is taken and
// committed after `set_confirmed` — the same argument as the purchase return,
// and the reason a return needed more of Phase B than a sale did: a credit note
// has a refund to PAY as well as goods to take, and every one of its steps runs
// in the unit, so a failure at any step leaves the row `("Draft", NULL)` with no
// movement, no finance row, no payment and an UNSPENT number.
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    format_customer_return_number, CustomerReturn, CustomerReturnDetail, CustomerReturnLine,
    CustomerReturnPayment, CustomerReturnStatus, MovementReason, MovementType, NewMovement,
    PriceRefusal, Sale, SaleDetail, SaleLine,
};
use crate::services::checked_money_sum;

/// A single confirmed refund, resolved BEFORE the unit is opened, so the write
/// phase below is a straight loop over a plan it cannot second-guess.
///
/// `account_id` and `method_id` are the PARENT PAYMENT's, not the credit note's:
/// each refund leaves the account and by the method the money arrived through,
/// which is what `SalesService::cancel` does when it refunds a sale per
/// originating account, and the reason the refund cap and this allocation are
/// the same rule seen from two sides.
#[derive(Debug, Clone, Copy)]
struct RefundPlan {
    account_id: i64,
    method_id: i64,
    amount: Decimal,
}

#[derive(Clone)]
pub struct CustomerReturnService<RR, DR, SR, C, P, B, S, A, T, PL, PY>
where
    RR: crate::repositories::CustomerReturnRepository,
    DR: crate::repositories::DocSequenceRepository,
    SR: crate::repositories::SaleRepository,
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
    pub sales: SR,
    pub inventory: crate::services::InventoryService<C, P, B, S>,
    pub transactions: crate::services::TransactionService<A, T>,
    /// The customer's signed journal (T2). A confirmed credit note appends the
    /// `Return` that cancels the sale's `Charge`, in the SAME unit as the goods
    /// movement. A refund's own entry is a different event on a different path
    /// (T3b), which is why this field is used for the return and nothing else.
    pub party_ledger: PL,
    /// The `payments` family (T3d): a cancelled credit note returns the money as a
    /// delivery going back out/in, so this side needs the same document.
    pub payments: PY,
}

impl<RR, DR, SR, C, P, B, S, A, T, PL, PY>
    CustomerReturnService<RR, DR, SR, C, P, B, S, A, T, PL, PY>
where
    RR: crate::repositories::CustomerReturnRepository,
    DR: crate::repositories::DocSequenceRepository,
    SR: crate::repositories::SaleRepository,
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
        sales: SR,
        inventory: crate::services::InventoryService<C, P, B, S>,
        transactions: crate::services::TransactionService<A, T>,
        party_ledger: PL,
        payments: PY,
    ) -> Self {
        Self {
            returns,
            sequences,
            sales,
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
    /// The purchase return's twin for the purchase family's own reason: `total`,
    /// `paid` and `due` are one fact and travel together, so no caller can
    /// publish a refundable figure derived from a total that does not exist.
    ///
    /// `tax_total` is ZERO and is not a forgotten figure. A credit note line
    /// freezes no tax — the column does not exist on the table — so
    /// `net_subtotal` and `total` are the same number and there is nothing to
    /// part out.
    ///
    /// Every fold is `checked_money_sum`, so a document of two `5e28` lines is
    /// refused rather than panicking on `Decimal`'s raw `+`: each line's subtotal
    /// is a bounded multiplication and the sum of a set of them is not.
    fn document_money(
        lines: &[CustomerReturnLine],
        payments: &[CustomerReturnPayment],
    ) -> Result<crate::models::RecordMoney, PriceRefusal> {
        let subtotals: Vec<Decimal> = lines.iter().map(|l| l.subtotal()).collect();
        let net_subtotal = checked_money_sum(subtotals.iter())?;
        let total = net_subtotal;
        let paid = checked_money_sum(payments.iter().map(|p| &p.amount))?;
        let due = total
            .checked_sub(paid)
            .ok_or(PriceRefusal::DocumentTotalTooLarge)?;
        Ok(crate::models::RecordMoney {
            net_subtotal,
            tax_total: Decimal::ZERO,
            total,
            paid,
            due,
            // `CustomerReturnDetail` has no `payment_status_for` of its own and
            // adding one would be a model change, so the document-level rule is
            // the one `SaleDetail` and `PurchaseDetail` already share.
            payment_status: SaleDetail::payment_status_for(total, paid),
        })
    }

    async fn detail_for(&self, customer_return: CustomerReturn) -> AppResult<CustomerReturnDetail> {
        let lines = self.returns.list_lines(customer_return.id).await?;
        let payments = self.returns.list_payments(customer_return.id).await?;
        let money = Self::document_money(&lines, &payments).map_err(AppError::PriceRefused)?;
        Ok(CustomerReturnDetail {
            customer_return,
            lines,
            payments,
            net_subtotal: money.net_subtotal,
            total: money.total,
            paid: money.paid,
            due: money.due,
            payment_status: money.payment_status,
        })
    }

    fn ensure_draft(customer_return: &CustomerReturn) -> AppResult<()> {
        if customer_return.status != CustomerReturnStatus::Draft {
            return Err(AppError::Validation(format!(
                "customer return {} is not editable (status {})",
                customer_return.id, customer_return.status
            )));
        }
        Ok(())
    }

    /// The parent of a credit note is CONFIRMED history, never a Draft and never
    /// a cancelled document: a credit note is evidence ABOUT a document that
    /// exists, and the price it carries is derived from the parent's line. A
    /// cancelled parent is refused for the sharper reason that its goods went
    /// back, so crediting it would credit goods that never arrived.
    async fn confirmed_parent(&self, sale_id: i64) -> AppResult<Sale> {
        let sale = self
            .sales
            .find_sale(sale_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale {sale_id} not found")))?;
        if sale.status != crate::models::SaleStatus::Confirmed {
            return Err(AppError::Validation(format!(
                "sale {sale_id} is {}: only a Confirmed sale can be credited",
                sale.status
            )));
        }
        Ok(sale)
    }

    /// The parent line a credit note line names, or `NotFound`. The service holds
    /// no `SqlitePool` and SQLs nothing, so this read goes through the sale
    /// repository exactly as every other read here does.
    async fn parent_line(&self, sale_line_id: i64) -> AppResult<SaleLine> {
        self.sales
            .find_line(sale_line_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("sale line {sale_line_id} not found")))
    }

    /// **THE RULE, IN FULL: a credit note line may claim at most what the parent
    /// line sold MINUS what already-CONFIRMED credit notes of that same parent
    /// line took.**
    ///
    /// Both terms are now real. The second was not computable before this
    /// repository gained `confirmed_qty_taken_by_sale_line`: no method on
    /// [`crate::repositories::CustomerReturnRepository`] looked OUTWARD from a
    /// parent line — `list_lines` is keyed by a RETURN id and `find_line` by a
    /// LINE id, so every read looked the wrong way — and a service that reached
    /// for `pool()` to ask itself would be the one thing the layering rule exists
    /// to prevent. With the hole open, selling five units, taking them back, and
    /// taking the same five back again refunded the customer twice; that is the
    /// RED in `a_second_confirmed_credit_note_of_the_same_parent_line_is_refused_because_the_allowance_is_spent`.
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
    /// figure the draft is measured against — a credit note being edited still
    /// sees its OWN full allowance, which is what lets an operator take a line
    /// from 1 up to 3 on a draft without being refused by the draft's earlier
    /// self. This is the decision, and it is deliberate rather than incidental: a
    /// draft has taken no goods and paid no money, so there is nothing to
    /// reserve. The consequence is that two drafts may each claim the whole line
    /// and the SECOND CONFIRM is what refuses — see
    /// `a_draft_reserves_nothing_so_a_second_draft_still_writes_and_only_the_first_confirm_wins`.
    /// The alternative (reserving at draft time) would need a reservation concept
    /// this app has no notion of, and would make a second draft die silently
    /// rather than be refused for a stated reason.
    ///
    /// `taken_already` is passed in rather than read here because all three call
    /// sites must read it ONCE PER LINE and the confirm path reads them in a
    /// loop; reading it inside would make that shape invisible.
    fn ensure_within_parent(
        parent: &SaleLine,
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
                "cannot credit {qty} of a sale line of {}: the line sold {} and \
                 {taken_already} of it is already credited by a confirmed return, \
                 leaving {} creditable",
                parent.id, parent.qty, remaining
            )));
        }
        Ok(())
    }

    // -- Draft -----------------------------------------------------------------

    /// Create a Draft credit note against a CONFIRMED sale. `customer_id` is
    /// COPIED off the parent rather than supplied by the caller: a credit note
    /// names the customer the sale names, because the money goes back to the
    /// sale, and a second place for the same fact to be wrong is one too many.
    /// `return_date` is the day the return is MADE — the goods come in today and
    /// the refund leaves today.
    pub async fn create_draft(
        &self,
        actor: i64,
        sale_id: i64,
        return_date: NaiveDate,
        notes: &Option<String>,
    ) -> AppResult<CustomerReturn> {
        let sale = self.confirmed_parent(sale_id).await?;
        let notes = Self::clean_notes(notes)?;
        self.returns
            .create_return(actor, sale.customer_id, sale_id, return_date, &notes)
            .await
    }

    pub async fn update_draft(
        &self,
        actor: i64,
        id: i64,
        return_date: NaiveDate,
        notes: &str,
    ) -> AppResult<CustomerReturn> {
        let customer_return = self.return_or_404(id).await?;
        Self::ensure_draft(&customer_return)?;
        if notes.chars().count() > 512 {
            return Err(AppError::Validation("notes must be <= 512 chars".into()));
        }
        self.returns
            .update_draft(id, actor, return_date, notes.trim())
            .await
    }

    async fn return_or_404(&self, id: i64) -> AppResult<CustomerReturn> {
        self.returns
            .find_return(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("customer return {id} not found")))
    }

    /// Add one line naming the PARENT SALE LINE it credits, at the parent's
    /// frozen price, with no price argument anywhere in the signature.
    ///
    /// **There is no price parameter and there cannot be one.** `create_line` is
    /// the only method that writes `unit_price`, it takes the figure as an
    /// argument, and this service hands it the parent line's own. That is
    /// decision 1 of the design expressed as a signature: a credit note is the
    /// absence of goods the customer bought, not a renegotiation.
    ///
    /// Note what is ALSO absent and is a model decision rather than this
    /// service's: there is no `unit_cost` on a credit note line, because `sale_lines`
    /// freezes tax but not cost. What a sale was actually PROFITABLE at needs the
    /// cost the goods carried on the day they sold, and the credit note is
    /// exactly when a merchant looks backwards. The app computes no margin today,
    /// so nothing needs the figure; the cost snapshot is to be added when a
    /// margin report exists, once, rather than paid for speculatively here.
    pub async fn add_line(
        &self,
        actor: i64,
        return_id: i64,
        sale_line_id: i64,
        qty: Decimal,
    ) -> AppResult<CustomerReturnLine> {
        let customer_return = self.return_or_404(return_id).await?;
        Self::ensure_draft(&customer_return)?;
        let parent = self.parent_line(sale_line_id).await?;
        let taken = self
            .returns
            .confirmed_qty_taken_by_sale_line(sale_line_id)
            .await?;
        Self::ensure_within_parent(&parent, taken, qty)?;
        let line = self
            .returns
            .create_line(return_id, sale_line_id, qty, parent.unit_price)
            .await?;
        self.touch(return_id, actor).await?;
        Ok(line)
    }

    /// Edit a credit note line's QUANTITY. There is deliberately no price
    /// argument: the repository's `update_line` takes none, so the frozen
    /// `unit_price` is not a thing an operator may move even if a caller wanted
    /// to.
    pub async fn update_line(
        &self,
        actor: i64,
        line_id: i64,
        qty: Decimal,
    ) -> AppResult<CustomerReturnLine> {
        let line = self.returns.find_line(line_id).await?.ok_or_else(|| {
            AppError::NotFound(format!("customer return line {line_id} not found"))
        })?;
        let customer_return = self.return_or_404(line.return_id).await?;
        Self::ensure_draft(&customer_return)?;
        let parent = self.parent_line(line.sale_line_id).await?;
        // A DRAFT reserves nothing, so the figure here excludes this line's own
        // document — which is what lets a draft be edited UP to the parent's full
        // quantity rather than being refused by its own earlier self.
        let taken = self
            .returns
            .confirmed_qty_taken_by_sale_line(line.sale_line_id)
            .await?;
        Self::ensure_within_parent(&parent, taken, qty)?;
        let line = self.returns.update_line(line_id, qty).await?;
        self.touch(line.return_id, actor).await?;
        Ok(line)
    }

    pub async fn remove_line(&self, actor: i64, line_id: i64) -> AppResult<()> {
        let line = self.returns.find_line(line_id).await?.ok_or_else(|| {
            AppError::NotFound(format!("customer return line {line_id} not found"))
        })?;
        let customer_return = self.return_or_404(line.return_id).await?;
        Self::ensure_draft(&customer_return)?;
        self.returns.delete_line(line_id).await?;
        self.touch(line.return_id, actor).await?;
        Ok(())
    }

    /// A line write is an edit of the document, so it stamps the draft's
    /// `updated_by` with this request's actor.
    async fn touch(&self, return_id: i64, actor: i64) -> AppResult<()> {
        let current = self.return_or_404(return_id).await?;
        self.returns
            .update_draft(return_id, actor, current.return_date, &current.notes)
            .await?;
        Ok(())
    }

    pub async fn get_detail(&self, return_id: i64) -> AppResult<CustomerReturnDetail> {
        let customer_return = self.return_or_404(return_id).await?;
        self.detail_for(customer_return).await
    }

    // -- List ------------------------------------------------------------------

    /// Every credit note the filter selects, each with its lines, its payments
    /// and its derived money. Mirrors `PurchasesService::list_details_filtered`
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
    /// than a choice: `PurchaseListFilter` carries a typed `customer: Option<String>`
    /// that the SERVICE turns into ids, because resolving a name needs the
    /// customers table and this service holds no customer repository — its five
    /// repositories are returns, sequences, sales, inventory and transactions.
    /// The repository therefore takes the resolved ids only, and a caller that
    /// has a name to resolve has to do it itself. Adding a `CustomerRepository`
    /// here is the natural fix and is a constructor change the routes will
    /// drive.
    pub async fn list(
        &self,
        filter: &crate::repositories::customer_return_repo::CustomerReturnListFilter,
    ) -> AppResult<Vec<CustomerReturnDetail>> {
        let customer_returns = self.returns.list_returns(filter).await?;
        let mut out = Vec::with_capacity(customer_returns.len());
        for customer_return in customer_returns {
            out.push(self.detail_for(customer_return).await?);
        }
        Ok(out)
    }

    // -- Confirm ---------------------------------------------------------------

    pub async fn confirm(&self, actor: i64, return_id: i64) -> AppResult<CustomerReturnDetail> {
        let customer_return = self.return_or_404(return_id).await?;
        if customer_return.status == CustomerReturnStatus::Confirmed {
            return Err(AppError::Validation(
                "customer return already confirmed".into(),
            ));
        }
        if customer_return.status == CustomerReturnStatus::Cancelled {
            return Err(AppError::Validation(
                "cancelled customer return cannot be confirmed".into(),
            ));
        }

        let lines = self.returns.list_lines(return_id).await?;
        if lines.is_empty() {
            return Err(AppError::Validation(
                "cannot confirm a customer return with no lines".into(),
            ));
        }

        // The parent is re-read HERE and not taken from the draft, so a credit
        // note whose parent has been cancelled since the line was added is
        // refused before a single write. The customer was copied at creation and
        // is not re-resolved: a deactivated customer is still the customer the
        // money goes back to.
        let sale = self.confirmed_parent(customer_return.sale_id).await?;

        // Per line: the quantity is real, it fits what is LEFT of the parent
        // line, and the product is resolvable. The creditable ceiling is
        // re-checked on the confirm path as well as on the write path, for the
        // same reason `SalesService::confirm` re-checks the credit limit: a
        // confirm must never accept a line the service's own writes would have
        // refused.
        //
        // **THIS IS THE READ THAT SPENDS THE ALLOWANCE.** A draft reserves
        // nothing, so at this point `confirmed_qty_taken_by_sale_line` still
        // excludes THIS document's own lines — and it must, or a credit note
        // could never be confirmed at all: every line would be measured against a
        // pool its own draft had already drawn from. The figure read here is the
        // sum over OTHER confirmed credit notes, which is precisely what a
        // confirm is allowed to consume and what a draft is allowed to ignore.
        let mut tracked: Vec<(CustomerReturnLine, SaleLine)> = Vec::new();
        for line in &lines {
            let parent = self.parent_line(line.sale_line_id).await?;
            let taken = self
                .returns
                .confirmed_qty_taken_by_sale_line(line.sale_line_id)
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
        // the sale side: a confirmation that cannot state what the credit note
        // costs must refuse with nothing written.
        let money = Self::document_money(&lines, &self.returns.list_payments(return_id).await?)
            .map_err(AppError::PriceRefused)?;
        let total = money.total;

        // ---- THE REFUND CAP -------------------------------------------------
        //
        // A credit note refunds AT MOST what the parent sale has actually
        // COLLECTED. The rule exists because a sale can be confirmed and only
        // partly paid, and refunding more than came in would leave a negative
        // receivable the app has no concept to hold.
        //
        // The refunds are allocated PER ORIGINATING ACCOUNT — each one leaves the
        // account and by the method the parent's payment arrived through — which
        // is exactly what `SalesService::cancel` does when it refunds a sale,
        // and it is the only allocation that can be justified: the credit note
        // does not know how the sale was paid, and the parent does.
        //
        // A PARENT THAT COLLECTED NOTHING IS NOT A REFUSAL. It writes no refund
        // rows and the goods still come back, because the cap is about how much
        // MONEY moves, not about whether the goods move. Migration 41 says the
        // same in its own comment.
        //
        // A PARENT THAT COLLECTED SOMETHING IS A CEILING. A credit note worth
        // more than came in is REFUSED, with the shortfall named, because this
        // app has no notion of a credit owed BY a customer: a shop that sells on
        // credit and takes the goods back before the customer pays has a
        // legitimate case the cap cannot hold, and it is v1's answer rather than
        // a solution. Do not invent a credit-balance mechanism here to make that
        // case fit; it needs a decision.
        let parent_payments = self.sales.list_payments(sale.id).await?;
        let collected = checked_money_sum(parent_payments.iter().map(|p| &p.amount))
            .map_err(AppError::PriceRefused)?;
        if collected > Decimal::ZERO && total > collected {
            return Err(AppError::Validation(format!(
                "credit note is worth {total} but sale {} has only collected {collected} from \
                 customer {}: a refund cannot exceed what was taken, and this app has no \
                 credit balance to hold the difference. Collect the sale first, or credit less.",
                sale.sale_number.as_deref().unwrap_or("(unnumbered)"),
                sale.customer_id
            )));
        }
        let plan = Self::refund_plan(total, &parent_payments)?;

        // ---- THE WRITE UNIT -------------------------------------------------
        //
        // Everything from here to the COMMIT is ONE transaction: the sequence
        // number, one stock movement per tracked line, one Expense per planned
        // refund, the `customer_return_payments` rows, and `set_confirmed`.
        //
        // EVERY repository call inside is an `_in` form. On a credit note this
        // matters MORE than on a purchase: the refund is an EXPENSE, so the unit
        // is what stops the shop having taken the goods back and then failed to
        // pay for them — or worse, paid for them and failed to take the goods.
        //
        // **NO SATELLITE WRITE, by decision 2 of the design.** A credit note does
        // not touch `product_supplier_costs`, in any step and in either
        // direction. A sale at a price the supplier already charged sets no
        // price, so `current_cost`, `previous_cost` and their dates are untouched
        // and the derived price-change alert correctly does not fire. There is
        // deliberately no `record_cost` call below: if you are reading this
        // method and looking for where the satellite should be written, the
        // answer is that it must not be.
        //
        // The BEGIN goes HERE and not one line earlier, on purpose. Every read
        // above it — the document, the parent, the per-line parent lookup, the
        // tracked predicate, the totals, the refund cap — is a pre-check, and a
        // pre-check buys EARLY refusal with a useful message rather than
        // reachability.
        //
        // ROLLBACK IS THE `?`. There is deliberately no explicit rollback arm
        // and no `unwrap_or` on the way out: every `?` here drops the
        // `Transaction`, sqlx rolls it back, and the `AppError` that caused it
        // propagates UNCHANGED. An explicit arm would be a place to swallow a
        // refusal, and the refusal IS the answer. Do not add one.
        let mut tx = self.returns.pool().begin().await?;

        // 1. The number. `SRET`, not the long form: the short prefix is the same
        //    decision the sale and purchase numbers took, for the same reason.
        let year = customer_return.return_date.year();
        let seq = self.sequences.next_number_in(&mut tx, "SRET", year).await?;
        let credit_note_number = format_customer_return_number(year, seq);

        // 2. One stock movement per line: In, reason Sale-return. The movement
        //    carries the CONFIRMING request's actor, never a fresh one.
        for (line, parent) in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: parent.product_id,
                        qty: line.qty,
                        movement_type: MovementType::In,
                        reason: MovementReason::SaleReturn,
                        reference: credit_note_number.clone(),
                        date: customer_return.return_date,
                    },
                )
                .await?;
        }

        // 3 and 4. One Expense per planned refund, `reference` = this credit
        //    note's own number, then the payment row that claims it. **An
        //    Expense is money LEAVING**, so `create_with_reference_in` applies
        //    the overdraft guard to each refund: a refund the shop cannot pay for
        //    is refused rather than promised, and because the check is inside the
        //    unit, the refusal takes the number and the movements back with it.
        //    This is the one behavioural asymmetry with the purchase return,
        //    where the refund is an Income and the guard never fires.
        for refund in &plan {
            let expense = self
                .transactions
                .create_with_reference_in(
                    &mut tx,
                    actor,
                    refund.account_id,
                    crate::models::TransactionKind::Expense,
                    refund.amount,
                    Some(credit_note_number.clone()),
                    Some(credit_note_number.clone()),
                    customer_return.return_date,
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
                    customer_return.return_date,
                    Some(expense.id),
                )
                .await?;

            // The money leg is a separate party obligation movement: one Refund
            // per delivery, located on this return (not the parent payment). Its
            // positive sign comes only from PartyEntryKind's rule; the account
            // above remains the parent payment's historical account.
            self.party_ledger
                .insert_in(
                    &mut tx,
                    &crate::models::NewPartyLedgerEntry {
                        party_type: crate::models::PartyType::Customer,
                        party_id: sale.customer_id,
                        kind: crate::models::PartyEntryKind::Refund,
                        amount: crate::models::PartyEntryKind::Refund
                            .signed_amount(refund.amount),
                        document_kind: crate::models::PartyDocumentKind::CustomerReturn,
                        document_id: return_id,
                        entry_date: customer_return.return_date,
                        reference: Some(credit_note_number.clone()),
                        created_by: actor,
                    },
                )
                .await?;
        }

        // 5. The customer's journal, in the SAME unit as the goods movement (T2
        // of odd/tasks/party-ledger.md). A credit note takes goods back, so it
        // CANCELS part of what the sale charged: a `Return` of `−total`, which
        // folds the customer's debt down by exactly the value of what they
        // returned.
        //
        // The goods leg is written independently from the refunds above: the
        // goods came back whether or not money did — a parent that collected
        // nothing legitimately produces a Return with no Refund. Each planned
        // cash delivery already appended its own Refund entry in the loop.
        //
        // Written even when the refund plan is empty: the Return is about the
        // GOODS, not about the money.
        self.party_ledger
            .insert_in(
                &mut tx,
                &crate::models::NewPartyLedgerEntry {
                    party_type: crate::models::PartyType::Customer,
                    party_id: sale.customer_id,
                    kind: crate::models::PartyEntryKind::Return,
                    amount: crate::models::PartyEntryKind::Return.signed_amount(total),
                    document_kind: crate::models::PartyDocumentKind::CustomerReturn,
                    document_id: return_id,
                    entry_date: customer_return.return_date,
                    reference: Some(credit_note_number.clone()),
                    created_by: actor,
                },
            )
            .await?;

        // 6. The document exists from here.
        let confirmed = self
            .returns
            .set_confirmed_in(&mut tx, return_id, actor, &credit_note_number)
            .await?;

        tx.commit().await?;

        // ---- AFTER THE COMMIT, DELIBERATELY -------------------------------
        //
        // `detail_for` reads the document's lines and payments, and it stays on
        // the pool on purpose: a pool read beneath an open unit cannot answer on
        // a one-connection pool (30s, then `PoolTimedOut`).
        self.detail_for(confirmed).await
    }

    /// Split a credit note's worth across the parent's payments, oldest first,
    /// each taking no more than that payment carried. A payment whose whole
    /// amount is consumed produces no row; a payment with nothing left to take
    /// is skipped, which is what makes a parent confirmed but unpaid produce an
    /// EMPTY plan rather than zero-amount refunds — and `create_with_reference`
    /// refuses an amount of zero in any case.
    ///
    /// Checked on the running remainder, because `remaining -= take` is a raw
    /// subtraction of an operator-controlled figure against another one. It
    /// cannot underflow by construction — `take` is `min(remaining, ...)` — and
    /// the checked form makes that a property of the code rather than a fact
    /// about the caller.
    fn refund_plan(
        total: Decimal,
        parent_payments: &[crate::models::SalePayment],
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

    /// Reverse a CONFIRMED credit note. A refund sent by mistake must be
    /// undoable, so the reversal is as real as the confirm: the stock leaves the
    /// shelf the other way, the money comes back the other way, and every
    /// reversal finance row is linked to the refund row it reverses through
    /// `set_payment_refund_transaction`, so a credit note that has itself been
    /// reversed carries BOTH links on each row.
    ///
    /// This mirrors `PurchasesService::cancel` step for step, including what it
    /// does NOT do. It is not a transaction: the existing annulment path on both
    /// families writes on the pool, and making this one atomic while the other
    /// two are not would be a change to behaviour with no unit to justify it.
    /// It has no aggregate balance pre-check, and here it genuinely cannot need
    /// one: the reversal of an Expense is an INCOME, so no account can be
    /// overdrawn by it and the guard `create_with_reference` applies to the
    /// confirm's Expense is exactly reversed by it.
    ///
    /// **THE MOVEMENT REASON IS `Adjust`, and that is an interpretation, not a
    /// given.** The confirm wrote `In` with reason `Sale-return`, so the reversal
    /// must be an `Out` — and the reason vocabulary offers `Sale` (goods out
    /// because we sold them), `Purchase-return`, `Loss` and `Adjust`. Every one of
    /// the first three states something this movement is not: no sale happened,
    /// no purchase came back, and no loss was recorded. `Adjust` is the only
    /// neutral one, and it is neutral rather than wrong because the `reference`
    /// on the movement still carries the credit note's own number, so the pair is
    /// traceable. Reported rather than silently chosen: adding a dedicated
    /// "reversal" reason would be a model and migration change, which is a later
    /// decision.
    pub async fn cancel(
        &self,
        actor: i64,
        return_id: i64,
        reason: Option<String>,
    ) -> AppResult<CustomerReturnDetail> {
        let customer_return = self.return_or_404(return_id).await?;
        if customer_return.status == CustomerReturnStatus::Cancelled {
            return Err(AppError::Validation(
                "customer return already cancelled".into(),
            ));
        }

        // The document's money is resolved BEFORE any write on this path, for
        // the reason `PurchasesService::cancel` states: without it a reversal
        // would move stock and money and only the read at the end would refuse.
        let lines = self.returns.list_lines(return_id).await?;
        let payments = self.returns.list_payments(return_id).await?;
        Self::document_money(&lines, &payments).map_err(AppError::PriceRefused)?;

        if customer_return.status == CustomerReturnStatus::Draft {
            // Draft -> Cancelled: discard, no stock or finance side effect.
            let cancelled = self
                .returns
                .set_cancelled(return_id, actor, reason.as_deref())
                .await?;
            return self.detail_for(cancelled).await;
        }

        let credit_note_number = customer_return.credit_note_number.clone().ok_or_else(|| {
            AppError::Internal("confirmed customer return missing credit_note_number".into())
        })?;

        // A partially-applied reversal is REFUSED, not doubled: a second pass
        // would take the goods off the shelf again and reverse every refund twice.
        let partial = payments
            .iter()
            .filter(|p| p.refund_transaction_id.is_some())
            .count();
        if partial > 0 {
            return Err(AppError::Validation(format!(
                "reversal already partially applied: {partial} of {} refunds already link a reversal; \
                 refusing to take the goods back a second time or duplicate the reversal",
                payments.len()
            )));
        }

        let mut tracked: Vec<(CustomerReturnLine, SaleLine)> = Vec::new();
        for line in &lines {
            let parent = self.parent_line(line.sale_line_id).await?;
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
        // The stock movements, the reversal deliveries and the cancellation are ONE
        // unit. Before, each reversal was posted by `create_with_reference` — a unit of
        // its own — and only then was the note flipped to Cancelled, so a failure in
        // between left the money reversed and the note still Confirmed.
        //
        // The unit opens BEFORE the stock, unlike `confirm` where the movement comes
        // after: here the goods leaving the shelf is the first half of the same
        // reversal, and a failure after it must take it back.
        let mut tx = self.returns.pool().begin().await?;

        // Stock Out: the goods the customer sent back leave the shelf again. The
        // movement carries the cancelling request's actor, like its reversal.
        for (line, parent) in &tracked {
            self.inventory
                .record_movement_in(
                    &mut tx,
                    actor,
                    NewMovement {
                        product_id: parent.product_id,
                        qty: line.qty,
                        movement_type: MovementType::Out,
                        reason: MovementReason::Adjust,
                        reference: credit_note_number.clone(),
                        date: customer_return.return_date,
                    },
                )
                .await?;
        }

        // The money comes back in, per originating account, as a delivery that REPLAYS
        // the account the refund went out of (decision 9). An `Income` is money
        // ENTERING, so no account can be overdrawn by this and the reversal cannot be
        // refused for want of funds — which is what makes a credit note genuinely
        // reversible, where a purchase return's reversal can be refused because the shop
        // has already spent the refund it received.
        for pay in &payments {
            let reversal_delivery = crate::services::payment_writer::record_delivery_in(
                &self.sequences,
                &self.transactions,
                &self.party_ledger,
                &self.payments,
                &mut tx,
                actor,
                crate::models::PaymentDirection::In,
                crate::models::PartyType::Customer,
                customer_return.customer_id,
                (crate::models::PartyDocumentKind::CustomerReturn, return_id),
                pay.method_id,
                pay.account_id,
                pay.amount,
                customer_return.return_date,
                Some(format!("cancellation of {credit_note_number}")),
                Some(credit_note_number.clone()),
                None,
                // No allocation: this reverses a document, it does not cover a debt.
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
    /// posted nothing: a Draft, and a credit note discarded while still Draft
    /// (Cancelled with `credit_note_number` still NULL). A confirmed credit note
    /// — even one cancelled afterwards — is REVERSED through `cancel` instead:
    /// deleting it would strand its movements and its refund transactions, and
    /// its number proves it was confirmed.
    ///
    /// No `actor` parameter, for the reason `PurchasesService::delete_draft` has
    /// none: nothing survives to stamp.
    ///
    /// WHAT THE REPOSITORY PREDICATE DOES NOT ESTABLISH, and what this method
    /// therefore still owes the reader: it answers "may this row be removed",
    /// and it says nothing about whether the row is CLEAN. On the sale family
    /// that gap was real. Here it is closed by the WRITES rather than by this
    /// clause — `confirm` is one transaction, so a Draft of a credit note has no
    /// committed payment, movement or finance row behind it — and that is a
    /// property measured by the `confirm_failure_*` tests, not inferred from SQL.
    pub async fn delete_draft(&self, id: i64) -> AppResult<()> {
        let customer_return = self.return_or_404(id).await?;
        let deletable = customer_return.status == CustomerReturnStatus::Draft
            || (customer_return.status == CustomerReturnStatus::Cancelled
                && customer_return.credit_note_number.is_none());
        if !deletable {
            return Err(AppError::Validation(format!(
                "customer return {id} is {}: only a draft or a discarded (never-confirmed) cancelled \
                 credit note can be deleted",
                customer_return.status
            )));
        }
        let deleted = self.returns.delete_draft(id).await?;
        if !deleted {
            return Err(AppError::Validation(format!(
                "customer return {id} is no longer deletable: only a draft or a discarded \
                 (never-confirmed) cancelled credit note can be deleted"
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
        customer_return_repo::CustomerReturnListFilter, CustomerReturnRepository,
        SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteCustomerReturnRepository, SqliteDocSequenceRepository, SqliteProductRepository,
        SqliteSaleRepository, SqliteStockMovementRepository, SqliteTransactionRepository,
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

    type Svc = CustomerReturnService<
        SqliteCustomerReturnRepository,
        SqliteDocSequenceRepository,
        SqliteSaleRepository,
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
        SqliteAccountRepository,
        SqliteTransactionRepository,
        SqlitePartyLedgerRepository,
        SqlitePaymentRepository,
    >;

    /// `max_connections(1)` is LOAD-BEARING for every test in this module. While
    /// `confirm`'s unit is open it holds the only connection the pool owns, so a
    /// repository call that reached for the pool instead of joining it could not
    /// answer at all — 30 seconds of sqlx acquire timeout, then `PoolTimedOut`.
    /// A wider pool would hide every `_in` regression here behind a second
    /// connection that reads and writes outside the unit.
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
    /// paths reachable on a fresh pool. `allow_negative = false` is the point of
    /// `svc_with_flags` here rather than a leftover: on a credit note the refund
    /// is an EXPENSE, so the overdraft guard is the rule this family actually
    /// runs, and a test that needs it must ask for it deliberately.
    async fn svc_with_flags(allow_stock: bool, allow_balance: bool) -> (Svc, SqlitePool) {
        let pool = test_pool().await;
        let s = CustomerReturnService::new(
            SqliteCustomerReturnRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            SqliteSaleRepository::new(pool.clone()),
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

    fn sale_date() -> NaiveDate {
        d(2024, 5, 2)
    }

    /// The credit note's own day, a MONTH later: a return is dated when it is
    /// MADE, so the number's year and the movement's date are not the parent's.
    fn return_date() -> NaiveDate {
        d(2024, 6, 1)
    }

    async fn actor(pool: &SqlitePool) -> i64 {
        test_support::audit_actor_id(pool).await.unwrap()
    }

    struct Parent {
        product_id: i64,
        sale_line_id: i64,
        return_id: i64,
    }

    /// A tracked product. A credit note brings stock IN, so the fixture does not
    /// pre-stock it: the level after a confirm is exactly what came back.
    async fn seed_product(s: &Svc, pool: &SqlitePool, sku: &str) -> crate::models::Product {
        s.inventory
            .create_product(
                actor(pool).await,
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
            .unwrap()
    }

    async fn seed_customer(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar(
            r#"INSERT INTO customers (name, is_walkin, is_active, created_by)
               VALUES (?, 0, 1, ?) RETURNING id"#,
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

    /// One CONFIRMED sale carrying one line. Seeded through raw SQL because the
    /// parent's own confirm path is `SalesService`'s subject and is tested there.
    /// `number` is caller-chosen because `sale_number` is UNIQUE and several
    /// tests need two parents.
    async fn seed_parent(
        pool: &SqlitePool,
        customer_id: i64,
        product_id: i64,
        qty: &str,
        unit_price: &str,
        number: &str,
    ) -> (i64, i64) {
        let sale = sqlx::query_scalar(
            r#"INSERT INTO sales (customer_id, customer_name, status, payment_type, sale_date, sale_number, created_by)
               VALUES (?, 'Named Customer', 'Confirmed', 'Credit', ?, ?, ?) RETURNING id"#,
        )
        .bind(customer_id)
        .bind(sale_date())
        .bind(number)
        .bind(actor(pool).await)
        .fetch_one(pool)
        .await
        .unwrap();
        let line = add_parent_line(pool, sale, product_id, qty, unit_price).await;
        (sale, line)
    }

    /// A SECOND line on an existing sale, of a DIFFERENT product. A sale may
    /// repeat a product, so the second line can even be of the same product —
    /// which is the one asymmetry between the two families a return test can
    /// show: `UNIQUE (return_id, sale_line_id)` is about LINES, not products.
    async fn add_parent_line(
        pool: &SqlitePool,
        sale_id: i64,
        product_id: i64,
        qty: &str,
        unit_price: &str,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(sale_id)
        .bind(product_id)
        .bind(qty)
        .bind(unit_price)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// What the parent sale has COLLECTED. Each tuple is `(account_name, amount)`
    /// and produces a real `transactions` row plus the `sale_payments` row that
    /// claims it, because the refund cap reads the PAYMENTS and the allocation
    /// reads their accounts.
    async fn collect(pool: &SqlitePool, sale_id: i64, number: &str, amounts: &[(&str, &str)]) {
        let who = actor(pool).await;
        for (account_name, amount) in amounts {
            let account = seed_account(pool, account_name).await;
            let method = owned_method(pool, account).await;
            let tx: i64 = sqlx::query_scalar(
                r#"INSERT INTO transactions (account_id, kind, amount, description, reference, date, created_by)
                   VALUES (?, 'Income', ?, 'collected from the customer', ?, ?, ?) RETURNING id"#,
            )
            .bind(account)
            .bind(dec(amount).to_string())
            .bind(number)
            .bind(sale_date())
            .bind(who)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query(
                r#"INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, transaction_id, created_by)
                   VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(sale_id)
            .bind(account)
            .bind(method)
            .bind(dec(amount).to_string())
            .bind(sale_date())
            .bind(tx)
            .bind(who)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    /// The standard shape: a product, a customer, a CONFIRMED sale of `qty` units
    /// at `unit_price` that collected `collected`, and a DRAFT credit note of
    /// `credited` of them.
    async fn draft_credit_note(
        s: &Svc,
        pool: &SqlitePool,
        sku: &str,
        customer: &str,
        qty: &str,
        unit_price: &str,
        collected: &[(&str, &str)],
        credited: &str,
    ) -> Parent {
        let number = format!("2024-SALE-{sku}");
        let product = seed_product(s, pool, sku).await;
        let customer_id = seed_customer(pool, customer).await;
        let (sale, line) =
            seed_parent(pool, customer_id, product.id, qty, unit_price, &number).await;
        collect(pool, sale, &number, collected).await;
        let who = actor(pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, credit_note.id, line, dec(credited))
            .await
            .unwrap();
        Parent {
            product_id: product.id,
            sale_line_id: line,
            return_id: credit_note.id,
        }
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
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_return_payments")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn sret_sequence_last(pool: &SqlitePool) -> Option<i64> {
        sqlx::query_as::<_, (i64,)>("SELECT last_number FROM doc_sequences WHERE doc_type = 'SRET'")
            .fetch_optional(pool)
            .await
            .unwrap()
            .map(|r| r.0)
    }

    async fn row_state(pool: &SqlitePool, return_id: i64) -> (String, Option<String>) {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, credit_note_number FROM customer_returns WHERE id = ?",
        )
        .bind(return_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The whole cost satellite as comparable TEXT. Byte for byte is the claim:
    /// a confirmed credit note must leave every column of every row exactly as
    /// it found them. On this family the satellite is doubly untouched — a
    /// credit note writes nothing AND it does not even have a cost to write,
    /// because `customer_return_lines` has no `unit_cost` column at all.
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

    /// Money LEAVING an account, written through raw SQL because the point is to
    /// put the account in a state the credit note's own refund did not create.
    /// This is what makes the overdraft-guard test a test of the guard.
    async fn drain(pool: &SqlitePool, account_name: &str, amount: &str) {
        sqlx::query(
            r#"INSERT INTO transactions (account_id, kind, amount, description, date, created_by)
               SELECT id, 'Expense', ?, 'spent on something else', ?, ? FROM accounts WHERE name = ?"#,
        )
        .bind(dec(amount).to_string())
        .bind(sale_date())
        .bind(actor(pool).await)
        .bind(account_name)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn inject(pool: &SqlitePool, ddl: String) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
            .execute(pool)
            .await
            .unwrap();
    }

    // ========================================================================
    // THE CREDIT NOTE
    // ========================================================================

    /// The mirror of the purchase return's headline case, and the three rows the
    /// design table says are the only difference: stock IN rather than out, the
    /// money as an EXPENSE rather than an Income, and therefore an overdraft
    /// guard that fires.
    ///
    /// Read from the database rather than from the returned view, because the
    /// view is derived from the same rows and would agree even if the writes had
    /// gone somewhere else.
    #[tokio::test]
    async fn a_credit_note_of_part_of_a_sale_freezes_the_parents_price_and_pays_the_money_back_as_an_expense(
    ) {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "MIRROR",
            "Mirror Customer",
            "3",
            "4",
            &[("mirror till", "12")],
            "2",
        )
        .await;

        let detail = s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        // The line took the PARENT's frozen price: the API has no price field.
        assert_eq!(detail.lines[0].sale_line_id, p.sale_line_id);
        assert_eq!(detail.lines[0].qty, dec("2"));
        assert_eq!(
            detail.lines[0].unit_price,
            dec("4"),
            "a credit note line is at the parent line's price, frozen when the line was added"
        );

        // The document total is the CREDITED quantity at that price, not the
        // parent's whole quantity: 2 x 4, not 3 x 4.
        assert_eq!(detail.total, dec("8"));
        assert_eq!(detail.net_subtotal, detail.total);
        assert_eq!(
            detail.paid,
            dec("8"),
            "the full value went back to the customer"
        );
        assert_eq!(detail.due, Decimal::ZERO);
        assert_eq!(detail.payment_status, PaymentStatus::Paid);

        // The stock came IN, with the reason that already existed for the
        // physical event (decision 3).
        assert_eq!(
            movement_reasons(&pool).await,
            vec!["Sale-return"],
            "the goods came back with reason Sale-return, which the stock CHECK already accepted"
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            dec("2"),
            "two units are on the shelf that were not there before"
        );

        // The money went OUT as an EXPENSE — the purchase return's exact mirror.
        assert_eq!(
            tx_kinds(&pool).await,
            vec!["Income", "Expense"],
            "the parent's own collection was an Income; the refund is an Expense"
        );
        assert_eq!(
            tx_references(&pool).await,
            vec!["2024-SALE-MIRROR", "2024-SRET-000001"],
            "the refund is stamped with the CREDIT NOTE's own number, not the parent's"
        );
        assert_eq!(payment_count(&pool).await, 1);
        assert_eq!(
            detail.payments[0].transaction_id.is_some(),
            true,
            "the refund row claims the finance row it produced"
        );

        // The number was taken in the return's own year under the short prefix,
        // and it is named for the DOCUMENT rather than for the family.
        assert_eq!(
            detail.customer_return.credit_note_number.as_deref(),
            Some("2024-SRET-000001")
        );
        assert_eq!(
            detail.customer_return.status,
            CustomerReturnStatus::Confirmed
        );
        assert_eq!(sret_sequence_last(&pool).await, Some(1));
    }

    /// The refund-table EXEMPTION from migration 44, proved end to end: a
    /// refund does not CHOOSE a pair, it REPLAYS the parent payment's pair
    /// (`RefundPlan` copies pay.account_id / pay.method_id). After the method
    /// is re-pointed to ANOTHER account, the credit note still confirms and
    /// the money comes back out of the box it went into — the method's
    /// current owner never receives it.
    #[tokio::test]
    async fn a_refund_replays_the_parent_payments_account_even_after_the_method_is_repointed() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "HISTORY",
            "History Customer",
            "3",
            "4",
            &[("history till", "12")],
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
        let (till_account,): (i64,) =
            sqlx::query_as("SELECT id FROM accounts WHERE name = 'history till'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (refund_account,): (i64,) =
            sqlx::query_as("SELECT account_id FROM customer_return_payments WHERE return_id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            refund_account, till_account,
            "the money goes back out of the box it went into"
        );
        // The refund's own finance row lands in that same historical account.
        let (expense_account,): (i64,) =
            sqlx::query_as("SELECT account_id FROM transactions WHERE kind = 'Expense'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            expense_account, till_account,
            "the refund Expense is stamped with the historical account"
        );
    }

    /// **THE OVERDRAFT GUARD, and the one behavioural asymmetry between the two
    /// families.** On a purchase return the refund is an Income and the guard
    /// never fires; on a credit note it is an Expense and the guard fires, as it
    /// does on a sale's annulment. This test builds the STRICT service on
    /// purpose — `svc()` passes `allow_negative = true`, so the strict branch
    /// would never run and the test would pass for the wrong reason, which is
    /// the exact trap `AGENTS.md` records for the purchase tests.
    ///
    /// The assertion is that nothing was written, which is the part the
    /// transaction buys: the guard fires inside the unit, so the number, the
    /// stock movement and the payment row all go back with it. The shop has not
    /// taken the goods back and not paid for them.
    #[tokio::test]
    async fn a_credit_note_the_shop_cannot_pay_for_is_refused_and_writes_nothing() {
        let (s, pool) = svc_with_flags(true, false).await;
        let p = draft_credit_note(
            &s,
            &pool,
            "GUARD",
            "Guard Customer",
            "3",
            "4",
            &[("guard till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        // The shop has already spent the money the sale brought in. The refund
        // is an Expense against THIS account — the account the parent's payment
        // arrived through, which is where the per-originating-account allocation
        // sends it — so draining it here is what puts the refund in reach of the
        // overdraft guard. Without the drain the collection alone funds the
        // refund and the test would pass for the wrong reason.
        drain(&pool, "guard till", "10").await;

        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("insufficient funds"),
                "the refusal must come from the M0 overdraft guard, not from a return rule: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }

        assert_eq!(sret_sequence_last(&pool).await, None, "no number was spent");
        assert_eq!(
            movement_count(&pool).await,
            0,
            "the goods did NOT come back on a document that could not be paid for"
        );
        assert_eq!(
            tx_count(&pool).await,
            2,
            "only the fixture's own two rows exist — the sale's collection and the \
             deliberate drain: the refund Expense rolled back with the guard's refusal"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None)
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            Decimal::ZERO,
            "and the shelf is exactly as empty as it started"
        );
    }

    /// The permissive build refuses nothing on this rule, which is what makes the
    /// guard test above a test of the guard rather than of a fixture: the same
    /// document, the same figures, the same fixture — only the constructor flag
    /// differs.
    #[tokio::test]
    async fn the_same_credit_note_confirms_when_the_account_may_go_negative() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "PERM",
            "Permissive Customer",
            "3",
            "4",
            &[("permissive till", "12")],
            "2",
        )
        .await;
        let detail = s.confirm(actor(&pool).await, p.return_id).await.unwrap();
        assert_eq!(detail.total, dec("8"));
        assert_eq!(payment_count(&pool).await, 1);
    }

    /// Two credit note lines may carry the SAME product, because a sale may
    /// carry it twice — and the constraint that forbids a repeated product on a
    /// PURCHASE (because the cost satellite holds one price per product and
    /// supplier) has no purchase here to constrain. `UNIQUE (return_id,
    /// sale_line_id)` is about LINES, not about products, and this is the one
    /// place the two families are not a clean mirror in what they permit.
    #[tokio::test]
    async fn two_credit_note_lines_may_carry_the_same_product_because_a_sale_may() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "REPEAT").await;
        let customer_id = seed_customer(&pool, "Repeat Customer").await;
        let number = "2024-SALE-REPEAT";
        let (sale, first) = seed_parent(&pool, customer_id, product.id, "2", "5", number).await;
        let second = add_parent_line(&pool, sale, product.id, "3", "7").await;
        collect(&pool, sale, number, &[("repeat till", "31")]).await;
        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, credit_note.id, first, dec("1"))
            .await
            .unwrap();
        s.add_line(who, credit_note.id, second, dec("2"))
            .await
            .unwrap();

        let detail = s.confirm(who, credit_note.id).await.unwrap();
        assert_eq!(
            detail.lines.len(),
            2,
            "both lines survive, at their own frozen prices"
        );
        assert_eq!(detail.lines[0].unit_price, dec("5"));
        assert_eq!(detail.lines[1].unit_price, dec("7"));
        assert_eq!(detail.total, dec("5") + dec("14"));
        assert_eq!(
            movement_count(&pool).await,
            2,
            "two movements for one product: the return line IS a separate movement"
        );
    }

    /// A credit note is a document about a document that EXISTS. A Draft parent
    /// has no settled price to credit against; a cancelled one already took its
    /// goods back. Both are refused at the parent read, before a row is written.
    #[tokio::test]
    async fn a_credit_note_of_a_draft_or_cancelled_sale_is_refused() {
        let (s, pool) = svc().await;
        let customer_id = seed_customer(&pool, "State Customer").await;
        let who = actor(&pool).await;

        let draft: i64 = sqlx::query_scalar(
            "INSERT INTO sales (customer_id, customer_name, status, payment_type, sale_date, created_by) \
             VALUES (?, 'Named Customer', 'Draft', 'Credit', ?, ?) RETURNING id",
        )
        .bind(customer_id)
        .bind(sale_date())
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
            "INSERT INTO sales (customer_id, customer_name, status, payment_type, sale_date, sale_number, created_by) \
             VALUES (?, 'Named Customer', 'Cancelled', 'Credit', ?, '2024-SALE-OLD', ?) RETURNING id",
        )
        .bind(customer_id)
        .bind(sale_date())
        .bind(who)
        .fetch_one(&pool)
        .await
        .unwrap();
        match s
            .create_draft(who, cancelled, return_date(), &None)
            .await
            .unwrap_err()
        {
            AppError::Validation(msg) => assert!(msg.contains("Cancelled"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }

        let returns: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_returns")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(returns, 0, "neither refusal wrote a row");
    }

    /// A confirmed credit note is not editable: its lines are the evidence of
    /// WHAT came back. The repository carries the same predicate in its own
    /// WHERE; this proves the SERVICE reaches it and answers with a Validation
    /// naming the state rather than a bare `Conflict`.
    #[tokio::test]
    async fn a_confirmed_credit_note_refuses_every_line_edit_and_says_why() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "FROZEN",
            "Frozen Customer",
            "3",
            "4",
            &[("frozen till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let cases: Vec<(&str, Option<AppError>)> = vec![
            (
                "add_line",
                s.add_line(who, p.return_id, p.sale_line_id, dec("1"))
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
                    "{what} must name the state the credit note rests in: {msg}"
                ),
                Some(other) => panic!("{what}: expected Validation, got {other:?}"),
                None => panic!("{what} was allowed on a confirmed credit note"),
            }
        }
    }

    /// A credit note line's QUANTITY is the only thing an operator may move. The
    /// frozen price survives an edit, and there is no value a caller could
    /// supply to move it — `update_line` takes no price and the repository's
    /// `update_line` takes none either.
    #[tokio::test]
    async fn editing_a_credit_note_line_moves_its_quantity_and_never_its_price() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "QTY",
            "Quantity Customer",
            "3",
            "4",
            &[("qty till", "12")],
            "1",
        )
        .await;
        let who = actor(&pool).await;
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let edited = s.update_line(who, return_line, dec("2")).await.unwrap();
        assert_eq!(edited.qty, dec("2"));
        assert_eq!(
            edited.unit_price,
            dec("4"),
            "the price is a copy of a frozen value and has no argument that could rewrite it"
        );

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

    /// A credit note line may never claim more units than the sale line actually
    /// sold. This is the half of the rule this layer can state, and it is a real
    /// bound rather than a placeholder.
    ///
    /// The comment on `ensure_within_parent` records why the OTHER half — the
    /// subtraction of what earlier CONFIRMED credit notes already took — is
    /// absent, and this test is the honest shape of the gap: on a fresh sale the
    /// ceiling is the parent's own quantity, and nothing below this line should
    /// be read as proving that a SECOND credit note of the same line is bounded.
    #[tokio::test]
    async fn crediting_more_units_than_the_sale_line_sold_is_refused() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "OVER").await;
        let customer_id = seed_customer(&pool, "Over Customer").await;
        let (sale, line) =
            seed_parent(&pool, customer_id, product.id, "5", "4", "2024-SALE-OVER").await;
        collect(&pool, sale, "2024-SALE-OVER", &[("over till", "20")]).await;
        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();

        match s
            .add_line(who, credit_note.id, line, dec("6"))
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
        s.add_line(who, credit_note.id, line, dec("5"))
            .await
            .unwrap();
        let lines: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_return_lines")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(lines, 1, "the refused line wrote nothing at all");
    }

    /// The same ceiling on `update_line` and on `confirm`, because a check that
    /// lived only on `add_line` would be one an operator walks around by adding
    /// a valid line and then widening it.
    #[tokio::test]
    async fn the_quantity_ceiling_holds_on_update_line_and_on_confirm_too() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "CEIL",
            "Ceiling Customer",
            "4",
            "2",
            &[("ceiling till", "8")],
            "1",
        )
        .await;
        let who = actor(&pool).await;
        let return_line = s.returns.list_lines(p.return_id).await.unwrap()[0].id;

        let err = s.update_line(who, return_line, dec("9")).await.unwrap_err();
        assert!(
            matches!(&err, AppError::Validation(msg) if msg.contains("9") && msg.contains("4")),
            "widening a credit note line past the parent's own quantity is refused: {err:?}"
        );
        assert_eq!(
            s.returns.list_lines(p.return_id).await.unwrap()[0].qty,
            dec("1"),
            "the refused edit left the line exactly as it was"
        );

        sqlx::query("UPDATE customer_return_lines SET qty = '9' WHERE id = ?")
            .bind(return_line)
            .execute(&pool)
            .await
            .unwrap();
        let err = s.confirm(who, p.return_id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::Validation(msg) if msg.contains("9")),
            "confirm re-checks the ceiling rather than trusting how the line was written: {err:?}"
        );
        assert_eq!(sret_sequence_last(&pool).await, None, "no number was spent");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(movement_count(&pool).await, 0, "no goods came back");
    }

    /// The same sale line cannot be credited twice on one credit note.
    /// `UNIQUE (return_id, sale_line_id)` is the schema's backstop and this
    /// proves the repository's `Conflict` reaches a caller as a stated refusal.
    #[tokio::test]
    async fn the_same_sale_line_cannot_appear_twice_on_one_credit_note() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "DUPL",
            "Duplicate Customer",
            "3",
            "4",
            &[("dupl till", "12")],
            "1",
        )
        .await;
        let err = s
            .returns
            .create_line(p.return_id, p.sale_line_id, dec("1"), dec("4"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::Conflict(msg) if msg.contains("already on this customer return")),
            "the schema refuses a second line for the same parent line: {err:?}"
        );
        let lines: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_return_lines")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(lines, 1);
    }

    /// THE REPEAT-RETURN HOLE, closed. Two CONFIRMED credit notes of the same
    /// parent sale line are the shape that refunds twice: sell 5, take them
    /// back, take them back again, and the customer is paid for the same goods
    /// twice.
    ///
    /// The first credit note is legal and spends the whole allowance. The second
    /// one names the same parent line, so the ceiling it must respect is the
    /// parent's own quantity MINUS what the first credit note already took — and
    /// that is zero.
    ///
    /// This test is written against the API the service already had when the
    /// hole was open, and it is the RED that proved it: both `add_line` and
    /// `confirm` answered `Ok` for the second document, because
    /// `ensure_within_parent` could see only the parent line's own qty.
    ///
    /// **THE REFUSAL LANDS TWICE, and both are asserted here.** `add_line`
    /// refuses it, because the aggregate measures CONFIRMED credit notes and the
    /// first one is confirmed — an early refusal with the figures the operator
    /// needs. Then the line is put on the second draft by RAW SQL and `confirm`
    /// refuses it again, which is the assertion that matters: the write path's
    /// guard is not what makes a confirm safe.
    #[tokio::test]
    async fn a_second_confirmed_credit_note_of_the_same_parent_line_is_refused_because_the_allowance_is_spent(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "CSPENT").await;
        let customer_id = seed_customer(&pool, "Spent Allowance Customer").await;
        let number = "2024-SALE-CSPENT";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "5", "4", number).await;
        collect(&pool, sale, number, &[("cspent wallet", "20")]).await;
        let who = actor(&pool).await;

        let first = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("5")).await.unwrap();
        let first_detail = s.confirm(who, first.id).await.unwrap();
        assert_eq!(
            first_detail.total,
            dec("20"),
            "the first credit note really did take the whole line: the refund cap is \
             not what limits the second one, this test would pass for the wrong \
             reason otherwise"
        );

        // -- the WRITE path refuses, early and with the figures ----------------
        let second = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        let err = s
            .add_line(who, second.id, line, dec("5"))
            .await
            .expect_err("the allowance is spent: nothing of this line is creditable")
            .to_string();
        assert!(
            err.contains('5') && err.contains("already"),
            "the refusal must state the quantity claimed and that an earlier \
             CONFIRMED credit note already has it: {err}"
        );
        assert_eq!(
            s.returns.list_lines(second.id).await.unwrap().len(),
            0,
            "the refused line wrote nothing"
        );

        // -- and the CONFIRM path refuses it too, on its own -------------------
        sqlx::query(
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
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
        // document, and the customer was not paid twice.
        assert_eq!(
            row_state(&pool, second.id).await,
            ("Draft".to_string(), None),
            "the refused second credit note must leave no numbered document behind"
        );
        assert_eq!(
            payment_count(&pool).await,
            1,
            "ONE refund exists: the customer was paid once, for the one real credit note"
        );
        assert_eq!(
            tx_count(&pool).await,
            2,
            "the fixture's own collection and the one refund the first note produced"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("5"),
            "five units came onto the shelf, not ten: the second note moved no stock"
        );
    }

    // ========================================================================
    // THE REFUND CAP
    // ========================================================================

    /// A parent that collected NOTHING is not a refusal. The cap is about how
    /// much MONEY moves, not about whether the goods move: the credit note goes
    /// through, the goods come back, and no payment row is written because there
    /// was nothing to refund. Migration 41 says this in its own comment.
    #[tokio::test]
    async fn a_credit_note_against_a_confirmed_but_unpaid_sale_writes_no_payment_row() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(&s, &pool, "UNPAID", "Unpaid Customer", "3", "4", &[], "2").await;
        let detail = s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        assert_eq!(
            detail.customer_return.status,
            CustomerReturnStatus::Confirmed,
            "the goods still came back: an unpaid parent is not a reason to keep them"
        );
        assert!(detail.payments.is_empty(), "there was nothing to refund");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            tx_count(&pool).await,
            0,
            "and no finance row exists: the parent collected nothing, so the refund is nothing"
        );
        assert_eq!(detail.paid, Decimal::ZERO);
        assert_eq!(detail.due, dec("8"));
        assert_eq!(
            detail.payment_status,
            PaymentStatus::Unpaid,
            "the credit note is worth 8 and none of it has gone back"
        );
        assert_eq!(movement_count(&pool).await, 1, "the goods still came in");
    }

    /// A parent that collected SOMETHING is a ceiling, and exceeding it is refused
    /// with the shortfall named. This is v1's answer and not a solution: the app
    /// has no notion of a credit owed BY a customer, so a shop that sells on
    /// credit and takes the goods back before the customer pays has nowhere for
    /// its case to live. No credit-balance mechanism is invented here to make it
    /// fit.
    #[tokio::test]
    async fn a_credit_note_worth_more_than_the_sale_collected_is_refused_naming_the_shortfall() {
        let (s, pool) = svc().await;
        // Three units at 4 is 12; only 4 was collected.
        let p = draft_credit_note(
            &s,
            &pool,
            "CAP",
            "Cap Customer",
            "3",
            "4",
            &[("cap till", "4")],
            "3",
        )
        .await;
        let who = actor(&pool).await;

        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("12") && msg.contains('4'),
                    "the refusal must state what the credit note is worth and what came in: {msg}"
                );
                assert!(
                    msg.contains("credit"),
                    "and it must say why the difference has nowhere to go: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        assert_eq!(sret_sequence_last(&pool).await, None);
        assert_eq!(movement_count(&pool).await, 0, "no goods came back");
        assert_eq!(
            tx_count(&pool).await,
            1,
            "only the fixture's own collection"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None)
        );
    }

    /// The allocation the cap is implemented with, read end to end: a credit note
    /// worth 10 against collections of 4 and 6 produces TWO refund rows, in the
    /// accounts the parent's payments arrived through. This is what
    /// `SalesService::cancel` does when it refunds a sale, and it is the only
    /// allocation a credit note can justify: the credit note does not know how
    /// the sale was paid, and the parent does.
    #[tokio::test]
    async fn a_refund_is_split_across_the_accounts_the_parents_payments_came_from() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SPLIT").await;
        let customer_id = seed_customer(&pool, "Split Customer").await;
        let number = "2024-SALE-SPLIT";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "5", "2", number).await;
        collect(
            &pool,
            sale,
            number,
            &[("split first", "4"), ("split second", "6")],
        )
        .await;
        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, credit_note.id, line, dec("5"))
            .await
            .unwrap();

        let detail = s.confirm(who, credit_note.id).await.unwrap();

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
            ledger_rows(&pool, credit_note.id).await,
            vec![
                ("Refund".to_string(), "4".to_string()),
                ("Refund".to_string(), "6".to_string()),
                ("Return".to_string(), "-10".to_string()),
            ],
            "the split refund plan writes one return-located Refund for each delivery"
        );

        let accounts: Vec<i64> =
            sqlx::query_scalar("SELECT account_id FROM customer_return_payments ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(accounts.len(), 2);
        assert_ne!(
            accounts[0], accounts[1],
            "two refunds from one account would not be an allocation at all"
        );

        let orphans: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transactions t \
             WHERE NOT EXISTS (SELECT 1 FROM customer_return_payments rp WHERE rp.transaction_id = t.id \
                               OR rp.refund_transaction_id = t.id) \
               AND NOT EXISTS (SELECT 1 FROM sale_payments sp WHERE sp.transaction_id = t.id \
                               OR sp.refund_transaction_id = t.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            orphans.0, 0,
            "every finance row is claimed by a payment row"
        );
    }

    /// **A confirmed credit note is NEVER partly refunded.** The three reachable
    /// shapes are: the parent collected NOTHING (empty plan, `Unpaid`); the parent
    /// collected AT LEAST the credit note is worth (`paid == total`, `Paid`); or
    /// the parent collected something but less, which is the refusal the cap
    /// exists for and creates no document at all.
    ///
    /// There is no fourth shape in which a document exists and is part-refunded,
    /// because the only code that writes a refund is the plan inside `confirm`
    /// and the plan either covers the whole value or never runs. `Partial` would
    /// need a refund recorded AFTER the document exists, and no such method is in
    /// this service's set — `create_payment` deliberately has no state gate for
    /// exactly that future caller, the same arrangement `record_payment` has on a
    /// sale.
    #[tokio::test]
    async fn a_credit_note_of_a_part_paid_sale_is_refused_rather_than_left_partly_refunded() {
        let (s, pool) = svc().await;
        // Two units at 3 is 6; the parent collected 4.
        let p = draft_credit_note(
            &s,
            &pool,
            "PART",
            "Part Customer",
            "2",
            "3",
            &[("part till", "4")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains('6') && msg.contains('4'),
                "the refusal must state what the credit note is worth and what came in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None)
        );

        // And the reachable counterpart: a credit note worth LESS than what came
        // in is refunded in full and reads Paid, because the plan takes `min` per
        // payment rather than a proportion of it.
        let cheap = draft_credit_note(
            &s,
            &pool,
            "CHEAP",
            "Cheap Customer",
            "3",
            "1",
            &[("cheap till", "26")],
            "2",
        )
        .await;
        let detail = s.confirm(who, cheap.return_id).await.unwrap();
        assert_eq!(detail.total, dec("2"));
        assert_eq!(detail.paid, dec("2"));
        assert_eq!(detail.payment_status, PaymentStatus::Paid);
        assert_eq!(detail.due, Decimal::ZERO);
    }

    /// The cap is a SUM, not a per-line check. Two CONFIRMED credit notes of 3
    /// against a parent line of 5: the first takes 3 and leaves 2, so the second
    /// is refused for being ONE unit over what is left.
    ///
    /// This is the shape a per-line check cannot see. `ensure_within_parent`
    /// compared the requested quantity against the parent's OWN qty, and 3 <= 5
    /// passes every time — so the second credit note was refused by nothing at
    /// all. The two claims are individually legal and jointly impossible, which
    /// is exactly what an aggregate is for.
    #[tokio::test]
    async fn two_confirmed_credit_notes_of_three_against_a_parent_line_of_five_leave_the_second_one_unit_over(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "CSUMCAP").await;
        let customer_id = seed_customer(&pool, "Summed Cap Customer").await;
        let number = "2024-SALE-CSUMCAP";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "5", "4", number).await;
        collect(&pool, sale, number, &[("csumcap wallet", "40")]).await;
        let who = actor(&pool).await;

        let first = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("3")).await.unwrap();
        s.confirm(who, first.id).await.unwrap();

        let second = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        // The WRITE is refused, and 2 is the figure in the message: the first
        // credit note took 3 of 5, so 2 remain and 3 is one unit over.
        let err = s
            .add_line(who, second.id, line, dec("3"))
            .await
            .expect_err("only two units remain creditable after the first credit note")
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
            r#"INSERT INTO customer_return_lines (return_id, sale_line_id, qty, unit_price)
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
            sret_sequence_last(&pool).await,
            Some(1),
            "no second number was spent"
        );
        assert_eq!(
            payment_count(&pool).await,
            1,
            "only the first credit note's refund exists"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("3"),
            "three units came onto the shelf, not six"
        );

        // And the remainder is genuinely creditable: 2 of the 5 goes through,
        // which is the positive half of a cap and is what proves the figure in
        // the refusal message is the real remaining allowance rather than a
        // constant that always refuses.
        let rest = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, rest.id, line, dec("2")).await.unwrap();
        assert_eq!(
            s.confirm(who, rest.id).await.unwrap().total,
            dec("8"),
            "the two units that were left are creditable, so the cap refuses only \
             what it should"
        );
    }

    /// THE SUBTLE ONE. A draft reserves nothing — which has two halves, and only
    /// one of them is obvious.
    ///
    /// The obvious half: a draft of the FULL quantity does not stop a second
    /// draft of the same quantity from being written. Both go in, because neither
    /// has spent anything: goods that have not come back cannot come back twice.
    ///
    /// The subtle half: CONFIRMING the first must then refuse the second, with
    /// the confirmation — not the write — being the moment the allowance is
    /// spent. So the rule is enforced on a read that happens at confirm time and
    /// not at draft time, and the second draft is NOT invalidated by the first
    /// one's success: it is still a valid draft, refused at its own confirm for a
    /// reason it can state.
    ///
    /// What must NOT happen, and is what a reservation design would produce: the
    /// second draft silently dying, or the first confirm overwriting the
    /// allowance so the second confirm fails for no stated reason.
    #[tokio::test]
    async fn a_draft_reserves_nothing_so_a_second_draft_still_writes_and_only_the_first_confirm_wins(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "CRESERVE").await;
        let customer_id = seed_customer(&pool, "Reservation Customer").await;
        let number = "2024-SALE-CRESERVE";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "5", "4", number).await;
        collect(&pool, sale, number, &[("creserve wallet", "20")]).await;
        let who = actor(&pool).await;

        let first = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, first.id, line, dec("5")).await.unwrap();
        let second = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, second.id, line, dec("5")).await.expect(
            "a draft reserves nothing, so a second draft of the same \
             quantity is still writable",
        );

        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_sale_line(line)
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
                .confirmed_qty_taken_by_sale_line(line)
                .await
                .unwrap(),
            dec("5"),
            "the allowance is spent by the CONFIRM, not by the draft"
        );

        match s.confirm(who, second.id).await.unwrap_err() {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("already"),
                    "the refusal must name the spent \
                 allowance: {msg}"
                )
            }
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

    /// Cancelling a CONFIRMED credit note gives its allowance BACK. The goods
    /// left the shelf again and the money came back in, so the same quantity is
    /// creditable again — otherwise the second note would be refused for a
    /// quantity the shop no longer holds.
    ///
    /// This is the state predicate earning its keep: `Confirmed` counts and
    /// `Cancelled` does not, so the same aggregate answers 5 before the
    /// cancellation and 0 after it.
    #[tokio::test]
    async fn cancelling_a_confirmed_credit_note_returns_its_allowance_so_the_same_quantity_may_be_credited_again(
    ) {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "CGIVEBACK").await;
        let customer_id = seed_customer(&pool, "Give Back Customer").await;
        let number = "2024-SALE-CGIVEBACK";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "5", "4", number).await;
        collect(&pool, sale, number, &[("cgiveback wallet", "20")]).await;
        let who = actor(&pool).await;

        let spent = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, spent.id, line, dec("5")).await.unwrap();
        s.confirm(who, spent.id).await.unwrap();
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_sale_line(line)
                .await
                .unwrap(),
            dec("5")
        );

        s.cancel(who, spent.id, Some("credited the wrong customer".into()))
            .await
            .unwrap();
        assert_eq!(
            s.returns
                .confirmed_qty_taken_by_sale_line(line)
                .await
                .unwrap(),
            Decimal::ZERO,
            "a CANCELLED credit note gave its quantity back: the aggregate counts \
             only Confirmed documents, and a cancelled one is not one"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            Decimal::ZERO,
            "the goods left the shelf again, which is why the allowance is free"
        );

        // And the whole quantity is genuinely creditable a second time, through
        // the real path rather than only through the aggregate's number.
        let again = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, again.id, line, dec("5")).await.unwrap();
        let detail = s.confirm(who, again.id).await.unwrap();
        assert_eq!(
            detail.total,
            dec("20"),
            "the full five units were credited a second time, and paid for"
        );
        assert_eq!(
            s.inventory.stock_for_decision(product.id).await.unwrap(),
            dec("5")
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
    /// filter would return all three documents and pass a test that only asked
    /// "are the expected ones present" — so each narrowing is asserted as an
    /// exact set, and one read is checked to return NOTHING where the shape says
    /// it must.
    #[tokio::test]
    async fn the_list_returns_the_documents_the_filter_selects_with_their_derived_money() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "CLISTME").await;
        let customer_id = seed_customer(&pool, "List Me Customer").await;
        let number = "2024-SALE-CLISTME";
        let (sale, line) = seed_parent(&pool, customer_id, product.id, "3", "4", number).await;
        collect(&pool, sale, number, &[("clistme wallet", "12")]).await;
        let who = actor(&pool).await;

        let mut ids = Vec::new();
        for (notes, credited) in [
            ("first draft", "1"),
            ("second draft", "2"),
            ("third draft", "1"),
        ] {
            let ret = s
                .create_draft(who, sale, return_date(), &Some(notes.into()))
                .await
                .unwrap();
            s.add_line(who, ret.id, line, dec(credited)).await.unwrap();
            ids.push(ret.id);
        }
        let [first, second, third] = ids[..] else {
            unreachable!("three credit notes were created")
        };
        s.confirm(who, second).await.unwrap();

        let all = s.list(&CustomerReturnListFilter::default()).await.unwrap();
        assert_eq!(
            all.iter().map(|d| d.customer_return.id).collect::<Vec<_>>(),
            vec![first, second, third],
            "the empty filter narrows nothing and the order is the repository's"
        );
        assert_eq!(
            all.iter().map(|d| d.total).collect::<Vec<_>>(),
            vec![dec("4"), dec("8"), dec("4")],
            "each document's total is derived from ITS OWN lines: the confirmed one \
             credits 2 units and the drafts 1 each"
        );
        assert_eq!(
            all[1].payment_status,
            PaymentStatus::Paid,
            "the confirmed document reports itself paid and the drafts report \
             themselves unpaid, so the list is not publishing one document's derived \
             state on another"
        );

        let drafts = s
            .list(&CustomerReturnListFilter {
                status: Some(CustomerReturnStatus::Draft),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            drafts
                .iter()
                .map(|d| d.customer_return.id)
                .collect::<Vec<_>>(),
            vec![first, third],
            "status narrows the set exactly"
        );
        assert!(
            s.list(&CustomerReturnListFilter {
                status: Some(CustomerReturnStatus::Cancelled),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a status no document holds returns nothing"
        );

        assert_eq!(
            s.list(&CustomerReturnListFilter {
                customer_ids: Some(vec![customer_id]),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
            3
        );
        assert!(
            s.list(&CustomerReturnListFilter {
                customer_ids: Some(Vec::new()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "a party filter matching no customer returns no document"
        );

        // The document's OWN day, which is a month after the parent's — a date
        // filter on the parent's day would return everything or nothing.
        assert_eq!(
            s.list(&CustomerReturnListFilter {
                from: Some(return_date()),
                to: Some(return_date()),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
            3,
            "all three are dated the same day"
        );
        assert!(
            s.list(&CustomerReturnListFilter {
                from: Some(sale_date()),
                to: Some(sale_date()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty(),
            "no credit note is dated on the PARENT's day, so that range is empty — \
             which is the proof the filter reads return_date and not sale_date"
        );

        assert_eq!(
            s.list(&CustomerReturnListFilter {
                number: Some("SRET-000001".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
            1,
            "the numbered document is found by a fragment of its own number"
        );

        assert!(
            s.list(&CustomerReturnListFilter {
                status: Some(CustomerReturnStatus::Draft),
                number: Some("SRET-000001".into()),
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

    /// Decision 2, measured rather than asserted, and MEASURED HARDER THAN THE
    /// PURCHASE-RETURN TWIN because this family has no cost to write in the
    /// first place: `customer_return_lines` has no `unit_cost` column. A credit
    /// note therefore cannot touch the satellite even by accident of a
    /// mis-mapped field, and this test proves the table is untouched anyway —
    /// against a NON-EMPTY satellite, so it does not pass for the wrong reason.
    /// **T3d on the credit-note side.** A cancelled credit note used to post each
    /// reversal in a unit of its own and only then flip the note: a failure in between
    /// left the money reversed and the note still Confirmed.
    #[tokio::test]
    async fn a_failure_while_cancelling_a_credit_note_rolls_the_reversals_back() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "T3D-CR-ATOMIC",
            "T3D CR Atomic",
            "2",
            "5",
            &[("t3d cr till", "10")],
            "2",
        )
        .await;
        s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        let tx_before = tx_count(&pool).await;
        sqlx::raw_sql(
            "CREATE TRIGGER injected_cancel_failure BEFORE UPDATE ON customer_returns \
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
            "SELECT COUNT(*) FROM customer_return_payments WHERE refund_transaction_id IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reversals, 0, "and no row claims one");
        assert_eq!(
            row_state(&pool, p.return_id).await.0,
            "Confirmed",
            "the note is still live"
        );
    }

    // -- the party ledger (T2) ---------------------------------------------------

    /// The journal rows this credit note wrote, as the sign rule stored them.
    async fn ledger_rows(pool: &SqlitePool, document_id: i64) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT kind, amount FROM party_ledger_entries \
             WHERE document_kind = 'CustomerReturn' AND document_id = ? ORDER BY id",
        )
        .bind(document_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// The customer's balance, through the ledger read.
    async fn balance(pool: &SqlitePool, customer_id: i64) -> Decimal {
        let repo = SqlitePartyLedgerRepository::new(pool.clone());
        repo.balance_for_party(crate::models::PartyType::Customer, customer_id)
            .await
            .unwrap()
    }

    /// A credit note writes the goods and cash legs independently: one `Return`
    /// of `−total` and one `Refund` per cash delivery.
    #[tokio::test]
    async fn a_confirmed_credit_note_appends_one_return_that_reduces_the_debt() {
        let (s, pool) = svc().await;
        // Three units at 4 is 12, and the whole 12 was collected, so the refund
        // plan is a full one and the cap is not in the way of this test.
        let p = draft_credit_note(
            &s,
            &pool,
            "LEDGER-NOTE",
            "Ledger Note Customer",
            "3",
            "4",
            &[("ledger till", "12")],
            "3",
        )
        .await;

        s.confirm(actor(&pool).await, p.return_id).await.unwrap();

        assert_eq!(
            ledger_rows(&pool, p.return_id).await,
            vec![
                ("Refund".to_string(), "12".to_string()),
                ("Return".to_string(), "-12".to_string()),
            ],
            "a credit note records the goods as Return -12 and the cash handed back as Refund +12"
        );

        // The parent sale and its collection are planted with raw SQL, so the
        // fixture has no Sale Charge or Payment entries. The return's goods and
        // money legs therefore fold to -12 + 12 = 0.
        let customer_id: i64 =
            sqlx::query_scalar("SELECT customer_id FROM customer_returns WHERE id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            balance(&pool, customer_id).await,
            Decimal::ZERO,
            "the Return reduces the debt and the Refund settles that amount"
        );
        // Sale-side rows are absent, confirming this zero is exactly the
        // return's -12 goods leg plus its +12 refund leg.
        let sale_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM party_ledger_entries WHERE document_kind = 'Sale'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            sale_rows, 0,
            "the fixture's raw-SQL parent wrote no ledger row; the return legs alone net to zero"
        );
    }

    /// A paid credit sale followed by a cash-refunded credit note nets to zero:
    /// Charge +12, Payment -12, Return -12 and Refund +12. The parent fixture
    /// uses raw SQL, so seed its already-earned sale/payment journal entries here.
    #[tokio::test]
    async fn a_fully_collected_sale_refunded_by_credit_note_folds_to_zero() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "LEDGER-IDENTITY",
            "Ledger Identity Customer",
            "3",
            "4",
            &[("identity till", "12")],
            "3",
        )
        .await;
        let (sale_id, customer_id): (i64, i64) = sqlx::query_as(
            "SELECT sale_id, customer_id FROM customer_returns WHERE id = ?",
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
                    party_type: crate::models::PartyType::Customer,
                    party_id: customer_id,
                    kind,
                    amount: kind.signed_amount(amount),
                    document_kind: crate::models::PartyDocumentKind::Sale,
                    document_id: sale_id,
                    entry_date: sale_date(),
                    reference: Some("2024-SALE-LEDGER-IDENTITY".into()),
                    created_by: who,
                })
                .await
                .unwrap();
        }

        s.confirm(who, p.return_id).await.unwrap();

        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT kind, amount, document_kind, document_id FROM party_ledger_entries \
             WHERE party_type = 'Customer' AND party_id = ? ORDER BY id",
        )
        .bind(customer_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Charge".into(), "12".into(), "Sale".into(), sale_id),
                ("Payment".into(), "-12".into(), "Sale".into(), sale_id),
                (
                    "Refund".into(),
                    "12".into(),
                    "CustomerReturn".into(),
                    p.return_id,
                ),
                (
                    "Return".into(),
                    "-12".into(),
                    "CustomerReturn".into(),
                    p.return_id,
                ),
            ],
            "the journal names all four obligation movements on their owning documents"
        );
        assert_eq!(balance(&pool, customer_id).await, Decimal::ZERO);
    }

    /// THE case the cap exists for, and the reason the Return is written even
    /// when the refund plan is EMPTY: a parent that collected nothing still takes
    /// its goods back, so the customer's debt must come down while no money moves
    /// at all. Writing the Return only when a refund happens would leave that
    /// customer owing for goods they returned.
    #[tokio::test]
    async fn a_fully_unpaid_parent_still_returns_the_debt_with_no_refund() {
        let (s, pool) = svc().await;
        // Nothing collected: the fixture's refund plan is empty by construction.
        let p = draft_credit_note(
            &s,
            &pool,
            "LEDGER-UNPAID",
            "Ledger Unpaid Customer",
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
            "the goods came back, so the debt comes down; the empty refund plan adds no Refund"
        );
        assert_eq!(
            tx_count(&pool).await,
            0,
            "and no cash entry exists: the Return is about the goods, not the money"
        );

        let customer_id: i64 =
            sqlx::query_scalar("SELECT customer_id FROM customer_returns WHERE id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        // The parent sale is planted with raw SQL and wrote no Charge, so the
        // journal holds only this Return: a credit of 10, which is exactly the
        // saldo a favor a shop owes that customer for goods it took back.
        assert_eq!(
            balance(&pool, customer_id).await,
            dec("-10"),
            "the Return is the whole journal, so the customer is owed 10"
        );
    }

    /// The write joins the caller's unit, so a failure after it takes the entry
    /// with it.
    #[tokio::test]
    async fn a_failed_credit_note_confirm_rolls_its_return_entry_back() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "LEDGER-ROLLBACK",
            "Ledger Rollback Customer",
            "2",
            "5",
            &[("ledger till rb", "10")],
            "2",
        )
        .await;

        sqlx::raw_sql(
            "CREATE TRIGGER injected_ledger_probe BEFORE UPDATE ON customer_returns \
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

    #[tokio::test]
    async fn a_confirmed_credit_note_leaves_the_cost_satellite_byte_identical() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "SAT").await;
        let customer_id = seed_customer(&pool, "Satellite Customer").await;
        let (sale, line) =
            seed_parent(&pool, customer_id, product.id, "3", "4", "2024-SALE-SAT").await;
        collect(&pool, sale, "2024-SALE-SAT", &[("sat till", "12")]).await;
        let supplier_id: i64 = sqlx::query_scalar(
            "INSERT INTO suppliers (name, is_active, created_by) VALUES ('Satellite Supplier', 1, ?) RETURNING id",
        )
        .bind(actor(&pool).await)
        .fetch_one(&pool)
        .await
        .unwrap();
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
            "the satellite has a row a credit note could damage"
        );

        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, credit_note.id, line, dec("2"))
            .await
            .unwrap();
        s.confirm(who, credit_note.id).await.unwrap();

        assert_eq!(
            satellite(&pool).await,
            before,
            "a credit note sets no price and carries no cost, so no part of the cost \
             satellite may move — not current_cost, not previous_cost, not a timestamp, \
             not the preference flag"
        );
    }

    /// The one write a purchase confirm has and a return never does, injected on
    /// the credit note: a trigger that ABORTS on ANY insert into the satellite,
    /// and the assertion is that it never fires. There is no satellite WINDOW on
    /// this family to be rolled back, because there is no satellite WRITE — which
    /// is the residue with no recovery path on `PurchasesService::confirm`, closed
    /// here by absence rather than by a transaction.
    #[tokio::test]
    async fn a_confirmed_credit_note_never_reaches_the_cost_satellite_so_there_is_no_satellite_window(
    ) {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "W5",
            "Satellite-Window Customer",
            "3",
            "4",
            &[("w5 till", "12")],
            "2",
        )
        .await;

        inject(
            &pool,
            "CREATE TRIGGER cret_w5 BEFORE INSERT ON product_supplier_costs \
             BEGIN SELECT RAISE(ABORT, 'a credit note must never write the cost satellite'); END"
                .to_string(),
        )
        .await;

        let detail = s
            .confirm(actor(&pool).await, p.return_id)
            .await
            .expect("a confirm that wrote the satellite would have been aborted by the trigger");
        assert_eq!(
            detail.customer_return.status,
            CustomerReturnStatus::Confirmed
        );
        assert_eq!(
            satellite(&pool).await.len(),
            0,
            "and the satellite is still empty: the trigger never had to fire"
        );
    }

    // ========================================================================
    // CANCEL
    // ========================================================================

    /// A refund sent by mistake must be undoable, and on a credit note the
    /// reversal is the SAFE direction: an Expense became an Income, so no account
    /// can be overdrawn by it and the reversal cannot be refused for want of
    /// funds. The goods go back out on the shelf, each reversal is linked to the
    /// refund row it reverses, and the movement reason is `Adjust` — an
    /// interpretation recorded on `cancel` and here, because the vocabulary has
    /// no "reversal" entry and every other candidate states something this
    /// movement is not.
    #[tokio::test]
    async fn cancelling_a_confirmed_credit_note_reverses_the_stock_and_the_money_and_links_the_pair(
    ) {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "CANCEL",
            "Cancel Customer",
            "3",
            "4",
            &[("cancel till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        s.confirm(who, p.return_id).await.unwrap();
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            dec("2")
        );

        let detail = s
            .cancel(who, p.return_id, Some("customer changed their mind".into()))
            .await
            .unwrap();

        assert_eq!(
            detail.customer_return.status,
            CustomerReturnStatus::Cancelled
        );
        assert_eq!(
            detail.customer_return.cancel_reason.as_deref(),
            Some("customer changed their mind")
        );
        assert_eq!(
            detail.customer_return.credit_note_number.as_deref(),
            Some("2024-SRET-000001"),
            "a cancelled credit note keeps its number: it was a real document"
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            Decimal::ZERO,
            "the goods went back out to the customer"
        );
        assert_eq!(
            movement_reasons(&pool).await,
            vec!["Sale-return", "Adjust"],
            "the reversal is Out with the neutral reason"
        );
        assert_eq!(
            tx_kinds(&pool).await,
            vec!["Income", "Expense", "Income"],
            "the Expense the refund produced is now reversed by an Income, which \
             no overdraft guard can refuse"
        );
        assert!(
            detail.payments[0].refund_transaction_id.is_some(),
            "the refund row carries BOTH links: the one it produced and the one that reverses it"
        );

        match s.cancel(who, p.return_id, None).await.unwrap_err() {
            AppError::Validation(msg) => assert!(
                msg.contains("already cancelled"),
                "the refusal must say the credit note is already cancelled: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(
            movement_count(&pool).await,
            2,
            "nothing moved a second time"
        );
    }

    /// A Draft credit note is discarded, not reversed: it never moved goods or
    /// money, so cancelling it is a status flip and nothing else. The number
    /// stays NULL, which is also what makes the row deletable afterwards.
    #[tokio::test]
    async fn cancelling_a_draft_credit_note_moves_no_stock_and_no_money_and_makes_it_deletable() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "DISCARD",
            "Discard Customer",
            "3",
            "4",
            &[("discard till", "12")],
            "1",
        )
        .await;
        let who = actor(&pool).await;

        let detail = s.cancel(who, p.return_id, None).await.unwrap();
        assert_eq!(
            detail.customer_return.status,
            CustomerReturnStatus::Cancelled
        );
        assert_eq!(detail.customer_return.credit_note_number, None);
        assert_eq!(movement_count(&pool).await, 0, "no goods came in at all");
        assert_eq!(
            tx_count(&pool).await,
            1,
            "only the fixture's own collection"
        );

        s.delete_draft(p.return_id).await.unwrap();
        match s.delete_draft(p.return_id).await.unwrap_err() {
            AppError::NotFound(msg) => assert!(msg.contains("not found"), "{msg}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// A credit note that was confirmed and then reversed keeps its number, so
    /// it is permanent audit trail and `delete_draft` must refuse it — the goods
    /// came in and the money went out, and both movements reference the document.
    #[tokio::test]
    async fn a_reversed_credit_note_keeps_its_number_and_is_no_longer_deletable() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "PERM",
            "Permanent Customer",
            "3",
            "4",
            &[("perm till", "12")],
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
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_returns WHERE id = ?")
                .bind(p.return_id)
                .fetch_one(&pool)
                .await
                .unwrap()
                .0;
        assert_eq!(rows, 1, "the refused delete removed nothing");
    }

    /// A credit note cannot be confirmed twice, and a cancelled one cannot be
    /// confirmed at all. The refused duplicate must not burn a second number.
    #[tokio::test]
    async fn a_credit_note_cannot_be_confirmed_twice_or_confirmed_after_being_cancelled() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "TWICE",
            "Twice Customer",
            "3",
            "4",
            &[("twice till", "12")],
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
            sret_sequence_last(&pool).await,
            Some(1),
            "the refused duplicate did not burn a second number"
        );

        s.cancel(who, p.return_id, None).await.unwrap();
        match s.confirm(who, p.return_id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(msg.contains("cancelled"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// A credit note with no lines is not a credit note. Refused before the
    /// number, so the failure is a Draft with nothing behind it rather than a
    /// numbered document with no stock and no money.
    #[tokio::test]
    async fn a_credit_note_with_no_lines_cannot_be_confirmed() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "EMPTY").await;
        let customer_id = seed_customer(&pool, "Empty Customer").await;
        let (sale, _line) =
            seed_parent(&pool, customer_id, product.id, "3", "4", "2024-SALE-EMPTY").await;
        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();

        match s.confirm(who, credit_note.id).await.unwrap_err() {
            AppError::Validation(msg) => assert!(msg.contains("no lines"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(sret_sequence_last(&pool).await, None);
        assert_eq!(
            row_state(&pool, credit_note.id).await,
            ("Draft".to_string(), None)
        );
        assert_eq!(movement_count(&pool).await, 0);
    }

    // ========================================================================
    // THE TRANSACTION — the residue windows
    // ========================================================================
    //
    // The credit note's windows, and they are WORSE than the purchase return's
    // in one respect the code cannot paper over: here the money is going OUT, so
    // a partial application would leave a shop that took the goods back and kept
    // the money. Every injection below asserts the ABSENCE of that.

    /// WINDOW 1 — between `next_number` and the FIRST stock movement. The
    /// number is unspent and `doc_sequences` has no SRET row at all.
    #[tokio::test]
    async fn confirm_failure_between_the_number_and_the_first_movement_leaves_nothing_written() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "W1",
            "W1 Customer",
            "3",
            "4",
            &[("w1 till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        let product_id = p.product_id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER cret_w1 BEFORE INSERT ON stock_movements \
                 WHEN NEW.reason = 'Sale-return' AND NEW.product_id = {product_id} \
                 BEGIN SELECT RAISE(ABORT, 'injected first-movement failure'); END"
            ),
        )
        .await;

        let err = s.confirm(who, p.return_id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected first-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sret_sequence_last(&pool).await,
            None,
            "the number was never spent: `doc_sequences` has no SRET row at all, \
             because the increment rolled back with the rest of the unit"
        );
        assert_eq!(movement_count(&pool).await, 0, "no goods came back in");
        assert_eq!(tx_count(&pool).await, 1, "finance never started");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, p.return_id).await,
            ("Draft".to_string(), None)
        );
    }

    /// WINDOW 2 — on the SECOND movement of a two-line credit note. This is the
    /// one a purchase cannot even reach: a purchase forbids a repeated product,
    /// while a SALE may carry the same product twice, so a credit note can move
    /// the same product twice inside ONE confirm and the second fold has to see
    /// the first movement.
    #[tokio::test]
    async fn confirm_failure_on_the_second_movement_rolls_the_first_one_back_too() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, &pool, "W2").await;
        let customer_id = seed_customer(&pool, "W2 Customer").await;
        let number = "2024-SALE-W2";
        let (sale, first) = seed_parent(&pool, customer_id, product.id, "3", "4", number).await;
        // The same product on a second line: legal on a sale, so legal here.
        let second = add_parent_line(&pool, sale, product.id, "2", "7").await;
        collect(&pool, sale, number, &[("w2 till", "26")]).await;
        let who = actor(&pool).await;
        let credit_note = s
            .create_draft(who, sale, return_date(), &None)
            .await
            .unwrap();
        s.add_line(who, credit_note.id, first, dec("2"))
            .await
            .unwrap();
        s.add_line(who, credit_note.id, second, dec("1"))
            .await
            .unwrap();

        inject(
            &pool,
            "CREATE TRIGGER cret_w2 BEFORE INSERT ON stock_movements \
             WHEN NEW.reason = 'Sale-return' AND NEW.qty = '1' \
             BEGIN SELECT RAISE(ABORT, 'injected second-movement failure'); END"
                .to_string(),
        )
        .await;

        let err = s.confirm(who, credit_note.id).await.unwrap_err();
        assert!(
            err.to_string().contains("injected second-movement failure"),
            "expected the injected failure to surface, got {err}"
        );

        assert_eq!(
            sret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT"
        );
        assert_eq!(
            movement_count(&pool).await,
            0,
            "no goods came back in: the first line's In rolled back with the \
             second one's failure, even though both lines were the SAME product"
        );
        assert_eq!(
            tx_count(&pool).await,
            1,
            "only the fixture's own collection exists"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, credit_note.id).await,
            ("Draft".to_string(), None)
        );
    }

    /// WINDOW 3 — between the refund Expense and the payment row that claims it.
    /// This is the residue that used to leave an orphan finance row stamped with
    /// a number no document carried. There is no orphan now: the Expense rolls
    /// back with the payment row that could not be written.
    #[tokio::test]
    async fn confirm_failure_between_the_expense_and_the_payment_row_leaves_no_orphan_expense() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "W3",
            "W3 Customer",
            "3",
            "4",
            &[("w3 till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        let return_id = p.return_id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER cret_w3 BEFORE INSERT ON customer_return_payments \
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
            sret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT"
        );
        assert_eq!(movement_count(&pool).await, 0, "the In rolled back too");
        assert_eq!(
            tx_count(&pool).await,
            1,
            "the refund Expense rolled back with the payment row that could not be written"
        );
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            row_state(&pool, return_id).await,
            ("Draft".to_string(), None)
        );

        let orphans: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transactions t \
             WHERE NOT EXISTS (SELECT 1 FROM customer_return_payments rp WHERE rp.transaction_id = t.id \
                               OR rp.refund_transaction_id = t.id) \
               AND NOT EXISTS (SELECT 1 FROM sale_payments sp WHERE sp.transaction_id = t.id \
                               OR sp.refund_transaction_id = t.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            orphans.0, 0,
            "no finance row is claimed by nobody: the orphan shape does not exist here"
        );
    }

    /// WINDOW 4 — on `set_confirmed` itself, the LAST step. THE residue that
    /// matters most on this family: the number spent, the goods on the shelf, the
    /// money OUT of the account AND the payment row committed, while the document
    /// still reads `("Draft", NULL)`. A `delete_draft` that trusted the status
    /// would remove that Draft, cascade the payment away, and leave the shop
    /// holding goods and having paid for them with no document to show. All of it
    /// rolls back now.
    #[tokio::test]
    async fn confirm_failure_on_set_confirmed_leaves_a_clean_draft_that_reports_nothing_refunded() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "W4",
            "W4 Customer",
            "3",
            "4",
            &[("w4 till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        let return_id = p.return_id;

        inject(
            &pool,
            format!(
                "CREATE TRIGGER cret_w4 BEFORE UPDATE ON customer_returns WHEN NEW.id = {return_id} \
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
            sret_sequence_last(&pool).await,
            None,
            "the number is UNSPENT: `doc_sequences` has no SRET row at all"
        );
        assert_eq!(
            movement_count(&pool).await,
            0,
            "no goods came back in: the In rolled back"
        );
        assert_eq!(tx_count(&pool).await, 1, "the refund Expense rolled back");
        assert_eq!(
            payment_count(&pool).await,
            0,
            "the payment rolled back: the customer was NOT refunded and no document says otherwise"
        );
        assert_eq!(
            row_state(&pool, return_id).await,
            ("Draft".to_string(), None)
        );
        assert_eq!(
            s.inventory.stock_for_decision(p.product_id).await.unwrap(),
            Decimal::ZERO,
            "and the shelf is exactly as it started"
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
            "nothing went back to the customer"
        );

        // And the Draft is safe to delete precisely because the unit rolled back:
        // there is no committed payment for its CASCADE to take with it and no
        // finance row for anyone to keep.
        s.delete_draft(return_id).await.unwrap();
        let lines: i64 = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_return_lines")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        assert_eq!(lines, 0, "the document and its lines are gone together");
    }

    /// A failed confirm must not poison the NEXT one: the unit rolled back, so
    /// the draft is still a Draft, its number is unspent, and a retry re-passes
    /// every predicate and succeeds with the FIRST number rather than a second.
    #[tokio::test]
    async fn a_confirm_that_failed_part_way_can_be_retried_unchanged() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "RETRY",
            "Retry Customer",
            "3",
            "4",
            &[("retry till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;

        inject(
            &pool,
            "CREATE TRIGGER cret_retry BEFORE INSERT ON customer_return_payments \
             BEGIN SELECT RAISE(ABORT, 'injected retry failure'); END"
                .to_string(),
        )
        .await;
        assert!(s.confirm(who, p.return_id).await.is_err());

        sqlx::raw_sql("DROP TRIGGER cret_retry")
            .execute(&pool)
            .await
            .unwrap();

        let detail = s.confirm(who, p.return_id).await.unwrap();
        assert_eq!(
            detail.customer_return.credit_note_number.as_deref(),
            Some("2024-SRET-000001"),
            "the retry took the FIRST number: the burned counter rolled back"
        );
        assert_eq!(payment_count(&pool).await, 1);
        assert_eq!(
            tx_count(&pool).await,
            2,
            "the fixture's own collection and the one refund"
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
    /// `PoolTimedOut`.
    ///
    /// What is left as a test is the PREMISE, asserted directly rather than
    /// inferred from a suite that happens to pass: while a unit is open the pool
    /// has no spare connection, so a seam that reached for it would stall.
    /// `set_confirmed_in` is the write exercised because it is the one whose
    /// read-back and `refuse_confirm` both have to stay on the caller's
    /// connection — a regression to `&SqlitePool` would pass every happy path in
    /// the repository's own file and fail only here.
    #[tokio::test]
    async fn confirm_opens_its_unit_on_the_only_connection_the_pool_owns() {
        let (s, pool) = svc().await;
        let p = draft_credit_note(
            &s,
            &pool,
            "SEAM",
            "Seam Customer",
            "3",
            "4",
            &[("seam till", "12")],
            "2",
        )
        .await;
        let who = actor(&pool).await;
        // Confirmed FIRST, so the seam under test has to take its REFUSAL path.
        s.confirm(who, p.return_id).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );
        let before = std::time::Instant::now();
        let refusal = s
            .returns
            .set_confirmed_in(&mut tx, p.return_id, who, "2024-SRET-000002")
            .await
            .expect_err("the credit note is Confirmed, so the DRAFT predicate must refuse")
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

        let second = draft_credit_note(
            &s,
            &pool,
            "SEAM2",
            "Seam Customer Two",
            "3",
            "4",
            &[("seam till two", "12")],
            "2",
        )
        .await;
        let detail = s.confirm(who, second.return_id).await.unwrap();
        assert_eq!(
            detail.customer_return.credit_note_number.as_deref(),
            Some("2024-SRET-000002"),
            "the second document took the second number, on a pool with one connection"
        );
    }

    /// Money can never be the sum of documents without a guard. Two credit note
    /// lines of `5e28` at a price of `1` are `1e29` together, above
    /// `Decimal::MAX`, and the raw `+` panics — so the fold is
    /// `checked_money_sum` and the refusal is the document-total rule, not a line
    /// rule, because every line is fine.
    #[test]
    fn a_document_total_built_from_two_enormous_lines_is_refused_rather_than_panicking() {
        let lines = vec![
            CustomerReturnLine {
                id: 1,
                return_id: 1,
                sale_line_id: 1,
                qty: dec("5e28"),
                unit_price: dec("1"),
                created_at: return_date().and_hms_opt(0, 0, 0).unwrap(),
            },
            CustomerReturnLine {
                id: 2,
                return_id: 1,
                sale_line_id: 2,
                qty: dec("5e28"),
                unit_price: dec("1"),
                created_at: return_date().and_hms_opt(0, 0, 0).unwrap(),
            },
        ];
        match Svc::document_money(&lines, &[]) {
            Err(PriceRefusal::DocumentTotalTooLarge) => {}
            other => panic!("expected DocumentTotalTooLarge, got {other:?}"),
        }
        assert!(matches!(
            Svc::document_money(&lines[..1], &[]),
            Ok(crate::models::RecordMoney { .. })
        ));
    }
}
