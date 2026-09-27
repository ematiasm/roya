use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    Account, AccountDetail, AccountWithBalance, PriceRefusal, SetMoney, Transaction,
};
use crate::repositories::{AccountRepository, TransactionRepository};

#[derive(Clone)]
pub struct AccountService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub accounts: A,
    pub transactions: T,
}

impl<A, T> AccountService<A, T>
where
    A: AccountRepository,
    T: TransactionRepository,
{
    pub fn new(accounts: A, transactions: T) -> Self {
        Self {
            accounts,
            transactions,
        }
    }

    /// `actor` is the audit actor: the acting user's id from the request's
    /// `Principal` (M5 Phase B). The account row records it as `created_by`.
    pub async fn create(&self, actor: i64, name: &str) -> AppResult<Account> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(AppError::Validation("account name cannot be empty".into()));
        }
        if trimmed.len() > 64 {
            return Err(AppError::Validation(
                "account name must be <= 64 chars".into(),
            ));
        }
        self.accounts.create(actor, trimmed).await
    }

    /// 404 when the account does not exist; used by the payment-method routes.
    pub async fn require_exists(&self, id: i64) -> AppResult<()> {
        if !self.accounts.exists(id).await? {
            return Err(AppError::NotFound(format!("account {id} not found")));
        }
        Ok(())
    }

    pub async fn list_with_balances(&self) -> AppResult<Vec<AccountWithBalance>> {
        self.accounts.list_with_balances().await
    }

    /// The accounts with their balances, plus the balance of ALL of them.
    ///
    /// The total is a SET sum over the same rows, so it is refused whenever any
    /// account is: a headline that carried while one of its accounts states the
    /// rule would be the operator adding a figure the page just told them does not
    /// exist. It is also the same figure as before for an ordinary set — a sum of
    /// per-account balances IS the sum of every transaction, and it now costs one
    /// read instead of one per account plus one for the total.
    pub async fn list_with_balances_and_total(
        &self,
    ) -> AppResult<(Vec<AccountWithBalance>, SetMoney)> {
        let accounts = self.accounts.list_with_balances().await?;
        let mut sum = Decimal::ZERO;
        let mut refusal: Option<PriceRefusal> = None;
        for account in &accounts {
            match account.balance.amount {
                Some(amount) => {
                    sum = sum
                        .checked_add(amount)
                        .ok_or(AppError::PriceRefused(PriceRefusal::AggregateTooLarge))?
                }
                None => refusal = account.balance.refusal,
            }
        }
        Ok((
            accounts,
            refusal.map_or_else(|| SetMoney::amount(sum), SetMoney::refused),
        ))
    }

    pub async fn list(&self) -> AppResult<Vec<Account>> {
        self.accounts.list().await
    }

    pub async fn get_detail(&self, id: i64) -> AppResult<AccountDetail> {
        let acc = self
            .accounts
            .find_with_balance(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("account {id} not found")))?;

        let txs: Vec<Transaction> = self.transactions.list_by_account(id).await?;

        Ok(AccountDetail {
            id: acc.id,
            name: acc.name,
            balance: acc.balance,
            created_by: acc.created_by,
            updated_by: acc.updated_by,
            created_at: acc.created_at,
            transactions: txs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::{SqliteAccountRepository, SqliteTransactionRepository};
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

    /// AC18 on the account surface: the account records the acting user's id
    /// (`created_by`, NOT NULL) and no editor until one edits it.
    #[tokio::test]
    async fn ac18_an_account_records_its_creator() {
        let pool = test_pool().await;
        let s = AccountService::new(
            SqliteAccountRepository::new(pool.clone()),
            SqliteTransactionRepository::new(pool.clone()),
        );
        let alice = test_support::seed_audit_user(&pool, "audit-alice", "Alice")
            .await
            .unwrap();

        let acc = s.create(alice, "Caja").await.unwrap();
        assert_eq!(acc.created_by, alice, "the account records its creator");
        assert_eq!(acc.updated_by, None);
    }
}
