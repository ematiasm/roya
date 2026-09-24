// M3 suppliers + product/supplier cost satellite (Slice E).
//
// SupplierService owns supplier CRUD (trim/validate name/phone/notes,
// deactivate, RESTRICT-aware delete) and the frozen option-A cost rule: a new
// cost that differs from the current one shifts current -> previous (value and
// date) and stores the new value with its date; an equal cost only refreshes
// the confirmation date, preserving the last genuinely different previous so
// the alert keeps working. The "supplier raised the price" alert is derived
// from previous vs current, never stored. `products.cost_price` is never
// written here: the satellite wins when the product has rows, and callers fall
// back to the column only when it does not.
use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{NewSupplier, PriceAlert, ProductSupplierCost, Supplier, UpdateSupplier};
use crate::repositories::{ProductSupplierCostRepository, SupplierRepository};

#[derive(Clone)]
pub struct SupplierService<SR, CR>
where
    SR: SupplierRepository,
    CR: ProductSupplierCostRepository,
{
    pub suppliers: SR,
    pub costs: CR,
}

impl<SR, CR> SupplierService<SR, CR>
where
    SR: SupplierRepository,
    CR: ProductSupplierCostRepository,
{
    pub fn new(suppliers: SR, costs: CR) -> Self {
        Self { suppliers, costs }
    }

    // -- validation helpers ---------------------------------------------------

    fn clean_name(name: &str) -> AppResult<String> {
        let t = name.trim();
        if t.is_empty() {
            return Err(AppError::Validation("supplier name is required".into()));
        }
        if t.chars().count() > 128 {
            return Err(AppError::Validation(
                "supplier name must be <= 128 chars".into(),
            ));
        }
        Ok(t.to_string())
    }

    fn clean_phone(phone: &Option<String>) -> AppResult<Option<String>> {
        match phone {
            None => Ok(None),
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Ok(None);
                }
                if t.chars().count() > 32 {
                    return Err(AppError::Validation("phone must be <= 32 chars".into()));
                }
                Ok(Some(t.to_string()))
            }
        }
    }

    fn clean_notes(notes: &Option<String>) -> AppResult<Option<String>> {
        match notes {
            None => Ok(None),
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Ok(None);
                }
                if t.chars().count() > 512 {
                    return Err(AppError::Validation("notes must be <= 512 chars".into()));
                }
                Ok(Some(t.to_string()))
            }
        }
    }

    /// NULL means no default term; zero is due immediately; explicit terms
    /// must be non-negative.
    fn clean_due_days(days: Option<i64>) -> AppResult<Option<i64>> {
        match days {
            Some(value) if value < 0 => Err(AppError::Validation(
                "supplier due days must be >= 0".into(),
            )),
            other => Ok(other),
        }
    }

    // -- supplier CRUD --------------------------------------------------------

    /// Create a supplier. `actor` is the acting user's id the route resolves
    /// from its `Principal`; it becomes the row's `created_by` and nothing the
    /// request itself can supply names it.
    pub async fn create_supplier(&self, actor: i64, input: NewSupplier) -> AppResult<Supplier> {
        let clean = NewSupplier {
            name: Self::clean_name(&input.name)?,
            phone: Self::clean_phone(&input.phone)?,
            notes: Self::clean_notes(&input.notes)?,
            due_days: Self::clean_due_days(input.due_days)?,
        };
        self.suppliers.create(actor, &clean).await
    }

    pub async fn update_supplier(
        &self,
        actor: i64,
        id: i64,
        patch: UpdateSupplier,
    ) -> AppResult<Supplier> {
        self.get_supplier(id).await?;
        let mut clean = UpdateSupplier::default();
        if let Some(ref name) = patch.name {
            clean.name = Some(Self::clean_name(name)?);
        }
        if let Some(ref phone) = patch.phone {
            clean.phone = Some(Self::clean_phone(phone)?);
        }
        if let Some(ref notes) = patch.notes {
            clean.notes = Some(Self::clean_notes(notes)?);
        }
        if let Some(days) = patch.due_days {
            clean.due_days = Some(Self::clean_due_days(days)?);
        }
        self.suppliers.update(id, actor, &clean).await
    }

    pub async fn get_supplier(&self, id: i64) -> AppResult<Supplier> {
        self.suppliers
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("supplier {id} not found")))
    }

    pub async fn list_suppliers(&self) -> AppResult<Vec<Supplier>> {
        self.suppliers.list().await
    }

    // -- picker reads (the supplier half of the product picker) --------------

    /// Upper bound for one supplier picker search: the same bound the product
    /// picker uses, so one fragment never dumps the whole roster into a page.
    pub const SUPPLIER_SEARCH_LIMIT: usize = 10;

    /// Bounded read behind `GET /web/supplier-search`: normalized name matching
    /// over the whole (small) supplier table fetched once, sorted by name. The
    /// empty query is not a search and never returns the roster. Inactive
    /// suppliers match too and each result carries its own `is_active` so the
    /// fragment can report it — the operator decides whether to pick one.
    pub async fn search_suppliers(&self, query: &str) -> AppResult<Vec<Supplier>> {
        let value = query.trim();
        if value.is_empty() {
            return Ok(Vec::new());
        }
        let needle = crate::models::normalize_search(value);
        let mut matches: Vec<Supplier> = self
            .suppliers
            .list()
            .await?
            .into_iter()
            .filter(|s| crate::models::normalize_search(&s.name).contains(&needle))
            .collect();
        matches.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        matches.truncate(Self::SUPPLIER_SEARCH_LIMIT);
        Ok(matches)
    }

    /// Resolve one typed supplier name for a caller that posts a name and no
    /// id (the creation and header paths, T3 and T4): the exact — normalized,
    /// case- and diacritic-insensitive — name maps to exactly one supplier
    /// because `suppliers.name` is UNIQUE, the same contract
    /// `resolve_product_ref` gives the product picker. An empty value is the
    /// required-field refusal; anything that is not an exact name is a 400
    /// naming the value and reporting how many partial matches the search
    /// found, so the operator picks from the list instead of a guess.
    /// Resolution deliberately does not go through `search_suppliers`: the
    /// picker's bound would truncate the candidate list, and with more than
    /// `SUPPLIER_SEARCH_LIMIT` partial matches two normalized-equal names
    /// could straddle the bound — one inside resolves silently while its
    /// look-alike sits outside, or an existing name reports "no exact
    /// match". The whole table is read and the exact matches selected here,
    /// with no truncation, so the count below cannot be fooled by the bound.
    pub async fn resolve_supplier_name(&self, raw: &str) -> AppResult<Supplier> {
        let value = raw.trim();
        if value.is_empty() {
            return Err(AppError::Validation("supplier is required".into()));
        }
        let needle = crate::models::normalize_search(value);
        let all = self.suppliers.list().await?;
        let matches: Vec<&Supplier> = all
            .iter()
            .filter(|s| crate::models::normalize_search(&s.name).contains(&needle))
            .collect();
        let exact: Vec<&&Supplier> = matches
            .iter()
            .filter(|s| crate::models::normalize_search(&s.name) == needle)
            .collect();
        match exact.len() {
            1 => Ok((**exact[0]).clone()),
            0 => {
                let noun = if matches.len() == 1 { "match" } else { "matches" };
                Err(AppError::Validation(format!(
                    "no exact match for \"{value}\" — the search found {} {noun}; pick one from the list",
                    matches.len()
                )))
            }
            // Only reachable when the storage holds case-variant duplicates
            // (`Pérez` and `perez` differ for UNIQUE but fold equal): refuse
            // rather than pick one.
            _ => Err(AppError::Validation(format!(
                "the name \"{value}\" matches {} suppliers with the same spelling; pick one from the list",
                exact.len()
            ))),
        }
    }

    /// Deactivate (`false`) a supplier instead of deleting it when it has
    /// history. The toggle is an edit: the row's `updated_by` carries `actor`.
    pub async fn set_active(&self, actor: i64, id: i64, active: bool) -> AppResult<Supplier> {
        self.get_supplier(id).await?;
        self.suppliers.set_active(id, actor, active).await
    }

    /// RESTRICT-aware delete: a supplier with cost rows cannot be deleted
    /// (future purchases add the same restriction); deactivate it instead.
    pub async fn delete_supplier(&self, id: i64) -> AppResult<()> {
        self.get_supplier(id).await?;
        if self.costs.count_by_supplier(id).await? > 0 {
            return Err(AppError::Validation(
                "cannot delete supplier with cost rows; deactivate it instead".into(),
            ));
        }
        if !self.suppliers.delete(id).await? {
            return Err(AppError::NotFound(format!("supplier {id} not found")));
        }
        Ok(())
    }

    // -- product/supplier cost satellite --------------------------------------

    /// Record a (product, supplier) cost. First call creates the row. A later
    /// call with a different cost shifts current -> previous with its date and
    /// stores the new value with `when`; a later call with the same cost only
    /// refreshes `current_cost_date`, keeping the last distinct previous.
    /// Unknown product/supplier surface as Validation through the repository FK
    /// mapping. `products.cost_price` is deliberately untouched.
    pub async fn record_cost(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
        cost: Decimal,
        when: NaiveDate,
    ) -> AppResult<ProductSupplierCost> {
        if cost < Decimal::ZERO {
            return Err(AppError::Validation("cost must be >= 0".into()));
        }
        match self.costs.find(product_id, supplier_id).await? {
            None => {
                self.costs
                    .create_cost(actor, product_id, supplier_id, cost, when)
                    .await
            }
            Some(existing) => {
                if when < existing.current_cost_date {
                    return Err(AppError::Validation(
                        "cost date cannot precede the current cost date".into(),
                    ));
                }
                if existing.current_cost == cost {
                    // Same price: do not shift, or the last genuinely different
                    // previous would be lost and the derived alert would always
                    // read Unchanged. Only refresh the confirmation date.
                    self.costs
                        .refresh_cost_date(actor, product_id, supplier_id, when)
                        .await
                } else {
                    self.costs
                        .shift_cost(actor, product_id, supplier_id, cost, when)
                        .await
                }
            }
        }
    }

    pub async fn find_cost(
        &self,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<Option<ProductSupplierCost>> {
        self.costs.find(product_id, supplier_id).await
    }

    pub async fn list_costs_for_product(
        &self,
        product_id: i64,
    ) -> AppResult<Vec<ProductSupplierCost>> {
        self.costs.list_by_product(product_id).await
    }

    /// Mark a supplier as the preferred one for a product, clearing any other
    /// preferred row of that product. Both writes carry `actor` as their
    /// `updated_by` (AC18).
    pub async fn set_preferred(
        &self,
        actor: i64,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<ProductSupplierCost> {
        self.costs
            .find(product_id, supplier_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "cost row for product {product_id} and supplier {supplier_id} not found"
                ))
            })?;
        self.costs
            .set_preferred(actor, product_id, supplier_id)
            .await
    }

    pub async fn clear_preferred(&self, actor: i64, product_id: i64) -> AppResult<()> {
        self.costs.clear_preferred(actor, product_id).await
    }

    /// Derived read rule: preferred supplier's current cost, else the lowest
    /// current cost, else `None` meaning the caller falls back to
    /// `products.cost_price`. Values are Decimals (TEXT ordering cannot be used).
    pub async fn reference_cost(&self, product_id: i64) -> AppResult<Option<Decimal>> {
        let costs = self.costs.list_by_product(product_id).await?;
        if costs.is_empty() {
            return Ok(None);
        }
        if let Some(preferred) = costs.iter().find(|c| c.is_preferred) {
            return Ok(Some(preferred.current_cost));
        }
        Ok(costs.iter().map(|c| c.current_cost).min())
    }

    /// Derived price alert for a (product, supplier) pair, `None` if no row.
    pub async fn price_alert(
        &self,
        product_id: i64,
        supplier_id: i64,
    ) -> AppResult<Option<PriceAlert>> {
        Ok(self
            .costs
            .find(product_id, supplier_id)
            .await?
            .map(|c| c.price_alert()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        NewProduct, NewSupplier, PriceAlert, ProductKind, Supplier, UpdateSupplier,
    };
    use crate::repositories::{
        ProductRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
        SqliteProductRepository, SqliteProductSupplierCostRepository,
        SqliteStockMovementRepository, SqliteSupplierRepository,
    };
    use crate::security::test_support;
    use crate::services::InventoryService;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;

    type Svc = SupplierService<SqliteSupplierRepository, SqliteProductSupplierCostRepository>;

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

    async fn svc() -> (Svc, SqlitePool) {
        let pool = test_pool().await;
        let s = SupplierService::new(
            SqliteSupplierRepository::new(pool.clone()),
            SqliteProductSupplierCostRepository::new(pool.clone()),
        );
        (s, pool)
    }

    /// The migration's sentinel account as a valid acting user for the
    /// mechanical call sites; the attribution tests below seed their own
    /// users to tell two actors apart. Borrow-flexible so owned test
    /// services and borrowed ones call it the same way.
    async fn audit_actor(s: impl std::borrow::Borrow<Svc>) -> i64 {
        crate::security::test_support::audit_actor_id(&s.borrow().suppliers.pool)
            .await
            .unwrap()
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    async fn seed_product(pool: &SqlitePool, sku: &str, cost_price: &str) -> i64 {
        let actor = crate::security::test_support::audit_actor_id(pool)
            .await
            .unwrap();
        SqliteProductRepository::new(pool.clone())
            .create(
                actor,
                &NewProduct {
                    sku: sku.into(),
                    name: format!("prod {sku}"),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec("10"),
                    cost_price: dec(cost_price),
                    track_stock: true,
                    min_stock: Some(dec("0")),
                    max_stock: Some(dec("10")),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn seed_supplier(s: &Svc, name: &str) -> Supplier {
        s.create_supplier(
            audit_actor(s).await,
            NewSupplier {
                name: name.into(),
                phone: None,
                notes: None,
                due_days: None,
            },
        )
        .await
        .unwrap()
    }

    // -- AC9 (satellite part) -------------------------------------------------

    #[tokio::test]
    async fn ac9_first_record_sets_current_without_previous() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-1", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP").await;

        let row = s
            .record_cost(
                audit_actor(&s).await,
                p,
                sup.id,
                dec("10.50"),
                d(2024, 5, 1),
            )
            .await
            .unwrap();

        assert_eq!(row.current_cost, dec("10.50"));
        assert_eq!(row.current_cost_date, d(2024, 5, 1));
        assert_eq!(row.previous_cost, None);
        assert_eq!(row.previous_cost_date, None);
        assert!(!row.is_preferred);
    }

    #[tokio::test]
    async fn ac9_second_record_shifts_current_into_previous_with_dates() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-2", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP 2").await;

        s.record_cost(audit_actor(&s).await, p, sup.id, dec("10"), d(2024, 5, 1))
            .await
            .unwrap();
        let row = s
            .record_cost(
                audit_actor(&s).await,
                p,
                sup.id,
                dec("12.25"),
                d(2024, 5, 10),
            )
            .await
            .unwrap();

        assert_eq!(row.current_cost, dec("12.25"));
        assert_eq!(row.current_cost_date, d(2024, 5, 10));
        assert_eq!(row.previous_cost, Some(dec("10")));
        assert_eq!(row.previous_cost_date, Some(d(2024, 5, 1)));
    }

    #[tokio::test]
    async fn ac9_price_alert_is_derived_from_previous_vs_current() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-3", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP 3").await;

        let first = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("10"), d(2024, 5, 1))
            .await
            .unwrap();
        assert_eq!(first.price_alert(), PriceAlert::Unchanged);

        let raised = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("12"), d(2024, 5, 2))
            .await
            .unwrap();
        assert_eq!(raised.price_alert(), PriceAlert::Raised);

        let lowered = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("8"), d(2024, 5, 3))
            .await
            .unwrap();
        assert_eq!(lowered.price_alert(), PriceAlert::Lowered);

        // Repeating the same cost must not erase the last distinct price:
        // previous stays 12, so the alert is still Lowered.
        let repeated = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("8"), d(2024, 5, 4))
            .await
            .unwrap();
        assert_eq!(repeated.previous_cost, Some(dec("12")));
        assert_eq!(repeated.price_alert(), PriceAlert::Lowered);

        let raised_again = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("9"), d(2024, 5, 5))
            .await
            .unwrap();
        assert_eq!(raised_again.previous_cost, Some(dec("8")));
        assert_eq!(raised_again.price_alert(), PriceAlert::Raised);

        assert_eq!(
            s.price_alert(p, sup.id).await.unwrap(),
            Some(PriceAlert::Raised)
        );
        assert_eq!(s.price_alert(p, 999_999).await.unwrap(), None);
    }

    #[tokio::test]
    async fn ac9_repeated_identical_cost_preserves_previous_and_its_date() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-4", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP 4").await;

        s.record_cost(audit_actor(&s).await, p, sup.id, dec("100"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("120"), d(2024, 5, 2))
            .await
            .unwrap();
        let repeated = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("120"), d(2024, 5, 5))
            .await
            .unwrap();

        assert_eq!(repeated.current_cost, dec("120"));
        assert_eq!(repeated.previous_cost, Some(dec("100")));
        assert_eq!(repeated.previous_cost_date, Some(d(2024, 5, 1)));
    }

    #[tokio::test]
    async fn ac9_repeated_identical_cost_refreshes_current_date() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-5", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP 5").await;

        s.record_cost(audit_actor(&s).await, p, sup.id, dec("120"), d(2024, 5, 1))
            .await
            .unwrap();
        let refreshed = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("120"), d(2024, 5, 7))
            .await
            .unwrap();

        assert_eq!(refreshed.current_cost, dec("120"));
        assert_eq!(refreshed.current_cost_date, d(2024, 5, 7));
        assert_eq!(refreshed.previous_cost, None);
    }

    #[tokio::test]
    async fn ac9_alert_survives_repeated_cost_and_tracks_next_real_change() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC9-6", "5").await;
        let sup = seed_supplier(&s, "AC9 SUP 6").await;

        s.record_cost(audit_actor(&s).await, p, sup.id, dec("100"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("120"), d(2024, 5, 2))
            .await
            .unwrap();
        for day in [5, 6, 7] {
            let repeated = s
                .record_cost(
                    audit_actor(&s).await,
                    p,
                    sup.id,
                    dec("120"),
                    d(2024, 5, day),
                )
                .await
                .unwrap();
            assert_eq!(repeated.previous_cost, Some(dec("100")));
            assert_eq!(repeated.price_alert(), PriceAlert::Raised);
        }

        let lowered = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("90"), d(2024, 5, 8))
            .await
            .unwrap();
        assert_eq!(lowered.previous_cost, Some(dec("120")));
        assert_eq!(lowered.price_alert(), PriceAlert::Lowered);

        let raised = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("95"), d(2024, 5, 9))
            .await
            .unwrap();
        assert_eq!(raised.previous_cost, Some(dec("90")));
        assert_eq!(raised.price_alert(), PriceAlert::Raised);
    }

    // -- AC10 -----------------------------------------------------------------

    #[tokio::test]
    async fn ac10_reference_cost_prefers_satellite_and_never_writes_cost_price() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC10-1", "5").await;
        let sup = seed_supplier(&s, "AC10 SUP").await;

        s.record_cost(audit_actor(&s).await, p, sup.id, dec("9.50"), d(2024, 5, 1))
            .await
            .unwrap();

        let prod = SqliteProductRepository::new(pool.clone())
            .find_by_id(p)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prod.cost_price, dec("5"), "cost_price must stay untouched");

        assert_eq!(s.reference_cost(p).await.unwrap(), Some(dec("9.50")));
    }

    #[tokio::test]
    async fn ac10_reference_cost_absent_without_rows() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC10-2", "5").await;
        assert_eq!(s.reference_cost(p).await.unwrap(), None);
    }

    // -- AC13 -----------------------------------------------------------------

    #[tokio::test]
    async fn ac13_delete_supplier_with_cost_rows_blocked() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC13-1", "5").await;
        let sup = seed_supplier(&s, "AC13 SUP").await;
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("7"), d(2024, 5, 1))
            .await
            .unwrap();

        let err = s.delete_supplier(sup.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.get_supplier(sup.id).await.is_ok());
    }

    #[tokio::test]
    async fn ac13_delete_supplier_without_history_works() {
        let (s, _pool) = svc().await;
        let sup = seed_supplier(&s, "AC13 FREE").await;
        s.delete_supplier(sup.id).await.unwrap();
        assert!(matches!(
            s.get_supplier(sup.id).await.unwrap_err(),
            AppError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn ac13_product_delete_blocked_when_cost_row_exists() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "AC13-2", "5").await;
        let sup = seed_supplier(&s, "AC13 SUP 2").await;
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("7"), d(2024, 5, 1))
            .await
            .unwrap();

        let inventory = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            true,
        );
        let err = inventory.delete_product(p).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(SqliteProductRepository::new(pool.clone())
            .find_by_id(p)
            .await
            .unwrap()
            .is_some());
    }

    // -- preferred supplier ---------------------------------------------------

    #[tokio::test]
    async fn preferred_switch_clears_previous_supplier_for_product() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "PREF-1", "5").await;
        let other = seed_product(&pool, "PREF-2", "5").await;
        let a = seed_supplier(&s, "PREF A").await;
        let b = seed_supplier(&s, "PREF B").await;
        s.record_cost(audit_actor(&s).await, p, a.id, dec("9"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, p, b.id, dec("8"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, other, a.id, dec("9"), d(2024, 5, 1))
            .await
            .unwrap();
        s.set_preferred(audit_actor(&s).await, other, a.id)
            .await
            .unwrap();

        let first = s
            .set_preferred(audit_actor(&s).await, p, a.id)
            .await
            .unwrap();
        assert!(first.is_preferred);
        let second = s
            .set_preferred(audit_actor(&s).await, p, b.id)
            .await
            .unwrap();
        assert!(second.is_preferred);

        assert!(!s.find_cost(p, a.id).await.unwrap().unwrap().is_preferred);
        assert!(s.find_cost(p, b.id).await.unwrap().unwrap().is_preferred);
        // Another product keeps its own preferred supplier.
        assert!(
            s.find_cost(other, a.id)
                .await
                .unwrap()
                .unwrap()
                .is_preferred
        );
    }

    // -- triangulation --------------------------------------------------------

    #[tokio::test]
    async fn tri_supplier_name_is_trimmed_and_duplicate_is_conflict() {
        let (s, _pool) = svc().await;
        let created = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: "  Distribuidora Sur  ".into(),
                    phone: Some("  555-1234  ".into()),
                    notes: Some("  entrega martes  ".into()),
                    due_days: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(created.name, "Distribuidora Sur");
        assert_eq!(created.phone.as_deref(), Some("555-1234"));
        assert_eq!(created.notes.as_deref(), Some("entrega martes"));

        let err = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: "Distribuidora Sur".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        assert!(s
            .suppliers
            .find_by_name("Distribuidora Sur")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn tri_supplier_validation_rejects_empty_and_oversized_fields() {
        let (s, _pool) = svc().await;
        for bad in ["", "   "] {
            let err = s
                .create_supplier(
                    audit_actor(&s).await,
                    NewSupplier {
                        name: bad.into(),
                        phone: None,
                        notes: None,
                        due_days: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
        let long_name = "n".repeat(129);
        let long_phone = "9".repeat(33);
        let long_notes = "x".repeat(513);
        let err = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: long_name,
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: "Largo".into(),
                    phone: Some(long_phone),
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: "Largo".into(),
                    phone: None,
                    notes: Some(long_notes),
                    due_days: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_update_supplier_changes_conflicts_and_clears_fields() {
        let (s, _pool) = svc().await;
        let a = s
            .create_supplier(
                audit_actor(&s).await,
                NewSupplier {
                    name: "A".into(),
                    phone: Some("111".into()),
                    notes: Some("n".into()),
                    due_days: None,
                },
            )
            .await
            .unwrap();
        let _b = seed_supplier(&s, "B").await;

        let updated = s
            .update_supplier(
                audit_actor(&s).await,
                a.id,
                UpdateSupplier {
                    name: Some("A2".into()),
                    phone: Some(Some("  222  ".into())),
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "A2");
        assert_eq!(updated.phone.as_deref(), Some("222"));
        assert_eq!(updated.notes.as_deref(), Some("n"));

        // Renaming onto another supplier's name is a conflict.
        let err = s
            .update_supplier(
                audit_actor(&s).await,
                a.id,
                UpdateSupplier {
                    name: Some("B".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");

        // Some(None) clears the field.
        let cleared = s
            .update_supplier(
                audit_actor(&s).await,
                a.id,
                UpdateSupplier {
                    phone: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cleared.phone, None);

        let err = s
            .update_supplier(audit_actor(&s).await, 999_999, UpdateSupplier::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_deactivate_supplier_keeps_cost_rows() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-DEACT", "5").await;
        let sup = seed_supplier(&s, "TRI DEACT").await;
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("6"), d(2024, 5, 1))
            .await
            .unwrap();

        let off = s
            .set_active(audit_actor(&s).await, sup.id, false)
            .await
            .unwrap();
        assert!(!off.is_active);
        assert!(s.find_cost(p, sup.id).await.unwrap().is_some());
        assert_eq!(s.reference_cost(p).await.unwrap(), Some(dec("6")));
        let all = s.list_suppliers().await.unwrap();
        assert!(all.iter().any(|x| x.id == sup.id && !x.is_active));
        assert!(matches!(
            s.delete_supplier(sup.id).await.unwrap_err(),
            AppError::Validation(_)
        ));
    }

    #[tokio::test]
    async fn tri_negative_cost_rejected_without_creating_row() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-NEG", "5").await;
        let sup = seed_supplier(&s, "TRI NEG").await;
        let err = s
            .record_cost(
                audit_actor(&s).await,
                p,
                sup.id,
                dec("-0.01"),
                d(2024, 5, 1),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(s.find_cost(p, sup.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tri_backdated_cost_rejected_keeping_row_unchanged() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-DATE", "5").await;
        let sup = seed_supplier(&s, "TRI DATE").await;
        s.record_cost(audit_actor(&s).await, p, sup.id, dec("10"), d(2024, 5, 10))
            .await
            .unwrap();
        let err = s
            .record_cost(audit_actor(&s).await, p, sup.id, dec("11"), d(2024, 5, 1))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let row = s.find_cost(p, sup.id).await.unwrap().unwrap();
        assert_eq!(row.current_cost, dec("10"));
        assert_eq!(row.current_cost_date, d(2024, 5, 10));
        assert_eq!(row.previous_cost, None);
    }

    #[tokio::test]
    async fn tri_unknown_product_or_supplier_cost_rejected() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-FK", "5").await;
        let sup = seed_supplier(&s, "TRI FK").await;

        let err = s
            .record_cost(
                audit_actor(&s).await,
                999_999,
                sup.id,
                dec("1"),
                d(2024, 5, 1),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .record_cost(audit_actor(&s).await, p, 999_999, dec("1"), d(2024, 5, 1))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(s.reference_cost(p).await.unwrap(), None);
    }

    #[tokio::test]
    async fn tri_partial_unique_index_blocks_second_preferred_row() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-PREF", "5").await;
        let a = seed_supplier(&s, "TRI PREF A").await;
        let b = seed_supplier(&s, "TRI PREF B").await;
        s.record_cost(audit_actor(&s).await, p, a.id, dec("9"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, p, b.id, dec("8"), d(2024, 5, 1))
            .await
            .unwrap();
        s.set_preferred(audit_actor(&s).await, p, a.id)
            .await
            .unwrap();

        let err = sqlx::query(
            r#"UPDATE product_supplier_costs SET is_preferred = 1
               WHERE product_id = ? AND supplier_id = ?"#,
        )
        .bind(p)
        .bind(b.id)
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE constraint failed"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn tri_reference_cost_prefers_marked_supplier_over_cheapest_then_lowest() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-REF", "5").await;
        let cheap = seed_supplier(&s, "TRI REF CHEAP").await;
        let marked = seed_supplier(&s, "TRI REF MARKED").await;
        s.record_cost(audit_actor(&s).await, p, cheap.id, dec("7"), d(2024, 5, 1))
            .await
            .unwrap();
        s.record_cost(audit_actor(&s).await, p, marked.id, dec("9"), d(2024, 5, 1))
            .await
            .unwrap();

        // No preferred yet: lowest wins.
        assert_eq!(s.reference_cost(p).await.unwrap(), Some(dec("7")));
        s.set_preferred(audit_actor(&s).await, p, marked.id)
            .await
            .unwrap();
        // Preferred wins even when it is not the cheapest.
        assert_eq!(s.reference_cost(p).await.unwrap(), Some(dec("9")));
        s.clear_preferred(audit_actor(&s).await, p).await.unwrap();
        assert_eq!(s.reference_cost(p).await.unwrap(), Some(dec("7")));
    }

    #[tokio::test]
    async fn tri_set_preferred_requires_existing_cost_row() {
        let (s, pool) = svc().await;
        let p = seed_product(&pool, "TRI-PREF-MISS", "5").await;
        let sup = seed_supplier(&s, "TRI PREF MISS").await;
        let err = s
            .set_preferred(audit_actor(&s).await, p, sup.id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    // -- AC18 (suppliers audit, slice S12): two actors on the supplier row,
    //    and on the cost row with its preferred flag ------------------------------

    /// Alice creates the supplier, Bob renames it, Alice deactivates it.
    /// The row names exactly the actor of the last change, never a fresh one.
    #[tokio::test]
    async fn ac18_a_supplier_records_two_different_actors() {
        let (s, pool) = svc().await;
        let creator = test_support::seed_audit_user(&pool, "sup-alice", "Alice")
            .await
            .unwrap();
        let editor = test_support::seed_audit_user(&pool, "sup-bob", "Bob")
            .await
            .unwrap();

        let supplier = s
            .create_supplier(
                creator,
                NewSupplier {
                    name: "Audit Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(supplier.created_by, creator, "the creator");
        assert_eq!(supplier.updated_by, None, "a fresh supplier has no editor");

        let renamed = s
            .update_supplier(
                editor,
                supplier.id,
                UpdateSupplier {
                    name: Some("Renamed Supplier".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.created_by, creator, "the creator never changes");
        assert_eq!(
            renamed.updated_by,
            Some(editor),
            "the edit names its editor"
        );

        // Deactivation is an edit too: the toggle carries the acting user.
        let deactivated = s.set_active(creator, supplier.id, false).await.unwrap();
        assert_eq!(deactivated.created_by, creator);
        assert_eq!(
            deactivated.updated_by,
            Some(creator),
            "the toggle names its actor"
        );
    }

    /// The cost row records, shifts and refreshes with the requesting actors,
    /// and the preferred flag's promotion carries its actor. All of them are
    /// writes by the request that caused them (AC18).
    #[tokio::test]
    async fn ac18_the_supplier_cost_and_its_preferred_flag_carry_their_actors() {
        let (s, pool) = svc().await;
        let alice = test_support::seed_audit_user(&pool, "cost-alice", "Alice")
            .await
            .unwrap();
        let bob = test_support::seed_audit_user(&pool, "cost-bob", "Bob")
            .await
            .unwrap();

        let product_id = seed_product(&pool, "COST AUD", "5").await;
        let supplier = s
            .create_supplier(
                bob,
                NewSupplier {
                    name: "Cost Audit Supplier".into(),
                    phone: None,
                    notes: None,
                    due_days: None,
                },
            )
            .await
            .unwrap();

        // Alice records the first cost: the row names her, no editor yet.
        let created = s
            .record_cost(alice, product_id, supplier.id, dec("10"), d(2024, 5, 1))
            .await
            .unwrap();
        assert_eq!(created.created_by, alice, "the recording request's actor");
        assert_eq!(created.updated_by, None, "a fresh cost row has no editor");

        // Bob shifts the price: same row, his actor in updated_by.
        let shifted = s
            .record_cost(bob, product_id, supplier.id, dec("12"), d(2024, 5, 10))
            .await
            .unwrap();
        assert_eq!(shifted.created_by, alice, "the creator never changes");
        assert_eq!(shifted.updated_by, Some(bob), "the shift names its writer");

        // Alice confirms the same price (refresh): the refresh is a write too.
        let refreshed = s
            .record_cost(alice, product_id, supplier.id, dec("12"), d(2024, 5, 20))
            .await
            .unwrap();
        assert_eq!(
            refreshed.updated_by,
            Some(alice),
            "the refresh names its writer"
        );

        // Bob prefers this supplier: the promoted row names him; the demoted
        // one (none here) would name him too.
        let preferred = s.set_preferred(bob, product_id, supplier.id).await.unwrap();
        assert_eq!(preferred.is_preferred, true);
        assert_eq!(
            preferred.updated_by,
            Some(bob),
            "the promotion names its writer"
        );

        // Alice clears it: the demoted row names her, like any edit.
        s.clear_preferred(alice, product_id).await.unwrap();
        let cleared = s.find_cost(product_id, supplier.id).await.unwrap().unwrap();
        assert_eq!(cleared.is_preferred, false);
        assert_eq!(
            cleared.updated_by,
            Some(alice),
            "the demotion names its writer"
        );
    }

    // -- picker reads (the supplier half of the product picker) --------------

    #[tokio::test]
    async fn supplier_search_matches_name_fragments_case_and_diacritic_insensitively() {
        let (s, _pool) = svc().await;
        let perez = seed_supplier(&s, "Pérez & Hijos").await;
        let acme = seed_supplier(&s, "ACME Distribución").await;
        seed_supplier(&s, "Unrelated Supplier").await;

        // Name fragment, case-insensitive.
        let by_fragment = s.search_suppliers("perez").await.unwrap();
        assert_eq!(by_fragment.len(), 1, "fragments match by name");
        assert_eq!(by_fragment[0].id, perez.id);

        // Diacritics fold the same way the product search folds them.
        let by_diacritic = s.search_suppliers("distribucion").await.unwrap();
        assert_eq!(by_diacritic.len(), 1);
        assert_eq!(by_diacritic[0].id, acme.id);

        // No match is an empty list, not an error.
        let none = s.search_suppliers("nothing-here").await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn supplier_search_reports_whether_each_match_is_active() {
        let (s, _pool) = svc().await;
        let live = seed_supplier(&s, "Live Supplier").await;
        let gone = seed_supplier(&s, "Gone Supplier").await;
        s.set_active(audit_actor(&s).await, gone.id, false)
            .await
            .unwrap();

        let matches = s.search_suppliers("supplier").await.unwrap();
        let ids: Vec<(i64, bool)> = matches.iter().map(|x| (x.id, x.is_active)).collect();
        assert_eq!(
            ids,
            vec![(gone.id, false), (live.id, true)],
            "both matches come back and each carries its own active flag"
        );
    }

    #[tokio::test]
    async fn supplier_search_is_bounded_and_the_empty_query_is_not_a_search() {
        let (s, _pool) = svc().await;
        for i in 0..(Svc::SUPPLIER_SEARCH_LIMIT + 3) {
            seed_supplier(&s, &format!("Bound Supplier {i:02}")).await;
        }

        let bounded = s.search_suppliers("bound supplier").await.unwrap();
        assert_eq!(
            bounded.len() as usize,
            Svc::SUPPLIER_SEARCH_LIMIT,
            "the picker never returns more than its bound"
        );

        // An empty or whitespace query is not a search: it returns the empty
        // result shape rather than the whole roster.
        assert!(s.search_suppliers("").await.unwrap().is_empty());
        assert!(s.search_suppliers("   ").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_supplier_name_resolves_the_exact_name_case_insensitively() {
        let (s, _pool) = svc().await;
        let perez = seed_supplier(&s, "Pérez & Hijos").await;
        seed_supplier(&s, "ACME Distribución").await;

        // Exact name, case- and diacritic-insensitive.
        let resolved = s.resolve_supplier_name("perez & hijos").await.unwrap();
        assert_eq!(resolved.id, perez.id);
        let resolved_upper = s.resolve_supplier_name("PÉREZ & HIJOS").await.unwrap();
        assert_eq!(resolved_upper.id, perez.id);
    }

    #[tokio::test]
    async fn resolve_supplier_name_refuses_an_unknown_name_naming_the_value() {
        let (s, _pool) = svc().await;
        seed_supplier(&s, "Pérez & Hijos").await;

        let err = s
            .resolve_supplier_name("Missing Supplier")
            .await
            .unwrap_err();
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("Missing Supplier"),
                    "the refusal must name the value the caller typed: {msg}"
                );
            }
            other => panic!("an unknown name is a 400 Validation, not {other:?}"),
        }

        // The empty value is the required-field refusal, not a roster dump.
        assert!(s.resolve_supplier_name("").await.is_err());
        assert!(s.resolve_supplier_name("   ").await.is_err());
    }

    #[tokio::test]
    async fn resolve_supplier_name_refuses_partial_and_ambiguous_look_alikes() {
        let (s, _pool) = svc().await;
        let perez = seed_supplier(&s, "Pérez & Hijos").await;
        seed_supplier(&s, "Pérez & Hermanos").await;

        // A fragment that matches but is not an exact name refuses: the
        // picker, not a guess, decides which supplier it is.
        let partial = s.resolve_supplier_name("Pérez").await.unwrap_err();
        match partial {
            AppError::Validation(msg) => {
                assert!(msg.contains("Pérez"), "the refusal names the value: {msg}");
            }
            other => panic!("a non-exact name is a 400 Validation, not {other:?}"),
        }

        // Even the exact prefix of another name refuses — only the exact
        // name resolves.
        assert!(s.resolve_supplier_name("Pérez &").await.is_err());

        // The exact name itself still resolves past its look-alike.
        let resolved = s.resolve_supplier_name("Pérez & Hijos").await.unwrap();
        assert_eq!(resolved.id, perez.id);
    }

    // The straddle defect: `search_suppliers` truncates to the picker's
    // bound, so with more than SUPPLIER_SEARCH_LIMIT partial matches two
    // normalized-equal names can straddle that bound — one inside is then
    // silently resolved while its look-alike sits outside. Resolution must
    // read past the bound and refuse.
    #[tokio::test]
    async fn resolve_supplier_name_refuses_colliding_look_alikes_that_straddle_the_picker_bound() {
        let (s, _pool) = svc().await;
        // Both colliders normalize to "perez & hijos" and every seeded name
        // contains that fragment. Byte order puts the uppercase collider
        // first and the ten longer partials between the two, so the bound
        // hides the lowercase look-alike.
        let perez_upper = seed_supplier(&s, "Pérez & Hijos").await;
        let perez_lower = seed_supplier(&s, "perez & hijos").await;
        assert_ne!(perez_upper.id, perez_lower.id);
        for i in 1..=Svc::SUPPLIER_SEARCH_LIMIT {
            seed_supplier(&s, &format!("Pérez & Hijos Norte {i:02}")).await;
        }

        let refused = s.resolve_supplier_name("perez & hijos").await.unwrap_err();
        match refused {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("same spelling"),
                    "two normalized-equal names must refuse as look-alikes, not pick one: {msg}"
                );
            }
            other => panic!("a colliding name is a 400 Validation, not {other:?}"),
        }
    }

    // The mirror case: an exact name that exists must still resolve when
    // more than SUPPLIER_SEARCH_LIMIT partial matches push it past the
    // picker's bound — "no exact match" would be a lie.
    #[tokio::test]
    async fn resolve_supplier_name_resolves_an_exact_name_beyond_the_picker_bound() {
        let (s, _pool) = svc().await;
        // Byte order sorts the exact name (lowercase 'p') after every
        // uppercase 'A' partial, so the bound truncates it away.
        let perez = seed_supplier(&s, "perez & hijos").await;
        for i in 1..=(Svc::SUPPLIER_SEARCH_LIMIT + 1) {
            seed_supplier(&s, &format!("AA Pérez & Hijos {i:02}")).await;
        }

        let resolved = s.resolve_supplier_name("perez & hijos").await.unwrap();
        assert_eq!(resolved.id, perez.id);
    }
}
