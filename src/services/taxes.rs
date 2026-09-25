use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewTax, ProductTax, ProductTaxBreakdownRow, ProductTaxPreview, ProductTaxView, Tax,
    TaxReferenceCounts, UpdateTax,
};
use crate::repositories::{ProductRepository, ProductTaxRepository, TaxRepository};
use crate::services::line_taxes::calculate_line_taxes;

/// Canonical reason a hard delete was refused because a `product_taxes` row
/// still links the tax to a product. The remedy is in the message's meaning:
/// unlink the product, then delete.
///
/// A presentation layer maps this marker to a localized message; it is domain
/// vocabulary, never operator copy, and it carries no database text.
pub const TAX_DELETE_BLOCKED_BY_PRODUCTS: &str = "tax is still linked to products";

/// Canonical reason a hard delete was refused because a sale-line or
/// purchase-line snapshot still references the tax. There is no remedy at all:
/// the snapshot is frozen history, so the tax must be KEPT. Deactivating it
/// stops new documents from charging it without touching the past.
pub const TAX_DELETE_BLOCKED_BY_HISTORY: &str = "tax is referenced by document history";

/// Canonical reason for a hard delete refused by the database's own
/// `ON DELETE RESTRICT` backstop when the application could not attribute the
/// reference to a family — a race that resolved itself between the count and
/// the delete, or a reference this build does not know how to name. It is
/// still an actionable conflict, never a raw database fault.
pub const TAX_DELETE_BLOCKED: &str = "tax is still referenced";

#[derive(Clone)]
pub struct TaxService<P, T, L>
where
    P: ProductRepository,
    T: TaxRepository,
    L: ProductTaxRepository,
{
    pub products: P,
    pub taxes: T,
    pub product_taxes: L,
}

