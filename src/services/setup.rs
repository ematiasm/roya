use crate::error::{AppError, AppResult};
use crate::models::{NewBusinessLocale, NewBusinessSettings};
use crate::repositories::setup_repo::{SetupRecord, SetupRepository};
use crate::security::password::PasswordHashing;

const MIN_PASSWORD_LEN: usize = 12;
const MIN_PASSWORD_MESSAGE: &str = "La contraseña debe tener al menos 12 caracteres.";
const USERNAME_MESSAGE: &str = "El nombre de usuario debe tener entre 3 y 64 caracteres: sólo letras minúsculas, números y . _ - (sin espacios, empezando y terminando con letra o número).";
const DISPLAY_NAME_MESSAGE: &str = "El nombre para mostrar debe tener entre 1 y 128 caracteres.";

#[derive(Debug, Clone, Copy)]
pub struct LocaleDefinition {
    pub code: &'static str,
    pub language_code: &'static str,
    pub display_name: &'static str,
}

pub const LOCALE_CATALOG: &[LocaleDefinition] = &[
    LocaleDefinition {
        code: "es-AR",
        language_code: "es",
        display_name: "Español (Argentina)",
    },
    LocaleDefinition {
        code: "es-ES",
        language_code: "es",
        display_name: "Español (España)",
    },
    LocaleDefinition {
        code: "en-US",
        language_code: "en",
        display_name: "English (United States)",
    },
];

#[derive(Clone)]
pub struct SetupInput {
    pub business_name: String,
    pub default_locale_code: String,
    pub currency_code: String,
    pub timezone: String,
    pub username: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Clone)]
pub struct SetupService<R, H> {
    repository: R,
    hasher: H,
}

impl<R, H> SetupService<R, H>
where
    R: SetupRepository,
    H: PasswordHashing,
{
    pub fn new(repository: R, hasher: H) -> Self {
        Self { repository, hasher }
    }

    pub async fn is_required(&self) -> AppResult<bool> {
        Ok(!self.repository.is_configured().await?)
    }

    pub async fn create(&self, input: &SetupInput) -> AppResult<()> {
        let business_name = input.business_name.trim();
        if business_name.is_empty() || business_name.chars().count() > 160 {
            return Err(AppError::Validation(
                "El nombre del negocio debe tener entre 1 y 160 caracteres.".into(),
            ));
        }

        let locale = LOCALE_CATALOG
            .iter()
            .find(|locale| locale.code == input.default_locale_code)
            .ok_or_else(|| AppError::Validation("Seleccioná un locale de la lista.".into()))?;

        let currency_code = input.currency_code.trim();
        if currency_code.len() != 3 || !currency_code.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(AppError::Validation(
                "La moneda debe ser un código de tres letras mayúsculas.".into(),
            ));
        }

        let timezone = input.timezone.trim();
        if timezone.is_empty() || timezone.chars().count() > 64 {
            return Err(AppError::Validation(
                "La zona horaria es obligatoria.".into(),
            ));
        }

        let username = input.username.trim();
        if !valid_username(username) {
            return Err(AppError::Validation(USERNAME_MESSAGE.into()));
        }

        let display_name = input.display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 128 {
            return Err(AppError::Validation(DISPLAY_NAME_MESSAGE.into()));
        }

        if input.password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AppError::Validation(MIN_PASSWORD_MESSAGE.into()));
        }

        let password_hash = self.hasher.hash(&input.password)?;
        self.repository
            .create_initial(&SetupRecord {
                settings: NewBusinessSettings {
                    business_name: business_name.into(),
                    default_locale_code: locale.code.into(),
                    currency_code: currency_code.into(),
                    timezone: timezone.into(),
                },
                // Seed every supported profile, not only the selected one: the
                // settings form can only offer locales that already exist, so a
                // fresh `es-AR` installation would otherwise be unable to switch
                // to `es-ES` or `en-US`. Only the selected locale starts enabled.
                // Language codes stay derived from the canonical locale code.
                locales: LOCALE_CATALOG
                    .iter()
                    .map(|definition| NewBusinessLocale {
                        locale_code: definition.code.into(),
                        language_code: definition.language_code.into(),
                        display_name: definition.display_name.into(),
                        is_enabled: definition.code == locale.code,
                    })
                    .collect(),
                username: username.into(),
                display_name: display_name.into(),
                password_hash,
            })
            .await
    }
}

fn valid_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if !(3..=64).contains(&bytes.len()) {
        return false;
    }
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let inner = |byte: u8| alnum(byte) || matches!(byte, b'.' | b'_' | b'-');
    alnum(bytes[0])
        && alnum(bytes[bytes.len() - 1])
        && bytes[1..bytes.len() - 1].iter().copied().all(inner)
}
