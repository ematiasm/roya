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

    /// Give a well-known account its defaults (idempotent). An unassigned
    /// same-named method is assigned; when the name only exists on another
    /// account a duplicate row is created (UNIQUE(account_id, name) permits
    /// it); a missing name is created fresh. Never steals: no existing
    /// ownership is ever changed. `actor` is the audit actor of the originating
    /// request (M5 Phase B): rows this call creates or reassigns carry it.
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
            if let Some(unassigned) = self.methods.find_unassigned_by_name(method_name).await? {
                self.methods
                    .set_method_account(actor, unassigned.id, Some(account_id))
                    .await?;
            } else {
                self.methods
                    .create_in_account(actor, method_name, account_id)
                    .await?;
            }
        }
        Ok(())
    }

    /// The account's own methods (ownership implies usability; there is no
    /// separate allowlist anymore).
    pub async fn catalog_for_account(&self, account_id: i64) -> AppResult<Vec<PaymentMethod>> {
        self.methods.list_by_account(account_id).await
    }

    /// Methods no account owns yet: assignable, but unusable for payments.
    pub async fn unassigned(&self) -> AppResult<Vec<PaymentMethod>> {
        self.methods.list_unassigned().await
    }

    /// Account ids with no owned methods; drives the self-diagnosing warning.
    pub async fn accounts_without_methods(&self) -> AppResult<Vec<i64>> {
        self.methods.list_accounts_without_methods().await
    }

    /// Derive the owning account of a method for payments: 404 unknown method,
    /// 400 inactive or unassigned (with an actionable message naming the fix).
    /// No stock/finance side effects.
    pub async fn resolve_account(&self, method_id: i64) -> AppResult<i64> {
        resolve_account_for(&self.methods, method_id).await
    }

    /// Set the account's method set to exactly `method_ids` (no merge).
    /// Unknown ids 404 before any write, so a rejected request never clears the
    /// existing set. Methods assigned to ANOTHER account are a 400 (never
    /// stolen silently); unassign those here first or duplicate the name.
    /// An empty list unassigns everything (the UI warns on method-less
    /// accounts). Returns the updated catalog. `actor` is the audit actor of
    /// the request (M5 Phase B): every reassignment records it.
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
            match method.account_id {
                Some(owner) if owner != account_id => {
                    return Err(AppError::Validation(format!(
                        "method {} belongs to account {owner}; unassign it there first or create a {} method in this account instead of reusing it",
                        method.name, method.name
                    )));
                }
                None => to_assign.push(*id),
                Some(_) => {}
            }
        }
        let current = self.methods.list_by_account(account_id).await?;
        for owned in &current {
            if !seen.contains(&owned.id) {
                self.methods
                    .set_method_account(actor, owned.id, None)
                    .await?;
            }
        }
        for id in to_assign {
            self.methods
                .set_method_account(actor, id, Some(account_id))
                .await?;
        }
        self.catalog_for_account(account_id).await
    }

    /// Attach one existing method to a new account at creation time: an
    /// unassigned method is assigned, a method owned elsewhere is duplicated by
    /// name (deduped: an existing same-named row in this account wins), a row
    /// already here is kept. Unknown ids 404. `actor` is the audit actor of the
    /// request (M5 Phase B).
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
        match method.account_id {
            Some(owner) if owner == account_id => Ok(method),
            Some(_) => {
                if let Some(existing) = self
                    .methods
                    .find_method_in_account(account_id, &method.name)
                    .await?
                {
                    Ok(existing)
                } else {
                    self.methods
                        .create_in_account(actor, &method.name, account_id)
                        .await
                }
            }
            None => {
                if let Some(existing) = self
                    .methods
                    .find_method_in_account(account_id, &method.name)
                    .await?
                {
                    self.methods
                        .set_method_account(actor, method.id, None)
                        .await?;
                    Ok(existing)
                } else {
                    self.methods
                        .set_method_account(actor, method.id, Some(account_id))
                        .await?;
                    self.methods
                        .find_method(method.id)
                        .await?
                        .ok_or_else(|| AppError::Internal("method vanished after assign".into()))
                }
            }
        }
    }
}

