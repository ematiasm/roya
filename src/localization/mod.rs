use std::str::FromStr;

use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::{BusinessLocale, BusinessSettings};
use crate::repositories::{
    BusinessLocaleRepository, BusinessSettingsRepository, SqliteBusinessLocaleRepository,
    SqliteBusinessSettingsRepository,
};

/// The effective presentation contract for one request.
///
/// The persisted locale is the formatting key. Language is retained separately
/// for later translation lookup, but it never overrides the locale's numeric or
/// date conventions. Values in this context are presentation metadata only:
/// repositories and domain services continue to receive canonical Decimals,
/// ISO dates, and API values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalizationContext {
    pub locale_code: String,
    pub language_code: String,
    pub currency_code: String,
    pub timezone: String,
}

impl LocalizationContext {
    /// The neutral context used while `/setup` is still available.
    pub fn fallback() -> Self {
        Self {
            locale_code: "en-US".into(),
            language_code: "en".into(),
            currency_code: "USD".into(),
            timezone: "UTC".into(),
        }
    }

    /// Format a Decimal without rounding or changing its scale.
    pub fn format_decimal(&self, value: Decimal) -> String {
        let (group, decimal_sep, _) = self.number_conventions();
        let text = value.to_string();
        let (sign, unsigned) = text
            .strip_prefix('-')
            .map_or(("", text.as_str()), |value| ("-", value));
        let (whole, decimal) = unsigned.split_once('.').unwrap_or((unsigned, ""));

        let mut grouped = String::new();
        for (index, character) in whole.chars().enumerate() {
            if index > 0 && (whole.len() - index) % 3 == 0 {
                grouped.push(group);
            }
            grouped.push(character);
        }

        if decimal.is_empty() {
            format!("{sign}{grouped}")
        } else {
            format!("{sign}{grouped}{decimal_sep}{decimal}")
        }
    }

    /// Format a Decimal as money, retaining the currency code from the
    /// business configuration. A code is honest for every supported currency;
    /// guessing a symbol would be a lie when the symbol is ambiguous.
    pub fn format_currency(&self, value: Decimal) -> String {
        format!("{} {}", self.format_decimal(value), self.currency_code)
    }

    pub fn format_quantity(&self, value: Decimal) -> String {
        self.format_decimal(value)
    }

    pub fn format_percentage(&self, value: Decimal) -> String {
        format!("{} %", self.format_decimal(value))
    }

    pub fn format_date(&self, value: NaiveDate) -> String {
        if self.uses_day_first() {
            value.format("%d/%m/%Y").to_string()
        } else {
            value.format("%m/%d/%Y").to_string()
        }
    }

    /// The business-local date in the canonical ISO form required by HTML
    /// date inputs and API filters.
    pub fn today_iso(&self) -> String {
        let timezone = self.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC);
        Utc::now()
            .with_timezone(&timezone)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    /// Format a canonical UTC SQLite timestamp in the configured business
    /// timezone. Invalid persisted timezone text falls back to UTC rather than
    /// taking down every page.
    pub fn format_timestamp(&self, value: NaiveDateTime) -> String {
        let utc = Utc.from_utc_datetime(&value);
        let timezone = self.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC);
        let local = utc.with_timezone(&timezone);
        if self.uses_day_first() {
            local.format("%d/%m/%Y %H:%M:%S").to_string()
        } else {
            local.format("%m/%d/%Y %H:%M:%S").to_string()
        }
    }

    /// Parse a Decimal from a web-form value written in this context's locale.
    /// Grouping is accepted only when its groups are valid; malformed grouping
    /// is rejected instead of silently changing a user's number.
    pub fn parse_decimal(&self, input: &str) -> Result<Decimal, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("decimal input is empty".into());
        }
        let (sign, unsigned) = input
            .strip_prefix('-')
            .map_or(("", input), |value| ("-", value));
        let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
        let (group, decimal, _) = self.number_conventions();

        let (whole, fraction) = if let Some((whole, fraction)) = unsigned.split_once(decimal) {
            if fraction.is_empty() || fraction.contains(decimal) || fraction.contains(group) {
                return Err("invalid decimal input".into());
            }
            (whole, Some(fraction))
        } else {
            (unsigned, None)
        };

        let canonical_whole = if whole.is_empty() {
            "0".into()
        } else {
            validate_grouped_digits(whole, group)?
        };
        let canonical = match fraction {
            Some(fraction) if !fraction.chars().all(|character| character.is_ascii_digit()) => {
                return Err("invalid decimal input".into())
            }
            Some(fraction) => format!("{canonical_whole}.{fraction}"),
            None => canonical_whole,
        };
        Decimal::from_str(&format!("{sign}{canonical}"))
            .map_err(|_| "invalid decimal input".to_string())
    }

    fn number_conventions(&self) -> (char, char, char) {
        if self.uses_comma_decimal() {
            ('.', ',', ',')
        } else {
            (',', '.', '.')
        }
    }

    fn uses_comma_decimal(&self) -> bool {
        self.locale_code
            .split('-')
            .next()
            .is_some_and(|language| language.eq_ignore_ascii_case("es"))
    }

    fn uses_day_first(&self) -> bool {
        self.uses_comma_decimal()
    }
}

fn validate_grouped_digits(value: &str, group: char) -> Result<String, String> {
    if value.chars().all(|character| character.is_ascii_digit()) && !value.contains(group) {
        return Ok(value.to_string());
    }
    let groups: Vec<&str> = value.split(group).collect();
    let first_valid = (1..=3).contains(&groups.first().map_or(0, |group| group.len()));
    let rest_valid = groups.iter().skip(1).all(|part| part.len() == 3);
    if !first_valid
        || !rest_valid
        || !groups
            .iter()
            .all(|part| part.chars().all(|character| character.is_ascii_digit()))
    {
        return Err("invalid decimal grouping".into());
    }
    Ok(groups.concat())
}

/// Resolve the effective context from the persisted business configuration.
pub fn resolve_context(
    settings: Option<&BusinessSettings>,
    locales: &[BusinessLocale],
) -> LocalizationContext {
    let Some(settings) = settings else {
        return LocalizationContext::fallback();
    };

    let selected = locales
        .iter()
        .find(|locale| {
            locale.is_enabled
                && locale
                    .locale_code
                    .eq_ignore_ascii_case(&settings.default_locale_code)
        })
        .or_else(|| {
            let language = settings
                .default_locale_code
                .split('-')
                .next()
                .unwrap_or_default();
            locales.iter().find(|locale| {
                locale.is_enabled && locale.language_code.eq_ignore_ascii_case(language)
            })
        })
        .or_else(|| locales.iter().find(|locale| locale.is_enabled));

    let locale_code = selected
        .map(|locale| locale.locale_code.clone())
        .unwrap_or_else(|| settings.default_locale_code.clone());
    let language_code = selected
        .map(|locale| locale.language_code.clone())
        .unwrap_or_else(|| {
            locale_code
                .split('-')
                .next()
                .unwrap_or("en")
                .to_ascii_lowercase()
        });

    LocalizationContext {
        locale_code,
        language_code,
        currency_code: settings.currency_code.clone(),
        timezone: settings.timezone.clone(),
    }
}

/// Load the request context through the existing business-configuration
/// repositories. No domain service or repository receives a locale.
pub async fn load_context(pool: &SqlitePool) -> AppResult<LocalizationContext> {
    let settings = SqliteBusinessSettingsRepository::new(pool.clone())
        .find(1)
        .await?;
    let locales = SqliteBusinessLocaleRepository::new(pool.clone())
        .list()
        .await?;
    Ok(resolve_context(settings.as_ref(), &locales))
}
