use std::collections::BTreeSet;

use crate::error::{AppError, AppResult};
use crate::models::{
    BusinessLocale, BusinessSettings, UpdateBusinessLocale, UpdateBusinessSettings,
};
use crate::repositories::BusinessConfigurationRepository;

#[derive(Debug, Clone)]
pub struct UpdateBusinessConfiguration {
    pub settings: UpdateBusinessSettings,
    pub locales: Vec<UpdateBusinessLocale>,
}

#[derive(Clone)]
pub struct SettingsService<R> {
    repository: R,
}

impl<R> SettingsService<R>
where
    R: BusinessConfigurationRepository,
{
    pub fn new(repository: R) -> Self {
        Self { repository }
    }

    pub async fn load(&self) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)> {
        self.repository.load().await
    }

    pub async fn update(
        &self,
        mut input: UpdateBusinessConfiguration,
    ) -> AppResult<(BusinessSettings, Vec<BusinessLocale>)> {
        let (_, current_locales) = self.repository.load().await?;
        validate(&mut input, &current_locales)?;
        self.repository
            .update(&input.settings, &input.locales)
            .await
    }
}

fn validate(
    input: &mut UpdateBusinessConfiguration,
    current_locales: &[BusinessLocale],
) -> AppResult<()> {
    input.settings.business_name = input.settings.business_name.trim().to_string();
    if input.settings.business_name.is_empty() || input.settings.business_name.chars().count() > 160
    {
        return Err(AppError::Validation(
            "El nombre del negocio debe tener entre 1 y 160 caracteres.".into(),
        ));
    }

    if input.settings.currency_code.len() != 3
        || !input
            .settings
            .currency_code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase())
    {
        return Err(AppError::Validation(
            "La moneda debe ser un código de tres letras mayúsculas.".into(),
        ));
    }

    input.settings.timezone = input.settings.timezone.trim().to_string();
    if input.settings.timezone.is_empty() || input.settings.timezone.chars().count() > 64 {
        return Err(AppError::Validation(
            "La zona horaria es obligatoria.".into(),
        ));
    }

    let current_codes: BTreeSet<String> = current_locales
        .iter()
        .map(|locale| locale.locale_code.clone())
        .collect();
    if input.locales.len() != current_codes.len() {
        return Err(AppError::Validation(
            "The submitted locale profiles do not match the configured locales.".into(),
        ));
    }

    let mut profile_codes = BTreeSet::new();
    for locale in &mut input.locales {
        locale.display_name = locale.display_name.trim().to_string();
        if locale.display_name.is_empty() || locale.display_name.chars().count() > 128 {
            return Err(AppError::Validation(
                "El nombre visible del locale debe tener entre 1 y 128 caracteres.".into(),
            ));
        }
        if !profile_codes.insert(locale.locale_code.clone()) {
            return Err(AppError::Validation(
                "A locale profile was submitted more than once.".into(),
            ));
        }
    }
    if profile_codes != current_codes {
        return Err(AppError::Validation(
            "The submitted locale profiles do not match the configured locales.".into(),
        ));
    }

    let default_exists = profile_codes.contains(&input.settings.default_locale_code);
    let default_enabled = input
        .locales
        .iter()
        .find(|locale| locale.locale_code == input.settings.default_locale_code)
        .is_some_and(|locale| locale.is_enabled);
    if !default_exists || !default_enabled {
        return Err(AppError::Validation(
            "The default locale must exist and remain enabled.".into(),
        ));
    }

    Ok(())
}
