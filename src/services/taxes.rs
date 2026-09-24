use rust_decimal::Decimal;

use crate::error::{AppError, AppResult};
use crate::models::{NewTax, ProductTax, ProductTaxView, Tax, UpdateTax};
use crate::repositories::{ProductRepository, ProductTaxRepository, TaxRepository};

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

    async fn require_product(&self, product_id: i64) -> AppResult<()> {
        self.products
            .find_by_id(product_id)
            .await?
            .map(|_| ())
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
