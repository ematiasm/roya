// M0 account-owned payment methods (finance-owned).
// Methods seeded Cash/Transfer/Debit/CreditCard/QR (no Other), unassigned until
// an account owns them. Each method belongs to at most one account:
// UNIQUE(account_id, name) lets two accounts each own a same-named method as
// separate rows. Sensible defaults: Caja->[Cash],
// Banco->[Transfer,Debit,CreditCard], MP->[QR,Transfer] (Transfer duplicates by
// design). Payments name only the method; the account is derived from ownership,
// so an invalid combination is impossible by construction.
use crate::error::{AppError, AppResult};
use crate::models::{PaymentMethod, PaymentMethodWithAccount};
use crate::repositories::PaymentMethodRepository;
use std::collections::HashSet;

#[derive(Clone)]
pub struct PaymentMethodService<PM>
where
    PM: PaymentMethodRepository,
{
    pub methods: PM,
}

impl<PM> PaymentMethodService<PM>
where
    PM: PaymentMethodRepository,
{
    pub fn new(methods: PM) -> Self {
        Self { methods }
    }

    pub async fn list(&self) -> AppResult<Vec<PaymentMethod>> {
        self.methods.list_methods().await
    }

    /// Every method with its owning account resolved, for method-only selects
    /// (`"Name — AccountName"`).
    pub async fn methods_with_accounts(&self) -> AppResult<Vec<PaymentMethodWithAccount>> {
        self.methods.list_with_accounts().await
    }

    /// Canonical method names in seed order.
    pub fn seeded_names() -> Vec<&'static str> {
        vec!["Cash", "Transfer", "Debit", "CreditCard", "QR"]
    }

    /// Sensible default method names for a well-known account name.
    /// Matching is exact (Caja/Banco/MP); unknown names get no defaults.
    pub fn default_method_names_for_account_name(name: &str) -> Vec<&'static str> {
        match name.trim() {
            "Caja" => vec!["Cash"],
            "Banco" => vec!["Transfer", "Debit", "CreditCard"],
            "MP" => vec!["QR", "Transfer"],
            _ => vec![],
        }
    }

    /// Give a well-known account its defaults (idempotent). When the name only
    /// exists on another account a duplicate row is created (UNIQUE(account_id,
    /// name) permits it); a missing name is created fresh. Never steals: no
    /// existing ownership is ever changed. `actor` is the audit actor of the
    /// originating request (M5 Phase B): rows this call creates carry it.
    ///
    /// There is no "adopt the unassigned method of this name" step any more:
    /// migration 45 made an unowned method unrepresentable, so the only way a
    /// default arrives is created-in-this-account or already-owned-here.
    pub async fn ensure_defaults_for_account(
        &self,
        actor: i64,
        account_id: i64,
        account_name: &str,
    ) -> AppResult<()> {
        for method_name in Self::default_method_names_for_account_name(account_name) {
            if self
                .methods
                .find_method_in_account(account_id, method_name)
                .await?
                .is_some()
            {
                continue;
            }
            self.methods
                .create_in_account(actor, method_name, account_id)
                .await?;
        }
        Ok(())
    }

    /// The account's own methods (ownership implies usability; there is no
    /// separate allowlist anymore).
    pub async fn catalog_for_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>> {
        self.methods.list_by_account(account_id).await
    }

    /// Account ids with no ACTIVE method; drives the self-diagnosing warning.
    /// "Active", not "a row exists": see the repository method's own comment.
    pub async fn accounts_without_methods(&self) -> AppResult<Vec<i64>> {
        self.methods.list_accounts_without_methods().await
    }

    /// Derive the owning account of a method for payments: 404 unknown method,
    /// 400 inactive or unassigned (with an actionable message naming the fix).
    /// No stock/finance side effects.
    pub async fn resolve_account(
        &self,
        method_id: i64,
        stated_account_id: Option<i64>,
    ) -> AppResult<i64> {
        resolve_account_for(&self.methods, method_id, stated_account_id).await
    }

    /// Set the account's method set to exactly `method_ids` (no merge).
    /// Unknown ids 404 before any write, so a rejected request never clears the
    /// existing set. Methods assigned to ANOTHER account are a 400 (never stolen
    /// silently); duplicate the name in this account instead. A method that was
    /// owned here and is no longer ticked is DEACTIVATED, never unowned —
    /// migration 45 removed the unowned state, and a method that left an account
    /// while still naming it in `account_id` is exactly the history a refund
    /// reads back. An empty list therefore leaves the account with no selectable
    /// method rather than with none owned (the UI warns either way). Returns the
    /// updated catalog. `actor` is the audit actor of the request (M5 Phase B):
    /// every deactivation and assignment records it.
    pub async fn replace_account_methods(
        &self,
        actor: i64,
        account_id: i64,
        method_ids: &[i64],
    ) -> AppResult<Vec<PaymentMethod>> {
        let mut seen = HashSet::new();
        let mut unique = Vec::with_capacity(method_ids.len());
        for id in method_ids {
            if seen.insert(*id) {
                unique.push(*id);
            }
        }
        let mut to_assign = Vec::with_capacity(unique.len());
        for id in &unique {
            let method = self
                .methods
                .find_method(*id)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("method {id} not found")))?;
            if method.account_id != account_id {
                return Err(AppError::Validation(format!(
                    "method {} belongs to account {}; create a {} method in this account instead of reusing it",
                    method.name, method.account_id, method.name
                )));
            }
            to_assign.push(*id);
        }
        // The catalog INCLUDES inactive methods on purpose: the editor has to
        // render an unticked box for a deactivated method so ticking it again
        // reactivates it (`set_active` below) instead of creating a duplicate
        // name the UNIQUE(account_id, name) would refuse.
        let current = self.methods.list_by_account(account_id).await?;
        for owned in &current {
            let wanted = seen.contains(&owned.id);
            if owned.is_active != wanted {
                self.methods.set_active(actor, owned.id, wanted).await?;
            }
        }
        for id in to_assign {
            let already_here = current.iter().any(|m| m.id == id);
            if !already_here {
                self.methods
                    .set_method_account(actor, id, account_id)
                    .await?;
            }
        }
        self.catalog_for_account(account_id).await
    }

    /// Attach one existing method to a new account at creation time: a method
    /// owned here is kept, a method owned elsewhere is duplicated by name
    /// (deduped: an existing same-named row in this account wins). Unknown ids
    /// 404. `actor` is the audit actor of the request (M5 Phase B).
    pub async fn assign_or_duplicate(
        &self,
        actor: i64,
        account_id: i64,
        method_id: i64,
    ) -> AppResult<PaymentMethod> {
        let method = self
            .methods
            .find_method(method_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("method {method_id} not found")))?;
        if method.account_id == account_id {
            if !method.is_active {
                self.methods.set_active(actor, method.id, true).await?;
                // Re-read rather than patch the copy: the caller acts on this
                // row, and a stale `is_active` would make a reactivated method
                // look deactivated to the screen that just ticked it.
                return self.methods.find_method(method.id).await?.ok_or_else(|| {
                    AppError::Internal("method vanished after reactivation".into())
                });
            }
            return Ok(method);
        }
        if let Some(existing) = self
            .methods
            .find_method_in_account(account_id, &method.name)
            .await?
        {
            if !existing.is_active {
                self.methods.set_active(actor, existing.id, true).await?;
                return self.methods.find_method(existing.id).await?.ok_or_else(|| {
                    AppError::Internal("method vanished after reactivation".into())
                });
            }
            return Ok(existing);
        }
        self.methods
            .create_in_account(actor, &method.name, account_id)
            .await
    }
}

