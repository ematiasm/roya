// M4 customers (Slice K1). CustomerService owns customer CRUD: trim and bound
// the name and the optional free-text fields, reject negative credit limits and
// payment terms, report duplicate names without blocking, and protect the seeded
// walk-in (never deleted, never deactivated, never created twice). Deleting a
// customer with sales is left to the RESTRICT foreign key that the sales link
// adds later; the repository maps it to Validation. Balance and ageing are
// derived reads and arrive in a later slice, once sales carry `customer_id`.
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{Customer, CustomerCreateResult, NewCustomer, UpdateCustomer};
use crate::repositories::CustomerRepository;

#[derive(Clone)]
pub struct CustomerService<R>
where
    R: CustomerRepository,
{
    pub customers: R,
}

impl<R> CustomerService<R>
where
    R: CustomerRepository,
{
    pub fn new(customers: R) -> Self {
        Self { customers }
    }

    // -- validation helpers ---------------------------------------------------

    /// Names are trimmed, non-empty and at most 128 chars; they are not unique.
    fn clean_name(name: &str) -> AppResult<String> {
        let t = name.trim();
        if t.is_empty() {
            return Err(AppError::Validation("customer name is required".into()));
        }
        if t.chars().count() > 128 {
            return Err(AppError::Validation(
                "customer name must be <= 128 chars".into(),
            ));
        }
        Ok(t.to_string())
    }

    /// Optional free text: trimmed, whitespace-only becomes NULL, bounded by the
    /// spec limits (phone/tax_id <= 32, address <= 256, notes <= 512).
    fn clean_optional(label: &str, value: &Option<String>, max: usize) -> AppResult<Option<String>> {
        match value {
            None => Ok(None),
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Ok(None);
                }
                if t.chars().count() > max {
                    return Err(AppError::Validation(format!(
                        "customer {label} must be <= {max} chars"
                    )));
                }
                Ok(Some(t.to_string()))
            }
        }
    }

    /// NULL means no limit; a set limit must be >= 0.
    fn clean_credit_limit(limit: Option<Decimal>) -> AppResult<Option<Decimal>> {
        match limit {
            Some(v) if v < Decimal::ZERO => Err(AppError::Validation(
                "credit limit must be >= 0".into(),
            )),
            other => Ok(other),
        }
    }

    /// NULL means no default term; a set term must be >= 0 days.
    fn clean_payment_days(days: Option<i64>) -> AppResult<Option<i64>> {
        match days {
            Some(d) if d < 0 => Err(AppError::Validation(
                "payment days must be >= 0".into(),
            )),
            other => Ok(other),
        }
    }

    fn clean_input(input: &NewCustomer) -> AppResult<NewCustomer> {
        Ok(NewCustomer {
            name: Self::clean_name(&input.name)?,
            phone: Self::clean_optional("phone", &input.phone, 32)?,
            address: Self::clean_optional("address", &input.address, 256)?,
            tax_id: Self::clean_optional("tax id", &input.tax_id, 32)?,
            notes: Self::clean_optional("notes", &input.notes, 512)?,
            is_walkin: input.is_walkin,
            credit_limit: Self::clean_credit_limit(input.credit_limit)?,
            payment_days: Self::clean_payment_days(input.payment_days)?,
        })
    }

    // -- customer CRUD --------------------------------------------------------

    /// Create a customer. A name is not unique (two people can share one), so the
    /// result carries any existing exact-name matches for the caller to warn
    /// about without blocking. The walk-in is seeded by migration and cannot be
    /// created a second time.
    pub async fn create_customer(&self, input: NewCustomer) -> AppResult<CustomerCreateResult> {
        let clean = Self::clean_input(&input)?;
        if clean.is_walkin {
            if let Some(existing) = self.customers.find_walkin().await? {
                return Err(AppError::Conflict(format!(
                    "walk-in customer {} already exists",
                    existing.id
                )));
            }
        }
        // Look the name up before inserting so the new row is not its own match.
        let name_matches = self.customers.find_by_name(&clean.name).await?;
        let customer = self.customers.create(&clean).await?;
        Ok(CustomerCreateResult {
            customer,
            name_matches,
        })
    }

    pub async fn update_customer(&self, id: i64, patch: UpdateCustomer) -> AppResult<Customer> {
        self.get_customer(id).await?;

        let mut clean = UpdateCustomer::default();
        if let Some(ref name) = patch.name {
            clean.name = Some(Self::clean_name(name)?);
        }
        if let Some(ref phone) = patch.phone {
            clean.phone = Some(Self::clean_optional("phone", phone, 32)?);
        }
        if let Some(ref address) = patch.address {
            clean.address = Some(Self::clean_optional("address", address, 256)?);
        }
        if let Some(ref tax_id) = patch.tax_id {
            clean.tax_id = Some(Self::clean_optional("tax id", tax_id, 32)?);
        }
        if let Some(ref notes) = patch.notes {
            clean.notes = Some(Self::clean_optional("notes", notes, 512)?);
        }
        if let Some(limit) = patch.credit_limit {
            clean.credit_limit = Some(Self::clean_credit_limit(limit)?);
        }
        if let Some(days) = patch.payment_days {
            clean.payment_days = Some(Self::clean_payment_days(days)?);
        }
        self.customers.update(id, &clean).await
    }

    pub async fn get_customer(&self, id: i64) -> AppResult<Customer> {
        self.customers
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("customer {id} not found")))
    }

    pub async fn list_customers(&self, only_active: bool) -> AppResult<Vec<Customer>> {
        self.customers.list(only_active).await
    }

    /// Deactivate keeps the row and its history. The walk-in can never be
    /// deactivated because cash sales default to it.
    pub async fn deactivate_customer(&self, id: i64) -> AppResult<Customer> {
        let customer = self.get_customer(id).await?;
        if customer.is_walkin {
            return Err(AppError::Validation(
                "walk-in customer cannot be deactivated".into(),
            ));
        }
        self.customers.set_active(id, false).await
    }

    pub async fn activate_customer(&self, id: i64) -> AppResult<Customer> {
        self.get_customer(id).await?;
        self.customers.set_active(id, true).await
    }

    /// Deleting a customer with sales is refused by the RESTRICT foreign key
    /// (surfaced as Validation); deactivate instead. The walk-in can never be
    /// deleted.
    pub async fn delete_customer(&self, id: i64) -> AppResult<()> {
        let customer = self.get_customer(id).await?;
        if customer.is_walkin {
            return Err(AppError::Validation(
                "walk-in customer cannot be deleted".into(),
            ));
        }
        if !self.customers.delete(id).await? {
            return Err(AppError::NotFound(format!("customer {id} not found")));
        }
        Ok(())
    }

    /// Reports whether a customer is the seeded walk-in (the cash default).
    pub async fn is_walkin(&self, id: i64) -> AppResult<bool> {
        Ok(self.get_customer(id).await?.is_walkin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Customer, NewCustomer, UpdateCustomer};
    use crate::repositories::SqliteCustomerRepository;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;

    type Svc = CustomerService<SqliteCustomerRepository>;

    async fn test_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool: REPLACE conflict resolution must
            // fire the walk-in BEFORE DELETE triggers.
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn svc() -> (Svc, SqlitePool) {
        let pool = test_pool().await;
        let s = CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
        (s, pool)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn new_customer(s: &Svc, name: &str) -> Customer {
        s.create_customer(NewCustomer {
            name: name.into(),
            phone: None,
            address: None,
            tax_id: None,
            notes: None,
            is_walkin: false,
            credit_limit: None,
            payment_days: None,
        })
        .await
        .unwrap()
        .customer
    }

    async fn seeded_walkin(s: &Svc) -> Customer {
        s.list_customers(false)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.is_walkin)
            .expect("the migration seeds one walk-in customer")
    }

    // -- AC1: the walk-in is seeded and protected -----------------------------

    #[tokio::test]
    async fn walkin_is_seeded_once_and_cannot_be_deactivated_or_deleted() {
        let (s, _pool) = svc().await;

        let all = s.list_customers(false).await.unwrap();
        let walkins: Vec<_> = all.iter().filter(|c| c.is_walkin).collect();
        assert_eq!(walkins.len(), 1, "exactly one walk-in is seeded");
        let w = walkins[0];
        assert_eq!(w.name, "Consumidor final");
        assert!(w.is_active);
        assert_eq!(w.credit_limit, None, "the walk-in has no limit");
        assert_eq!(w.payment_days, None, "the walk-in has no default term");
        assert!(s.is_walkin(w.id).await.unwrap());

        // A second walk-in cannot be created.
        let err = s
            .create_customer(NewCustomer {
                name: "Otro mostrador".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: true,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        assert_eq!(
            s.list_customers(false)
                .await
                .unwrap()
                .iter()
                .filter(|c| c.is_walkin)
                .count(),
            1
        );

        // The walk-in cannot be deactivated...
        let err = s.deactivate_customer(w.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.get_customer(w.id).await.unwrap().is_active);

        // ...and cannot be deleted.
        let err = s.delete_customer(w.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.get_customer(w.id).await.is_ok());
    }

    #[tokio::test]
    async fn is_walkin_is_false_for_regular_customers() {
        let (s, _pool) = svc().await;
        let regular = new_customer(&s, "Ana Pérez").await;
        assert!(!s.is_walkin(regular.id).await.unwrap());
    }

    // -- AC14: history blocks delete; deactivate keeps the row ----------------

    #[tokio::test]
    async fn customer_with_history_cannot_be_deleted_but_can_be_deactivated() {
        let (s, pool) = svc().await;
        let c = new_customer(&s, "Con historial").await;

        // Slice K1 has no sales.customer_id yet (K2 adds it). A RESTRICT
        // reference to the customer stands in for the sales foreign key so the
        // service path is exercised against the real SQLite constraint.
        sqlx::query(
            r#"CREATE TABLE test_sale_refs (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT
               )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO test_sale_refs (customer_id) VALUES (?)")
            .bind(c.id)
            .execute(&pool)
            .await
            .unwrap();

        let err = s.delete_customer(c.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.get_customer(c.id).await.is_ok(), "the row survives");

        let off = s.deactivate_customer(c.id).await.unwrap();
        assert!(!off.is_active);
        assert_eq!(s.get_customer(c.id).await.unwrap().name, "Con historial");
        assert!(s
            .list_customers(true)
            .await
            .unwrap()
            .iter()
            .all(|x| x.id != c.id));
        assert!(s
            .list_customers(false)
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == c.id));

        // Reactivation keeps the customer usable.
        assert!(s.activate_customer(c.id).await.unwrap().is_active);
        assert!(s
            .list_customers(true)
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == c.id));
    }

    #[tokio::test]
    async fn customer_without_history_is_deleted() {
        let (s, _pool) = svc().await;
        let c = new_customer(&s, "Sin historial").await;
        s.delete_customer(c.id).await.unwrap();
        assert!(s.get_customer(c.id).await.is_err());
        assert!(matches!(
            s.delete_customer(c.id).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    // -- AC15: duplicate names are accepted and reported ----------------------

    #[tokio::test]
    async fn duplicate_name_is_accepted_and_existing_matches_are_reported() {
        let (s, _pool) = svc().await;

        let first = s
            .create_customer(NewCustomer {
                name: "  Juan Pérez  ".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap();
        assert!(first.name_matches.is_empty(), "nothing exists yet");
        assert_eq!(first.customer.name, "Juan Pérez", "the name is trimmed");

        let second = s
            .create_customer(NewCustomer {
                name: "Juan Pérez".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap();
        assert_eq!(second.name_matches.len(), 1);
        assert_eq!(second.name_matches[0].id, first.customer.id);
        assert!(
            second
                .name_matches
                .iter()
                .all(|m| m.id != second.customer.id),
            "the new row is not reported as its own match"
        );
        assert_eq!(
            s.list_customers(false)
                .await
                .unwrap()
                .iter()
                .filter(|c| c.name == "Juan Pérez")
                .count(),
            2,
            "duplicates are allowed"
        );

        // The seeded walk-in is reported as a match too, without blocking.
        let third = s
            .create_customer(NewCustomer {
                name: "Consumidor final".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap();
        assert_eq!(third.name_matches.len(), 1);
        assert!(third.name_matches[0].is_walkin);
    }

    // -- triangulation --------------------------------------------------------

    #[tokio::test]
    async fn tri_name_validation_rejects_empty_and_oversized() {
        let (s, _pool) = svc().await;
        for bad in ["", "   ", "\t\n"] {
            let err = s
                .create_customer(NewCustomer {
                    name: bad.into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: None,
                    payment_days: None,
                })
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        let err = s
            .create_customer(NewCustomer {
                name: "n".repeat(129),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let created = s
            .create_customer(NewCustomer {
                name: "  Límite exacto  ".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap()
            .customer;
        assert_eq!(created.name, "Límite exacto");
        assert!(created.is_active, "new customers are active by default");
        assert!(!created.is_walkin);
    }

    #[tokio::test]
    async fn tri_optional_fields_are_trimmed_bounded_and_clearable() {
        let (s, _pool) = svc().await;
        let created = s
            .create_customer(NewCustomer {
                name: "Campos".into(),
                phone: Some("  555-1234  ".into()),
                address: Some("  Calle 1  ".into()),
                tax_id: Some("  30-123  ".into()),
                notes: Some("  fiado  ".into()),
                is_walkin: false,
                credit_limit: Some(dec("1500.50")),
                payment_days: Some(30),
            })
            .await
            .unwrap()
            .customer;
        assert_eq!(created.phone.as_deref(), Some("555-1234"));
        assert_eq!(created.address.as_deref(), Some("Calle 1"));
        assert_eq!(created.tax_id.as_deref(), Some("30-123"));
        assert_eq!(created.notes.as_deref(), Some("fiado"));
        assert_eq!(created.credit_limit, Some(dec("1500.50")));
        assert_eq!(created.payment_days, Some(30));

        // Whitespace-only optional values are stored as NULL.
        let blank = s
            .create_customer(NewCustomer {
                name: "Blancos".into(),
                phone: Some("   ".into()),
                address: Some("".into()),
                tax_id: Some(" \t ".into()),
                notes: Some("".into()),
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap()
            .customer;
        assert_eq!(blank.phone, None);
        assert_eq!(blank.address, None);
        assert_eq!(blank.tax_id, None);
        assert_eq!(blank.notes, None);

        // Bounds: phone/address/tax_id/notes.
        for (phone, address, tax_id, notes) in [
            (Some("9".repeat(33)), None, None, None),
            (None, Some("a".repeat(257)), None, None),
            (None, None, Some("t".repeat(33)), None),
            (None, None, None, Some("x".repeat(513))),
        ] {
            let err = s
                .create_customer(NewCustomer {
                    name: "Largos".into(),
                    phone,
                    address,
                    tax_id,
                    notes,
                    is_walkin: false,
                    credit_limit: None,
                    payment_days: None,
                })
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }

        // `None` leaves a field untouched; `Some(None)` clears it.
        let patched = s
            .update_customer(
                created.id,
                UpdateCustomer {
                    name: Some("  Campos 2  ".into()),
                    phone: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.name, "Campos 2");
        assert_eq!(patched.phone, None);
        assert_eq!(patched.address.as_deref(), Some("Calle 1"));
        assert_eq!(patched.credit_limit, Some(dec("1500.50")));
        assert_eq!(patched.payment_days, Some(30));

        // A patch can also change the limit and the term.
        let patched = s
            .update_customer(
                created.id,
                UpdateCustomer {
                    credit_limit: Some(Some(dec("2000"))),
                    payment_days: Some(Some(15)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.credit_limit, Some(dec("2000")));
        assert_eq!(patched.payment_days, Some(15));
    }

    #[tokio::test]
    async fn tri_negative_limit_and_term_are_rejected() {
        let (s, _pool) = svc().await;
        let err = s
            .create_customer(NewCustomer {
                name: "Negativo".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: Some(dec("-0.01")),
                payment_days: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .create_customer(NewCustomer {
                name: "Negativo".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: Some(-1),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let ok = new_customer(&s, "Cero válido").await;
        let patched = s
            .update_customer(
                ok.id,
                UpdateCustomer {
                    credit_limit: Some(Some(Decimal::ZERO)),
                    payment_days: Some(Some(0)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.credit_limit, Some(Decimal::ZERO));
        assert_eq!(patched.payment_days, Some(0));

        let err = s
            .update_customer(
                ok.id,
                UpdateCustomer {
                    credit_limit: Some(Some(dec("-1"))),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(
            s.get_customer(ok.id).await.unwrap().credit_limit,
            Some(Decimal::ZERO),
            "a rejected patch changes nothing"
        );
    }

    #[tokio::test]
    async fn tri_name_lookup_is_exact_and_case_sensitive() {
        let (s, _pool) = svc().await;
        let stored = s
            .create_customer(NewCustomer {
                name: "Pedro Gómez".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap();
        assert!(stored.name_matches.is_empty());

        // Different case and partial names are not duplicate warnings.
        for name in ["pedro gómez", "Pedro"] {
            let r = s
                .create_customer(NewCustomer {
                    name: name.into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: None,
                    payment_days: None,
                })
                .await
                .unwrap();
            assert!(
                r.name_matches.is_empty(),
                "{name} must not match exactly"
            );
        }

        // The repository lookup itself returns every exact-name row.
        let matches = s.customers.find_by_name("Pedro Gómez").await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, stored.customer.id);
    }

    #[tokio::test]
    async fn tri_list_only_active_filters_and_keeps_the_walkin() {
        let (s, _pool) = svc().await;
        let on = new_customer(&s, "Activo").await;
        let off = new_customer(&s, "Inactivo").await;
        s.deactivate_customer(off.id).await.unwrap();

        let active = s.list_customers(true).await.unwrap();
        assert!(active.iter().any(|c| c.id == on.id));
        assert!(active.iter().all(|c| c.id != off.id));
        assert!(active.iter().any(|c| c.is_walkin), "the walk-in is active");
        let all = s.list_customers(false).await.unwrap();
        assert!(all.iter().any(|c| c.id == off.id));
    }

    #[tokio::test]
    async fn tri_not_found_paths_and_exists_helper() {
        let (s, _pool) = svc().await;
        assert!(matches!(
            s.get_customer(999_999).await.unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            s.update_customer(999_999, UpdateCustomer::default())
                .await
                .unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            s.deactivate_customer(999_999).await.unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            s.activate_customer(999_999).await.unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            s.is_walkin(999_999).await.unwrap_err(),
            AppError::NotFound(_)
        ));

        let c = new_customer(&s, "Existe").await;
        assert!(s.customers.exists(c.id).await.unwrap());
        assert!(!s.customers.exists(c.id + 1_000).await.unwrap());
    }

    #[tokio::test]
    async fn tri_guarded_walkin_seed_cannot_duplicate_on_rerun() {
        let (s, pool) = svc().await;
        let w = seeded_walkin(&s).await;

        // Re-run the seed statement exactly as the migration wrote it: the guard
        // must leave the single existing walk-in untouched.
        sqlx::query(
            r#"INSERT INTO customers (name, is_walkin, credit_limit, payment_days)
               SELECT 'Consumidor final', 1, NULL, NULL
               WHERE NOT EXISTS (SELECT 1 FROM customers WHERE is_walkin = 1)"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let walkins: Vec<_> = s
            .list_customers(false)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.is_walkin)
            .collect();
        assert_eq!(walkins.len(), 1);
        assert_eq!(walkins[0].id, w.id);
    }

    #[tokio::test]
    async fn tri_database_backstop_refuses_a_second_walkin() {
        let (s, pool) = svc().await;
        let err = sqlx::query(
            r#"INSERT INTO customers (name, is_walkin) VALUES ('Impostor', 1)"#,
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE constraint failed"),
            "got {err}"
        );
        assert_eq!(
            s.list_customers(false)
                .await
                .unwrap()
                .iter()
                .filter(|c| c.is_walkin)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn tri_rename_onto_existing_name_is_allowed_and_all_matches_are_returned() {
        let (s, _pool) = svc().await;
        let first = new_customer(&s, "Homónimo").await;
        let second = new_customer(&s, "Homónimo").await;
        let third = new_customer(&s, "Otro").await;

        // Renaming onto an existing name is not a conflict: names are not unique.
        let renamed = s
            .update_customer(
                third.id,
                UpdateCustomer {
                    name: Some("Homónimo".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.name, "Homónimo");

        let matches = s.customers.find_by_name("Homónimo").await.unwrap();
        assert_eq!(matches.len(), 3);
        let ids: Vec<i64> = matches.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![first.id, second.id, renamed.id]);
    }

    /// The database backstop: direct SQL cannot deactivate or delete the
    /// walk-in, and the row survives both attempts.
    #[tokio::test]
    async fn walkin_direct_sql_deactivate_and_delete_are_aborted_by_triggers() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        let err = sqlx::query("UPDATE customers SET is_active = 0 WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("walk-in deactivate error: {err}");
        assert!(
            err.to_string()
                .contains("walk-in customer cannot be deactivated"),
            "got {err}"
        );
        let active: (i64,) = sqlx::query_as("SELECT is_active FROM customers WHERE id = ?")
            .bind(walkin.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(active.0, 1, "the walk-in row survives a direct deactivate");
        assert!(s.get_customer(walkin.id).await.unwrap().is_active);

        let err = sqlx::query("DELETE FROM customers WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("walk-in delete error: {err}");
        assert!(
            err.to_string().contains("walk-in customer cannot be deleted"),
            "got {err}"
        );
        assert!(
            s.get_customer(walkin.id).await.is_ok(),
            "the walk-in row survives a direct delete"
        );
    }

    /// The service check stays the first line of defence with its own wording;
    /// the trigger is only the backstop.
    #[tokio::test]
    async fn walkin_service_checks_still_report_clear_errors() {
        let (s, _pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        let err = s.deactivate_customer(walkin.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(
            err.to_string(),
            "validation error: walk-in customer cannot be deactivated"
        );

        let err = s.delete_customer(walkin.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(
            err.to_string(),
            "validation error: walk-in customer cannot be deleted"
        );
        assert!(s.get_customer(walkin.id).await.unwrap().is_active);
    }

    /// The triggers are not over-broad: regular customers deactivate and delete
    /// normally.
    #[tokio::test]
    async fn regular_customer_deactivate_and_delete_still_work() {
        let (s, _pool) = svc().await;

        let deactivated = new_customer(&s, "Se desactiva").await;
        let off = s.deactivate_customer(deactivated.id).await.unwrap();
        assert!(!off.is_active);
        assert!(!s.get_customer(deactivated.id).await.unwrap().is_active);

        let deleted = new_customer(&s, "Se borra").await;
        s.delete_customer(deleted.id).await.unwrap();
        assert!(matches!(
            s.get_customer(deleted.id).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    /// Only `is_active = 0` and DELETE are aborted: renaming the walk-in and
    /// editing its contact fields still works, through SQL and through the
    /// service.
    #[tokio::test]
    async fn walkin_name_and_contact_edits_are_still_allowed() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        sqlx::query(
            "UPDATE customers SET name = ?, phone = ?, address = ?, credit_limit = ? WHERE id = ?",
        )
        .bind("Consumidor final (renombrado)")
        .bind("555-0100")
        .bind("Mostrador")
        .bind("1500.00")
        .bind(walkin.id)
        .execute(&pool)
        .await
        .unwrap();

        let row = s.get_customer(walkin.id).await.unwrap();
        assert_eq!(row.name, "Consumidor final (renombrado)");
        assert_eq!(row.phone.as_deref(), Some("555-0100"));
        assert_eq!(row.address.as_deref(), Some("Mostrador"));
        assert_eq!(row.credit_limit, Some(dec("1500.00")));
        assert!(row.is_walkin && row.is_active);

        let updated = s
            .update_customer(
                walkin.id,
                UpdateCustomer {
                    name: Some("Consumidor final".into()),
                    credit_limit: Some(Some(dec("2000"))),
                    payment_days: Some(Some(30)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "Consumidor final");
        assert_eq!(updated.credit_limit, Some(dec("2000")));
        assert_eq!(updated.payment_days, Some(30));
        assert!(updated.is_active && updated.is_walkin);
    }

    /// The third trigger: the walk-in is permanent, so direct SQL cannot demote
    /// it and the row keeps `is_walkin = 1`.
    #[tokio::test]
    async fn walkin_direct_sql_demotion_is_aborted_by_trigger() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        let err = sqlx::query("UPDATE customers SET is_walkin = 0 WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("walk-in demote error: {err}");
        assert!(
            err.to_string().contains("walk-in customer cannot be demoted"),
            "got {err}"
        );

        let row = s.get_customer(walkin.id).await.unwrap();
        assert!(row.is_walkin, "the walk-in stays the walk-in");
        assert!(row.is_active, "the walk-in stays active");
    }

    /// The chain is closed: a demotion attempt cannot clear `is_walkin`, so the
    /// deactivation and delete triggers still key on it afterwards.
    #[tokio::test]
    async fn walkin_demotion_abort_keeps_the_other_two_triggers_closed() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        sqlx::query("UPDATE customers SET is_walkin = 0 WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();

        let err = sqlx::query("UPDATE customers SET is_active = 0 WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be deactivated"), "got {err}");

        let err = sqlx::query("DELETE FROM customers WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be deleted"), "got {err}");

        let row: (i64, i64) =
            sqlx::query_as("SELECT is_walkin, is_active FROM customers WHERE id = ?")
                .bind(walkin.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, (1, 1), "the walk-in survives the whole chain");
    }

    /// The trigger is not over-broad: a regular customer can be updated in every
    /// field of the patch DTO.
    #[tokio::test]
    async fn regular_customer_can_be_updated_in_every_field() {
        let (s, _pool) = svc().await;
        let regular = new_customer(&s, "Actualizable").await;

        let updated = s
            .update_customer(
                regular.id,
                UpdateCustomer {
                    name: Some("Actualizado".into()),
                    phone: Some(Some("555-9999".into())),
                    address: Some(Some("Calle 1".into())),
                    tax_id: Some(Some("20-12345678-9".into())),
                    notes: Some(Some("nota".into())),
                    credit_limit: Some(Some(dec("500"))),
                    payment_days: Some(Some(15)),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "Actualizado");
        assert_eq!(updated.phone.as_deref(), Some("555-9999"));
        assert_eq!(updated.address.as_deref(), Some("Calle 1"));
        assert_eq!(updated.tax_id.as_deref(), Some("20-12345678-9"));
        assert_eq!(updated.notes.as_deref(), Some("nota"));
        assert_eq!(updated.credit_limit, Some(dec("500")));
        assert_eq!(updated.payment_days, Some(15));
        assert!(!updated.is_walkin && updated.is_active);
    }

    /// REPLACE conflict resolution cannot remove or replace the walk-in: with
    /// `recursive_triggers` on, the BEFORE DELETE trigger fires for the rows
    /// REPLACE deletes too.
    #[tokio::test]
    async fn replace_forms_cannot_remove_the_walkin() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        // INSERT OR REPLACE.
        let err = sqlx::query(
            "INSERT OR REPLACE INTO customers (id, name, is_walkin, is_active) \
             VALUES (?, 'Replaced', 0, 1)",
        )
        .bind(walkin.id)
        .execute(&pool)
        .await
        .unwrap_err();
        eprintln!("INSERT OR REPLACE error: {err}");
        assert!(err.to_string().contains("cannot be deleted"), "got {err}");
        assert_walkin_survives(&s, walkin.id).await;

        // REPLACE INTO promoting a different row onto the walk-in slot.
        let err = sqlx::query(
            "REPLACE INTO customers (name, is_walkin) VALUES ('Second walkin', 1)",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        eprintln!("REPLACE INTO error: {err}");
        assert!(err.to_string().contains("cannot be deleted"), "got {err}");
        assert_walkin_survives(&s, walkin.id).await;

        // UPDATE OR REPLACE pushing a regular customer into the walk-in slot.
        let regular = new_customer(&s, "Regular replace").await;
        let err = sqlx::query("UPDATE OR REPLACE customers SET is_walkin = 1 WHERE id = ?")
            .bind(regular.id)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("UPDATE OR REPLACE error: {err}");
        assert!(err.to_string().contains("cannot be deleted"), "got {err}");
        assert_walkin_survives(&s, walkin.id).await;
        assert!(!s.get_customer(regular.id).await.unwrap().is_walkin);
    }

    async fn assert_walkin_survives(s: &Svc, id: i64) {
        let row = s.get_customer(id).await.unwrap();
        assert!(
            row.is_walkin && row.is_active,
            "the walk-in survives: {row:?}"
        );
        assert!(s.customers.find_walkin().await.unwrap().is_some());
    }

    /// `ignore_check_constraints` cannot smuggle a non-1 walk-in flag: the
    /// demote trigger rejects any new value other than 1.
    #[tokio::test]
    async fn ignore_check_constraints_cannot_demote_the_walkin() {
        let (s, pool) = svc().await;
        let walkin = seeded_walkin(&s).await;

        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&pool)
            .await
            .unwrap();
        let err = sqlx::query("UPDATE customers SET is_walkin = 2 WHERE id = ?")
            .bind(walkin.id)
            .execute(&pool)
            .await
            .unwrap_err();
        eprintln!("is_walkin=2 error: {err}");
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            err.to_string().contains("walk-in customer cannot be demoted"),
            "got {err}"
        );

        let stored: (i64,) = sqlx::query_as("SELECT is_walkin FROM customers WHERE id = ?")
            .bind(walkin.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored.0, 1, "the walk-in flag survives the update");
    }

    /// Legitimate operations are unaffected: regular customers can be created,
    /// renamed, deactivated and deleted, and the walk-in can be renamed with
    /// its nullable fields edited.
    #[tokio::test]
    async fn legitimate_customer_operations_are_unaffected_by_the_pragmas() {
        let (s, _pool) = svc().await;

        let regular = new_customer(&s, "Válido").await;
        let renamed = s
            .update_customer(
                regular.id,
                UpdateCustomer {
                    name: Some("Válido renombrado".into()),
                    phone: Some(Some("555".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.name, "Válido renombrado");
        let off = s.deactivate_customer(regular.id).await.unwrap();
        assert!(!off.is_active);
        s.delete_customer(regular.id).await.unwrap();
        assert!(matches!(
            s.get_customer(regular.id).await.unwrap_err(),
            AppError::NotFound(_)
        ));

        let walkin = seeded_walkin(&s).await;
        let edited = s
            .update_customer(
                walkin.id,
                UpdateCustomer {
                    name: Some("Consumidor final de mostrador".into()),
                    phone: Some(Some("555-0100".into())),
                    address: Some(Some("Mostrador".into())),
                    credit_limit: Some(Some(Decimal::ZERO)),
                    payment_days: Some(Some(0)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.name, "Consumidor final de mostrador");
        assert_eq!(edited.phone.as_deref(), Some("555-0100"));
        assert_eq!(edited.address.as_deref(), Some("Mostrador"));
        assert_eq!(edited.credit_limit, Some(Decimal::ZERO));
        assert_eq!(edited.payment_days, Some(0));
        assert!(edited.is_walkin && edited.is_active);
    }
}
