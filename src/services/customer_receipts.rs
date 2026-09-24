// M4 customers (Slice L). CustomerReceiptService owns the receipt document: one
// handover of money applied to a customer's confirmed credit sales, oldest debt
// first. The allocation is derived from the receivable `SalesService` exposes, so a
// receipt can only ever cover sales of its own customer, and every payment it
// creates still belongs to its sale and posts its own finance movement through
// `SalesService`. This file runs no SQL: receipts are stored through
// `CustomerReceiptRepository`, payments through `SalesService`, and the method's
// owning account is derived by `PaymentMethodService` before any write. The
// receipt stores no total: its amount is the derived SUM of the payments
// it groups, so an interrupted collection can leave fewer payments but never a
// document claiming more than it applied.
use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{CustomerReceipt, NewReceipt, ReceiptDetail, SaleDetail};
use crate::repositories::{
    AccountRepository, BarcodeRepository, CategoryRepository, CustomerReceiptRepository,
    CustomerRepository, DocSequenceRepository, PaymentMethodRepository, ProductRepository,
    SaleRepository, StockMovementRepository, TransactionRepository,
};
use crate::services::{PaymentMethodService, SalesService};

/// One planned payment: how much of one debt sale the collected amount covers.
#[derive(Debug, Clone, PartialEq)]
struct PlannedAllocation {
    sale_id: i64,
    amount: Decimal,
}

#[derive(Clone)]
pub struct CustomerReceiptService<RR, SR, DR, C, P, B, S, A, T, PM, CR>
where
    RR: CustomerReceiptRepository,
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: CategoryRepository,
    P: ProductRepository,
    B: BarcodeRepository,
    S: StockMovementRepository,
    A: AccountRepository,
    T: TransactionRepository,
    PM: PaymentMethodRepository,
    CR: CustomerRepository,
{
    pub receipts: RR,
    /// The receivable is read and every grouped payment is written through the
    /// sales service; receipts never touch the sales tables themselves.
    pub sales: SalesService<SR, DR, C, P, B, S, A, T, PM, CR>,
    /// Finance-owned allowlist for the `(account, method)` pair.
    pub payment_methods: PaymentMethodService<PM>,
}

impl<RR, SR, DR, C, P, B, S, A, T, PM, CR>
    CustomerReceiptService<RR, SR, DR, C, P, B, S, A, T, PM, CR>