impl<P, T, L> TaxService<P, T, L>
where
    P: ProductRepository,
    T: TaxRepository,
    L: ProductTaxRepository,
{
    pub fn new(products: P, taxes: T, product_taxes: L) -> Self {
        Self {
            products,
            taxes,
            product_taxes,
        }
    }

    pub async fn list_taxes(&self) -> AppResult<Vec<Tax>> {
        self.taxes.list().await
    }

    pub async fn get_tax(&self, id: i64) -> AppResult<Tax> {
        self.taxes
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("tax {id} not found")))
    }

    pub async fn create_tax(&self, actor: i64, input: NewTax) -> AppResult<Tax> {
        let code = validate_code(&input.code)?;
        let name = validate_name(&input.name)?;
        validate_rate(input.rate)?;
        if self.taxes.find_by_code(&code).await?.is_some() {
            return Err(AppError::Conflict("tax code already exists".into()));
        }
        self.taxes
            .create(
                actor,
                &NewTax {
                    code,
                    name,
                    rate: input.rate,
                    is_active: input.is_active,
                },
            )
            .await
    }

    pub async fn update_tax(&self, actor: i64, id: i64, patch: UpdateTax) -> AppResult<Tax> {
        let current = self.get_tax(id).await?;
        let code = match patch.code {
            Some(value) => validate_code(&value)?,
            None => current.code.clone(),
        };
        let name = match patch.name {
            Some(value) => validate_name(&value)?,
            None => current.name.clone(),
        };
        let rate = patch.rate.unwrap_or(current.rate);
        validate_rate(rate)?;
        if code != current.code {
            if let Some(other) = self.taxes.find_by_code(&code).await? {
                if other.id != id {
                    return Err(AppError::Conflict("tax code already exists".into()));
                }
            }
        }
        self.taxes
            .update(
                actor,
                id,
                &code,
                &name,
                rate,
                patch.is_active.unwrap_or(current.is_active),
            )
            .await
    }

    pub async fn deactivate_tax(&self, actor: i64, id: i64) -> AppResult<Tax> {
        self.get_tax(id).await?;
        self.taxes.deactivate(actor, id).await
    }

    /// Reactivate a tax the catalogue no longer charges.
    ///
    /// A first-class action, not a side effect of an edit: an operator who
    /// stopped charging a tax and later needs it back must not have to re-submit
    /// its code, name and rate to bring it back, and the row keeps the same
    /// audit shape as every other lifecycle change here.
    pub async fn activate_tax(&self, actor: i64, id: i64) -> AppResult<Tax> {
        let current = self.get_tax(id).await?;
        self.taxes
            .update(actor, id, &current.code, &current.name, current.rate, true)
            .await
    }

    /// What currently references one tax, split into the two families the
    /// delete safeguard distinguishes. A read: it counts and writes nothing, and
    /// the counts are what a presentation layer shows before asking for a
    /// destructive confirmation.
    pub async fn tax_references(&self, id: i64) -> AppResult<TaxReferenceCounts> {
        self.taxes.reference_counts(id).await
    }

    /// The hard delete: remove a tax definition outright, but only when nothing
    /// references it.
    ///
    /// Two reference families, and they are never treated as one number,
    /// because they demand different things from the operator (see
    /// [`TaxReferenceCounts`]). The refusal order is fixed and explained there:
    /// frozen history outranks a product link, since no amount of unlinking can
    /// free a tax a document already froze.
    ///
    /// The count and the delete cannot be one atomic statement here — the
    /// catalogue tables are the tax repository's and the reference check must
    /// be answerable on its own — so a reference can appear between them. The
    /// `ON DELETE RESTRICT` backstop is what closes that window, and its
    /// refusal is translated HERE into the same actionable conflict the
    /// application-level check produces, by re-reading the counts. The caller
    /// therefore cannot tell a raced refusal from an anticipated one, and no
    /// database text ever escapes.
    ///
    /// NO ACTOR PARAMETER: the `taxes` table records no delete audit, so there
    /// is nowhere to store who removed it (see `TaxRepository::hard_delete`).
    pub async fn delete_tax(&self, id: i64) -> AppResult<()> {
        self.get_tax(id).await?;
        let references = self.tax_references(id).await?;
        if let Some(refusal) = delete_refusal(&references) {
            return Err(refusal);
        }
        match self.taxes.hard_delete(id).await {
            Ok(true) => return Ok(()),
            // The row was already gone: a concurrent delete won, and the tax
            // this call asked to remove no longer exists either way.
            Ok(false) => return Err(AppError::NotFound(format!("tax {id} not found"))),
            // The database backstop fired. Re-read the counts and answer with the
            // real reason instead of forwarding the internal marker.
            Err(AppError::Conflict(_)) => {}
            Err(error) => return Err(error),
        }
        let raced = self.tax_references(id).await?;
        Err(delete_refusal(&raced).unwrap_or_else(|| AppError::Conflict(TAX_DELETE_BLOCKED.into())))
    }

    pub async fn list_product_taxes(&self, product_id: i64) -> AppResult<Vec<ProductTaxView>> {
        self.require_product(product_id).await?;
        let links = self.product_taxes.list_by_product(product_id).await?;
        let mut out = Vec::with_capacity(links.len());
        for link in links {
            let tax = self.get_tax(link.tax_id).await?;
            out.push(ProductTaxView { link, tax });
        }
        Ok(out)
    }

    pub async fn list_active_taxes_excluding(&self, product_id: i64) -> AppResult<Vec<Tax>> {
        let linked = self.list_product_taxes(product_id).await?;
        let linked_ids: std::collections::BTreeSet<i64> =
            linked.into_iter().map(|view| view.tax.id).collect();
        Ok(self
            .list_taxes()
            .await?
            .into_iter()
            .filter(|tax| tax.is_active && !linked_ids.contains(&tax.id))
            .collect())
    }

    pub async fn link_product_tax(
        &self,
        actor: i64,
        product_id: i64,
        tax_id: i64,
    ) -> AppResult<ProductTax> {
        self.require_product(product_id).await?;
        let tax = self.get_tax(tax_id).await?;
        if !tax.is_active {
            return Err(AppError::Conflict("inactive tax cannot be linked".into()));
        }
        self.product_taxes.link(actor, product_id, tax_id).await
    }

    pub async fn unlink_product_tax(&self, product_id: i64, tax_id: i64) -> AppResult<()> {
        self.require_product(product_id).await?;
        if !self.product_taxes.unlink(product_id, tax_id).await? {
            return Err(AppError::NotFound(format!(
                "tax {tax_id} is not linked to product {product_id}"
            )));
        }
        Ok(())
    }

    /// The product's tax-inclusive unit price preview: the stored net sale price
    /// plus every ACTIVE linked tax, run through the same calculation contract a
    /// document line write runs, so the two can never disagree by a cent.
    ///
    /// Purely derived. Nothing here writes: the stored `sale_price` stays net,
    /// which is what a document line snapshots and what the API reports.
    pub async fn product_price_preview(&self, product_id: i64) -> AppResult<ProductTaxPreview> {
        let product = self.require_product(product_id).await?;
        // The same resolution boundary the line write uses: linked AND active,
        // ordered by code then id so the breakdown is deterministic.
        let taxes = self
            .product_taxes
            .list_active_for_product(product_id)
            .await?;
        let calc = calculate_line_taxes(product.sale_price, &taxes);
        Ok(ProductTaxPreview {
            // The net the taxes were actually applied to, read from the
            // calculation itself instead of re-read from the product: one
            // source of truth for the figure the breakdown adds up to.
            net_price: calc.net_subtotal,
            breakdown: calc
                .taxes
                .iter()
                .map(|row| ProductTaxBreakdownRow {
                    code: row.code.clone(),
                    name: row.name.clone(),
                    rate: row.rate,
                    amount: row.amount,
                })
                .collect(),
            tax_total: calc.tax_total,
            total: calc.total,
        })
    }

    async fn require_product(&self, product_id: i64) -> AppResult<crate::models::Product> {
        self.products
            .find_by_id(product_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product {product_id} not found")))
    }
}

