use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{
    LadderInput, NewTax, ProductPriceLadder, ProductTax, ProductTaxBreakdownRow, ProductTaxView,
    Tax, TaxReferenceCounts, UpdateTax,
};
use crate::repositories::{ProductRepository, ProductTaxRepository, TaxRepository};
use crate::services::inventory::{derive_net_sale_price, validate_effective_prices};
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

    async fn require_product(&self, product_id: i64) -> AppResult<crate::models::Product> {
        self.products
            .find_by_id(product_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product {product_id} not found")))
    }

    /// The product price ladder: cost, markup, net sale price, every active
    /// linked tax with the amount it adds, the tax total and the tax-inclusive
    /// price — in the order an operator can sanity-check a price.
    ///
    /// Two contracts, both the ones a save and a document line already use, so
    /// the ladder cannot be a cent away from either:
    ///
    /// * [`derive_net_sale_price`] decides the net price, including whether one
    ///   can be derived at all. Its refusals are carried through as themselves
    ///   — a `PriceRefusal`, never a message — instead of being turned into a
    ///   zero, and a refusal publishes NO tax money: there is no net price for a
    ///   tax to apply to. The rendering that turns the carried rule into a
    ///   sentence in the operator's language belongs to the route, and it is one
    ///   function the save form and this ladder both go through.
    /// * [`calculate_line_taxes`] totals the taxes, resolved through the same
    ///   `list_active_for_product` read a line write uses, so linking or
    ///   unlinking a tax moves the ladder.
    ///
    /// Purely derived: this reads the product and the associations and writes
    /// nothing, on any path. The stored net price is what a save will persist
    /// and is never rewritten here.
    ///
    /// `None` is the drawer's own first render: no form values exist yet, and
    /// the stored row IS the answer. It is the same state `LadderInput::
    /// Unreadable` reports, minus the operator-facing hint, because nothing is
    /// wrong: nothing has been typed.
    ///
    /// THE AUDITED BOUNDARY, stated because "refuse everything the save refuses"
    /// is only meaningful with a list. The ladder mirrors every rule that can
    /// change a FIGURE it publishes, and each of them is the save path's own
    /// function, not a copy of it:
    ///
    /// | state | source | ladder |
    /// |---|---|---|
    /// | markup at or below -100 | `derive_net_sale_price` | refusal, no figure |
    /// | a markup with no positive cost | `derive_net_sale_price` | refusal, no figure |
    /// | a derived price that pins to zero | `validate_effective_prices` | refusal, no figure |
    /// | a negative price, product or service | `validate_effective_prices` | refusal, no figure |
    /// | a negative cost | `validate_effective_prices` | refusal, no figure |
    /// | an emptied manual price | the form-shape gate, as `LadderInput::Refused` | refusal, no figure |
    ///
    /// The price rule is applied to the kind IN THE FORM when the request names
    /// one, and to the product's STORED kind when it names none or names
    /// something that is not a kind. That is the whole reason a free service
    /// switched to a product in the form refuses here: the ladder is a preview
    /// of the save, and the save would refuse it.
    ///
    /// (One documented divergence, unreachable from the drawer's own select: the
    /// SAVE reads an EMPTY `kind` as `Product`, while the ladder reads an absent
    /// or unreadable one as the stored kind. The drawer\'s select always carries
    /// a value, so a browser cannot reach it; if a future form could, the two
    /// surfaces would answer differently for an empty select and only for that.)
    ///
    /// The remaining `validate_product` rules are deliberately NOT mirrored, and
    /// the reason is that none of them can change a number this fragment shows:
    /// the SKU/name/unit lengths, the category's existence, the track-stock
    /// flag, the min/max bounds and the "services cannot track stock" rule. A
    /// form that violates one of those is refused by the save for a reason that
    /// has nothing to do with a price, and the ladder keeps previewing the price
    /// it was asked about rather than refusing to answer a different question.
    /// If a future rule ever does touch a figure, it belongs in
    /// `derive_net_sale_price` or `validate_effective_prices` and in this table.
    pub async fn product_price_ladder(
        &self,
        product_id: i64,
        input: Option<LadderInput>,
    ) -> AppResult<ProductPriceLadder> {
        let product = self.require_product(product_id).await?;
        // The save path's price rule branches on the kind (a product must sell
        // for something, a service may cost nothing), so the ladder carries both
        // candidates: the form's when the request named one, this one otherwise.
        let stored_kind = product.kind;
        // The same resolution boundary the line write uses: linked AND active,
        // ordered by code then id so the ladder is deterministic.
        let taxes = self
            .product_taxes
            .list_active_for_product(product_id)
            .await?;

        // First the PRICE half of the ladder, from the form or from the stored
        // row, including the two states in which there is no net price to
        // publish.
        let mut ladder = match input.unwrap_or(LadderInput::Stored) {
            LadderInput::Form {
                kind,
                sale_price,
                cost_price,
                markup_pct,
            } => {
                // The price rule is the FORM's rule: the kind the operator has
                // selected right now, not the one the row was saved with. A free
                // service switched to a product is refused by the save for
                // being 0.00, and the ladder has to refuse it too, or it
                // publishes a price no save would accept. With no readable kind
                // in the form the product's own stored kind applies.
                let kind = kind.unwrap_or(stored_kind);
                // Derive, then apply the save path's OWN price and cost rules to
                // the effective price. Two refusals in a row are possible and
                // both are the save path's: a markup with no cost fails
                // derivation, and a derived price that pins to zero fails the
                // price rule. Either way the ladder holds a refusal and no
                // figure, which is the only honest answer for a state a save
                // would reject.
                let net =
                    derive_net_sale_price(cost_price, markup_pct, sale_price).and_then(|net| {
                        validate_effective_prices(kind, net, cost_price)?;
                        Ok(net)
                    });
                let (net_price, net_refusal) = match net {
                    Ok(net) => (net, None),
                    Err(refusal) => (Decimal::ZERO, Some(refusal)),
                };
                ProductPriceLadder {
                    cost_price,
                    markup_pct,
                    net_price,
                    net_is_derived: net_refusal.is_none() && markup_pct.is_some(),
                    net_refusal,
                    inputs_unreadable: false,
                    from_form: true,
                    breakdown: Vec::new(),
                    tax_total: Decimal::ZERO,
                    total: Decimal::ZERO,
                }
            }
            // A form-shape refusal the save shares, carrying the cost the form
            // does hold: there is no manual price, so there is no net price.
            LadderInput::Refused {
                refusal,
                cost_price,
            } => ProductPriceLadder {
                cost_price,
                markup_pct: None,
                net_price: Decimal::ZERO,
                net_is_derived: false,
                net_refusal: Some(refusal),
                inputs_unreadable: false,
                from_form: true,
                breakdown: Vec::new(),
                tax_total: Decimal::ZERO,
                total: Decimal::ZERO,
            },
            // No form values at all, or one that is not a number. The stored row
            // is the last state the save path accepted, and reporting it is the
            // only answer that is not invented.
            LadderInput::Stored => stored_ladder(&product, false),
            LadderInput::Unreadable => stored_ladder(&product, true),
        };

        // Then the TAX half, and only when there is a net price to apply them
        // to: a breakdown computed from a price that does not exist would be
        // exactly the fabrication this ladder exists to avoid.
        if ladder.net_refusal.is_none() {
            let calc = calculate_line_taxes(ladder.net_price, &taxes);
            ladder.breakdown = calc
                .taxes
                .iter()
                .map(|row| ProductTaxBreakdownRow {
                    code: row.code.clone(),
                    name: row.name.clone(),
                    rate: row.rate,
                    amount: row.amount,
                })
                .collect();
            // The net is read back off the calculation, never re-derived here:
            // one source of truth for the figure the breakdown adds up to.
            ladder.net_price = calc.net_subtotal;
            ladder.tax_total = calc.tax_total;
            ladder.total = calc.total;
        }
        Ok(ladder)
    }
}