/// Shared ownership resolution for services that hold the repository directly
/// (e.g. `SalesService`) instead of a `PaymentMethodService`.
///
/// Three refusals: an unknown method is a 404, an inactive method is a 400 naming
/// the fix, and naming a method in an account that does not own it is a 400.
///
/// There is no "unassigned" branch any more — migration 45 made `account_id` NOT
/// NULL, so ownership is the one thing every method has.
///
/// `stated_account_id` is the account the CALLER says the money belongs to. When
/// it is `Some`, it must be the method's owner: migration 44's trigger enforces
/// exactly that on the payment row, and today it enforces it as a raw SQLite
/// abort — a 500 with driver text for what is an operator's mistake. Checking it
/// here first turns that into the 400 it always was, with a message naming the
/// pair. The two checks cannot disagree about WHAT is legal, because the trigger
/// is asked the same question: `method.account_id = stated`.
pub async fn resolve_account_for<PM>(
    repo: &PM,
    method_id: i64,
    stated_account_id: Option<i64>,
) -> AppResult<i64>
where
    PM: PaymentMethodRepository,
{
    let method = repo
        .find_method(method_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("method {method_id} not found")))?;
    if !method.is_active {
        return Err(AppError::Validation(format!(
            "method {} is inactive",
            method.name
        )));
    }
    if let Some(stated) = stated_account_id {
        if stated != method.account_id {
            return Err(AppError::Validation(format!(
                "the payment method does not belong to the named account"
            )));
        }
    }
    Ok(method.account_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::SqlitePaymentMethodRepository;
    use crate::security::test_support;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

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

    /// Find-or-create by name, because migration 45 SEEDS `Caja`: a fixture that
    /// inserts the name blindly aborts on `UNIQUE(accounts.name)` and, worse,
    /// would be asking for a second `Caja` while the seeded one is the account
    /// the seed pair actually points at.
    async fn seed_account(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        if let Some(existing) =
            sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE name = ?")
                .bind(name)
                .fetch_optional(pool)
                .await
                .unwrap()
        {
            return existing;
        }
        // Fixture rows are system-planted data, so the actor is the migration's
        // sentinel account, resolved through the shared test support (the AC20
        // boundary scan forbids naming identity tables here).
        let actor = test_support::audit_actor_id(pool).await.unwrap();
        let row: (i64,) =
            sqlx::query_as("INSERT INTO accounts (name, created_by) VALUES (?, ?) RETURNING id")
                .bind(name)
                .bind(actor)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0
    }

    async fn method_id(
        svc: &PaymentMethodService<SqlitePaymentMethodRepository>,
        name: &str,
    ) -> i64 {
        svc.methods
            .find_method_by_name(name)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    fn catalog_names(catalog: &[PaymentMethod]) -> Vec<String> {
        let mut names: Vec<String> = catalog.iter().map(|m| m.name.clone()).collect();
        names.sort();
        names
    }

    /// Migration 45's own end state, asserted against a REAL migrated pool:
    /// the five names of `seeded_names()` were what migration 12 wrote and every
    /// one was unowned, which the seed cannot hold any more. `Cash` — the head of
    /// that order — is the one the seed adopts; the four history-less leftovers
    /// are deleted, because their only alternative was being adopted by an
    /// account nobody chose for them.
    #[tokio::test]
    async fn migration_45_leaves_only_cash_owned_by_the_seeded_caja() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo);
        let methods = svc.list().await.unwrap();
        let names: Vec<String> = methods.iter().map(|m| m.name.clone()).collect();
        assert_eq!(
            names,
            vec!["Cash"],
            "migration 45 keeps the method its seed owns and deletes the history-less leftovers"
        );
        assert!(!names.iter().any(|n| n == "Other"));
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            methods.iter().all(|m| m.account_id == caja),
            "every surviving method is owned, and by the seeded account"
        );
        assert_eq!(
            PaymentMethodService::<SqlitePaymentMethodRepository>::seeded_names()[0],
            "Cash",
            "the seed adopts the head of migration 12's order, not an arbitrary member"
        );
    }

    #[tokio::test]
    async fn sensible_defaults_for_caja_banco_mp() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo);
        // Caja already exists — migration 45 seeded it WITH its Cash — so this
        // is the idempotent case, and the interesting assertion below is that
        // running the defaults again does not try to create a second Cash.
        for name in ["Caja", "Banco", "MP"] {
            let id = seed_account(&pool, name).await;
            svc.ensure_defaults_for_account(
                test_support::audit_actor_id(&pool).await.unwrap(),
                id,
                name,
            )
            .await
            .unwrap();
        }
        async fn names(pool: &sqlx::SqlitePool, acc: i64) -> Vec<String> {
            let rows = SqlitePaymentMethodRepository::new(pool.clone())
                .list_by_account(acc)
                .await
                .unwrap();
            catalog_names(&rows)
        }
        let caja_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let banco_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Banco'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mp_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'MP'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(names(&pool, caja_id).await, vec!["Cash"]);
        assert_eq!(
            names(&pool, banco_id).await,
            vec!["CreditCard", "Debit", "Transfer"]
        );
        assert_eq!(names(&pool, mp_id).await, vec!["QR", "Transfer"]);
        // The two shared names exist once per account, not once globally.
        let transfers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM payment_methods WHERE name = 'Transfer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(transfers, 2, "one Transfer per account that wants it");
    }

    #[tokio::test]
    async fn shared_default_name_duplicates_instead_of_being_stolen() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let banco = seed_account(&pool, "Banco").await;
        let mp = seed_account(&pool, "MP").await;
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            banco,
            "Banco",
        )
        .await
        .unwrap();
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            mp,
            "MP",
        )
        .await
        .unwrap();

        let transfers: Vec<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT id, account_id FROM payment_methods WHERE name = 'Transfer' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            transfers.len(),
            2,
            "Transfer duplicates across accounts: {transfers:?}"
        );
        assert_eq!(transfers[0].1, Some(banco));
        assert_eq!(transfers[1].1, Some(mp));
        assert_ne!(transfers[0].0, transfers[1].0);

        // Idempotent: a second run assigns nothing new.
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            banco,
            "Banco",
        )
        .await
        .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM payment_methods")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 5 + 1, "only the one Transfer duplicate exists");
    }

    /// The seeded pair resolves with no configuration step: this is the promise
    /// T6 exists to keep, so it is asserted against the migration's own rows
    /// rather than against a fixture that plants what the migration should have.
    #[tokio::test]
    async fn resolve_account_derives_the_owner() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cash = method_id(&svc, "Cash").await;
        assert_eq!(svc.resolve_account(cash, None).await.unwrap(), caja);

        // A second account's own defaults resolve to IT, so the derivation is
        // per method and not "whatever account happens to exist".
        let banco = seed_account(&pool, "Banco").await;
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            banco,
            "Banco",
        )
        .await
        .unwrap();
        let transfer = svc
            .methods
            .find_method_in_account(banco, "Transfer")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(svc.resolve_account(transfer.id, None).await.unwrap(), banco);
    }

    #[tokio::test]
    async fn resolve_account_rejects_unknown_and_inactive() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let err = svc.resolve_account(999_999, None).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        let cash = method_id(&svc, "Cash").await;
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Inactive is a 400, through the same door the account editor uses.
        svc.methods
            .set_active(
                test_support::audit_actor_id(&pool).await.unwrap(),
                cash,
                false,
            )
            .await
            .unwrap();
        let err = svc.resolve_account(cash, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("inactive"), "got {err}");

        // Reactivating restores it: the refusal is a state, not a deletion.
        svc.methods
            .set_active(
                test_support::audit_actor_id(&pool).await.unwrap(),
                cash,
                true,
            )
            .await
            .unwrap();
        assert_eq!(svc.resolve_account(cash, None).await.unwrap(), caja);
    }

    /// The editor round trip, through the same entry point the account screen
    /// uses: tick, untick, tick again. The claim is that the SECOND tick
    /// reactivates the row that is already there instead of creating a duplicate
    /// — `UNIQUE(account_id, name)` is the trap, and a "create the missing
    /// default" reflex would hit it. `t6_unticking_deactivates_the_method_it_does_not_unassign_it`
    /// pins the same rule for the empty list; this one pins the id stability.
    #[tokio::test]
    async fn replace_account_methods_round_trips_a_name_without_duplicating_it() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cash = method_id(&svc, "Cash").await;
        // A second method of this account, created the way the UI creates one.
        let wallet = svc
            .methods
            .create_in_account(actor, "Wallet", caja)
            .await
            .unwrap();

        let both = svc
            .replace_account_methods(actor, caja, &[cash, wallet.id])
            .await
            .unwrap();
        assert_eq!(catalog_names(&both), vec!["Cash", "Wallet"]);

        // Untick Wallet: deactivated, still owned, still rendered.
        let one = svc
            .replace_account_methods(actor, caja, &[cash])
            .await
            .unwrap();
        let row = one
            .iter()
            .find(|m| m.id == wallet.id)
            .expect("the deactivated method stays in the catalog so it can be ticked again");
        assert!(!row.is_active, "unticking deactivates");
        assert_eq!(row.account_id, caja, "and it keeps its owner");

        // Tick it again: the SAME row comes back active. A second row for the
        // name would violate UNIQUE(account_id, name), so this is the assertion
        // that the editor reactivates rather than re-creates.
        let both_again = svc
            .replace_account_methods(actor, caja, &[cash, wallet.id])
            .await
            .unwrap();
        let restored = both_again
            .iter()
            .find(|m| m.id == wallet.id)
            .expect("the reactivated row is the row that was deactivated");
        assert!(restored.is_active, "ticking again reactivates");
        assert_eq!(catalog_names(&both_again), vec!["Cash", "Wallet"]);
        assert_eq!(svc.resolve_account(wallet.id, None).await.unwrap(), caja);
    }

    #[tokio::test]
    async fn replace_account_methods_rejects_unknown_without_clearing() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        // A LOCAL account with its OWN method: the seeded Cash belongs to Caja,
        // and a fixture that reused it here would be exercising the
        // belongs-to-another-account refusal instead of the unknown-id one.
        let acc = seed_account(&pool, "A").await;
        let wallet = svc
            .methods
            .create_in_account(actor, "Wallet", acc)
            .await
            .unwrap();
        svc.replace_account_methods(actor, acc, &[wallet.id])
            .await
            .unwrap();

        let err = svc
            .replace_account_methods(actor, acc, &[wallet.id, 999_999])
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        assert_eq!(
            catalog_names(&svc.catalog_for_account(acc).await.unwrap()),
            vec!["Wallet"],
            "a rejected replacement must not clear the existing set"
        );
        assert!(
            svc.methods
                .find_method(wallet.id)
                .await
                .unwrap()
                .unwrap()
                .is_active,
            "and it must not deactivate what it refused to keep"
        );
    }

    #[tokio::test]
    async fn replace_account_methods_never_steals_from_another_account() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let a = seed_account(&pool, "A").await;
        let b = seed_account(&pool, "B").await;
        let wallet = svc
            .methods
            .create_in_account(actor, "Wallet", a)
            .await
            .unwrap();
        svc.replace_account_methods(actor, a, &[wallet.id])
            .await
            .unwrap();

        // B ticks A's row: refused, and the message names the account that owns
        // it — the operator's only move is to create B's own method, because
        // "unassign it there first" no longer exists.
        let err = svc
            .replace_account_methods(actor, b, &[wallet.id])
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains(&format!("belongs to account {a}")),
            "the refusal must name the owner, got {err}"
        );
        assert!(
            err.to_string()
                .contains("create a Wallet method in this account"),
            "and the fix the operator can actually perform, got {err}"
        );
        assert_eq!(
            svc.resolve_account(wallet.id, None).await.unwrap(),
            a,
            "ownership unchanged"
        );
        assert!(
            svc.catalog_for_account(b).await.unwrap().is_empty(),
            "and B owns nothing"
        );
    }

    #[tokio::test]
    async fn replace_account_methods_accepts_empty_and_warns() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let acc = seed_account(&pool, "A").await;
        let wallet = svc
            .methods
            .create_in_account(actor, "Wallet", acc)
            .await
            .unwrap();
        svc.replace_account_methods(actor, acc, &[wallet.id])
            .await
            .unwrap();

        // Unticking everything leaves the account with no SELECTABLE method —
        // which is what the warning is about — while the row stays owned and
        // inactive, so the box can be ticked again.
        let catalog = svc.replace_account_methods(actor, acc, &[]).await.unwrap();
        assert!(
            catalog.iter().all(|m| !m.is_active),
            "nothing selectable is left: {catalog:?}"
        );
        assert!(
            catalog.iter().all(|m| m.account_id == acc),
            "and every row is still owned by this account"
        );
        assert_eq!(svc.accounts_without_methods().await.unwrap(), vec![acc]);
    }

    /// `assign_or_duplicate` is the account-creation door: a method owned here
    /// is kept, a method owned elsewhere is DUPLICATED by name (never stolen),
    /// and the seeded `Cash` is the concrete case the seed created — it belongs
    /// to `Caja`, so the first account that asks for it gets its own row.
    #[tokio::test]
    async fn assign_or_duplicate_keeps_owned_and_clones_a_name_owned_elsewhere() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let a = seed_account(&pool, "A").await;
        let b = seed_account(&pool, "B").await;
        let caja: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cash = method_id(&svc, "Cash").await;

        // The seeded owner asks for its own method: the same row comes back, no
        // duplicate is created.
        let kept = svc
            .assign_or_duplicate(
                test_support::audit_actor_id(&pool).await.unwrap(),
                caja,
                cash,
            )
            .await
            .unwrap();
        assert_eq!(kept.id, cash);
        assert_eq!(kept.account_id, caja);

        // Owned elsewhere: B gets a duplicate by name and Caja keeps its row.
        let dup = svc
            .assign_or_duplicate(test_support::audit_actor_id(&pool).await.unwrap(), b, cash)
            .await
            .unwrap();
        assert_eq!(dup.account_id, b);
        assert_ne!(dup.id, cash);
        assert_eq!(dup.name, "Cash");
        assert_eq!(svc.resolve_account(cash, None).await.unwrap(), caja);

        // And a deactivated method is REACTIVATED rather than duplicated: the
        // name is already here, so a second row would be a UNIQUE violation.
        svc.methods
            .set_active(
                test_support::audit_actor_id(&pool).await.unwrap(),
                dup.id,
                false,
            )
            .await
            .unwrap();
        let again = svc
            .assign_or_duplicate(test_support::audit_actor_id(&pool).await.unwrap(), b, cash)
            .await
            .unwrap();
        assert_eq!(again.id, dup.id, "the same name stays one row");
        assert!(again.is_active, "re-adding a method reactivates it");
        let _ = a;
    }

    #[tokio::test]
    async fn methods_with_accounts_renders_owner_labels() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        // No second account is created: `Caja` is the account the seed already
        // owns the Cash method with, and re-running the defaults over it must be
        // a no-op rather than a duplicate-name conflict.
        let acc = seed_account(&pool, "Caja").await;
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            acc,
            "Caja",
        )
        .await
        .unwrap();

        let options = svc.methods_with_accounts().await.unwrap();
        assert_eq!(options.len(), 1, "the seed leaves one method: {options:?}");
        let cash = options.iter().find(|m| m.name == "Cash").unwrap();
        assert_eq!(cash.account_id, acc);
        assert_eq!(cash.account_name.as_str(), "Caja");
        // With `account_id NOT NULL` and an INNER JOIN there is no owner-less row
        // left to render an "unassigned" label for; every option names one.
        assert!(
            options.iter().all(|m| m.account_name == "Caja"),
            "every option names its owning account: {options:?}"
        );
    }

    /// AC18 on the payment-method surface: a method records who created it,
    /// and a reassignment records the editor without erasing the creator. Two
    /// dedicated users make the two actors distinguishable.
    #[tokio::test]
    async fn ac18_method_creation_and_reassignment_store_the_actors() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo.clone());
        let alice = test_support::seed_audit_user(&pool, "audit-alice", "Alice")
            .await
            .unwrap();
        let bob = test_support::seed_audit_user(&pool, "audit-bob", "Bob")
            .await
            .unwrap();
        let acc = seed_account(&pool, "Caja").await;

        // Alice creates a method owned by the account (through the repo write
        // path the finance routes drive).
        let created = svc
            .methods
            .create_in_account(alice, "Solo", acc)
            .await
            .unwrap();
        assert_eq!(created.created_by, alice);
        assert_eq!(created.updated_by, None);
        let stored = svc.methods.find_method(created.id).await.unwrap().unwrap();
        assert_eq!(stored.created_by, alice);

        // Bob deactivates it (the change is a mutation the audit records).
        svc.methods
            .set_active(bob, created.id, false)
            .await
            .unwrap();
        let edited = svc.methods.find_method(created.id).await.unwrap().unwrap();
        assert_eq!(edited.created_by, alice, "the creator attribution survives");
        assert_eq!(
            edited.updated_by,
            Some(bob),
            "the change records the editor"
        );
    }

    /// T6 of `odd/tasks/payment-method-single-account.md`: the seed is not an
    /// allowance any more. A fresh database opens with ONE account (`Caja`) and
    /// the `Cash` method already owned by it, so the very first collection has a
    /// valid pair to name without any configuration step.
    ///
    /// The guard is the last thing asserted, and it is asserted where it lives:
    /// a `sale_payments` row naming the seeded pair is ACCEPTED by migration 44's
    /// trigger. The end-to-end collection — a Cash sale whose confirm writes the
    /// Income and the payment row together — is
    /// `smoke_tests::cash_sale_confirm_deducts_stock_and_links_exactly_one_income`;
    /// this test does NOT re-do it by hand, because a raw INSERT here would prove
    /// the guard and nothing about the finance path, and calling the whole
    /// service stack would duplicate that smoke test.
    #[tokio::test]
    async fn t6_fresh_database_opens_with_caja_owning_cash_and_can_collect() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());

        // Exactly one account, named Caja.
        let accounts: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM accounts ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            accounts,
            vec![(accounts[0].0, "Caja".to_string())],
            "a fresh install opens with one default account"
        );
        let caja = accounts[0].0;

        // Cash is owned by it and usable; the other seeded methods are gone
        // (they had no history to protect).
        let cash = method_id_from(&pool, "Cash").await;
        let stored = repo.find_method(cash).await.unwrap().unwrap();
        assert_eq!(stored.account_id, caja, "Cash belongs to Caja");
        assert!(stored.is_active, "Cash is selectable");
        let all = repo.list_methods().await.unwrap();
        assert_eq!(
            all.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
            vec!["Cash"],
            "history-less seed leftovers were deleted"
        );

        // And the pair is usable where it matters: the migration-44 guard on the
        // method-choosing table accepts exactly this (account, method) pair.
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let customer: i64 = sqlx::query_scalar(
            "INSERT INTO customers (name, created_by) VALUES ('T6 buyer', ?) RETURNING id",
        )
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        let sale: i64 = sqlx::query_scalar(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by)              VALUES ('Confirmed', 'Cash', ?, 'T6 buyer', '2024-05-01', ?) RETURNING id",
        )
        .bind(customer)
        .bind(actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments (sale_id, account_id, method_id, amount, date, created_by) \
             VALUES (?, ?, ?, '10', '2024-05-01', ?)",
        )
        .bind(sale)
        .bind(caja)
        .bind(cash)
        .bind(actor)
        .execute(&pool)
        .await
        .expect("the seeded pair must satisfy the migration-44 guard on sale_payments");
    }

    async fn method_id_from(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM payment_methods WHERE name = ? ORDER BY id LIMIT 1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// T6: unticking a method in the account editor must DEACTIVATE it — owned
    /// still, shown with the inactive badge, refused on use with the
    /// inactive-method message the operator can act on. No method may return to
    /// an unassigned state, because after migration 45 the schema refuses NULL.
    #[tokio::test]
    async fn t6_unticking_deactivates_the_method_it_does_not_unassign_it() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let cash = method_id_from(&pool, "Cash").await;

        // Untick everything: the method goes inactive, not unowned.
        svc.replace_account_methods(test_support::audit_actor_id(&pool).await.unwrap(), 1, &[])
            .await
            .unwrap();
        let stored = svc.methods.find_method(cash).await.unwrap().unwrap();
        assert_eq!(stored.account_id, 1, "the method must keep its owner");
        assert!(!stored.is_active, "unticking deactivates");

        // Using it is refused with the actionable inactive message.
        let err = svc.resolve_account(cash, None).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("is inactive"),
            "the refusal must name the fix, got {err}"
        );

        // Ticking it again reactivates.
        svc.replace_account_methods(
            test_support::audit_actor_id(&pool).await.unwrap(),
            1,
            &[cash],
        )
        .await
        .unwrap();
        let restored = svc.methods.find_method(cash).await.unwrap().unwrap();
        assert!(restored.is_active, "ticking again reactivates");
    }
}
