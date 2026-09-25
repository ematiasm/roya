// M1 inventory service: domain validations over repository traits.
// Boundary: never touches finance tables; stock is derived SUM in Rust.
use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    Category, MovementReason, MovementType, NewMovement, NewProduct, Product, ProductBarcode,
    ProductKind, ProductStock, StockMovement, UpdateProduct,
};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, StockMovementRepository,
};

#[derive(Clone)]
pub struct InventoryService<C, P, B, S>
where
    C: CategoryRepository,
    P: ProductRepository,
    B: BarcodeRepository,
    S: StockMovementRepository,
{
    pub categories: C,
    pub products: P,
    pub barcodes: B,
    pub movements: S,
    pub allow_negative_stock: bool,
}

impl<C, P, B, S> InventoryService<C, P, B, S>
where
    C: CategoryRepository,
    P: ProductRepository,
    B: BarcodeRepository,
    S: StockMovementRepository,
{
    pub fn new(categories: C, products: P, barcodes: B, movements: S, allow: bool) -> Self {
        Self {
            categories,
            products,
            barcodes,
            movements,
            allow_negative_stock: allow,
        }
    }

    // -- categories ---------------------------------------------------------

    fn validate_category_name(name: &str) -> AppResult<String> {
        let t = name.trim();
        if t.is_empty() {
            return Err(AppError::Validation("category name cannot be empty".into()));
        }
        if t.chars().count() > 128 {
            return Err(AppError::Validation(
                "category name must be <= 128 chars".into(),
            ));
        }
        Ok(t.to_string())
    }

    pub async fn create_category(
        &self,
        actor: i64,
        name: &str,
        parent_id: Option<i64>,
    ) -> AppResult<Category> {
        let clean = Self::validate_category_name(name)?;
        if let Some(pid) = parent_id {
            if !self.categories.exists(pid).await? {
                return Err(AppError::NotFound(format!("category {pid} not found")));
            }
        }
        if self
            .categories
            .find_by_parent_and_name(parent_id, &clean)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "category already exists under this parent".into(),
            ));
        }
        self.categories.create(actor, &clean, parent_id).await
    }

    /// `actor` is the audit actor of the editing request (M5 Phase B, slice
    /// S10): it lands on `updated_by` while the row's creator stays recorded.
    pub async fn update_category(
        &self,
        actor: i64,
        id: i64,
        name: Option<&str>,
        parent_id: Option<Option<i64>>,
    ) -> AppResult<Category> {
        let existing = self
            .categories
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("category {id} not found")))?;

        let new_name = match name {
            Some(n) => Self::validate_category_name(n)?,
            None => existing.name.clone(),
        };
        let new_parent = match parent_id {
            Some(inner) => inner,
            None => existing.parent_id,
        };

        if let Some(pid) = new_parent {
            if pid == id {
                return Err(AppError::Validation(
                    "category cannot be its own parent".into(),
                ));
            }
            if !self.categories.exists(pid).await? {
                return Err(AppError::NotFound(format!("category {pid} not found")));
            }
            // Ancestor walk (depth-bounded): reject descendant-parent cycles.
            let mut cursor = Some(pid);
            for _ in 0..100 {
                let cur = cursor.unwrap();
                if cur == id {
                    return Err(AppError::Validation("category cycle detected".into()));
                }
                let rec = self.categories.find_by_id(cur).await?;
                match rec.and_then(|c| c.parent_id) {
                    Some(up) => cursor = Some(up),
                    None => break,
                }
            }
            if cursor.is_some()
                && self
                    .categories
                    .find_by_id(cursor.unwrap())
                    .await?
                    .and_then(|c| c.parent_id)
                    .is_some()
            {
                // Walk exceeded depth without reaching a root: treat as invalid.
                // (Only reachable for pathological chains >= 100 deep.)
                return Err(AppError::Validation("category tree too deep".into()));
            }
        }

        if new_name != existing.name || new_parent != existing.parent_id {
            if let Some(other) = self
                .categories
                .find_by_parent_and_name(new_parent, &new_name)
                .await?
            {
                if other.id != id {
                    return Err(AppError::Conflict(
                        "category already exists under this parent".into(),
                    ));
                }
            }
        }

        self.categories
            .update(actor, id, &new_name, new_parent)
            .await
    }

    pub async fn delete_category(&self, id: i64) -> AppResult<()> {
        if self.categories.find_by_id(id).await?.is_none() {
            return Err(AppError::NotFound(format!("category {id} not found")));
        }
        if self.categories.count_children(id).await? > 0 {
            return Err(AppError::Validation(
                "cannot delete category with child categories".into(),
            ));
        }
        if self.products.count_by_category(id).await? > 0 {
            return Err(AppError::Validation(
                "cannot delete category with products".into(),
            ));
        }
        let deleted = self.categories.delete(id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!("category {id} not found")));
        }
        Ok(())
    }

    // -- products -----------------------------------------------------------

    /// Pins a markup-derived price to cents with half-up rounding
    /// (`RoundingStrategy::MidpointAwayFromZero` — half-up away from zero,
    /// retail convention; rust_decimal names it RoundingStrategy, not
    /// RoundingMode).
    ///
    /// This is deliberately the project's FIRST rounding helper. Until now
    /// every money operation was exact: multiplying an exact quantity by an
    /// exact price never produced a third decimal, so no `round_dp` existed
    /// anywhere. Deriving a price from a percentage is the first operation
    /// that can (80.00 * 1.3333 = 106.6640), so the derived value is pinned to
    /// cents here and only here. Manual prices keep the exact value the caller
    /// sent — never round those.
    fn round_derived_price_to_cents(price: Decimal) -> Decimal {
        use rust_decimal::RoundingStrategy;
        price.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
    }

    async fn validate_product(&self, input: NewProduct) -> AppResult<NewProduct> {
        let sku = input.sku.trim();
        if sku.is_empty() {
            return Err(AppError::Validation("sku cannot be empty".into()));
        }
        if sku.chars().count() > 64 {
            return Err(AppError::Validation("sku must be <= 64 chars".into()));
        }
        let name = input.name.trim();
        if name.is_empty() {
            return Err(AppError::Validation("product name cannot be empty".into()));
        }
        if name.chars().count() > 128 {
            return Err(AppError::Validation(
                "product name must be <= 128 chars".into(),
            ));
        }
        let unit = input.unit.trim();
        if unit.is_empty() || unit.chars().count() > 16 {
            return Err(AppError::Validation("unit must be 1..16 chars".into()));
        }
        // Markup-derived pricing (product-markup T4): when `markup_pct` is
        // set the sale price is DERIVED from cost and the incoming
        // `sale_price` is ignored. When it is `None` the price is manual and
        // everything below behaves exactly as before. Clearing a markup
        // (`Some(None)`) reaches this function as `None` after the update
        // merge, so the price keeps its last stored value and becomes manual
        // again — nothing reverts to any earlier price.
        let (sale_price, markup_pct) = match input.markup_pct {
            None => (input.sale_price, None),
            Some(m) => {
                if m <= Decimal::from(-100) {
                    return Err(AppError::Validation("markup_pct must be > -100".into()));
                }
                // `cost_price` is NOT NULL DEFAULT '0': "no cost" manifests
                // as 0, not NULL. A zero (or negative) cost must be rejected
                // instead of silently deriving a free price.
                if input.cost_price <= Decimal::ZERO {
                    return Err(AppError::Validation(
                        "cost_price must be > 0 when markup_pct is set".into(),
                    ));
                }
                // sale_price = cost_price * (1 + markup_pct/100). The
                // percentage's scale shift is a multiplication by 0.01,
                // because this project never divides a `Decimal`.
                //
                // Every operand here is user-supplied and unbounded, and
                // rust_decimal's `Mul`/`Add` PANIC on overflow, so the bare
                // operators would let an authenticated caller 500 the
                // handler. The checked forms turn the same inputs into a
                // validation error instead. There is deliberately no upper
                // bound on `markup_pct`: whether a markup is plausible is a
                // product decision, not an arithmetic one.
                let factor = m
                    .checked_mul(Decimal::new(1, 2))
                    .and_then(|shift| Decimal::ONE.checked_add(shift))
                    .and_then(|f| input.cost_price.checked_mul(f));
                let derived = match factor {
                    Some(f) => Self::round_derived_price_to_cents(f),
                    None => {
                        return Err(AppError::Validation(
                            "markup_pct or cost_price is too large to derive a sale_price".into(),
                        ));
                    }
                };
                (derived, Some(m))
            }
        };
        // The price rule applies to the EFFECTIVE price: the derived one when
        // markup is set, the incoming one otherwise. A caller that supplies a
        // markup must not also be forced to send a meaningful sale_price.
        match input.kind {
            ProductKind::Product => {
                if sale_price <= Decimal::ZERO {
                    return Err(AppError::Validation(
                        "sale_price must be > 0 for products".into(),
                    ));
                }
            }
            ProductKind::Service => {
                if sale_price < Decimal::ZERO {
                    return Err(AppError::Validation("sale_price cannot be negative".into()));
                }
            }
        }
        if input.cost_price < Decimal::ZERO {
            return Err(AppError::Validation("cost_price cannot be negative".into()));
        }
        if let Some(cid) = input.category_id {
            if !self.categories.exists(cid).await? {
                return Err(AppError::NotFound(format!("category {cid} not found")));
            }
        }

        if input.kind == ProductKind::Service {
            if input.track_stock {
                return Err(AppError::Validation("services cannot track stock".into()));
            }
            if input.min_stock.is_some() || input.max_stock.is_some() {
                return Err(AppError::Validation(
                    "services cannot have min/max stock".into(),
                ));
            }
        } else if input.track_stock {
            let (Some(min), Some(max)) = (input.min_stock, input.max_stock) else {
                return Err(AppError::Validation(
                    "tracked products require min_stock and max_stock".into(),
                ));
            };
            if min < Decimal::ZERO {
                return Err(AppError::Validation("min_stock must be >= 0".into()));
            }
            if max < min {
                return Err(AppError::Validation(
                    "max_stock must be >= min_stock".into(),
                ));
            }
        } else if input.min_stock.is_some() || input.max_stock.is_some() {
            return Err(AppError::Validation(
                "min/max stock require track_stock".into(),
            ));
        }

        let location = match input.location {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    if t.chars().count() > 64 {
                        return Err(AppError::Validation("location must be <= 64 chars".into()));
                    }
                    Some(t.to_string())
                }
            }
            None => None,
        };
        let notes = match input.notes {
            Some(s) => {
                if s.chars().count() > 1024 {
                    return Err(AppError::Validation("notes must be <= 1024 chars".into()));
                }
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            None => None,
        };

        Ok(NewProduct {
            sku: sku.to_string(),
            name: name.to_string(),
            kind: input.kind,
            category_id: input.category_id,
            unit: unit.to_string(),
            // The effective values: the derived price when markup is set, the
            // incoming manual price otherwise. This return value is what gets
            // persisted, so a derived price computed but not fed back here
            // would be silently dropped.
            sale_price,
            cost_price: input.cost_price,
            markup_pct,
            track_stock: input.track_stock,
            min_stock: input.min_stock,
            max_stock: input.max_stock,
            location,
            notes,
        })
    }

    /// `actor` is the audit actor of the creating request (M5 Phase B, slice
    /// S10): the acting user's id from the `Principal`, threaded down, never
    /// invented here.
    pub async fn create_product(&self, actor: i64, input: NewProduct) -> AppResult<Product> {
        let clean = self.validate_product(input).await?;
        if self.products.find_by_sku(&clean.sku).await?.is_some() {
            return Err(AppError::Conflict("sku already exists".into()));
        }
        self.products.create(actor, &clean).await
    }

    /// Patch edit over the current row. The patch merges onto the loaded product
    /// (`None` = leave unchanged; `Some(None)` clears), then the merged row runs
    /// the same `validate_product` rules as create, so an edit can never bypass a
    /// business rule. The only edit-specific check is the duplicate SKU, done
    /// against other rows only so re-sending the same SKU is not a conflict.
    /// Because the merged row re-derives the price, a patch that only changes
    /// `cost_price` while `markup_pct` is set recomputes `sale_price` — a
    /// sister feature relies on exactly that path to keep the formula in one
    /// place.
    /// `actor` is the audit actor of the editing request: it lands on
    /// `updated_by` while `created_by` keeps the row's creator.
    pub async fn update_product(
        &self,
        actor: i64,
        id: i64,
        patch: UpdateProduct,
    ) -> AppResult<Product> {
        let current = self.get_product(id).await?;
        let merged = NewProduct {
            sku: patch.sku.unwrap_or_else(|| current.sku.clone()),
            name: patch.name.unwrap_or_else(|| current.name.clone()),
            kind: patch.kind.unwrap_or(current.kind),
            category_id: patch.category_id.unwrap_or(current.category_id),
            unit: patch.unit.unwrap_or_else(|| current.unit.clone()),
            sale_price: patch.sale_price.unwrap_or(current.sale_price),
            cost_price: patch.cost_price.unwrap_or(current.cost_price),
            // Double option: outer None = leave unchanged, Some(None) = clear
            // back to "no markup, manual price", Some(Some(v)) = set. NULL is a
            // real value, so the merge must preserve it, never default it.
            markup_pct: patch.markup_pct.unwrap_or(current.markup_pct),
            track_stock: patch.track_stock.unwrap_or(current.track_stock),
            min_stock: patch.min_stock.unwrap_or(current.min_stock),
            max_stock: patch.max_stock.unwrap_or(current.max_stock),
            location: patch.location.unwrap_or_else(|| current.location.clone()),
            notes: patch.notes.unwrap_or_else(|| current.notes.clone()),
        };
        let clean = self.validate_product(merged).await?;
        if clean.sku != current.sku {
            if let Some(other) = self.products.find_by_sku(&clean.sku).await? {
                if other.id != id {
                    return Err(AppError::Conflict("sku already exists".into()));
                }
            }
        }
        self.products.update(actor, id, &clean).await
    }

    /// A lifecycle toggle is a product update: `updated_by` carries the acting
    /// user the same way an edit does.
    pub async fn set_product_active(
        &self,
        actor: i64,
        id: i64,
        active: bool,
    ) -> AppResult<Product> {
        self.get_product(id).await?;
        self.products.set_active(actor, id, active).await
    }

    pub async fn delete_product(&self, id: i64) -> AppResult<()> {
        self.get_product(id).await?;
        if self.movements.count_by_product(id).await? > 0 {
            return Err(AppError::Validation(
                "cannot delete product with stock movements".into(),
            ));
        }
        // Barcodes cascade via FK; finance tables untouched.
        let deleted = self.products.delete(id).await?;
        if !deleted {
            return Err(AppError::NotFound(format!("product {id} not found")));
        }
        Ok(())
    }

    pub async fn get_product(&self, id: i64) -> AppResult<Product> {
        self.products
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product {id} not found")))
    }

    /// One stock movement by id — the documents drawer's per-movement read.
    /// A thin wrapper over the repository's own `find_by_id`; an unknown id is
    /// the standard `NotFound`, naming the family.
    pub async fn get_movement(&self, id: i64) -> AppResult<StockMovement> {
        self.movements
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("stock movement {id} not found")))
    }

    // -- picker reads (N4) --------------------------------------------------

    /// Upper bound for one picker search: enough choices without dumping the
    /// whole catalogue into a fragment.
    pub const PRODUCT_SEARCH_LIMIT: i64 = 10;

    /// Bounded read behind `GET /web/product-search.json`: normalized name, SKU and
    /// barcode matching over the small catalogue, with derived stock. The empty
    /// query is not a search and never returns the catalogue.
    pub async fn search_products(&self, query: &str) -> AppResult<Vec<ProductStock>> {
        let value = query.trim();
        if value.is_empty() {
            return Ok(Vec::new());
        }
        let products = self
            .match_catalogue(value, Some(Self::PRODUCT_SEARCH_LIMIT as usize))
            .await?;
        self.with_stock(products).await
    }

    /// Catalogue filter behind the products list (N5): the same normalized
    /// name/SKU/barcode matching the picker uses, without its row bound, combined
    /// with the existing category filter. An empty query contributes no constraint,
    /// so the list stays whole.
    pub async fn filter_products(
        &self,
        query: &str,
        category_id: Option<i64>,
    ) -> AppResult<Vec<ProductStock>> {
        let value = query.trim();
        let products = if value.is_empty() {
            self.products.list().await?
        } else {
            self.match_catalogue(value, None).await?
        };
        let products = match category_id {
            Some(cid) => products
                .into_iter()
                .filter(|product| product.category_id == Some(cid))
                .collect(),
            None => products,
        };
        self.with_stock(products).await
    }

    /// The one matching definition for the party and catalogue searches: fold both
    /// sides with `normalize_search` so case and Spanish diacritics do not matter,
    /// over the whole (small) catalogue fetched once. `limit` bounds the picker; the
    /// catalogue list passes `None`. If the catalogue ever stops being small, this
    /// needs a normalized index instead.
    async fn match_catalogue(&self, query: &str, limit: Option<usize>) -> AppResult<Vec<Product>> {
        let needle = crate::models::normalize_search(query);
        let products = self.products.list().await?;
        let by_id: std::collections::BTreeMap<i64, usize> = products
            .iter()
            .enumerate()
            .map(|(index, product)| (product.id, index))
            .collect();
        let mut matched: std::collections::BTreeSet<usize> = products
            .iter()
            .enumerate()
            .filter(|(_, product)| {
                crate::models::normalize_search(&product.name).contains(&needle)
                    || crate::models::normalize_search(&product.sku).contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        for barcode in self.products.list_barcodes().await? {
            if crate::models::normalize_search(&barcode.code).contains(&needle) {
                if let Some(index) = by_id.get(&barcode.product_id) {
                    matched.insert(*index);
                }
            }
        }
        let mut out: Vec<Product> = matched
            .into_iter()
            .map(|index| products[index].clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        if let Some(limit) = limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// Derive stock and the reorder suggestion for a product list. Shared by the
    /// picker search and the catalogue filter so both read stock the same way.
    async fn with_stock(&self, products: Vec<Product>) -> AppResult<Vec<ProductStock>> {
        let mut out = Vec::with_capacity(products.len());
        for product in products {
            let stock = self.movements.stock_for_product(product.id).await?;
            let suggested = Self::suggestion_for(&product, stock);
            out.push(ProductStock {
                product,
                stock,
                suggested,
            });
        }
        Ok(out)
    }

    /// Resolve one typed or scanned value for a line: exact barcode, then exact
    /// SKU (case-insensitive), then a numeric id. That is what makes an exact scan
    /// a single step with no selection.
    ///
    /// Nothing exact is a 400 naming how many partial matches the picker search
    /// found, so the user can pick one from the list. Generic on purpose: the
    /// purchase record page reuses it unchanged.
    pub async fn resolve_product_ref(&self, raw: &str) -> AppResult<Product> {
        let value = raw.trim();
        if value.is_empty() {
            return Err(AppError::Validation("product is required".into()));
        }
        if let Some(barcode) = self.barcodes.find_by_code(value).await? {
            if let Some(product) = self.products.find_by_id(barcode.product_id).await? {
                return Ok(product);
            }
        }
        if let Some(product) = self.products.find_by_sku_ci(value).await? {
            return Ok(product);
        }
        if let Ok(id) = value.parse::<i64>() {
            if let Some(product) = self.products.find_by_id(id).await? {
                return Ok(product);
            }
        }
        let matches = self.search_products(value).await?;
        let noun = if matches.len() == 1 {
            "match"
        } else {
            "matches"
        };
        Err(AppError::Validation(format!(
            "no exact match for \"{value}\" — the search found {} {noun}; pick one from the list",
            matches.len()
        )))
    }

    // -- barcodes -----------------------------------------------------------

    pub async fn add_barcode(&self, product_id: i64, code: &str) -> AppResult<ProductBarcode> {
        let clean = code.trim();
        if clean.is_empty() || clean.chars().count() > 64 {
            return Err(AppError::Validation("barcode must be 1..64 chars".into()));
        }
        self.get_product(product_id).await?;
        if self.barcodes.find_by_code(clean).await?.is_some() {
            return Err(AppError::Conflict("barcode already exists".into()));
        }
        self.barcodes.create(product_id, clean).await
    }

    // -- movements ----------------------------------------------------------

    fn signed_delta(t: MovementType, qty: Decimal) -> Decimal {
        match t {
            MovementType::In => qty,
            MovementType::Out => -qty,
            MovementType::Adjust => qty,
        }
    }

    /// `actor` is the audit actor of the request recording the movement (M5
    /// Phase B, slice S10): for a movement produced inside a sale or purchase
    /// confirm, the flow passes ITS request's actor down — the same argument
    /// that stamps the finance rows — so the movement never records a fresh
    /// actor (AC18).
    pub async fn record_movement(
        &self,
        actor: i64,
        input: NewMovement,
    ) -> AppResult<StockMovement> {
        match input.movement_type {
            MovementType::In | MovementType::Out => {
                if input.qty <= Decimal::ZERO {
                    return Err(AppError::Validation("qty must be > 0".into()));
                }
            }
            MovementType::Adjust => {
                if input.qty == Decimal::ZERO {
                    return Err(AppError::Validation("adjust qty cannot be zero".into()));
                }
            }
        }
        if input.reference.chars().count() > 256 {
            return Err(AppError::Validation(
                "reference must be <= 256 chars".into(),
            ));
        }
        let product = self
            .products
            .find_by_id(input.product_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product {} not found", input.product_id)))?;
        if !product.is_active {
            return Err(AppError::Validation("product is inactive".into()));
        }
        if product.kind == ProductKind::Service || !product.track_stock {
            return Err(AppError::Validation(
                "cannot record movement for service or untracked product".into(),
            ));
        }

        let delta = Self::signed_delta(input.movement_type, input.qty);
        if !self.allow_negative_stock && delta < Decimal::ZERO {
            let current = self.movements.stock_for_product(product.id).await?;
            if current + delta < Decimal::ZERO {
                return Err(AppError::Validation(format!(
                    "insufficient stock: {current} would become {}",
                    current + delta
                )));
            }
        }

        self.movements.create(actor, &input).await
    }

    // -- derived ------------------------------------------------------------

    fn suggestion_for(product: &Product, stock: Decimal) -> Option<Decimal> {
        if !product.track_stock {
            return None;
        }
        match (product.min_stock, product.max_stock) {
            (Some(min), Some(max)) if stock <= min => Some(max - stock),
            _ => None,
        }
    }

    pub async fn stock(&self, product_id: i64) -> AppResult<Decimal> {
        self.get_product(product_id).await?;
        self.movements.stock_for_product(product_id).await
    }

    pub async fn product_stock(&self, product_id: i64) -> AppResult<ProductStock> {
        let product = self.get_product(product_id).await?;
        let stock = self.movements.stock_for_product(product_id).await?;
        let suggested = Self::suggestion_for(&product, stock);
        Ok(ProductStock {
            product,
            stock,
            suggested,
        })
    }

    pub async fn low_stock(&self) -> AppResult<Vec<ProductStock>> {
        let all = self.products.list().await?;
        let mut out = Vec::new();
        for p in all {
            if !p.is_active || !p.track_stock {
                continue;
            }
            let (Some(min), Some(_)) = (p.min_stock, p.max_stock) else {
                continue;
            };
            let stock = self.movements.stock_for_product(p.id).await?;
            if stock <= min {
                let suggested = Self::suggestion_for(&p, stock);
                out.push(ProductStock {
                    product: p,
                    stock,
                    suggested,
                });
            }
        }
        Ok(out)
    }

    pub async fn negative_stock(&self) -> AppResult<Vec<ProductStock>> {
        let all = self.products.list().await?;
        let mut out = Vec::new();
        for p in all {
            if !p.is_active || !p.track_stock {
                continue;
            }
            let stock = self.movements.stock_for_product(p.id).await?;
            if stock < Decimal::ZERO {
                let suggested = Self::suggestion_for(&p, stock);
                out.push(ProductStock {
                    product: p,
                    stock,
                    suggested,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UpdateProduct;
    use crate::repositories::{
        SqliteBarcodeRepository, SqliteCategoryRepository, SqliteProductRepository,
        SqliteStockMovementRepository,
    };
    use crate::security::test_support;
    use rust_decimal::Decimal;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    type Svc = InventoryService<
        SqliteCategoryRepository,
        SqliteProductRepository,
        SqliteBarcodeRepository,
        SqliteStockMovementRepository,
    >;

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

    async fn svc(allow: bool) -> Svc {
        let pool = test_pool().await;
        InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            allow,
        )
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// The acting user for fixture writes: the migration's sentinel. The
    /// attribution tests create dedicated users instead, so two actors can be
    /// told apart.
    async fn actor(s: &Svc) -> i64 {
        crate::security::test_support::audit_actor_id(&s.products.pool)
            .await
            .unwrap()
    }

    fn product_input(sku: &str) -> NewProduct {
        NewProduct {
            sku: sku.to_string(),
            name: format!("prod {sku}"),
            kind: ProductKind::Product,
            category_id: None,
            unit: "un".to_string(),
            sale_price: dec("10"),
            cost_price: dec("5"),
            track_stock: true,
            min_stock: Some(dec("5")),
            max_stock: Some(dec("50")),
            location: None,
            notes: None,
            markup_pct: None,
        }
    }

    fn movement(pid: i64, qty: &str, t: MovementType) -> NewMovement {
        NewMovement {
            product_id: pid,
            qty: dec(qty),
            movement_type: t,
            reason: MovementReason::Initial,
            reference: String::new(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
        }
    }

    #[tokio::test]
    async fn ac1_duplicate_sku_is_conflict() {
        let s = svc(true).await;
        s.create_product(actor(&s).await, product_input("SKU-1"))
            .await
            .unwrap();
        let err = s
            .create_product(actor(&s).await, product_input("SKU-1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac2_service_rejects_movements() {
        let s = svc(true).await;
        let svc_prod = NewProduct {
            kind: ProductKind::Service,
            track_stock: false,
            min_stock: None,
            max_stock: None,
            ..product_input("SRV-1")
        };
        let created = s.create_product(actor(&s).await, svc_prod).await.unwrap();
        assert!(!created.track_stock);
        let err = s
            .record_movement(actor(&s).await, movement(created.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac3_bad_qty_and_unknown_product() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("AC3"))
            .await
            .unwrap();
        let err = s
            .record_movement(actor(&s).await, movement(p.id, "0", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .record_movement(actor(&s).await, movement(99999, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac4_category_cycle_rejected() {
        let s = svc(true).await;
        let root = s
            .create_category(actor(&s).await, "root", None)
            .await
            .unwrap();
        let child = s
            .create_category(actor(&s).await, "child", Some(root.id))
            .await
            .unwrap();
        // descendant-parent cycle
        let err = s
            .update_category(actor(&s).await, root.id, None, Some(Some(child.id)))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // self-parent cycle
        let err = s
            .update_category(actor(&s).await, root.id, None, Some(Some(root.id)))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac5_delete_nonempty_category_blocked() {
        let s = svc(true).await;
        let root = s.create_category(actor(&s).await, "r", None).await.unwrap();
        s.create_category(actor(&s).await, "c", Some(root.id))
            .await
            .unwrap();
        let err = s.delete_category(root.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let leaf = s
            .create_category(actor(&s).await, "leaf", None)
            .await
            .unwrap();
        let mut inp = product_input("CAT-P");
        inp.category_id = Some(leaf.id);
        s.create_product(actor(&s).await, inp).await.unwrap();
        let err = s.delete_category(leaf.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let empty = s
            .create_category(actor(&s).await, "empty", None)
            .await
            .unwrap();
        s.delete_category(empty.id).await.unwrap();
    }

    #[tokio::test]
    async fn ac6_strict_mode_blocks_negative() {
        let s = svc(false).await;
        let p = s
            .create_product(actor(&s).await, product_input("STRICT"))
            .await
            .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Purchase,
                ..movement(p.id, "5", MovementType::In)
            },
        )
        .await
        .unwrap();
        let err = s
            .record_movement(
                actor(&s).await,
                NewMovement {
                    reason: MovementReason::Sale,
                    ..movement(p.id, "10", MovementType::Out)
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(s.stock(p.id).await.unwrap(), dec("5"));
    }

    #[tokio::test]
    async fn ac7_permissive_mode_allows_negative_and_lists_it() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("PERM"))
            .await
            .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Purchase,
                ..movement(p.id, "5", MovementType::In)
            },
        )
        .await
        .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Sale,
                ..movement(p.id, "10", MovementType::Out)
            },
        )
        .await
        .unwrap();
        assert_eq!(s.stock(p.id).await.unwrap(), dec("-5"));
        let neg = s.negative_stock().await.unwrap();
        assert!(neg.iter().any(|ps| ps.product.id == p.id));
    }

    // -- triangulate: alternate/negative cases protecting the contract --

    #[tokio::test]
    async fn tri_stock_sums_signed_movements_and_suggests_reorder() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("SUM"))
            .await
            .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Initial,
                ..movement(p.id, "20", MovementType::In)
            },
        )
        .await
        .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Sale,
                ..movement(p.id, "8", MovementType::Out)
            },
        )
        .await
        .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Adjust,
                ..movement(p.id, "-2", MovementType::Adjust)
            },
        )
        .await
        .unwrap();
        // 20 - 8 - 2 = 10
        let ps = s.product_stock(p.id).await.unwrap();
        assert_eq!(ps.stock, dec("10"));
        assert!(ps.suggested.is_none());

        // Drop to low stock: 10 - 8 = 2 <= min(5) => suggested = max - stock = 48
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Sale,
                ..movement(p.id, "8", MovementType::Out)
            },
        )
        .await
        .unwrap();
        let ps = s.product_stock(p.id).await.unwrap();
        assert_eq!(ps.stock, dec("2"));
        assert_eq!(ps.suggested.unwrap(), dec("48"));
        let low = s.low_stock().await.unwrap();
        assert!(low.iter().any(|x| x.product.id == p.id));
    }

    #[tokio::test]
    async fn tri_duplicate_barcode_is_conflict() {
        let s = svc(true).await;
        let a = s
            .create_product(actor(&s).await, product_input("BC-A"))
            .await
            .unwrap();
        let b = s
            .create_product(actor(&s).await, product_input("BC-B"))
            .await
            .unwrap();
        s.add_barcode(a.id, "7790001").await.unwrap();
        let err = s.add_barcode(a.id, "7790001").await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        let err = s.add_barcode(b.id, "7790001").await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_product_delete_restrict_with_movements_cascade_barcodes() {
        let s = svc(true).await;
        let with_hist = s
            .create_product(actor(&s).await, product_input("DEL-H"))
            .await
            .unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Initial,
                ..movement(with_hist.id, "3", MovementType::In)
            },
        )
        .await
        .unwrap();
        let err = s.delete_product(with_hist.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Without movements delete succeeds and cascades barcodes.
        let plain = s
            .create_product(actor(&s).await, product_input("DEL-C"))
            .await
            .unwrap();
        let bc = s.add_barcode(plain.id, "CASCADE-1").await.unwrap();
        s.delete_product(plain.id).await.unwrap();
        assert!(s.get_product(plain.id).await.is_err());
        // Barcode row is gone via ON DELETE CASCADE.
        let pool = s.products.pool.clone();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM product_barcodes WHERE id = ?")
            .bind(bc.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);
    }

    #[tokio::test]
    async fn tri_service_and_untracked_validations() {
        let s = svc(true).await;
        // Service with min/max rejected.
        let bad = NewProduct {
            min_stock: Some(dec("1")),
            ..product_input("SRV-BAD")
        };
        let bad = NewProduct {
            kind: ProductKind::Service,
            track_stock: false,
            max_stock: None,
            ..bad
        };
        assert!(matches!(
            s.create_product(actor(&s).await, bad).await.unwrap_err(),
            AppError::Validation(_)
        ));
        // Tracked without min/max rejected.
        let bad = NewProduct {
            min_stock: None,
            max_stock: None,
            ..product_input("TRK-BAD")
        };
        assert!(matches!(
            s.create_product(actor(&s).await, bad).await.unwrap_err(),
            AppError::Validation(_)
        ));
        // Untracked product rejects movements.
        let untracked = NewProduct {
            track_stock: false,
            min_stock: None,
            max_stock: None,
            ..product_input("UNTRK")
        };
        let u = s.create_product(actor(&s).await, untracked).await.unwrap();
        let err = s
            .record_movement(actor(&s).await, movement(u.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Inactive product rejects movements.
        let p = s
            .create_product(actor(&s).await, product_input("INACT"))
            .await
            .unwrap();
        s.set_product_active(actor(&s).await, p.id, false)
            .await
            .unwrap();
        let err = s
            .record_movement(actor(&s).await, movement(p.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_root_duplicate_name_and_adjust_zero() {
        let s = svc(true).await;
        s.create_category(actor(&s).await, "dup", None)
            .await
            .unwrap();
        let err = s
            .create_category(actor(&s).await, "dup", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        // Same name under different parent is fine.
        let other = s
            .create_category(actor(&s).await, "other", None)
            .await
            .unwrap();
        s.create_category(actor(&s).await, "dup", Some(other.id))
            .await
            .unwrap();

        let p = s
            .create_product(actor(&s).await, product_input("ADJ0"))
            .await
            .unwrap();
        let err = s
            .record_movement(actor(&s).await, movement(p.id, "0", MovementType::Adjust))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_inventory_ops_touch_no_finance_rows() {
        let pool = test_pool().await;
        let s = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            true,
        );
        let p = s
            .create_product(actor(&s).await, product_input("NOFIN"))
            .await
            .unwrap();
        s.add_barcode(p.id, "NOFIN-BC").await.unwrap();
        s.record_movement(
            actor(&s).await,
            NewMovement {
                reason: MovementReason::Purchase,
                ..movement(p.id, "4", MovementType::In)
            },
        )
        .await
        .unwrap();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);
    }

    // -- N4: the picker search and line-value resolution ----------------------

    fn named(sku: &str, name: &str) -> NewProduct {
        NewProduct {
            name: name.to_string(),
            ..product_input(sku)
        }
    }

    /// AC8 (read side): one query path matches name, SKU and barcode, and the
    /// result carries derived stock.
    #[tokio::test]
    async fn n4_search_matches_name_sku_and_barcode() {
        let s = svc(true).await;
        let yerba = s
            .create_product(actor(&s).await, named("YERBA-500", "Yerba Mate"))
            .await
            .unwrap();
        let galletas = s
            .create_product(actor(&s).await, named("GAL-10", "Galletitas"))
            .await
            .unwrap();
        s.add_barcode(yerba.id, "7790000000017").await.unwrap();
        s.record_movement(actor(&s).await, movement(yerba.id, "7", MovementType::In))
            .await
            .unwrap();

        let by_name = s.search_products("yerba").await.unwrap();
        assert_eq!(by_name.len(), 1, "name match: {by_name:?}");
        assert_eq!(by_name[0].product.id, yerba.id);
        assert_eq!(by_name[0].stock, dec("7"), "stock rides along");

        let by_sku = s.search_products("yerba-5").await.unwrap();
        assert_eq!(by_sku.len(), 1, "sku match: {by_sku:?}");
        assert_eq!(by_sku[0].product.id, yerba.id);

        // The barcode matches through the product_barcodes table.
        let by_barcode = s.search_products("7790000").await.unwrap();
        assert_eq!(by_barcode.len(), 1, "barcode match: {by_barcode:?}");
        assert_eq!(by_barcode[0].product.id, yerba.id);

        let other = s.search_products("galletitas").await.unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].product.id, galletas.id);
    }

    /// AC8 (negative): an empty query never dumps the catalogue, the result set
    /// is bounded, and LIKE wildcards in the typed value are literal text.
    #[tokio::test]
    async fn n4_search_is_bounded_and_empty_query_returns_nothing() {
        let s = svc(true).await;
        for i in 0..15 {
            s.create_product(
                actor(&s).await,
                named(&format!("BULK-{i:02}"), &format!("Bulk item {i}")),
            )
            .await
            .unwrap();
        }

        assert!(s.search_products("").await.unwrap().is_empty());
        assert!(s.search_products("   ").await.unwrap().is_empty());
        assert!(s.search_products("nothing-here").await.unwrap().is_empty());

        let bulk = s.search_products("bulk").await.unwrap();
        assert_eq!(
            bulk.len(),
            Svc::PRODUCT_SEARCH_LIMIT as usize,
            "the search endpoint must stay bounded"
        );

        // `%` is a user-typed character, not a wildcard: it must not match all.
        assert!(s.search_products("%").await.unwrap().is_empty());
    }

    /// AC9 (resolution): exact barcode, then exact SKU case-insensitive, then a
    /// numeric id, so an exact scan needs no selection step.
    #[tokio::test]
    async fn n4_resolve_prefers_barcode_then_sku_then_id() {
        let s = svc(true).await;
        let scanned = s
            .create_product(actor(&s).await, named("SCAN-SKU", "Scanned product"))
            .await
            .unwrap();
        s.add_barcode(scanned.id, "12345").await.unwrap();
        // Same text as the barcode on purpose: the barcode must win.
        let sku_twin = s
            .create_product(actor(&s).await, named("12345", "SKU twin"))
            .await
            .unwrap();

        assert_eq!(
            s.resolve_product_ref("12345").await.unwrap().id,
            scanned.id,
            "exact barcode wins"
        );

        let plain = s
            .create_product(actor(&s).await, named("PLAIN-9", "Plain product"))
            .await
            .unwrap();
        assert_eq!(
            s.resolve_product_ref("plain-9").await.unwrap().id,
            plain.id,
            "SKU match is case-insensitive"
        );

        assert_eq!(
            s.resolve_product_ref(&sku_twin.id.to_string())
                .await
                .unwrap()
                .id,
            sku_twin.id,
            "a numeric value resolves as an id when no barcode or SKU matches"
        );
    }

    /// AC12: a value that resolves to nothing names the partial matches the
    /// search found, so the user knows whether to pick or to fix the input.
    #[tokio::test]
    async fn n4_resolve_unknown_explains_the_search_match_count() {
        let s = svc(true).await;
        s.create_product(actor(&s).await, named("Y-500", "Yerba Mate"))
            .await
            .unwrap();
        s.create_product(actor(&s).await, named("Y-900", "Yerba Premium"))
            .await
            .unwrap();

        let err = s.resolve_product_ref("yerba").await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("no exact match"), "{msg}");
        assert!(msg.contains("2 matches"), "{msg}");

        let err = s.resolve_product_ref("missing").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("0 matches"), "{msg}");

        let err = s.resolve_product_ref("").await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "{err:?}");
    }

    // -- T1 redesign-products: the product edit path -------------------------

    /// A full patch changes every editable field, keeps id/is_active, and the
    /// returned row is what the service persisted.
    #[tokio::test]
    async fn update_product_full_edit_changes_every_field() {
        let s = svc(true).await;
        let cat = s
            .create_category(actor(&s).await, "edit-cat", None)
            .await
            .unwrap();
        let p = s
            .create_product(actor(&s).await, product_input("EDIT-1"))
            .await
            .unwrap();
        let patch = UpdateProduct {
            sku: Some("EDIT-2".into()),
            name: Some("Edited product".into()),
            kind: Some(ProductKind::Product),
            category_id: Some(Some(cat.id)),
            unit: Some("kg".into()),
            sale_price: Some(dec("20.50")),
            cost_price: Some(dec("8.25")),
            track_stock: Some(true),
            min_stock: Some(Some(dec("2"))),
            max_stock: Some(Some(dec("80"))),
            location: Some(Some("shelf 3".into())),
            notes: Some(Some("edited".into())),
            markup_pct: None,
        };
        let updated = s
            .update_product(actor(&s).await, p.id, patch)
            .await
            .unwrap();
        assert_eq!(updated.id, p.id);
        assert_eq!(updated.sku, "EDIT-2");
        assert_eq!(updated.name, "Edited product");
        assert_eq!(updated.kind, ProductKind::Product);
        assert_eq!(updated.category_id, Some(cat.id));
        assert_eq!(updated.unit, "kg");
        assert_eq!(updated.sale_price, dec("20.50"));
        assert_eq!(updated.cost_price, dec("8.25"));
        assert!(updated.track_stock);
        assert_eq!(updated.min_stock, Some(dec("2")));
        assert_eq!(updated.max_stock, Some(dec("80")));
        assert_eq!(updated.location.as_deref(), Some("shelf 3"));
        assert_eq!(updated.notes.as_deref(), Some("edited"));
        assert!(updated.is_active, "edit must not flip is_active");
    }

    /// The empty patch is a no-op: every stored field survives untouched.
    #[tokio::test]
    async fn update_product_default_patch_leaves_product_untouched() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("NOPATCH"))
            .await
            .unwrap();
        let before = s.get_product(p.id).await.unwrap();
        let updated = s
            .update_product(actor(&s).await, p.id, UpdateProduct::default())
            .await
            .unwrap();
        for field in [
            updated.sku == before.sku,
            updated.name == before.name,
            updated.kind == before.kind,
            updated.category_id == before.category_id,
            updated.unit == before.unit,
            updated.sale_price == before.sale_price,
            updated.cost_price == before.cost_price,
            updated.track_stock == before.track_stock,
            updated.min_stock == before.min_stock,
            updated.max_stock == before.max_stock,
            updated.location == before.location,
            updated.notes == before.notes,
            updated.is_active == before.is_active,
        ] {
            assert!(
                field,
                "an empty patch must change nothing: {before:?} -> {updated:?}"
            );
        }
    }

    /// Some(None) clears a nullable field; untracking then clearing min/max
    /// must satisfy (not violate) the tracked-products rule.
    #[tokio::test]
    async fn update_product_clears_min_max_when_untracked() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("CLR"))
            .await
            .unwrap();
        let updated = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    track_stock: Some(false),
                    min_stock: Some(None),
                    max_stock: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!updated.track_stock);
        assert_eq!(updated.min_stock, None);
        assert_eq!(updated.max_stock, None);
    }

    /// The cleaned SKU of another product is a Conflict; the same product's own
    /// SKU (even re-sent) is accepted.
    #[tokio::test]
    async fn update_product_sku_conflict_only_against_other_rows() {
        let s = svc(true).await;
        let a = s
            .create_product(actor(&s).await, product_input("UPD-A"))
            .await
            .unwrap();
        let b = s
            .create_product(actor(&s).await, product_input("UPD-B"))
            .await
            .unwrap();

        let err = s
            .update_product(
                actor(&s).await,
                a.id,
                UpdateProduct {
                    sku: Some("UPD-B".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");

        // Same product re-sending its own SKU while changing the name.
        let ok = s
            .update_product(
                actor(&s).await,
                b.id,
                UpdateProduct {
                    sku: Some("UPD-B".into()),
                    name: Some("Renamed B".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(ok.name, "Renamed B");
    }

    /// Every create rule still holds on edit: a service cannot track stock,
    /// max must be >= min, and a product needs sale_price > 0. A rejected
    /// patch leaves no partial write behind.
    #[tokio::test]
    async fn update_product_invalid_patch_is_validation() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("INV-P"))
            .await
            .unwrap();

        // Service that tracks stock.
        let err = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    kind: Some(ProductKind::Service),
                    track_stock: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // max < min (merged with the current min).
        let err = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    max_stock: Some(Some(dec("1"))),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // sale_price 0 for a product.
        let err = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    sale_price: Some(Decimal::ZERO),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // No failed patch left a partial write behind.
        let after = s.get_product(p.id).await.unwrap();
        assert_eq!(after.sku, "INV-P");
        assert_eq!(after.sale_price, dec("10"));
    }

    /// An unknown id is NotFound before any merge or write happens.
    #[tokio::test]
    async fn update_product_unknown_id_is_not_found() {
        let s = svc(true).await;
        let err = s
            .update_product(
                actor(&s).await,
                99999,
                UpdateProduct {
                    sku: Some("GHOST".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    // -- audit attribution (M5 Phase B, slice S10, AC18) --------------------------

    /// AC18 on the inventory surface: a product records who created it, and an
    /// edit records the editor WITHOUT erasing the creator. Two dedicated
    /// users make the assertion meaningful: the row carries the creator's id
    /// on `created_by` and the editor's on `updated_by`. Categories carry the
    /// same audit the same way.
    #[tokio::test]
    async fn ac18_inventory_create_and_update_store_two_different_actors() {
        let s = svc(true).await;
        let pool = s.products.pool.clone();
        let alice = test_support::seed_audit_user(&pool, "inv-alice", "Alice")
            .await
            .unwrap();
        let bob = test_support::seed_audit_user(&pool, "inv-bob", "Bob")
            .await
            .unwrap();

        let product = s
            .create_product(alice, product_input("AUDIT-P"))
            .await
            .unwrap();
        assert_eq!(product.created_by, alice, "the product records its creator");
        assert_eq!(product.updated_by, None);

        let updated = s
            .update_product(
                bob,
                product.id,
                UpdateProduct {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            updated.created_by, alice,
            "the creator attribution survives the edit"
        );
        assert_eq!(updated.updated_by, Some(bob), "the edit records the editor");

        // A lifecycle toggle is a product update: `updated_by` carries the
        // acting user the same way, without erasing the creator.
        let deactivated = s
            .set_product_active(alice, product.id, false)
            .await
            .unwrap();
        assert_eq!(
            deactivated.created_by, alice,
            "the creator is still recorded"
        );
        assert_eq!(
            deactivated.updated_by,
            Some(alice),
            "the toggle records its actor"
        );

        let category = s.create_category(alice, "Audit cat", None).await.unwrap();
        assert_eq!(category.created_by, alice);
        assert_eq!(category.updated_by, None);
        let renamed = s
            .update_category(bob, category.id, Some("Renamed cat"), None)
            .await
            .unwrap();
        assert_eq!(renamed.created_by, alice, "the creator survives the edit");
        assert_eq!(renamed.updated_by, Some(bob), "the edit records the editor");
    }

    /// AC18: a movement records the actor of the request that caused it, and
    /// the movement's actor is INDEPENDENT of the product's creator — a
    /// movement recorded by another user carries that user, never the
    /// product's creator and never a fresh actor.
    #[tokio::test]
    async fn ac18_a_movement_records_the_actor_of_the_request_that_caused_it() {
        let s = svc(true).await;
        let pool = s.products.pool.clone();
        let creator = test_support::seed_audit_user(&pool, "mv-product", "Product Op")
            .await
            .unwrap();
        let operator = test_support::seed_audit_user(&pool, "mv-stock", "Stock Op")
            .await
            .unwrap();

        let p = s
            .create_product(creator, product_input("MV-ACT"))
            .await
            .unwrap();
        let mv = s
            .record_movement(operator, movement(p.id, "5", MovementType::In))
            .await
            .unwrap();
        assert_eq!(
            mv.created_by, operator,
            "the movement records the acting user"
        );
        assert_ne!(
            mv.created_by, p.created_by,
            "the movement's actor is the request's, not the product's"
        );
        assert_eq!(mv.updated_by, None, "an append-only movement has no editor");
        let stored = s.movements.find_by_id(mv.id).await.unwrap().unwrap();
        assert_eq!(stored.created_by, operator);
    }

    /// `get_movement` is the read-by-id the documents drawer uses: found
    /// returns the stored movement, absent is the standard `NotFound` error,
    /// never a panic and never an empty default.
    #[tokio::test]
    async fn get_movement_returns_the_stored_row_or_not_found() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, product_input("GETMV"))
            .await
            .unwrap();
        let recorded = s
            .record_movement(actor(&s).await, movement(p.id, "5", MovementType::In))
            .await
            .unwrap();

        let found = s.get_movement(recorded.id).await.unwrap();
        assert_eq!(found.id, recorded.id);
        assert_eq!(found.product_id, p.id);
        assert_eq!(found.qty, dec("5"));
        assert_eq!(found.movement_type, MovementType::In);

        let missing = s.get_movement(999_999).await;
        assert!(
            matches!(&missing, Err(AppError::NotFound(msg)) if msg.contains("movement")),
            "an unknown movement must be NotFound naming the family: {missing:?}"
        );
    }

    // -- markup-derived pricing (product-markup T4) --------------------------

    /// A markup input over the standard fixture. The manual sale_price is
    /// deliberately absurd (999): whenever markup_pct is set the request price
    /// must be ignored, so a derived 10 proves the override, not a coincidence.
    fn markup_input(sku: &str, cost: &str, markup: &str) -> NewProduct {
        NewProduct {
            sale_price: dec("999"),
            cost_price: dec(cost),
            markup_pct: Some(dec(markup)),
            ..product_input(sku)
        }
    }

    #[tokio::test]
    async fn markup_derives_sale_price_from_cost() {
        let s = svc(true).await;
        // cost 5 with a 100% markup derives 5 * (1 + 100/100) = 10.
        let p = s
            .create_product(actor(&s).await, markup_input("MK-1", "5", "100"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("10"));
        assert_eq!(p.markup_pct, Some(dec("100")));
    }

    #[tokio::test]
    async fn markup_ignores_the_incoming_sale_price() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, markup_input("MK-2", "5", "100"))
            .await
            .unwrap();
        assert_eq!(
            p.sale_price,
            dec("10"),
            "the supplied 999 must be ignored when markup_pct is set"
        );
    }

    #[tokio::test]
    async fn markup_none_leaves_the_manual_price_alone() {
        let s = svc(true).await;
        let mut inp = product_input("MK-3");
        inp.sale_price = dec("7"); // cost stays 5: without markup nothing derives
        let p = s.create_product(actor(&s).await, inp).await.unwrap();
        assert_eq!(p.sale_price, dec("7"));
        assert_eq!(p.markup_pct, None);
        // A manual price patch still applies verbatim when markup is not set.
        let updated = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    sale_price: Some(dec("9")),
                    ..UpdateProduct::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.sale_price, dec("9"));
        assert_eq!(updated.markup_pct, None);
    }

    #[tokio::test]
    async fn clearing_markup_keeps_the_last_price_and_makes_it_manual() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, markup_input("MK-4", "5", "100"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("10"));
        // Clear the markup without touching sale_price: the price keeps its
        // last value (10) and becomes manual again; it does NOT revert.
        let cleared = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    markup_pct: Some(None),
                    ..UpdateProduct::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cleared.markup_pct, None);
        assert_eq!(cleared.sale_price, dec("10"));
        // Manual again: the price is now freely editable.
        let manual = s
            .update_product(
                actor(&s).await,
                cleared.id,
                UpdateProduct {
                    sale_price: Some(dec("3")),
                    ..UpdateProduct::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(manual.sale_price, dec("3"));
    }

    #[tokio::test]
    async fn markup_without_a_cost_is_rejected() {
        let s = svc(true).await;
        // "No cost" is 0 (cost_price is NOT NULL DEFAULT '0'), never a NULL.
        let err = s
            .create_product(actor(&s).await, markup_input("MK-5", "0", "50"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            s.products.find_by_sku("MK-5").await.unwrap().is_none(),
            "a rejected derivation must write nothing"
        );
    }

    #[tokio::test]
    async fn markup_at_or_below_minus_100_is_rejected() {
        let s = svc(true).await;
        for m in ["-100", "-150"] {
            let err = s
                .create_product(actor(&s).await, markup_input("MK-6", "5", m))
                .await
                .unwrap_err();
            assert!(
                matches!(err, AppError::Validation(_)),
                "markup {m}: {err:?}"
            );
            assert!(
                s.products.find_by_sku("MK-6").await.unwrap().is_none(),
                "a rejected derivation must write nothing"
            );
        }
    }

    /// The derivation multiplies and adds user-supplied unbounded Decimals,
    /// and rust_decimal's `Mul`/`Add` panic on overflow: without the checked
    /// forms an authenticated caller could 500 the handler with an extreme
    /// markup. The overflow must be a validation error instead, and a
    /// rejected derivation must write nothing. A panic fails these tests
    /// anyway, which is exactly what makes them discriminate.
    #[tokio::test]
    async fn an_overflowing_markup_derivation_is_a_validation_error_not_a_panic() {
        let s = svc(true).await;
        // A markup at the extreme end of the representable range: the factor
        // alone still fits (rust_decimal rescales intermediates, 5 * ~7.9e26
        // stays under the 7.9e28 cap), so the final `cost_price * factor` is
        // the step that overflows, with an ordinary cost of 1000.
        let err = s
            .create_product(
                actor(&s).await,
                markup_input("MK-OVF-1", "1000", "79228162514264337593543950335"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            s.products.find_by_sku("MK-OVF-1").await.unwrap().is_none(),
            "a rejected derivation must write nothing"
        );
    }

    /// The mirror case: an ordinary markup with a cost so large the final
    /// `cost_price * factor` overflows the representable range.
    #[tokio::test]
    async fn an_overflowing_cost_derivation_is_a_validation_error_not_a_panic() {
        let s = svc(true).await;
        let err = s
            .create_product(
                actor(&s).await,
                markup_input("MK-OVF-2", "79228162514264337593543950335", "50"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            s.products.find_by_sku("MK-OVF-2").await.unwrap().is_none(),
            "a rejected derivation must write nothing"
        );
    }

    #[tokio::test]
    async fn derived_price_is_rounded_to_cents() {
        let s = svc(true).await;
        // 80 * (1 + 33.33/100) = 106.664 unrounded; pinned to 106.66 half-up.
        let p = s
            .create_product(actor(&s).await, markup_input("MK-7", "80", "33.33"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("106.66"));
    }

    #[tokio::test]
    async fn patching_cost_price_recomputes_the_derived_price() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, markup_input("MK-8", "5", "100"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("10"));
        // Load-bearing path: a sister feature updates cost_price through a
        // plain cost patch precisely so this recomputation fires and the
        // formula stays in one place. The markup persists, the derived price
        // follows the new cost.
        let updated = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    cost_price: Some(dec("6")),
                    ..UpdateProduct::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.cost_price, dec("6"));
        assert_eq!(updated.markup_pct, Some(dec("100")));
        assert_eq!(updated.sale_price, dec("12"));
    }

    #[tokio::test]
    async fn a_failed_derivation_writes_nothing() {
        let s = svc(true).await;
        let p = s
            .create_product(actor(&s).await, markup_input("MK-9", "5", "100"))
            .await
            .unwrap();
        let err = s
            .update_product(
                actor(&s).await,
                p.id,
                UpdateProduct {
                    cost_price: Some(dec("0")),
                    ..UpdateProduct::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let stored = s.get_product(p.id).await.unwrap();
        assert_eq!(stored.cost_price, dec("5"));
        assert_eq!(stored.sale_price, dec("10"));
        assert_eq!(stored.markup_pct, Some(dec("100")));
    }

    // -- hardening: the gaps the independent verification found ----------------

    /// A derived price can round DOWN to zero from a positive cost. For a
    /// product that must be refused, never stored as a free price: the
    /// effective-price rule catches it before any write happens.
    #[tokio::test]
    async fn a_derived_price_that_rounds_to_zero_is_rejected_for_products() {
        let s = svc(true).await;
        // 0.01 * (1 + -99.5/100) = 0.00005, which rounds to 0.00.
        let err = s
            .create_product(actor(&s).await, markup_input("MK-10", "0.01", "-99.5"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert!(
            s.products.find_by_sku("MK-10").await.unwrap().is_none(),
            "a rejected derivation must write nothing"
        );
    }

    /// The same rounding IS legal for a Service, because the pre-existing rule
    /// allows a free service (`sale_price >= 0`). Pinned deliberately: the two
    /// kinds diverge here, and that is a decision, not an accident.
    #[tokio::test]
    async fn a_service_derived_price_may_round_to_zero() {
        let s = svc(true).await;
        let input = NewProduct {
            kind: ProductKind::Service,
            track_stock: false,
            min_stock: None,
            max_stock: None,
            ..markup_input("MK-11", "0.01", "-99.5")
        };
        let p = s.create_product(actor(&s).await, input).await.unwrap();
        assert_eq!(p.sale_price, dec("0"));
        assert_eq!(p.markup_pct, Some(dec("-99.5")));
    }

    /// Only the rejection boundary was covered. Just above it the markup is
    /// legal, and the derived price still has to satisfy the product rule.
    #[tokio::test]
    async fn markup_just_above_minus_100_is_accepted() {
        let s = svc(true).await;
        // 5 * (1 - 99/100) = 0.05, positive, so the product rule passes.
        let p = s
            .create_product(actor(&s).await, markup_input("MK-12", "5", "-99"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("0.05"));
        assert_eq!(p.markup_pct, Some(dec("-99")));
    }

    /// The price rule runs against the EFFECTIVE price, so a nonsense request
    /// price must not block a request that supplies a markup. The other
    /// override test sends a valid 999, which would pass either way.
    #[tokio::test]
    async fn a_nonsense_sale_price_is_ignored_when_markup_is_set() {
        let s = svc(true).await;
        let input = NewProduct {
            sale_price: dec("0"),
            ..markup_input("MK-13", "5", "100")
        };
        let p = s.create_product(actor(&s).await, input).await.unwrap();
        assert_eq!(p.sale_price, dec("10"));
    }

    /// The rounding helper belongs to the derived price only. A manual price
    /// keeps the exact value it was sent, scale included.
    #[tokio::test]
    async fn manual_prices_are_not_rounded() {
        let s = svc(true).await;
        let input = NewProduct {
            sale_price: dec("7.777"),
            ..product_input("MK-14")
        };
        let p = s.create_product(actor(&s).await, input).await.unwrap();
        assert_eq!(p.sale_price, dec("7.777"), "manual prices stay exact");
        assert_eq!(p.sale_price.to_string(), "7.777");
    }

    /// Pins the strategy choice, not merely that some rounding happens:
    /// 10.005 is an exact midpoint, and half-up away from zero gives 10.01.
    /// The other rounding test rounds a non-midpoint DOWN, so it would pass
    /// under any strategy and cannot prove this one.
    #[tokio::test]
    async fn derived_price_rounds_the_midpoint_up() {
        let s = svc(true).await;
        // 10 * (1 + 0.05/100) = 10.005 exactly.
        let p = s
            .create_product(actor(&s).await, markup_input("MK-15", "10", "0.05"))
            .await
            .unwrap();
        assert_eq!(p.sale_price, dec("10.01"));
    }
}