/// Shared ownership resolution for services that hold the repository directly
/// (e.g. `SalesService`) instead of a `PaymentMethodService`.
pub async fn resolve_account_for<PM>(repo: &PM, method_id: i64) -> AppResult<i64>
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
    method.account_id.ok_or_else(|| {
        AppError::Validation(format!(
            "method {} is not assigned to any account; assign it to an account before collecting or paying with it",
            method.name
        ))
    })
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

    async fn seed_account(pool: &sqlx::SqlitePool, name: &str) -> i64 {
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

    #[tokio::test]
    async fn seeded_methods_start_unassigned_without_other() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool);
        let svc = PaymentMethodService::new(repo);
        let methods = svc.list().await.unwrap();
        let names: Vec<String> = methods.iter().map(|m| m.name.clone()).collect();
        let seeded: Vec<String> =
            PaymentMethodService::<SqlitePaymentMethodRepository>::seeded_names()
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(names, seeded, "seed order is the canonical order");
        assert!(!names.iter().any(|n| n == "Other"));
        assert!(methods.iter().all(|m| m.account_id.is_none()));
    }

    #[tokio::test]
    async fn sensible_defaults_for_caja_banco_mp() {
        let pool = test_pool().await;
        let repo = SqlitePaymentMethodRepository::new(pool.clone());
        let svc = PaymentMethodService::new(repo);
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
        let caja_id: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = 'Caja'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let banco_id: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = 'Banco'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mp_id: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = 'MP'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(names(&pool, caja_id.0).await, vec!["Cash"]);
        assert_eq!(
            names(&pool, banco_id.0).await,
            vec!["CreditCard", "Debit", "Transfer"]
        );
        assert_eq!(names(&pool, mp_id.0).await, vec!["QR", "Transfer"]);
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

    #[tokio::test]
    async fn resolve_account_derives_the_owner() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Caja").await;
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            acc,
            "Caja",
        )
        .await
        .unwrap();
        let cash = method_id(&svc, "Cash").await;
        assert_eq!(svc.resolve_account(cash).await.unwrap(), acc);
    }

    #[tokio::test]
    async fn resolve_account_rejects_unknown_inactive_and_unassigned() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let err = svc.resolve_account(999_999).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");

        // Unassigned is a 400 naming the fix.
        let cash = method_id(&svc, "Cash").await;
        let err = svc.resolve_account(cash).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains("not assigned to any account"),
            "got {err}"
        );

        // Inactive is a 400 too.
        let acc = seed_account(&pool, "Caja").await;
        svc.methods
            .set_method_account(
                test_support::audit_actor_id(&pool).await.unwrap(),
                cash,
                Some(acc),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE payment_methods SET is_active = 0 WHERE id = ?")
            .bind(cash)
            .execute(&pool)
            .await
            .unwrap();
        let err = svc.resolve_account(cash).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("inactive"), "got {err}");
    }

    #[tokio::test]
    async fn replace_account_methods_assigns_and_unassigns() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "A").await;
        let cash = method_id(&svc, "Cash").await;
        let transfer = method_id(&svc, "Transfer").await;

        let catalog = svc
            .replace_account_methods(
                test_support::audit_actor_id(&pool).await.unwrap(),
                acc,
                &[cash],
            )
            .await
            .unwrap();
        assert_eq!(catalog_names(&catalog), vec!["Cash"]);

        let catalog = svc
            .replace_account_methods(
                test_support::audit_actor_id(&pool).await.unwrap(),
                acc,
                &[transfer],
            )
            .await
            .unwrap();
        assert_eq!(
            catalog_names(&catalog),
            vec!["Transfer"],
            "replace removes Cash"
        );
        assert_eq!(
            svc.methods
                .find_method(cash)
                .await
                .unwrap()
                .unwrap()
                .account_id,
            None,
            "removed methods are unassigned, not deleted"
        );
    }

    #[tokio::test]
    async fn replace_account_methods_rejects_unknown_without_clearing() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "A").await;
        let cash = method_id(&svc, "Cash").await;
        svc.replace_account_methods(
            test_support::audit_actor_id(&pool).await.unwrap(),
            acc,
            &[cash],
        )
        .await
        .unwrap();

        let err = svc
            .replace_account_methods(
                test_support::audit_actor_id(&pool).await.unwrap(),
                acc,
                &[cash, 999_999],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
        assert_eq!(
            catalog_names(&svc.catalog_for_account(acc).await.unwrap()),
            vec!["Cash"],
            "a rejected replacement must not clear the existing set"
        );
    }

    #[tokio::test]
    async fn replace_account_methods_never_steals_from_another_account() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let a = seed_account(&pool, "A").await;
        let b = seed_account(&pool, "B").await;
        let cash = method_id(&svc, "Cash").await;
        svc.replace_account_methods(
            test_support::audit_actor_id(&pool).await.unwrap(),
            a,
            &[cash],
        )
        .await
        .unwrap();

        let err = svc
            .replace_account_methods(
                test_support::audit_actor_id(&pool).await.unwrap(),
                b,
                &[cash],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("belongs to account"), "got {err}");
        assert_eq!(
            svc.resolve_account(cash).await.unwrap(),
            a,
            "ownership unchanged"
        );
    }

    #[tokio::test]
    async fn replace_account_methods_accepts_empty_and_warns() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "A").await;
        let cash = method_id(&svc, "Cash").await;
        svc.replace_account_methods(
            test_support::audit_actor_id(&pool).await.unwrap(),
            acc,
            &[cash],
        )
        .await
        .unwrap();

        let catalog = svc
            .replace_account_methods(test_support::audit_actor_id(&pool).await.unwrap(), acc, &[])
            .await
            .unwrap();
        assert!(catalog.is_empty());
        assert_eq!(svc.accounts_without_methods().await.unwrap(), vec![acc]);
    }

    #[tokio::test]
    async fn assign_or_duplicate_assigns_free_and_clones_owned() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let a = seed_account(&pool, "A").await;
        let b = seed_account(&pool, "B").await;
        let cash = method_id(&svc, "Cash").await;

        // Free method is assigned.
        let owned = svc
            .assign_or_duplicate(test_support::audit_actor_id(&pool).await.unwrap(), a, cash)
            .await
            .unwrap();
        assert_eq!(owned.account_id, Some(a));

        // Owned elsewhere: B gets a duplicate, A keeps its row.
        let dup = svc
            .assign_or_duplicate(test_support::audit_actor_id(&pool).await.unwrap(), b, cash)
            .await
            .unwrap();
        assert_eq!(dup.account_id, Some(b));
        assert_ne!(dup.id, cash);
        assert_eq!(dup.name, "Cash");
        assert_eq!(svc.resolve_account(cash).await.unwrap(), a);
    }

    #[tokio::test]
    async fn methods_with_accounts_renders_owner_labels() {
        let pool = test_pool().await;
        let svc = PaymentMethodService::new(SqlitePaymentMethodRepository::new(pool.clone()));
        let acc = seed_account(&pool, "Caja").await;
        svc.ensure_defaults_for_account(
            test_support::audit_actor_id(&pool).await.unwrap(),
            acc,
            "Caja",
        )
        .await
        .unwrap();

        let options = svc.methods_with_accounts().await.unwrap();
        assert_eq!(options.len(), 5);
        let cash = options.iter().find(|m| m.name == "Cash").unwrap();
        assert_eq!(cash.account_id, Some(acc));
        assert_eq!(cash.account_name.as_deref(), Some("Caja"));
        let qr = options.iter().find(|m| m.name == "QR").unwrap();
        assert_eq!(qr.account_id, None);
        assert_eq!(qr.account_name, None);
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

        // Bob unassigns it (the reassignment is a mutation the audit records).
        svc.methods
            .set_method_account(bob, created.id, None)
            .await
            .unwrap();
        let edited = svc.methods.find_method(created.id).await.unwrap().unwrap();
        assert_eq!(edited.created_by, alice, "the creator attribution survives");
        assert_eq!(
            edited.updated_by,
            Some(bob),
            "the reassignment records the editor"
        );
    }
}