where
    RR: CustomerReceiptRepository,
    SR: SaleRepository,
    DR: DocSequenceRepository,
    C: CategoryRepository,
    P: ProductRepository,
    B: BarcodeRepository,
    S: StockMovementRepository,
    A: AccountRepository,
    T: TransactionRepository,
    PM: PaymentMethodRepository,
    CR: CustomerRepository,
{
    pub fn new(
        receipts: RR,
        sales: SalesService<SR, DR, C, P, B, S, A, T, PM, CR>,
        payment_methods: PaymentMethodService<PM>,
    ) -> Self {
        Self {
            receipts,
            sales,
            payment_methods,
        }
    }

    // -- Collection -----------------------------------------------------------

    /// Collect one handover of money for a customer: validate everything before any
    /// write, then create the receipt and one grouped payment per covered sale,
    /// oldest debt first. Returns the receipt with the payments it groups; the amount
    /// it reports is derived from that sum, never stored.
    pub async fn collect(
        &self,
        actor: i64,
        customer_id: i64,
        method_id: i64,
        amount: Decimal,
        date: NaiveDate,
        notes: Option<String>,
    ) -> AppResult<ReceiptDetail> {
        // The customer must exist (404) and the amount must be positive.
        let customer = self.sales.customers.get_customer(customer_id).await?;
        if amount <= Decimal::ZERO {
            return Err(AppError::Validation("amount must be > 0".into()));
        }
        // The account is derived from the method's owner before any write
        // (400 inactive/unassigned, no side effect).
        let account_id = self.payment_methods.resolve_account(method_id).await?;
        let notes = Self::clean_notes(notes)?;

        // The receivable: a collection never exceeds what the customer owes, and the
        // outstanding figure is what the allocation is checked against.
        let outstanding = self.sales.customer_balance(customer_id).await?;
        if amount > outstanding {
            return Err(AppError::Validation(format!(
                "amount {amount} exceeds the outstanding debt {outstanding} of customer {} ({customer_id})",
                customer.name
            )));
        }

        let debts = self.sales.customer_debt_sales(customer_id).await?;
        let plan = Self::plan_allocations(amount, &debts);
        let planned: Decimal = plan.iter().map(|allocation| allocation.amount).sum();
        if planned != amount {
            return Err(AppError::Internal(format!(
                "collection plan {planned} does not consume the collected amount {amount}"
            )));
        }
        // The plan comes from this customer's receivable, so every sale is already
        // theirs; the guard states that invariant before any write.
        for allocation in &plan {
            self.ensure_sale_belongs_to_customer(customer_id, allocation.sale_id)
                .await?;
        }

        // One receipt groups one payment per covered sale. The receipt itself posts
        // no movement; every grouped payment posts its own through SalesService. The
        // receipt carries the collection request's actor, the same one every grouped
        // payment and its finance row get (AC18).
        let receipt = self
            .receipts
            .create(
                actor,
                &NewReceipt {
                    customer_id,
                    account_id,
                    method_id,
                    date,
                    notes,
                },
            )
            .await?;
        for allocation in &plan {
            self.sales
                .record_payment_with_receipt(
                    actor,
                    allocation.sale_id,
                    method_id,
                    allocation.amount,
                    date,
                    Some(receipt.id),
                )
                .await?;
        }

        // The amount is not stored: the returned detail derives it from the payments
        // that were actually created, so a failure partway through the loop cannot
        // leave a document claiming more than it applied.
        self.receipt_detail(receipt).await
    }

    // -- Reads and delete -------------------------------------------------------

    /// One receipt with the payments it groups; 404 when it does not exist.
    pub async fn get_receipt(&self, receipt_id: i64) -> AppResult<ReceiptDetail> {
        let receipt = self
            .receipts
            .find_by_id(receipt_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("receipt {receipt_id} not found")))?;
        self.receipt_detail(receipt).await
    }

    /// Receipts of one customer, oldest first, each with its grouped payments and
    /// derived total.
    pub async fn list_receipts(&self, customer_id: i64) -> AppResult<Vec<ReceiptDetail>> {
        let receipts = self.receipts.list_by_customer(customer_id).await?;
        let mut out = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            out.push(self.receipt_detail(receipt).await?);
        }
        Ok(out)
    }

    /// Delete a receipt. Once a payment references it the database RESTRICTs the
    /// delete and the failure surfaces as a clear Validation error: the grouping
    /// document cannot vanish while the payments it explains still exist.
    pub async fn delete_receipt(&self, receipt_id: i64) -> AppResult<()> {
        self.get_receipt(receipt_id).await?;
        if !self.receipts.delete(receipt_id).await? {
            return Err(AppError::NotFound(format!(
                "receipt {receipt_id} not found"
            )));
        }
        Ok(())
    }

    async fn receipt_detail(&self, receipt: CustomerReceipt) -> AppResult<ReceiptDetail> {
        let allocations = self.receipts.list_allocations(receipt.id).await?;
        let detail = ReceiptDetail::new(receipt, allocations);
        // Resolve the display names through the same read paths the rest of the
        // interface uses, so the receipt list never prints an internal key.
        let account_name = self
            .sales
            .transactions
            .accounts
            .find_by_id(detail.receipt.account_id)
            .await?
            .map(|account| account.name)
            .unwrap_or_else(|| "Unknown account".to_string());
        let method_name = self
            .payment_methods
            .methods
            .find_method(detail.receipt.method_id)
            .await?
            .map(|method| method.name)
            .unwrap_or_else(|| "Unknown method".to_string());
        Ok(detail.with_names(account_name, method_name))
    }

    // -- Planning and guards ----------------------------------------------------

    /// Oldest first: walk the customer's debt sales in due order and fill each one
    /// until the amount is exhausted. The last covered sale may take a partial
    /// amount and nothing beyond it is planned. `debts` is already ordered by
    /// `due_date`, then `sale_date`, then id, and every entry has `due > 0`.
    fn plan_allocations(amount: Decimal, debts: &[SaleDetail]) -> Vec<PlannedAllocation> {
        let mut remaining = amount;
        let mut plan = Vec::new();
        for detail in debts {
            if remaining <= Decimal::ZERO {
                break;
            }
            if detail.due <= Decimal::ZERO {
                continue;
            }
            let take = detail.due.min(remaining);
            plan.push(PlannedAllocation {
                sale_id: detail.sale.id,
                amount: take,
            });
            remaining -= take;
        }
        plan
    }

    /// The receipt can only ever be applied to sales of its own customer. The
    /// allocation is derived from that customer's receivable, so this holds by
    /// construction; the guard makes the invariant explicit before any payment is
    /// written and turns a future planning bug into a refused request instead of a
    /// misapplied receipt.
    async fn ensure_sale_belongs_to_customer(
        &self,
        customer_id: i64,
        sale_id: i64,
    ) -> AppResult<()> {
        let detail = self.sales.get_detail(sale_id).await?;
        if detail.sale.customer_id != customer_id {
            return Err(AppError::Validation(format!(
                "receipt for customer {customer_id} cannot be applied to sale {sale_id} of customer {}",
                detail.sale.customer_id
            )));
        }
        Ok(())
    }

    /// Receipt notes: trimmed, whitespace-only becomes NULL, at most 256 chars.
    fn clean_notes(notes: Option<String>) -> AppResult<Option<String>> {
        match notes {
            None => Ok(None),
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                if trimmed.chars().count() > 256 {
                    return Err(AppError::Validation(
                        "receipt notes must be <= 256 chars".into(),
                    ));
                }
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;

    use crate::models::{
        MovementReason, MovementType, NewCustomer, NewMovement, NewProduct, NewSale, PaymentStatus,
        PaymentType, ProductKind,
    };
    use crate::repositories::{
        SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteCustomerReceiptRepository, SqliteCustomerRepository, SqliteDocSequenceRepository,
        SqlitePaymentMethodRepository, SqliteProductRepository, SqliteSaleRepository,
        SqliteStockMovementRepository, SqliteTransactionRepository,
    };
    use crate::security::test_support;
    use crate::services::{CustomerService, InventoryService, TransactionService};

    type ReceiptSvc = crate::services::CustomerReceiptService<
        SqliteCustomerReceiptRepository,
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

    async fn test_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests below seed their own users instead, because
    /// there the point is telling two actors apart.
    async fn audit_actor(s: &ReceiptSvc) -> i64 {
        test_support::audit_actor_id(&s.sales.transactions.accounts.pool)
            .await
            .unwrap()
    }

    /// Production wiring, in-memory: the receipt service wraps the same sales
    /// service the routes use, so a receipt reaches sales and finance exactly the
    /// way it does in the app.
    async fn svc() -> (ReceiptSvc, SqlitePool) {
        let pool = test_pool().await;
        let inventory = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            true,
        );
        let transactions = TransactionService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
            false,
        );
        let customers = CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
        let method_repo = SqlitePaymentMethodRepository::new(pool.clone());
        let sales = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory,
            transactions,
            method_repo.clone(),
            customers,
            true,
        );
        let receipts = SqliteCustomerReceiptRepository::new(pool.clone());
        let payment_methods = PaymentMethodService::new(method_repo);
        (
            CustomerReceiptService::new(receipts, sales, payment_methods),
            pool,
        )
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    async fn seed_product(s: &ReceiptSvc, sku: &str, price: &str) -> i64 {
        let product = s
            .sales
            .inventory
            .create_product(
                test_support::audit_actor_id(&s.sales.inventory.products.pool)
                    .await
                    .unwrap(),
                NewProduct {
                    sku: sku.into(),
                    name: format!("prod {sku}"),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec(price),
                    cost_price: dec("1"),
                    track_stock: true,
                    min_stock: Some(dec("0")),
                    max_stock: Some(dec("100")),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        s.sales
            .inventory
            .record_movement(
                test_support::audit_actor_id(&s.sales.inventory.products.pool)
                    .await
                    .unwrap(),
                NewMovement {
                    product_id: product.id,
                    qty: dec("1000"),
                    movement_type: MovementType::In,
                    reason: MovementReason::Initial,
                    reference: "".into(),
                    date: d(2024, 1, 1),
                },
            )
            .await
            .unwrap();
        product.id
    }

    async fn seed_account(s: &ReceiptSvc, name: &str) -> i64 {
        s.sales
            .transactions
            .accounts
            .create(audit_actor(s).await, name)
            .await
            .unwrap()
            .id
    }

    async fn seed_customer(s: &ReceiptSvc, name: &str) -> i64 {
        s.sales
            .customers
            .create_customer(
                audit_actor(s).await,
                NewCustomer {
                    name: name.into(),
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
            .unwrap()
            .customer
            .id
    }

    async fn method_id(s: &ReceiptSvc, name: &str) -> i64 {
        s.sales
            .payment_methods
            .find_method_by_name(name)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn allow(s: &ReceiptSvc, account_id: i64, method_id: i64) {
        s.sales
            .payment_methods
            .set_method_account(audit_actor(s).await, method_id, Some(account_id))
            .await
            .unwrap();
    }

    /// A confirmed credit sale with one product line.
    async fn credit_sale(
        s: &ReceiptSvc,
        customer_id: i64,
        product_id: i64,
        qty: &str,
        due: NaiveDate,
    ) -> SaleDetail {
        credit_sale_on(s, customer_id, product_id, qty, d(2024, 5, 2), due).await
    }

    /// Same, with an explicit sale date so due-date ties can be ordered.
    async fn credit_sale_on(
        s: &ReceiptSvc,
        customer_id: i64,
        product_id: i64,
        qty: &str,
        sale_date: NaiveDate,
        due: NaiveDate,
    ) -> SaleDetail {
        let sale = s
            .sales
            .create_draft(
                audit_actor(s).await,
                NewSale {
                    customer_id,
                    payment_type: PaymentType::Credit,
                    sale_date,
                    due_date: Some(due),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.sales
            .add_line(sale.id, product_id, dec(qty), None)
            .await
            .unwrap();
        s.sales
            .confirm(audit_actor(&s).await, sale.id, None)
            .await
            .unwrap()
    }

    async fn receipt_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_receipts")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn payment_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    async fn tx_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM transactions")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    /// Fixture: three debts of 30, 20 and 50 with due dates in that order.
    struct ThreeDebts {
        first: SaleDetail,
        second: SaleDetail,
        third: SaleDetail,
    }

    async fn three_debts(s: &ReceiptSvc, customer: i64, product: i64) -> ThreeDebts {
        ThreeDebts {
            first: credit_sale(s, customer, product, "3", d(2024, 6, 1)).await,
            second: credit_sale(s, customer, product, "2", d(2024, 6, 15)).await,
            third: credit_sale(s, customer, product, "5", d(2024, 7, 1)).await,
        }
    }

    // -- AC10: one receipt groups the payments of one handover -------------------

    #[tokio::test]
    async fn ac10_receipt_groups_a_payment_per_sale_and_keeps_every_transaction_link() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-1", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let debts = three_debts(&s, customer, product).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("80"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        assert_eq!(detail.total, dec("80"));
        assert_eq!(detail.allocations.len(), 3, "one payment per covered sale");

        // Method-only collection: the account is derived from the method's
        // owner and lands on the receipt, every grouped payment and every
        // finance movement.
        assert_eq!(detail.receipt.account_id, account);
        assert_eq!(detail.receipt.method_id, cash);
        assert!(
            detail
                .allocations
                .iter()
                .all(|p| p.account_id == account && p.method_id == cash),
            "every grouped payment carries the derived account"
        );

        let sale_ids: Vec<i64> = detail.allocations.iter().map(|p| p.sale_id).collect();
        assert_eq!(
            sale_ids,
            vec![
                debts.first.sale.id,
                debts.second.sale.id,
                debts.third.sale.id
            ]
        );
        let amounts: Vec<Decimal> = detail.allocations.iter().map(|p| p.amount).collect();
        assert_eq!(amounts, vec![dec("30"), dec("20"), dec("30")]);

        // Each grouped payment posts its own finance movement, stamped with the
        // number of the sale it pays; the receipt itself posts none.
        let numbers: HashMap<i64, String> = [&debts.first, &debts.second, &debts.third]
            .into_iter()
            .map(|detail| {
                (
                    detail.sale.id,
                    detail
                        .sale
                        .sale_number
                        .clone()
                        .expect("confirmed sale has a number"),
                )
            })
            .collect();
        let transactions = s
            .sales
            .transactions
            .transactions
            .list_by_account(account)
            .await
            .unwrap();
        assert_eq!(
            transactions.len(),
            3,
            "the receipt posts no movement of its own"
        );
        for payment in &detail.allocations {
            let tx_id = payment
                .transaction_id
                .expect("every grouped payment keeps its transaction link");
            assert_eq!(payment.receipt_id, Some(detail.receipt.id));
            let tx = s
                .sales
                .transactions
                .transactions
                .find_by_id(tx_id)
                .await
                .unwrap()
                .unwrap();
            assert!(tx.is_income());
            assert_eq!(tx.amount, payment.amount);
            assert_eq!(
                tx.reference.as_deref(),
                Some(numbers[&payment.sale_id].as_str())
            );
        }
        // The payments are visible from the sales they belong to.
        for payment in &detail.allocations {
            let sale = s.sales.get_detail(payment.sale_id).await.unwrap();
            assert!(
                sale.payments.iter().any(|p| p.id == payment.id),
                "the grouped payment still belongs to its sale"
            );
        }
        assert_eq!(tx_count(&pool).await, 3);
    }

    #[tokio::test]
    async fn ac10_receipt_total_equals_the_sum_of_its_allocations() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-2", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        three_debts(&s, customer, product).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("80"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let stored = s
            .receipts
            .list_allocations(detail.receipt.id)
            .await
            .unwrap();
        let sum: Decimal = stored.iter().map(|payment| payment.amount).sum();
        assert_eq!(sum, detail.total);
        // The derived read is cross-checked against raw SQL on purpose: this is
        // exactly the value that used to be stored and could disagree with the
        // payments. It stays a test-only assertion, not a runtime check.
        let raw: Vec<(String,)> =
            sqlx::query_as("SELECT amount FROM sale_payments WHERE receipt_id = ? ORDER BY id")
                .bind(detail.receipt.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        let raw_sum: Decimal = raw
            .iter()
            .map(|(amount,)| Decimal::from_str(amount).unwrap())
            .sum();
        assert_eq!(detail.total, raw_sum);
        assert_eq!(detail.total, dec("80"));
        // The customer paid exactly the receipt total: 100 owed, 80 applied.
        assert_eq!(s.sales.customer_balance(customer).await.unwrap(), dec("20"));
        for payment in &stored {
            assert_eq!(payment.receipt_id, Some(detail.receipt.id));
            assert!(payment.transaction_id.is_some());
        }
    }

    // -- AC11: same-customer rule and no overpay --------------------------------

    #[tokio::test]
    async fn ac11_a_receipt_only_covers_sales_of_its_own_customer() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-3", "10").await;
        let ana = seed_customer(&s, "Ana").await;
        let beto = seed_customer(&s, "Beto").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let ana_sale = credit_sale(&s, ana, product, "3", d(2024, 6, 1)).await;
        credit_sale(&s, beto, product, "4", d(2024, 6, 1)).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                ana,
                cash,
                dec("30"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let sale_ids: Vec<i64> = detail.allocations.iter().map(|p| p.sale_id).collect();
        assert_eq!(sale_ids, vec![ana_sale.sale.id]);
        assert_eq!(s.sales.customer_balance(ana).await.unwrap(), Decimal::ZERO);
        assert_eq!(
            s.sales.customer_balance(beto).await.unwrap(),
            dec("40"),
            "another customer's debt is untouched"
        );
    }

    #[tokio::test]
    async fn ac11_the_same_customer_guard_refuses_a_foreign_sale() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-4", "10").await;
        let ana = seed_customer(&s, "Ana").await;
        let beto = seed_customer(&s, "Beto").await;
        let beto_sale = credit_sale(&s, beto, product, "4", d(2024, 6, 1)).await;

        let err = s
            .ensure_sale_belongs_to_customer(ana, beto_sale.sale.id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("cannot be applied"));
        // The legitimate pair passes.
        s.ensure_sale_belongs_to_customer(beto, beto_sale.sale.id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ac11_overpaying_a_sale_through_a_receipt_is_rejected() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-5", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;

        // A receipt may never apply more to a sale than the sale still owes.
        let receipt = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: customer,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 20),
                    notes: None,
                },
            )
            .await
            .unwrap();
        let err = s
            .sales
            .record_payment_with_receipt(
                audit_actor(&s).await,
                sale.sale.id,
                cash,
                dec("40"),
                d(2024, 6, 20),
                Some(receipt.id),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("overpay"));
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(tx_count(&pool).await, 0);
        assert_eq!(s.sales.customer_balance(customer).await.unwrap(), dec("30"));
    }

    // -- AC12: a payment without a receipt is unchanged -------------------------

    #[tokio::test]
    async fn ac12_payment_without_a_receipt_still_works_exactly_as_before() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-6", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;

        let payment = s
            .sales
            .record_payment(
                audit_actor(&s).await,
                sale.sale.id,
                cash,
                dec("15"),
                d(2024, 6, 20),
            )
            .await
            .unwrap();

        assert_eq!(
            payment.receipt_id, None,
            "a direct payment carries no receipt"
        );
        assert!(payment.transaction_id.is_some());
        let after = s.sales.get_detail(sale.sale.id).await.unwrap();
        assert_eq!(after.paid, dec("15"));
        assert_eq!(after.due, dec("15"));
        assert_eq!(after.payment_status, PaymentStatus::Partial);
        assert_eq!(tx_count(&pool).await, 1);
        assert_eq!(receipt_count(&pool).await, 0);
    }

    // -- AC13: a referenced receipt cannot be deleted ---------------------------

    #[tokio::test]
    async fn ac13_deleting_a_receipt_referenced_by_a_payment_is_refused() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-7", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;
        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("30"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let err = s.delete_receipt(detail.receipt.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("payments still reference it"));
        assert!(
            s.receipts
                .find_by_id(detail.receipt.id)
                .await
                .unwrap()
                .is_some(),
            "the refused delete leaves the receipt and its grouping intact"
        );
        assert_eq!(
            s.sales.get_detail(sale.sale.id).await.unwrap().paid,
            dec("30")
        );

        // An unreferenced receipt is deletable; a second delete is a 404.
        let empty = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: customer,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 21),
                    notes: None,
                },
            )
            .await
            .unwrap();
        s.delete_receipt(empty.id).await.unwrap();
        assert!(s.receipts.find_by_id(empty.id).await.unwrap().is_none());
        assert!(matches!(
            s.delete_receipt(empty.id).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    /// AC18 (receipts audit, M5 Phase B slice S11): a collection carries the
    /// acting user of ITS request into the receipt document and into EVERY
    /// grouped payment — the S9/S10 twin of the flow-actor tests — never the
    /// sale's creator and never a fresh one.
    #[tokio::test]
    async fn ac18_the_collection_flow_receipt_and_its_payments_carry_the_flows_actor() {
        let (s, pool) = svc().await;
        let collector = test_support::seed_audit_user(&pool, "coll-bob", "Bob")
            .await
            .unwrap();

        let product = seed_product(&s, "COLL-AUD", "10").await;
        let customer = seed_customer(&s, "coll-customer").await;
        let account = seed_account(&s, "coll-wallet").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;

        // The sale is confirmed by the mechanical sentinel actor (its creator);
        // the collection is Bob's request, so the receipt and its payments name
        // HIM and stay distinguishable from the sale's creator.
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;
        assert_ne!(
            sale.sale.created_by, collector,
            "the two actors are distinguishable"
        );

        let detail = s
            .collect(collector, customer, cash, dec("30"), d(2024, 6, 20), None)
            .await
            .unwrap();

        assert_eq!(
            detail.receipt.created_by, collector,
            "the collection request's actor"
        );
        assert_eq!(
            detail.receipt.updated_by, None,
            "the receipt has no edit path"
        );
        for payment in &detail.allocations {
            assert_eq!(
                payment.created_by, collector,
                "every grouped payment carries the collection request's actor"
            );
            assert_ne!(
                payment.created_by, sale.sale.created_by,
                "not the sale's creator"
            );
            assert_eq!(payment.updated_by, None, "a fresh payment has no editor");
        }
    }

    // -- Allocation order and shape ---------------------------------------------

    #[tokio::test]
    async fn collection_exactly_covers_one_sale() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-8", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("30"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        assert_eq!(detail.allocations.len(), 1);
        assert_eq!(detail.total, dec("30"));
        let after = s.sales.get_detail(sale.sale.id).await.unwrap();
        assert_eq!(after.paid, dec("30"));
        assert_eq!(after.due, Decimal::ZERO);
        assert_eq!(after.payment_status, PaymentStatus::Paid);
    }

    #[tokio::test]
    async fn collection_spans_three_sales_with_a_partial_on_the_last() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-9", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let debts = three_debts(&s, customer, product).await;

        s.collect(
            audit_actor(&s).await,
            customer,
            cash,
            dec("80"),
            d(2024, 6, 20),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            s.sales.get_detail(debts.first.sale.id).await.unwrap().due,
            Decimal::ZERO
        );
        assert_eq!(
            s.sales.get_detail(debts.second.sale.id).await.unwrap().due,
            Decimal::ZERO
        );
        assert_eq!(
            s.sales.get_detail(debts.third.sale.id).await.unwrap().due,
            dec("20")
        );
    }

    #[tokio::test]
    async fn collection_pays_the_oldest_debt_first_not_the_largest() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-10", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let oldest = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await; // 30
        let largest = credit_sale(&s, customer, product, "10", d(2024, 6, 15)).await; // 100

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("40"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let sale_ids: Vec<i64> = detail.allocations.iter().map(|p| p.sale_id).collect();
        assert_eq!(sale_ids, vec![oldest.sale.id, largest.sale.id]);
        let amounts: Vec<Decimal> = detail.allocations.iter().map(|p| p.amount).collect();
        assert_eq!(amounts, vec![dec("30"), dec("10")]);
        assert_eq!(
            s.sales.get_detail(oldest.sale.id).await.unwrap().due,
            Decimal::ZERO,
            "the oldest debt is cleared first even though it is the smallest"
        );
        assert_eq!(
            s.sales.get_detail(largest.sale.id).await.unwrap().due,
            dec("90")
        );
    }

    #[tokio::test]
    async fn collection_breaks_due_date_ties_by_sale_date_then_id() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-18", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let due = d(2024, 6, 10);
        // Created in this order, so the id order disagrees with the sale-date order.
        let created_first = credit_sale_on(&s, customer, product, "4", d(2024, 5, 5), due).await;
        let earliest = credit_sale_on(&s, customer, product, "2", d(2024, 5, 1), due).await;
        let same_date = credit_sale_on(&s, customer, product, "3", d(2024, 5, 1), due).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("90"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let sale_ids: Vec<i64> = detail.allocations.iter().map(|p| p.sale_id).collect();
        assert_eq!(
            sale_ids,
            vec![earliest.sale.id, same_date.sale.id, created_first.sale.id],
            "same due date: earliest sale date first, then the lower id"
        );
    }

    #[tokio::test]
    async fn collection_continues_after_a_direct_payment_on_the_same_sale() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-19", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let sale = credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await; // 30

        // A direct payment reduces the debt the receipt can allocate over.
        s.sales
            .record_payment(
                audit_actor(&s).await,
                sale.sale.id,
                cash,
                dec("10"),
                d(2024, 6, 10),
            )
            .await
            .unwrap();
        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("20"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        assert_eq!(detail.total, dec("20"));
        assert_eq!(detail.allocations.len(), 1);
        assert_eq!(detail.allocations[0].amount, dec("20"));
        assert_eq!(
            s.sales.customer_balance(customer).await.unwrap(),
            Decimal::ZERO
        );
        let after = s.sales.get_detail(sale.sale.id).await.unwrap();
        assert_eq!(after.paid, dec("30"));
        assert_eq!(after.payment_status, PaymentStatus::Paid);
        let direct = after
            .payments
            .iter()
            .find(|payment| payment.receipt_id.is_none())
            .expect("the direct payment stays ungrouped");
        assert_eq!(direct.amount, dec("10"));
    }

    #[tokio::test]
    async fn planner_fills_the_oldest_debt_first_and_partials_only_the_last() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-11", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let debts = three_debts(&s, customer, product).await;
        let all = s.sales.customer_debt_sales(customer).await.unwrap();

        let plan = ReceiptSvc::plan_allocations(dec("80"), &all);
        let takes: Vec<(i64, Decimal)> = plan.iter().map(|a| (a.sale_id, a.amount)).collect();
        assert_eq!(
            takes,
            vec![
                (debts.first.sale.id, dec("30")),
                (debts.second.sale.id, dec("20")),
                (debts.third.sale.id, dec("30")),
            ]
        );

        // An amount equal to the whole receivable leaves nothing out.
        let plan = ReceiptSvc::plan_allocations(dec("100"), &all);
        let total: Decimal = plan.iter().map(|a| a.amount).sum();
        assert_eq!(total, dec("100"));
        assert_eq!(plan.len(), 3);
    }

    // -- Rejections leave no trace ----------------------------------------------

    #[tokio::test]
    async fn over_collection_is_rejected_and_leaves_no_receipt_or_payment_behind() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-12", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await; // 30

        let err = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("31"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("30"),
            "the outstanding figure must be quoted: {msg}"
        );
        assert_eq!(receipt_count(&pool).await, 0);
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(tx_count(&pool).await, 0);
        assert_eq!(s.sales.customer_balance(customer).await.unwrap(), dec("30"));
    }

    #[tokio::test]
    async fn unassigned_method_is_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-13", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        // Cash belongs to no account: no account can be derived for it.
        let cash = method_id(&s, "Cash").await;
        credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await; // 30

        let err = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("10"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("not assigned to any account"));
        assert_eq!(receipt_count(&pool).await, 0);
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(tx_count(&pool).await, 0);
        assert_eq!(s.sales.customer_balance(customer).await.unwrap(), dec("30"));
    }

    #[tokio::test]
    async fn zero_or_negative_amounts_are_rejected_without_side_effects() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-14", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;
        // The customer does owe: only the collected amount is wrong.

        for amount in [dec("0"), dec("-5")] {
            let err = s
                .collect(
                    audit_actor(&s).await,
                    customer,
                    cash,
                    amount,
                    d(2024, 6, 20),
                    None,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        assert_eq!(receipt_count(&pool).await, 0);
        assert_eq!(payment_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn unknown_customer_is_a_404_before_any_write() {
        let (s, pool) = svc().await;
        let cash = method_id(&s, "Cash").await;

        let err = s
            .collect(
                audit_actor(&s).await,
                999_999,
                cash,
                dec("10"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        assert_eq!(receipt_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn walkin_never_carries_a_debt_and_never_appears_in_an_allocation() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-15", "10").await;
        let ana = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let walkin = s
            .sales
            .customers
            .customers
            .find_walkin()
            .await
            .unwrap()
            .unwrap()
            .id;
        credit_sale(&s, ana, product, "3", d(2024, 6, 1)).await;

        // A credit sale to the walk-in is refused at confirm time, so the walk-in
        // has no receivable and can never be part of an allocation.
        assert!(s
            .sales
            .customer_debt_sales(walkin)
            .await
            .unwrap()
            .is_empty());
        let err = s
            .collect(
                audit_actor(&s).await,
                walkin,
                cash,
                dec("10"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(receipt_count(&pool).await, 0);
        assert!(s.list_receipts(walkin).await.unwrap().is_empty());
    }

    // -- Notes and reads --------------------------------------------------------

    #[tokio::test]
    async fn notes_are_trimmed_and_bounded_before_any_write() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-16", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        credit_sale(&s, customer, product, "3", d(2024, 6, 1)).await;

        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("10"),
                d(2024, 6, 20),
                Some("  half now  ".into()),
            )
            .await
            .unwrap();
        assert_eq!(detail.receipt.notes.as_deref(), Some("half now"));

        let err = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("10"),
                d(2024, 6, 20),
                Some("x".repeat(257)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(
            receipt_count(&pool).await,
            1,
            "the rejected note writes nothing"
        );
        assert_eq!(payment_count(&pool).await, 1);
    }

    #[tokio::test]
    async fn receipt_reads_return_the_grouped_payments() {
        let (s, _pool) = svc().await;
        let product = seed_product(&s, "R-17", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        three_debts(&s, customer, product).await;
        let detail = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("80"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap();

        let read = s.get_receipt(detail.receipt.id).await.unwrap();
        assert_eq!(read.receipt.id, detail.receipt.id);
        assert_eq!(read.total, detail.total);
        assert_eq!(read.allocations.len(), 3);

        let listed = s.list_receipts(customer).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].receipt.id, detail.receipt.id);
        assert_eq!(listed[0].total, dec("80"));

        assert!(matches!(
            s.get_receipt(999_999).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    /// An injected failure on the second grouped payment: the collection aborts,
    /// but the receipt state is coherent. Its derived total is what it actually
    /// applied, never the amount that was requested.
    #[tokio::test]
    async fn injected_failure_mid_collection_leaves_a_coherent_receipt() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-20", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let debts = three_debts(&s, customer, product).await; // 30, 20, 50

        // Inject a failure on the payment insert of the second covered sale, the
        // way the verifier did.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER injected_payment_failure BEFORE INSERT ON sale_payments \
             WHEN NEW.sale_id = {} BEGIN SELECT RAISE(ABORT, 'injected payment failure'); END",
            debts.second.sale.id
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("80"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();
        eprintln!("injected mid-loop failure: {err}");

        let receipt_id: (i64,) = sqlx::query_as("SELECT id FROM customer_receipts")
            .fetch_one(&pool)
            .await
            .unwrap();
        let detail = s.get_receipt(receipt_id.0).await.unwrap();
        assert_eq!(detail.allocations.len(), 1, "only the first payment landed");
        assert_eq!(
            detail.total,
            dec("30"),
            "the receipt reports what it applied, never the requested 80"
        );
        assert_ne!(detail.total, dec("80"));

        // The created payment is a valid grouped payment whose own movement is
        // correct and whose sale received exactly it.
        let payment = &detail.allocations[0];
        assert_eq!(payment.sale_id, debts.first.sale.id);
        assert_eq!(payment.amount, dec("30"));
        assert_eq!(payment.receipt_id, Some(receipt_id.0));
        let tx_id = payment
            .transaction_id
            .expect("grouped payment keeps its link");
        let tx = s
            .sales
            .transactions
            .transactions
            .find_by_id(tx_id)
            .await
            .unwrap()
            .unwrap();
        assert!(tx.is_income());
        assert_eq!(tx.amount, dec("30"));
        assert_eq!(
            tx.reference.as_deref(),
            debts.first.sale.sale_number.as_deref()
        );
        assert_eq!(
            s.sales.get_detail(debts.first.sale.id).await.unwrap().due,
            Decimal::ZERO
        );
        assert_eq!(
            s.sales.get_detail(debts.second.sale.id).await.unwrap().due,
            dec("20"),
            "the failed allocation did not pay its sale"
        );

        // The sales flow creates the movement before the payment row (no shared
        // transaction across modules), so the aborted insert can leave an orphan
        // movement; nothing claims it and the receipt total ignores it.
        let txs = s
            .sales
            .transactions
            .transactions
            .list_by_account(account)
            .await
            .unwrap();
        assert_eq!(txs.len(), 2, "one linked movement plus the aborted one");
        let orphan = txs.iter().find(|tx| tx.id != tx_id).unwrap();
        assert_eq!(orphan.amount, dec("20"));
        let links: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE transaction_id = ?")
                .bind(orphan.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(links.0, 0, "no payment claims the orphan movement");

        // Raw-SQL cross-check: the receipt cannot claim an amount it did not apply.
        let raw: Vec<(String,)> =
            sqlx::query_as("SELECT amount FROM sale_payments WHERE receipt_id = ?")
                .bind(receipt_id.0)
                .fetch_all(&pool)
                .await
                .unwrap();
        let raw_sum: Decimal = raw
            .iter()
            .map(|(amount,)| Decimal::from_str(amount).unwrap())
            .sum();
        assert_eq!(detail.total, raw_sum);
    }

    /// A failure on the first grouped payment leaves a receipt with zero
    /// allocations: it reports zero and is deletable because nothing references it.
    #[tokio::test]
    async fn injected_failure_on_the_first_payment_leaves_a_deletable_empty_receipt() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-21", "10").await;
        let customer = seed_customer(&s, "Ana").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let debts = three_debts(&s, customer, product).await;

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER injected_payment_failure BEFORE INSERT ON sale_payments \
             WHEN NEW.sale_id = {} BEGIN SELECT RAISE(ABORT, 'injected payment failure'); END",
            debts.first.sale.id
        )))
        .execute(&pool)
        .await
        .unwrap();

        let err = s
            .collect(
                audit_actor(&s).await,
                customer,
                cash,
                dec("80"),
                d(2024, 6, 20),
                None,
            )
            .await
            .unwrap_err();
        eprintln!("injected first-payment failure: {err}");

        let receipt_id: (i64,) = sqlx::query_as("SELECT id FROM customer_receipts")
            .fetch_one(&pool)
            .await
            .unwrap();
        let detail = s.get_receipt(receipt_id.0).await.unwrap();
        assert!(detail.allocations.is_empty(), "no payment landed");
        assert_eq!(detail.total, Decimal::ZERO, "an empty receipt reports zero");
        assert_eq!(payment_count(&pool).await, 0);
        assert_eq!(
            s.sales.get_detail(debts.first.sale.id).await.unwrap().due,
            dec("30"),
            "no sale was paid"
        );

        // Nothing references the receipt, so cleanup through the service works.
        s.delete_receipt(receipt_id.0).await.unwrap();
        assert!(s.receipts.find_by_id(receipt_id.0).await.unwrap().is_none());
    }

    /// Finding 1: a payment may only be grouped under a receipt of its own
    /// customer. The trigger fires on insert and on update, and only when a
    /// receipt is set, so ungrouped payments and legitimate groupings are
    /// untouched.
    #[tokio::test]
    async fn payment_cannot_be_grouped_under_another_customers_receipt() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-22", "10").await;
        let ana = seed_customer(&s, "Ana").await;
        let beto = seed_customer(&s, "Beto").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let ana_sale = credit_sale(&s, ana, product, "3", d(2024, 6, 1)).await;
        let ana_sale_two = credit_sale(&s, ana, product, "2", d(2024, 6, 10)).await;
        let beto_receipt = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: beto,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 20),
                    notes: None,
                },
            )
            .await
            .unwrap();
        let ana_receipt = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: ana,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 20),
                    notes: None,
                },
            )
            .await
            .unwrap();

        // Direct SQL insert of a mismatched pair is aborted.
        let err = sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by) \
             VALUES (?, ?, ?, '10', '2024-06-20', ?, ?)",
        )
        .bind(ana_sale.sale.id)
        .bind(account)
        .bind(cash)
        .bind(beto_receipt.id)
        .bind(audit_actor(&s).await)
        .execute(&pool)
        .await
        .unwrap_err();
        eprintln!("mismatched insert error: {err}");
        assert!(
            err.to_string().contains("another customer's receipt"),
            "got {err}"
        );

        // A matching pair is accepted, and a payment with a NULL receipt_id is
        // unaffected.
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by) \
             VALUES (?, ?, ?, '10', '2024-06-20', ?, ?)",
        )
        .bind(ana_sale.sale.id)
        .bind(account)
        .bind(cash)
        .bind(ana_receipt.id)
        .bind(audit_actor(&s).await)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by) \
             VALUES (?, ?, ?, '5', '2024-06-20', NULL, ?)",
        )
        .bind(ana_sale.sale.id)
        .bind(account)
        .bind(cash)
        .bind(audit_actor(&s).await)
        .execute(&pool)
        .await
        .unwrap();
        let free_payment: (i64,) =
            sqlx::query_as("SELECT id FROM sale_payments WHERE sale_id = ? AND receipt_id IS NULL")
                .bind(ana_sale.sale.id)
                .fetch_one(&pool)
                .await
                .unwrap();

        // UPDATE onto another customer's receipt is aborted...
        let err = sqlx::query("UPDATE sale_payments SET receipt_id = ? WHERE id = ?")
            .bind(beto_receipt.id)
            .bind(free_payment.0)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("mismatched update error: {err}");
        assert!(
            err.to_string().contains("another customer's receipt"),
            "got {err}"
        );

        // ...while grouping it under a receipt of its own customer works, on a
        // second sale of the same customer too.
        sqlx::query("UPDATE sale_payments SET receipt_id = ? WHERE id = ?")
            .bind(ana_receipt.id)
            .bind(free_payment.0)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, receipt_id, created_by) \
             VALUES (?, ?, ?, '7', '2024-06-20', ?, ?)",
        )
        .bind(ana_sale_two.sale.id)
        .bind(account)
        .bind(cash)
        .bind(ana_receipt.id)
        .bind(audit_actor(&s).await)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            s.receipts
                .list_allocations(ana_receipt.id)
                .await
                .unwrap()
                .len(),
            3,
            "one receipt grouping several payments of its own customer"
        );

        // The service path still groups its own customer's payment.
        let paid = s
            .sales
            .record_payment_with_receipt(
                audit_actor(&s).await,
                ana_sale.sale.id,
                cash,
                dec("1"),
                d(2024, 6, 21),
                Some(ana_receipt.id),
            )
            .await
            .unwrap();
        assert_eq!(paid.receipt_id, Some(ana_receipt.id));
    }

    /// Slice M: even a caller that hands the service a receipt id cannot group a
    /// payment under another customer's receipt. The trigger aborts the insert;
    /// the service maps that to a clean Validation (400) instead of a database
    /// error, and no payment row is written. No route exposes the receipt id at
    /// all, so this is the backstop behind the interface.
    #[tokio::test]
    async fn service_refuses_grouping_a_payment_under_another_customers_receipt() {
        let (s, pool) = svc().await;
        let product = seed_product(&s, "R-23", "10").await;
        let ana = seed_customer(&s, "Ana").await;
        let beto = seed_customer(&s, "Beto").await;
        let account = seed_account(&s, "Caja").await;
        let cash = method_id(&s, "Cash").await;
        allow(&s, account, cash).await;
        let ana_sale = credit_sale(&s, ana, product, "3", d(2024, 6, 1)).await; // 30
        let beto_receipt = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: beto,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 20),
                    notes: None,
                },
            )
            .await
            .unwrap();

        let err = s
            .sales
            .record_payment_with_receipt(
                audit_actor(&s).await,
                ana_sale.sale.id,
                cash,
                dec("10"),
                d(2024, 6, 20),
                Some(beto_receipt.id),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("another customer"),
            "the message must name the rule: {err}"
        );
        assert_eq!(
            payment_count(&pool).await,
            0,
            "the refused grouping writes no payment"
        );
        assert_eq!(s.sales.customer_balance(ana).await.unwrap(), dec("30"));
        assert_eq!(
            s.receipts
                .list_allocations(beto_receipt.id)
                .await
                .unwrap()
                .len(),
            0
        );

        // The same call with a receipt of the sale's own customer still works.
        let ana_receipt = s
            .receipts
            .create(
                audit_actor(&s).await,
                &NewReceipt {
                    customer_id: ana,
                    account_id: account,
                    method_id: cash,
                    date: d(2024, 6, 20),
                    notes: None,
                },
            )
            .await
            .unwrap();
        let paid = s
            .sales
            .record_payment_with_receipt(
                audit_actor(&s).await,
                ana_sale.sale.id,
                cash,
                dec("10"),
                d(2024, 6, 20),
                Some(ana_receipt.id),
            )
            .await
            .unwrap();
        assert_eq!(paid.receipt_id, Some(ana_receipt.id));
        assert_eq!(s.sales.customer_balance(ana).await.unwrap(), dec("20"));
    }
}