/// The stored row read as a ladder: the last state the save path accepted.
/// `unreadable` says whether the operator has to be told why the ladder is not
/// previewing their typing.
fn stored_ladder(product: &crate::models::Product, unreadable: bool) -> ProductPriceLadder {
    ProductPriceLadder {
        cost_price: product.cost_price,
        markup_pct: product.markup_pct,
        net_price: product.sale_price,
        net_is_derived: product.markup_pct.is_some(),
        net_refusal: None,
        inputs_unreadable: unreadable,
        from_form: false,
        breakdown: Vec::new(),
        tax_total: Decimal::ZERO,
        total: Decimal::ZERO,
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
    use crate::models::{NewProduct, PriceRefusal, ProductKind};
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

    /// The ladder is a read of the CURRENT catalogue: a product with no linked
    /// tax ladders to its own net price, and a linked tax changes nothing about
    /// the stored price.
    #[tokio::test]
    async fn price_ladder_without_taxes_is_the_stored_net_price() {
        let (s, _pool, product_id) = svc_with_product("100").await;

        let ladder = s.product_price_ladder(product_id, None).await.unwrap();
        assert_eq!(ladder.net_price, dec("100"));
        assert!(ladder.net_refusal.is_none());
        assert!(ladder.breakdown.is_empty());
        assert_eq!(ladder.tax_total, dec("0"));
        assert_eq!(ladder.total, dec("100"));

        let product = s.products.find_by_id(product_id).await.unwrap().unwrap();
        assert_eq!(
            product.sale_price,
            dec("100"),
            "the ladder must never write the stored net price"
        );
    }

    /// Several linked taxes are additive, and the breakdown the operator reads
    /// reconciles with the total they are shown.
    #[tokio::test]
    async fn price_ladder_sums_every_linked_tax_additively() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        let iibb = link(&s, "IIBB10", "10").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();
        s.link_product_tax(1, product_id, iibb).await.unwrap();

        let ladder = s.product_price_ladder(product_id, None).await.unwrap();
        assert_eq!(ladder.net_price, dec("100"));
        assert_eq!(ladder.breakdown.len(), 2);
        assert_eq!(
            ladder.breakdown[0].code, "IIBB10",
            "ordered by code, then id"
        );
        assert_eq!(ladder.breakdown[0].rate, dec("10"));
        assert_eq!(ladder.breakdown[0].amount, dec("10"));
        assert_eq!(ladder.breakdown[1].code, "IVA21");
        assert_eq!(ladder.breakdown[1].amount, dec("21"));
        assert_eq!(ladder.tax_total, dec("31"), "additive, not compounded");
        assert_eq!(ladder.total, dec("131"));
        let sum: Decimal = ladder.breakdown.iter().map(|row| row.amount).sum();
        assert_eq!(sum, ladder.tax_total, "the breakdown must reconcile");
    }

    /// A DEACTIVATED tax is not a tax the product will be charged, so it leaves
    /// the ladder — the same exclusion the document line write performs.
    #[tokio::test]
    async fn price_ladder_excludes_a_deactivated_tax() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        let iibb = link(&s, "IIBB10", "10").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();
        s.link_product_tax(1, product_id, iibb).await.unwrap();
        s.deactivate_tax(1, iibb).await.unwrap();

        let ladder = s.product_price_ladder(product_id, None).await.unwrap();
        assert_eq!(ladder.breakdown.len(), 1);
        assert_eq!(ladder.breakdown[0].code, "IVA21");
        assert_eq!(ladder.tax_total, dec("21"));
        assert_eq!(ladder.total, dec("121"));
    }

    /// The ladder shares the one half-up money rule, so a net price with a
    /// third decimal ladders to the value a line would charge.
    #[tokio::test]
    async fn price_ladder_rounds_the_tax_inclusive_price_half_up() {
        let (s, _pool, product_id) = svc_with_product("10.005").await;
        let iva = link(&s, "IVA21", "21").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();

        let ladder = s.product_price_ladder(product_id, None).await.unwrap();
        assert_eq!(ladder.net_price, dec("10.005"), "shown net, unrounded");
        assert_eq!(ladder.breakdown[0].amount, dec("2.10"));
        assert_eq!(ladder.tax_total, dec("2.10"));
        assert_eq!(
            ladder.total,
            dec("12.11"),
            "half-up, the same total a line of this product would carry"
        );
    }

    /// The ladder derives the net price through the SAME rule the save path
    /// enforces, so a preview can never state a price a save would refuse or a
    /// different one. A markup over a positive cost derives; the stored row
    /// never changes because of the preview.
    #[tokio::test]
    async fn price_ladder_derives_the_net_price_through_the_save_paths_rule() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();

        let ladder = s
            .product_price_ladder(
                product_id,
                Some(LadderInput::Form {
                    kind: None,
                    sale_price: dec("999"),
                    cost_price: dec("10"),
                    markup_pct: Some(dec("100")),
                }),
            )
            .await
            .unwrap();
        assert_eq!(ladder.net_price, dec("20"), "10 * (1 + 100/100)");
        assert!(ladder.net_is_derived);
        assert!(ladder.net_refusal.is_none());
        assert_eq!(ladder.breakdown[0].amount, dec("4.20"));
        assert_eq!(ladder.total, dec("24.20"));

        let product = s.products.find_by_id(product_id).await.unwrap().unwrap();
        assert_eq!(
            product.sale_price,
            dec("100"),
            "a ladder over unsaved values persists nothing"
        );
    }

    /// A markup with no cost cannot produce a price, so the ladder carries the
    /// SAVE PATH'S OWN refusal and publishes no tax money at all — a breakdown
    /// built on a price that does not exist is the fabrication this ladder
    /// exists to prevent.
    #[tokio::test]
    async fn price_ladder_refuses_to_publish_tax_money_without_a_net_price() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();

        let ladder = s
            .product_price_ladder(
                product_id,
                Some(LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: Decimal::ZERO,
                    markup_pct: Some(dec("50")),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            ladder.net_refusal,
            Some(PriceRefusal::MarkupNeedsPositiveCost),
            "the save path's own rule, not a second wording of it"
        );
        assert!(!ladder.net_is_derived);
        assert!(ladder.breakdown.is_empty());
        assert_eq!(ladder.tax_total, Decimal::ZERO);
        assert_eq!(ladder.total, Decimal::ZERO);
    }

    /// An unreadable field previews nothing: the ladder reports the last state
    /// the save path accepted and says why, rather than guessing at a price.
    #[tokio::test]
    async fn price_ladder_of_an_unreadable_field_shows_the_last_saved_state() {
        let (s, _pool, product_id) = svc_with_product("100").await;

        let ladder = s
            .product_price_ladder(product_id, Some(LadderInput::Unreadable))
            .await
            .unwrap();
        assert!(ladder.inputs_unreadable);
        assert_eq!(ladder.net_price, dec("100"));
        assert!(ladder.net_refusal.is_none());

        let fresh = s.product_price_ladder(product_id, None).await.unwrap();
        assert!(
            !fresh.inputs_unreadable,
            "no form at all is not a problem: nothing has been typed"
        );
    }

    /// THE REFUSAL MATRIX, in one place: every state the SAVE path refuses must
    /// leave the ladder with no net price, no per-tax amount, no tax total and
    /// no tax-inclusive price. Each row is a real save refusal, taken from the
    /// rules `validate_product` applies to the EFFECTIVE price and the cost.
    #[tokio::test]
    async fn price_ladder_refuses_exactly_what_the_save_path_refuses() {
        let (s, _pool, product_id) = svc_with_product("100").await;
        let iva = link(&s, "IVA21", "21").await;
        s.link_product_tax(1, product_id, iva).await.unwrap();

        // (label, form values, the save path's own refusal)
        let cases: [(&str, LadderInput, Option<PriceRefusal>); 5] = [
            (
                "markup at the -100 boundary",
                LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: dec("10"),
                    markup_pct: Some(dec("-100")),
                },
                Some(PriceRefusal::MarkupNotAboveMinus100),
            ),
            (
                "a markup with no cost",
                LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: Decimal::ZERO,
                    markup_pct: Some(dec("50")),
                },
                Some(PriceRefusal::MarkupNeedsPositiveCost),
            ),
            (
                "a derived price that pins to zero",
                LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: dec("0.05"),
                    markup_pct: Some(dec("-99")),
                },
                Some(PriceRefusal::SalePriceNotPositiveForProduct),
            ),
            (
                "a negative cost with a manual price",
                LadderInput::Form {
                    kind: None,
                    sale_price: dec("42"),
                    cost_price: dec("-5"),
                    markup_pct: None,
                },
                Some(PriceRefusal::CostPriceNegative),
            ),
            (
                "an emptied manual price",
                LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: dec("5"),
                    markup_pct: None,
                },
                // A manual price of zero on a PRODUCT is the price rule, not the
                // form-shape one; the form-shape refusal is the route's, and it
                // arrives here already resolved as `Refused`.
                Some(PriceRefusal::SalePriceNotPositiveForProduct),
            ),
        ];
        for (label, input, expected) in cases {
            let ladder = s
                .product_price_ladder(product_id, Some(input))
                .await
                .unwrap();
            assert_eq!(
                ladder.net_refusal, expected,
                "{label}: the ladder must carry the save path's own refusal"
            );
            assert!(
                ladder.breakdown.is_empty(),
                "{label}: a refused price publishes no per-tax amount: {ladder:?}"
            );
            assert_eq!(ladder.tax_total, Decimal::ZERO, "{label}");
            assert_eq!(ladder.total, Decimal::ZERO, "{label}");
            assert!(!ladder.net_is_derived, "{label}");
        }

        // The mirror is KIND-AWARE, not a blanket "zero is bad": a SERVICE may
        // legitimately cost nothing, and the save path accepts it, so the ladder
        // must publish the figure instead of inventing a refusal.
        let pool = _pool;
        let service = seed_service(&s, &pool, "LADDER-SVC").await;
        // The same tax, linked to the service too: a zero-priced service must
        // still publish the tax that applies to it.
        s.link_product_tax(1, service, iva).await.unwrap();
        let zero = s
            .product_price_ladder(
                service,
                Some(LadderInput::Form {
                    kind: None,
                    sale_price: dec("0"),
                    cost_price: dec("0.05"),
                    markup_pct: Some(dec("-99")),
                }),
            )
            .await
            .unwrap();
        assert!(
            zero.net_refusal.is_none(),
            "a service priced at zero is a price the save path accepts: {zero:?}"
        );
        assert_eq!(zero.net_price, Decimal::ZERO);
        assert_eq!(zero.breakdown.len(), 1, "and its tax still applies");
        assert_eq!(zero.total, Decimal::ZERO);

        // A NEGATIVE price is refused for a service too, with its own rule.
        let negative = s
            .product_price_ladder(
                service,
                Some(LadderInput::Form {
                    kind: None,
                    sale_price: dec("-1"),
                    cost_price: dec("5"),
                    markup_pct: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            negative.net_refusal,
            Some(PriceRefusal::SalePriceNegative),
            "the service rule is its own, not the product rule"
        );
        assert!(negative.breakdown.is_empty());
    }

    /// A service seeded for the kind-aware rules: the ladder mirrors
    /// `validate_product`, and that validation branches on the product kind.
    async fn seed_service(s: &Svc, pool: &sqlx::SqlitePool, sku: &str) -> i64 {
        let inventory = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            s.products.clone(),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            true,
        );
        inventory
            .create_product(
                test_support::audit_actor_id(pool).await.unwrap(),
                NewProduct {
                    sku: sku.into(),
                    name: "a service".into(),
                    kind: ProductKind::Service,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: dec("20"),
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
            .unwrap()
            .id
    }

    /// An unknown product is a 404, not a silent zero ladder: the drawer already
    /// resolved the product, so this only guards the contract.
    #[tokio::test]
    async fn price_ladder_of_an_unknown_product_is_not_found() {
        let (s, _pool, _product_id) = svc_with_product("100").await;
        let err = s.product_price_ladder(999_999, None).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    }
}
