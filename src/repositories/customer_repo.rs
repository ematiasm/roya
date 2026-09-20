use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{Customer, NewCustomer, UpdateCustomer};

fn parse_decimal(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
}

fn row_to_customer(row: sqlx::sqlite::SqliteRow) -> Customer {
    let walkin: i64 = row.get("is_walkin");
    let active: i64 = row.get("is_active");
    let credit: Option<String> = row.get("credit_limit");
    Customer {
        id: row.get("id"),
        name: row.get("name"),
        phone: row.get("phone"),
        address: row.get("address"),
        tax_id: row.get("tax_id"),
        notes: row.get("notes"),
        is_walkin: walkin == 1,
        is_active: active == 1,
        credit_limit: credit.as_deref().map(parse_decimal),
        payment_days: row.get("payment_days"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_db_err(e: sqlx::Error) -> AppError {
    let s = e.to_string();
    if s.contains("UNIQUE constraint failed") {
        if s.contains("is_walkin") || s.contains("one_walkin") {
            AppError::Conflict("only one walk-in customer is allowed".into())
        } else {
            AppError::Conflict("customer already exists".into())
        }
    } else if s.contains("FOREIGN KEY constraint failed") {
        AppError::Validation("invalid reference for customer".into())
    } else {
        AppError::Database(e)
    }
}

#[async_trait]
pub trait CustomerRepository: Send + Sync {
    async fn create(&self, actor: i64, input: &NewCustomer) -> AppResult<Customer>;
    async fn find_by_id(&self, id: i64) -> AppResult<Option<Customer>>;
    /// Every customer whose name matches exactly, for the duplicate warning.
    async fn find_by_name(&self, name: &str) -> AppResult<Vec<Customer>>;
    /// The seeded cash default, when it exists.
    async fn find_walkin(&self) -> AppResult<Option<Customer>>;
    /// `only_active = false` returns deactivated customers too.
    async fn list(&self, only_active: bool) -> AppResult<Vec<Customer>>;
    /// Update the editable fields (service guarantees cleaned values).
    async fn update(&self, id: i64, actor: i64, patch: &UpdateCustomer) -> AppResult<Customer>;
    async fn set_active(&self, id: i64, actor: i64, active: bool) -> AppResult<Customer>;
    /// DELETE is RESTRICTed by sales once `sales.customer_id` exists.
    async fn delete(&self, id: i64) -> AppResult<bool>;
    async fn exists(&self, id: i64) -> AppResult<bool>;
}

#[derive(Clone)]
pub struct SqliteCustomerRepository {
    pub pool: SqlitePool,
}

impl SqliteCustomerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CustomerRepository for SqliteCustomerRepository {
    async fn create(&self, actor: i64, input: &NewCustomer) -> AppResult<Customer> {
        let row = sqlx::query(
            r#"INSERT INTO customers
                   (name, phone, address, tax_id, notes, is_walkin, credit_limit, payment_days, created_by)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, payment_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(&input.name)
        .bind(input.phone.clone())
        .bind(input.address.clone())
        .bind(input.tax_id.clone())
        .bind(input.notes.clone())
        .bind(if input.is_walkin { 1i64 } else { 0i64 })
        .bind(input.credit_limit.map(|d| d.to_string()))
        .bind(input.payment_days)
        .bind(actor)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_customer(row))
    }

    async fn find_by_id(&self, id: i64) -> AppResult<Option<Customer>> {
        let row = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, payment_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_customer))
    }

    async fn find_by_name(&self, name: &str) -> AppResult<Vec<Customer>> {
        let rows = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, payment_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE name = ? ORDER BY id"#,
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_customer).collect())
    }

    async fn find_walkin(&self) -> AppResult<Option<Customer>> {
        let row = sqlx::query(
            r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                      credit_limit, payment_days, created_by, updated_by, created_at, updated_at
               FROM customers WHERE is_walkin = 1 ORDER BY id LIMIT 1"#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_customer))
    }

    async fn list(&self, only_active: bool) -> AppResult<Vec<Customer>> {
        let rows = if only_active {
            sqlx::query(
                r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                          credit_limit, payment_days, created_by, updated_by, created_at, updated_at
                   FROM customers WHERE is_active = 1 ORDER BY id"#,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"SELECT id, name, phone, address, tax_id, notes, is_walkin, is_active,
                          credit_limit, payment_days, created_by, updated_by, created_at, updated_at
                   FROM customers ORDER BY id"#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(row_to_customer).collect())
    }

    async fn update(&self, id: i64, actor: i64, patch: &UpdateCustomer) -> AppResult<Customer> {
        let existing = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("customer {id} not found")))?;

        let name = patch.name.clone().unwrap_or(existing.name);
        let phone = match &patch.phone {
            Some(inner) => inner.clone(),
            None => existing.phone,
        };
        let address = match &patch.address {
            Some(inner) => inner.clone(),
            None => existing.address,
        };
        let tax_id = match &patch.tax_id {
            Some(inner) => inner.clone(),
            None => existing.tax_id,
        };
        let notes = match &patch.notes {
            Some(inner) => inner.clone(),
            None => existing.notes,
        };
        let credit_limit = match &patch.credit_limit {
            Some(inner) => inner.map(|d| d.to_string()),
            None => existing.credit_limit.map(|d| d.to_string()),
        };
        let payment_days = match patch.payment_days {
            Some(inner) => inner,
            None => existing.payment_days,
        };

        let row = sqlx::query(
            r#"UPDATE customers
               SET name = ?, phone = ?, address = ?, tax_id = ?, notes = ?,
                   credit_limit = ?, payment_days = ?,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, payment_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(name)
        .bind(phone)
        .bind(address)
        .bind(tax_id)
        .bind(notes)
        .bind(credit_limit)
        .bind(payment_days)
        .bind(actor)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(row_to_customer(row))
    }

    async fn set_active(&self, id: i64, actor: i64, active: bool) -> AppResult<Customer> {
        let row = sqlx::query(
            r#"UPDATE customers
               SET is_active = ?,
                   updated_by = ?,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ?
               RETURNING id, name, phone, address, tax_id, notes, is_walkin, is_active,
                         credit_limit, payment_days, created_by, updated_by, created_at, updated_at"#,
        )
        .bind(if active { 1i64 } else { 0i64 })
        .bind(actor)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;
        row.map(row_to_customer)
            .ok_or_else(|| AppError::NotFound(format!("customer {id} not found")))
    }

    async fn delete(&self, id: i64) -> AppResult<bool> {
        let res = sqlx::query(r#"DELETE FROM customers WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("FOREIGN KEY constraint failed") {
                    AppError::Validation(
                        "cannot delete customer with sales; deactivate it instead".into(),
                    )
                } else {
                    AppError::Database(e)
                }
            })?;
        Ok(res.rows_affected() > 0)
    }

    async fn exists(&self, id: i64) -> AppResult<bool> {
        let row: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM customers WHERE id = ?"#)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }
}
