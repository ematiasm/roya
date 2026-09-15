use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{Transaction, TransactionFilter, TransactionKind};
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

    pub async fn create(
        &self,
        account_id: i64,
        kind: TransactionKind,
        amount: Decimal,
        description: Option<String>,
        date: NaiveDate,
    ) -> AppResult<Transaction> {
        self.validate_amount(amount)?;
        self.validate_date(date)?;

        if !self.accounts.exists(account_id).await? {
            return Err(AppError::NotFound(format!("account {account_id} not found")));
        }

        // If Expense and negative not allowed, check resulting balance
        if !self.allow_negative && kind == TransactionKind::Expense {
            let current = self.transactions.balance_for_account(account_id).await?;
            let next = current - amount;
            if next < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "insufficient funds: balance {current} would become {next}"
                )));
            }
        }

        let desc = description.unwrap_or_default();
        if desc.len() > 256 {
            return Err(AppError::Validation(
                "description must be <= 256 chars".into(),
            ));
        }

        self.transactions
            .create(account_id, kind, amount, &desc, date)
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
            let projected = current_balance - orig_signed + new_signed;
            if projected < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "update would cause negative balance: projected {projected}"
                )));
            }
        }

        self.transactions.update(&existing).await
    }

    pub async fn delete(&self, id: i64) -> AppResult<()> {
        let tx = self
            .transactions
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("transaction {id} not found")))?;

        if !self.allow_negative && tx.kind == TransactionKind::Income {
            // Removing income could make balance negative
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
