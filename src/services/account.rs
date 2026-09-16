use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{Account, AccountDetail, AccountWithBalance, Transaction};
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

    pub async fn create(&self, name: &str) -> AppResult<Account> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(AppError::Validation("account name cannot be empty".into()));
        }
        if trimmed.len() > 64 {
            return Err(AppError::Validation(
                "account name must be <= 64 chars".into(),
            ));
        }
        self.accounts.create(trimmed).await
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
            created_at: acc.created_at,
            transactions: txs,
        })
    }

    pub async fn total_balance(&self) -> AppResult<Decimal> {
        self.accounts.total_balance().await
    }
}
