use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{PriceRefusal, Transaction, TransactionFilter, TransactionKind};
use crate::repositories::{AccountRepository, TransactionRepository};

#[derive(Clone)]
pub struct TransactionService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub accounts: A,
    pub transactions: T,
    pub allow_negative: bool,
}

impl<A, T> TransactionService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub fn new(accounts: A, transactions: T, allow_negative: bool) -> Self {
        Self {
            accounts,
            transactions,
            allow_negative,
        }
    }

    fn validate_amount(&self, amount: Decimal) -> AppResult<()> {
        if amount <= Decimal::ZERO {
            return Err(AppError::Validation("amount must be > 0".into()));
        }
        if amount.scale() > 2 {
            // Allow up to 2 decimals for finance; could be relaxed but we validate.
            // rust_decimal supports more; we just warn? For now allow any but log.
        }
        Ok(())
    }

    fn validate_date(&self, date: NaiveDate) -> AppResult<()> {
        // No future restriction by default, but could enforce. We'll allow any.
        let _ = date;
        Ok(())
    }

    /// Manual transaction entry point (no document reference). Kept as the
    /// 6-argument form so manual callers keep working; delegates to
    /// `create_with_reference`. `actor` is the audit actor: the acting user's
    /// id from the request's `Principal` (M5 Phase B) — the movement records
    /// who recorded it.
    pub async fn create(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.create_with_reference(actor, account_id, kind, amount, description, None, date)
            .await
    }

    /// Create a transaction, optionally stamped with an opaque source `reference`
    /// (the document number written by sales/purchases). Finance only stores the
    /// string; it never knows about the document that produced it. `actor` is
    /// the audit actor of the ORIGINATING request: a movement produced inside a
    /// document flow carries that flow's acting user, never a fresh actor
    /// (AC18) — the flow passes it down, finance never invents one.
    pub async fn create_with_reference(
        &self,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        reference: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.validate_amount(amount)?;
        self.validate_date(date)?;

        if !self.accounts.exists(account_id).await? {
            return Err(AppError::NotFound(format!(
                "account {account_id} not found"
            )));
        }

        // The resulting balance is computed BEFORE the write and for BOTH kinds,
        // which is two bounds in one place and the reason neither is optional.
        //
        // The overdraft guard is the one this code always had, and it stays: an
        // Expense may not overdraw an account that forbids it. The range check is
        // new, and it is the same argument as a document total, one level out: a
        // transaction is a bounded single write, and the SUM of a set of them is
        // not bounded — two incomes of `4e28` each carry and are `8e28` together.
        // What this pre-check buys is EARLY refusal with a useful message, not
        // reachability: the repository's fold is the guarantee, and it folds in
        // `ORDER BY id` — the same order the writes append in, so the two walk the
        // same prefixes as well as agreeing on the total.
        let current = self.transactions.balance_for_account(account_id).await?;
        let signed = match kind {
            TransactionKind::Income => amount,
            TransactionKind::Expense => -amount,
        };
        let next = current
            .checked_add(signed)
            .ok_or(AppError::PriceRefused(PriceRefusal::AggregateTooLarge))?;
        if !self.allow_negative && kind == TransactionKind::Expense && next < Decimal::ZERO {
            return Err(AppError::Validation(format!(
                "insufficient funds: balance {current} would become {next}"
            )));
        }

        let desc = description.unwrap_or_default();
        if desc.len() > 256 {
            return Err(AppError::Validation(
                "description must be <= 256 chars".into(),
            ));
        }

        self.transactions
            .create(
                actor,
                account_id,
                kind,
                amount,
                &desc,
                reference.as_deref(),
                date,
            )
            .await
    }

    /// [`Self::create_with_reference`] inside a transaction the CALLER owns. A
    /// SIBLING, not a wrapper: the public method above keeps running on the pool
    /// for every manual-entry caller and keeps opening its own transaction, and
    /// changing that would be a behaviour change with a wide blast radius. This
    /// opens nothing — it hands the caller's connection to the three repository
    /// forms.
    ///
    /// This is the strongest correctness case in the sequence, and the READ is
    /// why: the balance is read, a decision is taken against it, and the row is
    /// then written on the same account, with no transaction anywhere between
    /// the read and the write. On the pool those are two independent units. A
    /// reader on another connection sees a stale balance; a reader here sees the
    /// unit's own writes. Left on the pool, a document with two withdrawals
    /// against one account re-reads the opening figure for both: `10` where the
    /// account actually holds `4`, and a withdrawal gets approved against a
    /// balance the same unit has already spent. Inside one unit the second fold
    /// sees the first line's row and refuses it.
    ///
    /// `accounts.exists_in` moves for the same reason and matters most in the
    /// earliest window: the existence check is the first statement in the unit,
    /// and on the pool an account created by an earlier step of the same unit
    /// does not exist yet.
    ///
    /// `create_in` also refreshes `accounts.cached_balance`, and that refresh
    /// was already connection-bound (`sync_cached` has always taken a
    /// `&mut SqliteConnection`) — so the cache moves inside the caller's unit
    /// too, and a rollback takes it back. A cache that disagrees with the rows
    /// behind it is the same defect the rollback case is about.
    ///
    /// Every validation, every refusal and the ORDER of them are
    /// `create_with_reference`'s, unchanged: amount, then the account lookup,
    /// then the level fold, then the description length. Only the executor
    /// differs.
    pub async fn create_with_reference_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor: i64,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        reference: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.validate_amount(amount)?;
        self.validate_date(date)?;

        if !self.accounts.exists_in(tx, account_id).await? {
            return Err(AppError::NotFound(format!(
                "account {account_id} not found"
            )));
        }

        // The same fold `create_with_reference` performs, on the caller's
        // connection, so a second line against the same account folds what the
        // first line wrote rather than the opening snapshot. The reasoning in
        // the comment above it still holds unchanged.
        let current = self
            .transactions
            .balance_for_account_in(tx, account_id)
            .await?;
        let signed = match kind {
            TransactionKind::Income => amount,
            TransactionKind::Expense => -amount,
        };
        let next = current
            .checked_add(signed)
            .ok_or(AppError::PriceRefused(PriceRefusal::AggregateTooLarge))?;
        if !self.allow_negative && kind == TransactionKind::Expense && next < Decimal::ZERO {
            return Err(AppError::Validation(format!(
                "insufficient funds: balance {current} would become {next}"
            )));
        }

        let desc = description.unwrap_or_default();
        if desc.len() > 256 {
            return Err(AppError::Validation(
                "description must be <= 256 chars".into(),
            ));
        }

        self.transactions
            .create_in(
                tx,
                actor,
                account_id,
                kind,
                amount,
                &desc,
                reference.as_deref(),
                date,
            )
            .await
    }

    pub async fn list(&self, filter: TransactionFilter) -> AppResult<Vec<Transaction>> {
        if let Some(aid) = filter.account_id {
            if !self.accounts.exists(aid).await? {
                return Err(AppError::NotFound(format!("account {aid} not found")));
            }
        }
        if let (Some(from), Some(to)) = (filter.from, filter.to) {
            if from > to {
                return Err(AppError::Validation("'from' must be <= 'to'".into()));
            }
        }
        self.transactions.list(&filter).await
    }

    pub async fn update(
        &self,
        actor: i64,
        id: i64,
        kind: Option<TransactionKind>,
        amount: Option<Decimal>,
        description: Option<String>,
        date: Option<NaiveDate>,
    ) -> AppResult<Transaction> {
        let mut existing = self
            .transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))?;

        if let Some(a) = amount {
            self.validate_amount(a)?;
            existing.amount = a;
        }
        if let Some(k) = kind {
            existing.kind = k;
        }
        if let Some(d) = description {
            if d.len() > 256 {
                return Err(AppError::Validation(
                    "description must be <= 256 chars".into(),
                ));
            }
            existing.description = d;
        }
        if let Some(dt) = date {
            self.validate_date(dt)?;
            existing.date = dt;
        }

        // Negative-balance guard: compute projected balance after edit.
        if !self.allow_negative {
            // `existing` holds the mutated values, but DB still has the original.
            // current_balance is derived from DB (original included).
            let current_balance = self
                .transactions
                .balance_for_account(existing.account_id)
                .await?;
            let orig = self.transactions.find_by_id(id).await?.unwrap();
            let orig_signed = match orig.kind {
                TransactionKind::Income => orig.amount,
                TransactionKind::Expense => -orig.amount,
            };
            let new_signed = match existing.kind {
                TransactionKind::Income => existing.amount,
                TransactionKind::Expense => -existing.amount,
            };
            // CHECKED, and the same rule the balance fold uses: this projection is
            // a set sum over the account's transactions with one row swapped, and
            // reverting a large expense can push it past what `Decimal` carries
            // (`5.1e28 + 4.9e28`). Two raw operators here were a panic on a route
            // every `finance.write` caller can reach, including an empty-body
            // update — which recomputes this on every edit.
            let projected = current_balance
                .checked_sub(orig_signed)
                .and_then(|balance| balance.checked_add(new_signed))
                .ok_or(AppError::PriceRefused(PriceRefusal::AggregateTooLarge))?;
            if projected < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "update would cause negative balance: projected {projected}"
                )));
            }
        }

        self.transactions.update(&existing, actor).await
    }

    pub async fn delete(&self, id: i64) -> AppResult<()> {
        let tx = self
            .transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))?;

        if !self.allow_negative && tx.kind == TransactionKind::Income {
            // Removing income could make balance negative.
            //
            // The subtraction is bounded, and the bound is the `allow_negative`
            // guard right here: that flag is what keeps every balance in the table
            // at or above zero, and `amount > 0` is validated on the way in, so
            // `current - amount` lands in `[-MAX, MAX]` and cannot leave the range.
            // Unlike the projection above there is no second term here, and no term
            // that can carry a sign the other one cannot.
            let current = self.transactions.balance_for_account(tx.account_id).await?;
            let projected = current - tx.amount; // Income removal subtracts
            if projected < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "deleting this income would cause negative balance: projected {projected}"
                )));
            }
        }

        let deleted = self.transactions.delete(id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!("transaction {id} not found")));
        }
        Ok(())
    }

    pub async fn get(&self, id: i64) -> AppResult<Transaction> {
        self.transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::{SqliteAccountRepository, SqliteTransactionRepository};
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    // -- migration backfill ----------------------------------------------------

    #[tokio::test]
    async fn backfill_sets_reference_only_for_document_shaped_descriptions() {
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
        // Stop before the traceability migration so the rows below look exactly
        // like data written by M2/M3 before this change.
        migrator.run_to(20240101000018, &pool).await.unwrap();

        let account_id: (i64,) = sqlx::query_as(
            "INSERT INTO accounts (name, cached_balance) VALUES ('legacy', '0') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let sale_id: (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_name, sale_date) \
             VALUES ('Confirmed', 'Cash', 'legacy', '2024-01-01') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date) \
             VALUES (?, ?, 1, '10', '2024-01-01')",
        )
        .bind(sale_id.0)
        .bind(account_id.0)
        .execute(&pool)
        .await
        .unwrap();

        // Document-shaped descriptions written by M2/M3 before this change, plus
        // lookalikes that must never be matched ("do not invent matches").
        let legacy_descriptions = [
            "2024-SALE-000012",
            "2024-PURCH-000007",
            "Salary",
            "2024-SALE-12",
            "2024-SALE-000012 extra",
            "2024-sale-000012",
            "2024-PURCH-0000078",
            "",
        ];
        for description in legacy_descriptions {
            sqlx::query(
                "INSERT INTO transactions (account_id, kind, amount, description, date) \
                 VALUES (?, 'Income', '10', ?, '2024-01-01')",
            )
            .bind(account_id.0)
            .bind(description)
            .execute(&pool)
            .await
            .unwrap();
        }

        migrator.run(&pool).await.unwrap();

        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT description, reference FROM transactions ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let reference_for = |description: &str| {
            rows.iter()
                .find(|(d, _)| d == description)
                .and_then(|(_, r)| r.clone())
        };
        assert_eq!(
            reference_for("2024-SALE-000012").as_deref(),
            Some("2024-SALE-000012")
        );
        assert_eq!(
            reference_for("2024-PURCH-000007").as_deref(),
            Some("2024-PURCH-000007")
        );
        for unrelated in [
            "Salary",
            "2024-SALE-12",
            "2024-SALE-000012 extra",
            "2024-sale-000012",
            "2024-PURCH-0000078",
            "",
        ] {
            assert_eq!(
                reference_for(unrelated),
                None,
                "must not invent a reference for {unrelated:?}"
            );
        }

        // Historical payments stay unlinked: there is no deterministic way to know
        // which transaction a pre-existing payment created, but the transaction is
        // still traceable through its `reference`.
        let legacy_payment: (Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT transaction_id, refund_transaction_id FROM sale_payments LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(legacy_payment, (None, None));
    }

    // -- RESTRICT delete quality -----------------------------------------------

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

    async fn svc() -> (
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
        sqlx::SqlitePool,
    ) {
        let pool = test_pool().await;
        let s = TransactionService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
            true,
        );
        (s, pool)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d() -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 5, 1).unwrap()
    }

    /// Raw fixture: link a transaction to a synthetic sale payment. Finance
    /// tests stay domain-agnostic and never import sales/purchases types.
    /// `customer_id` is NOT NULL by design and resolves the seeded walk-in
    /// instead of hardcoding its id.
    async fn link_sale_payment(pool: &sqlx::SqlitePool, tx_id: i64, account_id: i64) {
        let sale_id: (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by) \
             VALUES ('Confirmed', 'Cash', \
                     (SELECT id FROM customers WHERE is_walkin = 1), 'fixture', '2024-05-01', ?) \
             RETURNING id",
        )
        .bind(crate::security::test_support::audit_actor_id(pool).await.unwrap())
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments \
             (sale_id, account_id, method_id, amount, date, transaction_id, created_by) \
             VALUES (?, ?, 1, '10', '2024-05-01', ?, ?)",
        )
        .bind(sale_id.0)
        .bind(account_id)
        .bind(tx_id)
        .bind(
            crate::security::test_support::audit_actor_id(pool)
                .await
                .unwrap(),
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// Raw fixture: link a transaction to a synthetic purchase payment.
    async fn link_purchase_payment(pool: &sqlx::SqlitePool, tx_id: i64, account_id: i64) {
        let supplier_id: (i64,) = sqlx::query_as(
            "INSERT INTO suppliers (name, created_by) VALUES ('fixture supplier', ?) RETURNING id",
        )
        .bind(
            crate::security::test_support::audit_actor_id(pool)
                .await
                .unwrap(),
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let purchase_id: (i64,) = sqlx::query_as(
            "INSERT INTO purchases (supplier_id, status, payment_type, purchase_date, created_by) \
             VALUES (?, 'Confirmed', 'Credit', '2024-05-01', ?) RETURNING id",
        )
        .bind(supplier_id.0)
        .bind(
            crate::security::test_support::audit_actor_id(pool)
                .await
                .unwrap(),
        )
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO purchase_payments \
             (purchase_id, account_id, method_id, amount, date, transaction_id, created_by) \
             VALUES (?, ?, 1, '20', '2024-05-01', ?, ?)",
        )
        .bind(purchase_id.0)
        .bind(account_id)
        .bind(tx_id)
        .bind(
            crate::security::test_support::audit_actor_id(pool)
                .await
                .unwrap(),
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_linked_transaction_is_conflict_and_keeps_row_and_payment() {
        let (s, pool) = svc().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = s.accounts.create(actor, "Caja").await.unwrap();
        let sale_tx = s
            .create_with_reference(
                actor,
                acc.id,
                TransactionKind::Expense,
                dec("10"),
                Some("sale payment".into()),
                None,
                d(),
            )
            .await
            .unwrap();
        let purchase_tx = s
            .create_with_reference(
                actor,
                acc.id,
                TransactionKind::Expense,
                dec("20"),
                Some("purchase payment".into()),
                None,
                d(),
            )
            .await
            .unwrap();
        link_sale_payment(&pool, sale_tx.id, acc.id).await;
        link_purchase_payment(&pool, purchase_tx.id, acc.id).await;

        for tx in [&sale_tx, &purchase_tx] {
            let err = s.delete(tx.id).await.unwrap_err();
            assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains("cancel the document"),
                "message must be actionable, got: {msg}"
            );
            assert!(
                s.transactions.find_by_id(tx.id).await.unwrap().is_some(),
                "the linked transaction row must survive"
            );
        }

        let sale_links: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE transaction_id = ?")
                .bind(sale_tx.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sale_links.0, 1, "sale payment must stay linked");
        let purchase_links: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchase_payments WHERE transaction_id = ?")
                .bind(purchase_tx.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(purchase_links.0, 1, "purchase payment must stay linked");
    }

    #[tokio::test]
    async fn delete_unlinked_transaction_still_works() {
        let (s, pool) = svc().await;
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = s.accounts.create(actor, "Caja").await.unwrap();
        let tx = s
            .create_with_reference(
                actor,
                acc.id,
                TransactionKind::Expense,
                dec("5"),
                Some("manual".into()),
                None,
                d(),
            )
            .await
            .unwrap();

        s.delete(tx.id).await.unwrap();
        assert!(s.transactions.find_by_id(tx.id).await.unwrap().is_none());
    }

    // -- Phase B: the seam that joins a caller's transaction --------------------

    /// The strict build, for the folds that only refuse when the rule is on.
    /// `svc()` is `allow_negative = true`, where an overdraw is legal — a test
    /// about the overdraft guard built on it would pass for the wrong reason.
    async fn strict_svc() -> (
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
        sqlx::SqlitePool,
    ) {
        let pool = test_pool().await;
        let s = TransactionService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
            false,
        );
        (s, pool)
    }

    /// Read `cached_balance` on the caller's own connection, which is the only
    /// way to see the column while the unit holds the pool's only connection.
    async fn cached_on(tx: &mut sqlx::SqliteConnection, account_id: i64) -> String {
        let (balance,): (String,) =
            sqlx::query_as("SELECT cached_balance FROM accounts WHERE id = ?")
                .bind(account_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        balance
    }

    async fn cached_balance(pool: &sqlx::SqlitePool, account_id: i64) -> String {
        let (balance,): (String,) =
            sqlx::query_as("SELECT cached_balance FROM accounts WHERE id = ?")
                .bind(account_id)
                .fetch_one(pool)
                .await
                .unwrap();
        balance
    }

    /// The seam joins the unit and opens nothing, and the CACHE proves both:
    /// `sync_cached` already ran on the connection it was handed, so the column
    /// moves inside the unit and a rollback takes it back. A seam that reached
    /// for the pool could not answer on this `max_connections(1)` fixture at
    /// all — 30s, then `PoolTimedOut` — and `try_acquire` answering `None` is
    /// the premise that makes that true rather than a race.
    #[tokio::test]
    async fn create_with_reference_in_writes_into_the_callers_transaction_and_a_rollback_takes_the_cached_balance_back_too(
    ) {
        let (s, pool) = svc().await;
        let who = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = s.accounts.create(who, "Caja seam").await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        assert!(
            pool.try_acquire().is_none(),
            "the pool still has a spare connection, so this test would not prove anything"
        );

        let started = std::time::Instant::now();
        let written = s
            .create_with_reference_in(
                &mut tx,
                who,
                acc.id,
                TransactionKind::Income,
                dec("10"),
                Some("2024-SALE-000001".into()),
                Some("2024-SALE-000001".into()),
                d(),
            )
            .await
            .expect(
                "create_with_reference_in reached for the pool; with the only connection held by \
                 the caller's transaction that is a 30s PoolTimedOut, not an answer",
            );
        let elapsed = started.elapsed();

        let folded_inside = s
            .transactions
            .balance_for_account_in(&mut tx, acc.id)
            .await
            .unwrap();
        let cached_inside = cached_on(&mut tx, acc.id).await;
        tx.rollback().await.unwrap();

        assert_eq!(
            folded_inside,
            dec("10"),
            "the caller's transaction lost the write"
        );
        assert_eq!(
            cached_inside, "10",
            "the cached refresh is one unit with the row, so it moves inside the transaction too"
        );
        // MEASURED in this module rather than inherited: 30.000s is sqlx's
        // acquire timeout on this one-connection pool, five seconds is four
        // orders of magnitude below a joined write's cost and one above the
        // six seconds of headroom it needs.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "create_with_reference_in took {elapsed:?}; that is a nested BEGIN stalling for a \
             connection, not a joined write"
        );
        assert!(s
            .transactions
            .find_by_id(written.id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            s.transactions.balance_for_account(acc.id).await.unwrap(),
            Decimal::ZERO
        );
        assert_eq!(
            cached_balance(&pool, acc.id).await,
            "0",
            "a rolled-back unit must not leave a cache that disagrees with the rows behind it"
        );
    }

    /// The strongest correctness case in the sequence: the balance is READ, then
    /// written against, inside one unit with no transaction between them.
    ///
    /// Two withdrawals of 6 from an account holding 10: the second pre-check
    /// must fold the 4 the first line left. On the pool it re-reads 10 both
    /// times and waves both through — a withdrawal approved against a balance
    /// the same unit has already spent. Inside the unit the second is refused,
    /// and the refusal names the figures the fold actually saw.
    #[tokio::test]
    async fn create_with_reference_in_folds_the_balance_inside_the_callers_transaction_so_two_withdrawals_of_one_unit_cannot_both_pass(
    ) {
        let (s, pool) = strict_svc().await;
        let who = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = s.accounts.create(who, "Caja fold").await.unwrap();
        // The committed opening balance, written through the PUBLIC method.
        s.create_with_reference(
            who,
            acc.id,
            TransactionKind::Income,
            dec("10"),
            Some("opening".into()),
            None,
            d(),
        )
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        // 10 - 6 = 4: the first withdrawal of the document.
        s.create_with_reference_in(
            &mut tx,
            who,
            acc.id,
            TransactionKind::Expense,
            dec("6"),
            Some("line 1".into()),
            None,
            d(),
        )
        .await
        .unwrap();
        // 4 - 6 = -2, and only a fold that saw the first line gets there.
        let refused = s
            .create_with_reference_in(
                &mut tx,
                who,
                acc.id,
                TransactionKind::Expense,
                dec("6"),
                Some("line 2".into()),
                None,
                d(),
            )
            .await
            .unwrap_err();
        let cached_inside = cached_on(&mut tx, acc.id).await;
        tx.rollback().await.unwrap();

        match refused {
            AppError::Validation(msg) => assert!(
                msg.contains("balance 4 would become -2"),
                "the refusal must name the figures the fold saw inside the unit, got: {msg}"
            ),
            other => panic!("an overdraft is a 400 Validation, not {other:?}"),
        }
        assert_eq!(
            cached_inside, "4",
            "the first line's cache refresh rode the unit"
        );
        assert_eq!(
            s.transactions.balance_for_account(acc.id).await.unwrap(),
            dec("10"),
            "the rollback must take the first withdrawal back with the refused one"
        );
        assert_eq!(cached_balance(&pool, acc.id).await, "10");
    }

    /// The additive claim, proved rather than asserted: the public method is not
    /// a wrapper of the new one. It still runs on the pool, still commits its own
    /// row and its own cache refresh, and still answers the same refusals.
    #[tokio::test]
    async fn the_public_create_with_reference_still_commits_and_refreshes_the_cache() {
        let (s, pool) = strict_svc().await;
        let who = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = s.accounts.create(who, "Caja public").await.unwrap();

        let written = s
            .create_with_reference(
                who,
                acc.id,
                TransactionKind::Income,
                dec("10"),
                Some("2024-SALE-000001".into()),
                Some("2024-SALE-000001".into()),
                d(),
            )
            .await
            .unwrap();
        let stored = s
            .transactions
            .find_by_id(written.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.account_id, acc.id);
        assert_eq!(stored.kind, TransactionKind::Income);
        assert_eq!(stored.amount, dec("10"));
        assert_eq!(stored.description, "2024-SALE-000001");
        assert_eq!(stored.reference, Some("2024-SALE-000001".to_string()));
        assert_eq!(stored.created_by, who);
        assert_eq!(stored.updated_by, None, "a new money row has no editor");
        assert_eq!(
            s.transactions.balance_for_account(acc.id).await.unwrap(),
            dec("10")
        );
        assert_eq!(cached_balance(&pool, acc.id).await, "10");

        // The validations keep their order and their variants: amount, then the
        // account lookup, then the level fold, then the description length.
        assert!(matches!(
            s.create_with_reference(
                who,
                acc.id,
                TransactionKind::Income,
                dec("0"),
                None,
                None,
                d()
            )
            .await
            .unwrap_err(),
            AppError::Validation(_)
        ));
        assert!(matches!(
            s.create_with_reference(
                who,
                99_999,
                TransactionKind::Income,
                dec("1"),
                None,
                None,
                d()
            )
            .await
            .unwrap_err(),
            AppError::NotFound(_)
        ));
        // The overdraft guard still fires on Expense and only on Expense.
        assert!(matches!(
            s.create_with_reference(
                who,
                acc.id,
                TransactionKind::Expense,
                dec("11"),
                None,
                None,
                d()
            )
            .await
            .unwrap_err(),
            AppError::Validation(_)
        ));
        assert!(s
            .create_with_reference(
                who,
                acc.id,
                TransactionKind::Income,
                dec("11"),
                None,
                None,
                d()
            )
            .await
            .is_ok());
        assert_eq!(cached_balance(&pool, acc.id).await, "21");
    }

    // -- audit attribution (M5 Phase B, slice S9, AC18) --------------------------

    /// AC18 on the finance surface: a movement records who created it, and an
    /// edit records the editor WITHOUT erasing the creator. Two dedicated
    /// users make the assertion meaningful: the row must carry the editor's
    /// id on `updated_by` and the creator's id on `created_by`.
    #[tokio::test]
    async fn ac18_create_and_update_store_two_different_actors() {
        let (s, pool) = svc().await;
        let alice = test_support::seed_audit_user(&pool, "audit-alice", "Alice")
            .await
            .unwrap();
        let bob = test_support::seed_audit_user(&pool, "audit-bob", "Bob")
            .await
            .unwrap();

        let acc = s.accounts.create(alice, "Caja").await.unwrap();
        assert_eq!(acc.created_by, alice, "the account records its creator");
        assert_eq!(acc.updated_by, None);

        // The movement is recorded by Bob (the flow's request actor).
        let tx = s
            .create_with_reference(
                bob,
                acc.id,
                TransactionKind::Income,
                dec("10"),
                Some("ingreso".into()),
                None,
                d(),
            )
            .await
            .unwrap();
        assert_eq!(tx.created_by, bob, "the movement records the acting user");
        assert_eq!(tx.updated_by, None, "an unedited movement has no editor");

        // Alice edits it: `updated_by` carries HER, `created_by` keeps Bob.
        let updated = s
            .update(alice, tx.id, None, Some(dec("12")), None, None)
            .await
            .unwrap();
        assert_eq!(
            updated.created_by, bob,
            "the creator attribution survives the edit"
        );
        assert_eq!(
            updated.updated_by,
            Some(alice),
            "the edit records the editor"
        );
    }

    /// AC18: a movement produced INSIDE another document flow carries the
    /// flow's request actor, not a fresh one. Finance is the table's owner;
    /// the flow passes its acting user down, and the test proves the stored
    /// id is the flow's, distinct from the account's creator.
    #[tokio::test]
    async fn ac18_a_flow_created_row_carries_the_flows_actor() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "flow-account", "Account Op")
            .await
            .unwrap();
        let operator = test_support::seed_audit_user(&pool, "flow-payment", "Paying Op")
            .await
            .unwrap();

        let acc = s.accounts.create(creator, "Caja").await.unwrap();
        // A manual Expense (the flow's own write path) carries the flow's
        // actor, which here differs from the account's creator.
        let tx = s
            .create_with_reference(
                operator,
                acc.id,
                TransactionKind::Expense,
                dec("5"),
                Some("2024-SALE-000001".into()),
                Some("2024-SALE-000001".into()),
                d(),
            )
            .await
            .unwrap();
        assert_eq!(tx.created_by, operator, "the flow's actor, not a fresh one");
        assert_ne!(
            tx.created_by, acc.created_by,
            "the two actors are distinguishable"
        );
        let stored = s.transactions.find_by_id(tx.id).await.unwrap().unwrap();
        assert_eq!(stored.created_by, operator);
        assert_eq!(stored.updated_by, None);
    }
}
