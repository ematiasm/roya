// M1 inventory service: domain validations over repository traits.
// Boundary: never touches finance tables; stock is derived SUM in Rust.
use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    Category, MovementReason, MovementType, NewMovement, NewProduct, Product, ProductBarcode,
    ProductKind, ProductStock, StockMovement,
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
        self.categories.create(&clean, parent_id).await
    }

    pub async fn update_category(
        &self,
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
                    return Err(AppError::Validation(
                        "category cycle detected".into(),
                    ));
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

        self.categories.update(id, &new_name, new_parent).await
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
            return Err(AppError::Validation(
                "unit must be 1..16 chars".into(),
            ));
        }
        match input.kind {
            ProductKind::Product => {
                if input.sale_price <= Decimal::ZERO {
                    return Err(AppError::Validation(
                        "sale_price must be > 0 for products".into(),
                    ));
                }
            }
            ProductKind::Service => {
                if input.sale_price < Decimal::ZERO {
                    return Err(AppError::Validation(
                        "sale_price cannot be negative".into(),
                    ));
                }
            }
        }
        if input.cost_price < Decimal::ZERO {
            return Err(AppError::Validation(
                "cost_price cannot be negative".into(),
            ));
        }
        if let Some(cid) = input.category_id {
            if !self.categories.exists(cid).await? {
                return Err(AppError::NotFound(format!("category {cid} not found")));
            }
        }

        if input.kind == ProductKind::Service {
            if input.track_stock {
                return Err(AppError::Validation(
                    "services cannot track stock".into(),
                ));
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
                        return Err(AppError::Validation(
                            "location must be <= 64 chars".into(),
                        ));
                    }
                    Some(t.to_string())
                }
            }
            None => None,
        };
        let notes = match input.notes {
            Some(s) => {
                if s.chars().count() > 1024 {
                    return Err(AppError::Validation(
                        "notes must be <= 1024 chars".into(),
                    ));
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
            sale_price: input.sale_price,
            cost_price: input.cost_price,
            track_stock: input.track_stock,
            min_stock: input.min_stock,
            max_stock: input.max_stock,
            location,
            notes,
        })
    }

    pub async fn create_product(&self, input: NewProduct) -> AppResult<Product> {
        let clean = self.validate_product(input).await?;
        if self.products.find_by_sku(&clean.sku).await?.is_some() {
            return Err(AppError::Conflict("sku already exists".into()));
        }
        self.products.create(&clean).await
    }

    pub async fn set_product_active(&self, id: i64, active: bool) -> AppResult<Product> {
        self.get_product(id).await?;
        self.products.set_active(id, active).await
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

    // -- barcodes -----------------------------------------------------------

    pub async fn add_barcode(
        &self,
        product_id: i64,
        code: &str,
    ) -> AppResult<ProductBarcode> {
        let clean = code.trim();
        if clean.is_empty() || clean.chars().count() > 64 {
            return Err(AppError::Validation(
                "barcode must be 1..64 chars".into(),
            ));
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

    pub async fn record_movement(&self, input: NewMovement) -> AppResult<StockMovement> {
        match input.movement_type {
            MovementType::In | MovementType::Out => {
                if input.qty <= Decimal::ZERO {
                    return Err(AppError::Validation("qty must be > 0".into()));
                }
            }
            MovementType::Adjust => {
                if input.qty == Decimal::ZERO {
                    return Err(AppError::Validation(
                        "adjust qty cannot be zero".into(),
                    ));
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

        self.movements.create(&input).await
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
    use crate::repositories::{
        SqliteBarcodeRepository, SqliteCategoryRepository, SqliteProductRepository,
        SqliteStockMovementRepository,
    };
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
        s.create_product(product_input("SKU-1")).await.unwrap();
        let err = s.create_product(product_input("SKU-1")).await.unwrap_err();
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
        let created = s.create_product(svc_prod).await.unwrap();
        assert!(!created.track_stock);
        let err = s
            .record_movement(movement(created.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac3_bad_qty_and_unknown_product() {
        let s = svc(true).await;
        let p = s.create_product(product_input("AC3")).await.unwrap();
        let err = s
            .record_movement(movement(p.id, "0", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        let err = s
            .record_movement(movement(99999, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac4_category_cycle_rejected() {
        let s = svc(true).await;
        let root = s.create_category("root", None).await.unwrap();
        let child = s
            .create_category("child", Some(root.id))
            .await
            .unwrap();
        // descendant-parent cycle
        let err = s
            .update_category(root.id, None, Some(Some(child.id)))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // self-parent cycle
        let err = s
            .update_category(root.id, None, Some(Some(root.id)))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ac5_delete_nonempty_category_blocked() {
        let s = svc(true).await;
        let root = s.create_category("r", None).await.unwrap();
        s.create_category("c", Some(root.id)).await.unwrap();
        let err = s.delete_category(root.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let leaf = s.create_category("leaf", None).await.unwrap();
        let mut inp = product_input("CAT-P");
        inp.category_id = Some(leaf.id);
        s.create_product(inp).await.unwrap();
        let err = s.delete_category(leaf.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let empty = s.create_category("empty", None).await.unwrap();
        s.delete_category(empty.id).await.unwrap();
    }

    #[tokio::test]
    async fn ac6_strict_mode_blocks_negative() {
        let s = svc(false).await;
        let p = s.create_product(product_input("STRICT")).await.unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Purchase,
            ..movement(p.id, "5", MovementType::In)
        })
        .await
        .unwrap();
        let err = s
            .record_movement(NewMovement {
                reason: MovementReason::Sale,
                ..movement(p.id, "10", MovementType::Out)
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        assert_eq!(s.stock(p.id).await.unwrap(), dec("5"));
    }

    #[tokio::test]
    async fn ac7_permissive_mode_allows_negative_and_lists_it() {
        let s = svc(true).await;
        let p = s.create_product(product_input("PERM")).await.unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Purchase,
            ..movement(p.id, "5", MovementType::In)
        })
        .await
        .unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Sale,
            ..movement(p.id, "10", MovementType::Out)
        })
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
        let p = s.create_product(product_input("SUM")).await.unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Initial,
            ..movement(p.id, "20", MovementType::In)
        })
        .await
        .unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Sale,
            ..movement(p.id, "8", MovementType::Out)
        })
        .await
        .unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Adjust,
            ..movement(p.id, "-2", MovementType::Adjust)
        })
        .await
        .unwrap();
        // 20 - 8 - 2 = 10
        let ps = s.product_stock(p.id).await.unwrap();
        assert_eq!(ps.stock, dec("10"));
        assert!(ps.suggested.is_none());

        // Drop to low stock: 10 - 8 = 2 <= min(5) => suggested = max - stock = 48
        s.record_movement(NewMovement {
            reason: MovementReason::Sale,
            ..movement(p.id, "8", MovementType::Out)
        })
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
        let a = s.create_product(product_input("BC-A")).await.unwrap();
        let b = s.create_product(product_input("BC-B")).await.unwrap();
        s.add_barcode(a.id, "7790001").await.unwrap();
        let err = s.add_barcode(a.id, "7790001").await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        let err = s.add_barcode(b.id, "7790001").await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_product_delete_restrict_with_movements_cascade_barcodes() {
        let s = svc(true).await;
        let with_hist = s.create_product(product_input("DEL-H")).await.unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Initial,
            ..movement(with_hist.id, "3", MovementType::In)
        })
        .await
        .unwrap();
        let err = s.delete_product(with_hist.id).await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        // Without movements delete succeeds and cascades barcodes.
        let plain = s.create_product(product_input("DEL-C")).await.unwrap();
        let bc = s.add_barcode(plain.id, "CASCADE-1").await.unwrap();
        s.delete_product(plain.id).await.unwrap();
        assert!(s.get_product(plain.id).await.is_err());
        // Barcode row is gone via ON DELETE CASCADE.
        let pool = s.products.pool.clone();
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM product_barcodes WHERE id = ?")
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
            s.create_product(bad).await.unwrap_err(),
            AppError::Validation(_)
        ));
        // Tracked without min/max rejected.
        let bad = NewProduct {
            min_stock: None,
            max_stock: None,
            ..product_input("TRK-BAD")
        };
        assert!(matches!(
            s.create_product(bad).await.unwrap_err(),
            AppError::Validation(_)
        ));
        // Untracked product rejects movements.
        let untracked = NewProduct {
            track_stock: false,
            min_stock: None,
            max_stock: None,
            ..product_input("UNTRK")
        };
        let u = s.create_product(untracked).await.unwrap();
        let err = s
            .record_movement(movement(u.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        // Inactive product rejects movements.
        let p = s.create_product(product_input("INACT")).await.unwrap();
        s.set_product_active(p.id, false).await.unwrap();
        let err = s
            .record_movement(movement(p.id, "1", MovementType::In))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tri_root_duplicate_name_and_adjust_zero() {
        let s = svc(true).await;
        s.create_category("dup", None).await.unwrap();
        let err = s.create_category("dup", None).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "got {err:?}");
        // Same name under different parent is fine.
        let other = s.create_category("other", None).await.unwrap();
        s.create_category("dup", Some(other.id)).await.unwrap();

        let p = s.create_product(product_input("ADJ0")).await.unwrap();
        let err = s
            .record_movement(movement(p.id, "0", MovementType::Adjust))
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
        let p = s.create_product(product_input("NOFIN")).await.unwrap();
        s.add_barcode(p.id, "NOFIN-BC").await.unwrap();
        s.record_movement(NewMovement {
            reason: MovementReason::Purchase,
            ..movement(p.id, "4", MovementType::In)
        })
        .await
        .unwrap();
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);
    }
}