fn validate_code(value: &str) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::Validation("tax code cannot be empty".into()));
    }
    if value.chars().count() > 32 {
        return Err(AppError::Validation("tax code must be <= 32 chars".into()));
    }
    Ok(value.to_string())
}

fn validate_name(value: &str) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::Validation("tax name cannot be empty".into()));
    }
    if value.chars().count() > 128 {
        return Err(AppError::Validation("tax name must be <= 128 chars".into()));
    }
    Ok(value.to_string())
}

fn validate_rate(rate: Decimal) -> AppResult<()> {
    if rate < Decimal::ZERO {
        return Err(AppError::Validation("tax rate cannot be negative".into()));
    }
    Ok(())
}

/// The one place a hard-delete refusal is decided, so the application-level
/// check and the raced re-check can never disagree about which reason applies.
///
/// FROZEN HISTORY FIRST, and the reason is not precedence for its own sake: a
/// `product_taxes` link is something the operator can undo right now, while a
/// document snapshot never releases its reference. Reporting "unlink the
/// products" for a tax a sale already froze would send the operator through
/// work that cannot possibly end in the deletion they asked for.
fn delete_refusal(references: &TaxReferenceCounts) -> Option<AppError> {
    if references.is_deletable() {
        return None;
    }
    if references.document_snapshots > 0 {
        return Some(AppError::Conflict(TAX_DELETE_BLOCKED_BY_HISTORY.into()));
    }
    // The counts are not deletable, no snapshot holds it, and a link does: the
    // one remaining reason there can be.
    Some(AppError::Conflict(TAX_DELETE_BLOCKED_BY_PRODUCTS.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NewProduct, ProductKind};
    use crate::repositories::{
        SqliteBarcodeRepository, SqliteCategoryRepository, SqliteProductRepository,
        SqliteProductTaxRepository, SqliteStockMovementRepository, SqliteTaxRepository,
    };
    use crate::security::test_support;
    use crate::services::InventoryService;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    type Svc = TaxService<SqliteProductRepository, SqliteTaxRepository, SqliteProductTaxRepository>;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    async fn test_pool() -> sqlx::SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// A product at `sale_price`, tracked, with the service wired the way the
    /// application wires it (the inventory service holds the shared product
    /// repository, the tax service holds the same one).
    async fn svc_with_product(price: &str) -> (Svc, sqlx::SqlitePool, i64) {
        let pool = test_pool().await;
        let products = SqliteProductRepository::new(pool.clone());
        let inventory = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            products.clone(),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            true,
        );
        let actor = test_support::audit_actor_id(&pool).await.unwrap();
        let product = inventory
            .create_product(
                actor,
                NewProduct {
                    sku: "TAX-PREVIEW".into(),
                    name: "taxed product".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec(price),
                    cost_price: dec("5"),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        let s = TaxService::new(
            products,
            SqliteTaxRepository::new(pool.clone()),
            SqliteProductTaxRepository::new(pool.clone()),
        );
        (s, pool, product.id)
    }

    async fn link(s: &Svc, code: &str, rate: &str) -> i64 {
        s.create_tax(
            1,
            NewTax {
                code: code.into(),
                name: format!("tax {code}"),
                rate: dec(rate),
                is_active: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// The preview is a read of the CURRENT catalogue: a product with no linked
    /// tax previews as its net price, and a linked tax changes nothing about the
    /// stored price.
    #[tokio::test]
    async fn tax_preview_without_taxes_is_the_stored_net_price() {
        let (s, _pool, product_id) = svc_with_product("100").await;

        let preview = s.product_price_preview(product_id).await.unwrap();
        assert_eq!(preview.net_price, dec("100"));
        assert!(preview.breakdown.is_empty());
        assert_eq!(preview.tax_total, dec("0"));
        assert_eq!(preview.total, dec("100"));

        let product = s.products.find_by_id(product_id).await.unwrap().unwrap();
        assert_eq!(
            product.sale_price,
            dec("100"),
            "the preview must never write the stored net price"
        );
    }

    /// Several linked taxes are additive, and the breakdown the operator reads
    /// reconciles with the total they are shown.
    #[tokio::test]
    async fn tax_preview_sums_every_linked_tax_additively() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        let iibb = link(&s, "IIBB10", "10").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();
        s.link_product_tax(1, product_id, iibb).await.unwrap();

        let preview = s.product_price_preview(product_id).await.unwrap();
        assert_eq!(preview.net_price, dec("100"));
        assert_eq!(preview.breakdown.len(), 2);
        assert_eq!(
            preview.breakdown[0].code, "IIBB10",
            "ordered by code, then id"
        );
        assert_eq!(preview.breakdown[0].rate, dec("10"));
        assert_eq!(preview.breakdown[0].amount, dec("10"));
        assert_eq!(preview.breakdown[1].code, "IVA21");
        assert_eq!(preview.breakdown[1].amount, dec("21"));
        assert_eq!(preview.tax_total, dec("31"), "additive, not compounded");
        assert_eq!(preview.total, dec("131"));
        let sum: Decimal = preview.breakdown.iter().map(|row| row.amount).sum();
        assert_eq!(sum, preview.tax_total, "the breakdown must reconcile");
    }

    /// A DEACTIVATED tax is not a tax the product will be charged, so it leaves
    /// the preview — the same exclusion the document line write performs.
    #[tokio::test]
    async fn tax_preview_excludes_a_deactivated_tax() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        let iibb = link(&s, "IIBB10", "10").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();
        s.link_product_tax(1, product_id, iibb).await.unwrap();
        s.deactivate_tax(1, iibb).await.unwrap();

        let preview = s.product_price_preview(product_id).await.unwrap();
        assert_eq!(preview.breakdown.len(), 1);
        assert_eq!(preview.breakdown[0].code, "IVA21");
        assert_eq!(preview.tax_total, dec("21"));
        assert_eq!(preview.total, dec("121"));
    }

    /// The preview shares the one half-up money rule, so a net price with a
    /// third decimal previews as the value a line would charge.
    #[tokio::test]
    async fn tax_preview_rounds_the_tax_inclusive_price_half_up() {
        let (s, _pool, product_id) = svc_with_product("10.005").await;
        let iva = link(&s, "IVA21", "21").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();

        let preview = s.product_price_preview(product_id).await.unwrap();
        assert_eq!(preview.net_price, dec("10.005"), "shown net, unrounded");
        assert_eq!(preview.breakdown[0].amount, dec("2.10"));
        assert_eq!(preview.tax_total, dec("2.10"));
        assert_eq!(
            preview.total,
            dec("12.11"),
            "half-up, the same total a line of this product would carry"
        );
    }

    /// An unknown product is a 404, not a silent zero preview: the drawer already
    /// resolved the product, so this only guards the contract.
    #[tokio::test]
    async fn tax_preview_of_an_unknown_product_is_not_found() {
        let (s, _pool, _product_id) = svc_with_product("100").await;
        let err = s.product_price_preview(999_999).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    }
}
