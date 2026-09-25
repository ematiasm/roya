use std::str::FromStr;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use rust_decimal::Decimal;
use sqlx::{sqlite::SqlitePoolOptions, Row};
use tower::ServiceExt;

use crate::models::{
    NewBusinessLocale, NewBusinessSettings, NewTax, Tax, UpdateBusinessLocale,
    UpdateBusinessSettings,
};
use crate::repositories::{
    BusinessLocaleRepository, BusinessSettingsRepository, ProductTaxRepository,
    SqliteProductTaxRepository, SqliteTaxRepository, TaxRepository,
};
use crate::routes::{router, AppState};
use crate::security::test_support;
use crate::services::settings::UpdateBusinessConfiguration;

async fn configured_pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let settings = crate::repositories::SqliteBusinessSettingsRepository::new(pool.clone());
    settings
        .create(&NewBusinessSettings {
            business_name: "Acme Store".into(),
            default_locale_code: "es-AR".into(),
            currency_code: "ARS".into(),
            timezone: "America/Argentina/Buenos_Aires".into(),
        })
        .await
        .unwrap();
    let locales = crate::repositories::SqliteBusinessLocaleRepository::new(pool.clone());
    for input in [
        NewBusinessLocale {
            locale_code: "es-AR".into(),
            language_code: "es".into(),
            display_name: "Español (Argentina)".into(),
            is_enabled: true,
        },
        NewBusinessLocale {
            locale_code: "en-US".into(),
            language_code: "en".into(),
            display_name: "English (United States)".into(),
            is_enabled: true,
        },
    ] {
        locales.create(&input).await.unwrap();
    }
    pool
}

async fn configured_state() -> AppState {
    let pool = configured_pool().await;
    let state = test_support::app_state(pool);
    state.refresh_setup_requirement().await.unwrap();
    assert!(!state.setup_required());
    state
}

/// A brand-new installation that has already completed first-run setup: the
/// state `/settings` really starts from on a fresh deployment. Distinct from
/// [`configured_pool`], which hand-seeds an arbitrary two-profile fixture.
async fn fresh_setup_state() -> AppState {
    fresh_setup_state_with_default("es-AR").await
}

async fn fresh_setup_state_with_default(default_locale_code: &str) -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let state = test_support::app_state(pool);
    state
        .setup_service
        .create(&crate::services::setup::SetupInput {
            business_name: "Acme Store".into(),
            default_locale_code: default_locale_code.into(),
            currency_code: "ARS".into(),
            timezone: "America/Argentina/Buenos_Aires".into(),
            username: "setup.admin".into(),
            display_name: "Setup Admin".into(),
            password: "correct horse battery staple".into(),
        })
        .await
        .unwrap();
    state.refresh_setup_requirement().await.unwrap();
    assert!(!state.setup_required());
    state
}

async fn existing_single_locale_state() -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run_to(20240101000037, &pool).await.unwrap();
    sqlx::query(
        "INSERT INTO business_settings \
         (id, business_name, default_locale_code, currency_code, timezone) \
         VALUES (1, 'Acme Store', 'es-AR', 'ARS', 'America/Argentina/Buenos_Aires')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) \
         VALUES ('es-AR', 'es', 'Español (Argentina)', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    migrator.run(&pool).await.unwrap();
    let state = test_support::app_state(pool);
    state.refresh_setup_requirement().await.unwrap();
    assert!(!state.setup_required());
    state
}

fn valid_update() -> UpdateBusinessConfiguration {
    UpdateBusinessConfiguration {
        settings: UpdateBusinessSettings {
            business_name: "Roya Market".into(),
            default_locale_code: "en-US".into(),
            currency_code: "USD".into(),
            timezone: "UTC".into(),
        },
        locales: vec![
            UpdateBusinessLocale {
                locale_code: "es-AR".into(),
                display_name: "Español (Argentina)".into(),
                is_enabled: false,
            },
            UpdateBusinessLocale {
                locale_code: "en-US".into(),
                display_name: "English (United States)".into(),
                is_enabled: true,
            },
        ],
    }
}

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_form(app: &axum::Router, cookie: &str, payload: &str) -> axum::response::Response {
    post_form_to(app, cookie, "/settings", payload, &[]).await
}

/// A form POST to an ARBITRARY path, optionally carrying HTMX's request marker:
/// the taxes tab is an HTMX surface, so a test that must prove the fragment
/// contract asks for it the way the browser does.
async fn post_form_to(
    app: &axum::Router,
    cookie: &str,
    uri: &str,
    payload: &str,
    htmx: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    for (name, value) in htmx {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(payload.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn body(response: axum::response::Response) -> String {
    String::from_utf8_lossy(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .into_owned()
}

#[tokio::test]
async fn settings_currency_selector_renders_catalog_and_selects_persisted_code() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state);
    let cookie = test_support::cookie_for(&token);

    let response = get(&app, "/settings", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;

    assert!(page
        .contains("<select id=\"currency_code\" class=\"field\" name=\"currency_code\" required>"));
    assert!(page.contains("<option value=\"ARS\" selected>Argentine Peso</option>"));
    assert!(page.contains("<option value=\"EUR\">Euro</option>"));
    assert!(page.contains("<option value=\"USD\">United States Dollar</option>"));
    assert!(!page.contains("name=\"currency_code\" required minlength=\"3\""));
}

#[tokio::test]
async fn settings_currency_selector_preserves_legacy_code_as_selected_fallback() {
    let state = configured_state().await;
    let mut update = valid_update();
    update.settings.currency_code = "XBT".into();
    let (settings, _) = state.settings_service.update(update).await.unwrap();
    assert_eq!(settings.currency_code, "XBT");

    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state);
    let cookie = test_support::cookie_for(&token);

    let response = get(&app, "/settings", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;

    assert!(page.contains("<option value=\"XBT\" selected>XBT</option>"));
    assert_eq!(page.matches("value=\"XBT\"").count(), 1);
}

#[tokio::test]
async fn settings_currency_selector_preserves_submitted_currency_on_validation_redisplay() {
    let state = configured_state().await;
    let mut update = valid_update();
    update.settings.currency_code = "USD".into();
    let (settings, _) = state.settings_service.update(update).await.unwrap();
    assert_eq!(settings.currency_code, "USD");

    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state);
    let cookie = test_support::cookie_for(&token);

    let response = post_form(
        &app,
        &cookie,
        "business_name=Acme+Store&default_locale_code=en-US&currency_code=EUR&timezone=+++\
         &locale_code_0=en-US&locale_code_1=es-AR\
         &display_name_0=English+(United+States)&display_name_1=Espa%C3%B1ol+(Argentina)\
         &enabled_0=on",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let page = body(response).await;

    assert!(page.contains("<option value=\"EUR\" selected>Euro</option>"));
    assert!(!page.contains("<option value=\"ARS\" selected>"));
}

#[tokio::test]
async fn settings_persistence_updates_settings_and_every_locale_profile_with_audit_timestamps() {
    let state = configured_state().await;
    sqlx::query(
        "UPDATE business_settings SET updated_at = '2000-01-01T00:00:00.000Z'; \
         UPDATE business_locales SET updated_at = '2000-01-01T00:00:00.000Z'",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let (settings, locales) = state.settings_service.update(valid_update()).await.unwrap();

    assert_eq!(settings.business_name, "Roya Market");
    assert_eq!(settings.default_locale_code, "en-US");
    assert_eq!(settings.currency_code, "USD");
    assert_eq!(settings.timezone, "UTC");
    assert!(settings.updated_at.to_string().starts_with("20"));
    assert!(settings.updated_at.to_string().as_str() > "2000-01-01");
    assert_eq!(locales.len(), 2);
    assert_eq!(locales[0].locale_code, "es-AR");
    assert_eq!(locales[0].display_name, "Español (Argentina)");
    assert!(!locales[0].is_enabled);
    assert!(locales[0].updated_at.to_string().as_str() > "2000-01-01");
    assert_eq!(locales[1].locale_code, "en-US");
    assert_eq!(locales[1].display_name, "English (United States)");
    assert!(locales[1].is_enabled);
    assert!(locales[1].updated_at.to_string().as_str() > "2000-01-01");
}

#[tokio::test]
async fn settings_validation_rejects_currency_timezone_and_unknown_default_without_writes() {
    let state = configured_state().await;
    let cases = [
        ("currency_code", "usd"),
        ("timezone", "   "),
        ("default_locale_code", "fr-FR"),
        ("display_name", "   "),
    ];
    for (field, value) in cases {
        let mut update = valid_update();
        match field {
            "currency_code" => update.settings.currency_code = value.into(),
            "timezone" => update.settings.timezone = value.into(),
            "default_locale_code" => update.settings.default_locale_code = value.into(),
            "display_name" => update.locales[0].display_name = value.into(),
            _ => unreachable!(),
        }
        assert!(
            state.settings_service.update(update).await.is_err(),
            "{field}"
        );
    }

    let row: (String, String, String, String) = sqlx::query_as(
        "SELECT business_name, default_locale_code, currency_code, timezone \
         FROM business_settings WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "Acme Store".into(),
            "es-AR".into(),
            "ARS".into(),
            "America/Argentina/Buenos_Aires".into()
        )
    );
    let enabled: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM business_locales WHERE is_enabled = 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(enabled, 2);
}

#[tokio::test]
async fn settings_default_locale_must_be_enabled_and_can_be_changed_before_disabling_the_old_default(
) {
    let state = configured_state().await;
    let (settings, locales) = state.settings_service.update(valid_update()).await.unwrap();
    assert_eq!(settings.default_locale_code, "en-US");
    assert!(!locales[0].is_enabled);
    assert!(locales[1].is_enabled);

    let mut unsafe_update = valid_update();
    unsafe_update.settings.default_locale_code = "es-AR".into();
    let error = state
        .settings_service
        .update(unsafe_update)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("enabled"),
        "the disabled default must be refused explicitly: {error}"
    );

    let persisted: (String, i64) = sqlx::query_as(
        "SELECT s.default_locale_code, l.is_enabled \
         FROM business_settings s JOIN business_locales l \
         ON l.locale_code = s.default_locale_code WHERE s.id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(persisted, ("en-US".into(), 1));
}

#[tokio::test]
async fn settings_permission_is_seeded_and_irrevocable_from_the_protected_admin_role() {
    let state = configured_state().await;
    let permission =
        sqlx::query("SELECT id, description FROM permissions WHERE code = 'settings.manage'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        permission.get::<String, _>("description"),
        "Administrar la configuración del negocio"
    );
    let admin_grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM role_permissions rp \
         JOIN roles r ON r.id = rp.role_id \
         JOIN permissions p ON p.id = rp.permission_id \
         WHERE r.code = 'admin' AND p.code = 'settings.manage'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(admin_grants, 1);

    let permission_id = permission.get::<i64, _>("id");
    let revoke = sqlx::query("DELETE FROM role_permissions WHERE permission_id = ?")
        .bind(permission_id)
        .execute(&state.pool)
        .await;
    assert!(
        revoke.is_err(),
        "the protected-role trigger must refuse revocation"
    );
    let admin_grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM role_permissions rp \
         JOIN roles r ON r.id = rp.role_id \
         JOIN permissions p ON p.id = rp.permission_id \
         WHERE r.code = 'admin' AND p.code = 'settings.manage'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(admin_grants, 1);
}

#[tokio::test]
async fn settings_page_is_gated_and_its_sidebar_entry_is_permission_matched() {
    let state = configured_state().await;
    test_support::seed_session_without_roles(&state.pool)
        .await
        .unwrap();
    let app = router(state.clone());

    let refused = get(&app, "/settings", test_support::TEST_COOKIE).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let refused = body(refused).await;
    assert!(refused.contains("settings.manage"));
    assert!(!refused.contains("data-nav=\"settings\""));

    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let allowed = get(&app, "/settings", &test_support::cookie_for(&token)).await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let allowed = body(allowed).await;
    assert!(allowed.contains("Configuración del negocio"));
    assert!(allowed.contains("data-nav=\"settings\""));
    assert!(allowed.contains("aria-current=\"page\""));
    assert!(allowed.contains("name=\"business_name\""));
    assert!(allowed.contains("name=\"default_locale_code\""));
    assert!(allowed.contains("name=\"currency_code\""));
    assert!(allowed.contains("name=\"timezone\""));
    assert!(allowed.contains("name=\"display_name_0\""));
    assert!(allowed.contains("name=\"enabled_0\""));
}

#[tokio::test]
async fn settings_validation_renders_localized_known_errors_without_changing_service_contracts() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let form = |currency: &str| {
        format!(
            "business_name=Acme+Store&default_locale_code=es-AR&currency_code={currency}\
             &timezone=America%2FArgentina%2FBuenos_Aires\
             &locale_code_0=es-AR&locale_code_1=en-US\
             &display_name_0=Espa%C3%B1ol+(Argentina)&display_name_1=English+(United+States)\
             &enabled_0=on&enabled_1=on"
        )
    };

    let spanish = post_form(&app, &cookie, &form("usd")).await;
    assert_eq!(spanish.status(), StatusCode::BAD_REQUEST);
    let spanish = body(spanish).await;
    assert!(
        spanish.contains("La moneda debe ser un código de tres letras mayúsculas."),
        "{spanish}"
    );

    let valid_update = form("USD")
        .replace("default_locale_code=es-AR", "default_locale_code=en-US")
        .replace("&enabled_0=on", "");
    let update = post_form(&app, &cookie, &valid_update).await;
    assert_eq!(update.status(), StatusCode::SEE_OTHER);

    let english_form =
        form("usd").replace("default_locale_code=es-AR", "default_locale_code=en-US");
    let english = post_form(&app, &cookie, &english_form).await;
    assert_eq!(english.status(), StatusCode::BAD_REQUEST);
    let english = body(english).await;
    assert!(
        english.contains("The currency must be a three-letter uppercase code."),
        "{english}"
    );

    let persisted: (String, String) = sqlx::query_as(
        "SELECT default_locale_code, currency_code FROM business_settings WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(persisted, ("en-US".into(), "USD".into()));
}

#[tokio::test]
async fn settings_form_persists_the_complete_profile_and_renders_the_new_configuration() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let payload =
        "business_name=Roya+Market&default_locale_code=en-US&currency_code=USD&timezone=UTC\
&locale_code_0=es-AR&locale_code_1=en-US\
&display_name_0=Espa%C3%B1ol+(Argentina)&display_name_1=English+(United+States)\
&enabled_1=on";
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_string());
    let response_body = body(response).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "settings form was rejected: {response_body}"
    );
    assert_eq!(location.as_deref(), Some("/settings?saved=true"));

    let page = get(&app, "/settings?saved=true", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    assert!(page.contains("Settings saved"));
    assert!(page.contains(">Business settings</h2>"));
    assert!(page.contains("data-action=\"Save settings\""));
    assert!(page.contains("value=\"Roya Market\""));
    assert!(page.contains("value=\"USD\""));
    assert!(page.contains("value=\"en-US\" selected"));
    assert!(page.contains("value=\"UTC\""));
    assert!(page.contains("value=\"Español (Argentina)\""));
    assert!(page.contains("value=\"English (United States)\""));

    let persisted: (String, String, String, String) = sqlx::query_as(
        "SELECT business_name, default_locale_code, currency_code, timezone \
         FROM business_settings WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        persisted,
        (
            "Roya Market".into(),
            "en-US".into(),
            "USD".into(),
            "UTC".into()
        )
    );
    let enabled_codes: Vec<String> = sqlx::query_scalar(
        "SELECT locale_code FROM business_locales WHERE is_enabled = 1 ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(enabled_codes, vec!["en-US".to_string()]);
}

#[tokio::test]
async fn settings_page_renders_default_locale_first_and_round_trips_all_profiles() {
    let state = fresh_setup_state_with_default("en-US").await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    let page = get(&app, "/settings", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    for (index, (code, display_name)) in [
        ("en-US", "English (United States)"),
        ("es-AR", "Español (Argentina)"),
        ("es-ES", "Español (España)"),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            page.contains(&format!("name=\"locale_code_{index}\" value=\"{code}\"")),
            "the configured default must lead the stable locale order: {page}"
        );
        assert!(
            page.contains(&format!(
                "name=\"display_name_{index}\" value=\"{display_name}\""
            )),
            "positional display names must follow their locale rows: {page}"
        );
    }

    let response = post_form(
        &app,
        &cookie,
        "business_name=Acme+Store&default_locale_code=en-US&currency_code=ARS\
         &timezone=America%2FArgentina%2FBuenos_Aires\
         &locale_code_0=en-US&locale_code_1=es-AR&locale_code_2=es-ES\
         &display_name_0=English+(United+States)&display_name_1=Espa%C3%B1ol+(Argentina)\
         &display_name_2=Espa%C3%B1ol+(Espa%C3%B1a)&enabled_0=on",
    )
    .await;
    let status = response.status();
    let response = body(response).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{response}");

    let profiles: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT locale_code, display_name, is_enabled FROM business_locales ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        profiles,
        vec![
            ("es-AR".into(), "Español (Argentina)".into(), false),
            ("es-ES".into(), "Español (España)".into(), false),
            ("en-US".into(), "English (United States)".into(), true),
        ]
    );
}

/// The reported bug: a fresh `es-AR` installation could not change its
/// presentation language because setup persisted a single locale profile.
/// Setup must seed every supported profile so `/settings` can offer it, and
/// switching the default to `en-US` must re-render the next request in English.
#[tokio::test]
async fn fresh_installation_can_switch_its_presentation_language_from_settings() {
    let state = fresh_setup_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    // Every supported profile is offered, and the seeded default is presented
    // in Spanish.
    let page = get(&app, "/settings", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    for (code, display_name) in [
        ("es-AR", "Español (Argentina)"),
        ("es-ES", "Español (España)"),
        ("en-US", "English (United States)"),
    ] {
        assert!(
            page.contains(&format!("value=\"{code}\"")),
            "/settings must offer {code} on a fresh installation: {page}"
        );
        assert!(
            page.contains(display_name),
            "/settings must offer the {code} display name: {page}"
        );
    }
    assert!(
        page.contains(">Configuración del negocio</h2>"),
        "the seeded es-AR locale presents Spanish before the switch: {page}"
    );

    // The operator makes English the default and the only enabled locale.
    let switch = post_form(
        &app,
        &cookie,
        "business_name=Acme+Store&default_locale_code=en-US&currency_code=ARS\
         &timezone=America%2FArgentina%2FBuenos_Aires\
         &locale_code_0=es-AR&locale_code_1=es-ES&locale_code_2=en-US\
         &display_name_0=Espa%C3%B1ol+(Argentina)&display_name_1=Espa%C3%B1ol+(Espa%C3%B1a)\
         &display_name_2=English+(United+States)&enabled_2=on",
    )
    .await;
    assert_eq!(switch.status(), StatusCode::SEE_OTHER, "{switch:?}");
    assert_eq!(switch.headers()[header::LOCATION], "/settings?saved=true");

    // The next request renders English presentation.
    let page = get(&app, "/settings?saved=true", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    assert!(
        page.contains(">Business settings</h2>"),
        "the switched default must present English: {page}"
    );
    assert!(page.contains("Settings saved"));
    assert!(page.contains("value=\"en-US\" selected"));

    let persisted: (String, String) = sqlx::query_as(
        "SELECT s.default_locale_code, l.language_code \
         FROM business_settings s JOIN business_locales l \
         ON l.locale_code = s.default_locale_code WHERE s.id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(persisted, ("en-US".into(), "en".into()));
    let enabled_codes: Vec<String> = sqlx::query_scalar(
        "SELECT locale_code FROM business_locales WHERE is_enabled = 1 ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(enabled_codes, vec!["en-US".to_string()]);
}

#[tokio::test]
async fn existing_single_profile_database_is_backfilled_and_can_switch_to_english() {
    let state = existing_single_locale_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    let locales: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT locale_code, language_code, display_name, is_enabled \
         FROM business_locales ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        locales,
        vec![
            (
                "es-AR".into(),
                "es".into(),
                "Español (Argentina)".into(),
                true
            ),
            (
                "es-ES".into(),
                "es".into(),
                "Español (España)".into(),
                false
            ),
            (
                "en-US".into(),
                "en".into(),
                "English (United States)".into(),
                false
            ),
        ]
    );

    let page = get(&app, "/settings", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    for (code, display_name) in [
        ("es-AR", "Español (Argentina)"),
        ("es-ES", "Español (España)"),
        ("en-US", "English (United States)"),
    ] {
        assert!(page.contains(&format!("value=\"{code}\"")));
        assert!(page.contains(display_name));
    }
    assert!(page.contains(">Configuración del negocio</h2>"));

    let switch = post_form(
        &app,
        &cookie,
        "business_name=Acme+Store&default_locale_code=en-US&currency_code=ARS\
         &timezone=America%2FArgentina%2FBuenos_Aires\
         &locale_code_0=es-AR&locale_code_1=es-ES&locale_code_2=en-US\
         &display_name_0=Espa%C3%B1ol+(Argentina)&display_name_1=Espa%C3%B1ol+(Espa%C3%B1a)\
         &display_name_2=English+(United+States)&enabled_2=on",
    )
    .await;
    assert_eq!(switch.status(), StatusCode::SEE_OTHER, "{switch:?}");
    assert_eq!(switch.headers()[header::LOCATION], "/settings?saved=true");

    let page = get(&app, "/settings?saved=true", &cookie).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = body(page).await;
    assert!(page.contains(">Business settings</h2>"));
    assert!(page.contains("Settings saved"));
    assert!(page.contains("value=\"en-US\" selected"));

    let persisted: (String, String) = sqlx::query_as(
        "SELECT s.default_locale_code, l.language_code \
         FROM business_settings s JOIN business_locales l \
         ON l.locale_code = s.default_locale_code WHERE s.id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(persisted, ("en-US".into(), "en".into()));
    let enabled_codes: Vec<String> = sqlx::query_scalar(
        "SELECT locale_code FROM business_locales WHERE is_enabled = 1 ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(enabled_codes, vec!["en-US".to_string()]);
}

// ---------------------------------------------------------------------------
// T3 — the permission-gated Taxes tab and the hard-delete safeguard
// ---------------------------------------------------------------------------
//
// THE CANONICAL TAX-DEFINITION SURFACE ON THE WEB (T3, narrowed by U1).
//
// SCOPE FIRST: everything in this block is about the WEB surface. The JSON API
// is a different surface with different permissions and is deliberately not
// covered by the claim below — the residual is named at the bottom.
//
// Settings owns tax definitions on the web, so every mutation a definition
// needs is addressed under `/web/settings/taxes…` and every one of those routes
// is gated by `settings.manage`.
//
// There USED to be a second WEB surface: `/web/taxes…`, gated by
// `inventory.write`, rendered by the Products screen's tax catalogue. It is
// gone. The product-price-ladder unit (U1) removed the catalogue from
// `templates/products.html` and unregistered those routes, so the same form is
// no longer reachable from two screens under two different permissions, and no
// `inventory.write` WEB route can create, rename, re-rate or
// activate/deactivate a tax. `tax_web_definition_administration_is_exclusive_
// to_settings_manage` in `tax_tests.rs` is the test that proves it; it replaces
// `tax_inventory_catalogue_keeps_its_own_write_access_for_an_inventory_writer`,
// which pinned the opposite claim on purpose before U1 inverted it.
//
// What is deliberately NOT settings-gated is the product-tax ASSOCIATION:
// linking and unlinking a tax to a product is inventory state, so it stays on
// `POST /web/product-taxes` and `POST /web/product-taxes/unlink` behind
// `inventory.write`. The two surfaces never share a handler — a principal may
// hold one permission and not the other.
//
// THE RESIDUAL: the JSON API still administers tax definitions behind
// `inventory.write` (`POST /api/taxes`, `PUT /api/taxes/{id}` and
// `POST /api/taxes/{id}/deactivate`, all `Require<InventoryWrite>` in
// `src/routes/inventory_api.rs`). Pre-existing, not a U1 regression, and left
// alone on purpose: narrowing it is an open product decision, not a side effect
// of de-duplicating the web catalogue. So the claim above is "the WEB catalogue
// is settings-only" — never "tax administration is settings-only".

/// One tax created through the real repository, for the fixtures that only need
/// one to exist before the surface under test is exercised.
async fn existing_tax(state: &AppState, code: &str, rate: &str) -> Tax {
    let actor: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'sistema'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    SqliteTaxRepository::new(state.pool.clone())
        .create(
            actor,
            &NewTax {
                code: code.into(),
                name: format!("Tax {code}"),
                rate: Decimal::from_str(rate).unwrap(),
                is_active: true,
            },
        )
        .await
        .unwrap()
}

async fn linked_product(state: &AppState, sku: &str, tax_id: i64) -> i64 {
    let actor: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'sistema'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let product_id: i64 = sqlx::query_scalar(
        "INSERT INTO products (sku, name, kind, unit, sale_price, track_stock, created_by)
         VALUES (?, ?, 'Product', 'unit', '10', 0, ?) RETURNING id",
    )
    .bind(sku)
    .bind(format!("Product {sku}"))
    .bind(actor)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    SqliteProductTaxRepository::new(state.pool.clone())
        .link(actor, product_id, tax_id)
        .await
        .unwrap();
    product_id
}

fn htmx() -> [(&'static str, &'static str); 1] {
    [("HX-Request", "true")]
}

/// The Taxes tab is a view of the SAME single-form page, not a second page:
/// the business tab keeps its own form untouched, the taxes tab renders the
/// catalogue instead, and both are reachable from one tab strip.
#[tokio::test]
async fn settings_taxes_tab_is_reachable_and_leaves_the_business_tab_untouched() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    let business = get(&app, "/settings", &cookie).await;
    assert_eq!(business.status(), StatusCode::OK);
    let business = body(business).await;
    assert!(
        business.contains("data-settings-tab=\"business\""),
        "the resting page must stay the business tab: {business}"
    );
    assert!(
        business.contains("aria-current=\"page\""),
        "the selected tab must be marked: {business}"
    );
    for field in [
        "name=\"business_name\"",
        "name=\"default_locale_code\"",
        "name=\"currency_code\"",
        "name=\"timezone\"",
        "name=\"display_name_0\"",
        "name=\"enabled_0\"",
    ] {
        assert!(
            business.contains(field),
            "the business tab must keep its own form ({field}): {business}"
        );
    }
    assert!(
        !business.contains("id=\"settings-tax-list\""),
        "the taxes catalogue must not render inside the business tab: {business}"
    );

    let taxes = get(&app, "/settings?tab=taxes", &cookie).await;
    assert_eq!(taxes.status(), StatusCode::OK);
    let taxes = body(taxes).await;
    assert!(
        taxes.contains("data-settings-tab=\"taxes\""),
        "the taxes tab must be selectable: {taxes}"
    );
    assert!(
        taxes.contains("id=\"settings-tax-list\""),
        "the taxes tab must carry the catalogue: {taxes}"
    );
    assert!(
        taxes.contains("name=\"code\""),
        "the catalogue must offer the tax fields: {taxes}"
    );
    assert!(
        !taxes.contains("name=\"business_name\""),
        "the taxes tab must not render the business form: {taxes}"
    );

    let fragment = get(&app, "/web/settings/taxes", &cookie).await;
    assert_eq!(
        fragment.status(),
        StatusCode::OK,
        "the catalogue list must be addressable as its own fragment"
    );
}

/// Reading the Taxes tab is a `settings.manage` read. A principal without the
/// permission can neither see the tab nor fetch its fragment, and the refusal
/// page carries no catalogue at all.
#[tokio::test]
async fn settings_taxes_tab_is_refused_without_the_settings_manage_permission() {
    let state = configured_state().await;
    test_support::seed_session_without_roles(&state.pool)
        .await
        .unwrap();
    let app = router(state.clone());

    let refused = get(&app, "/settings?tab=taxes", test_support::TEST_COOKIE).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let refused = body(refused).await;
    assert!(refused.contains("settings.manage"), "{refused}");
    assert!(
        !refused.contains("id=\"settings-tax-list\""),
        "a refused principal must not see the catalogue: {refused}"
    );

    let fragment = get(&app, "/web/settings/taxes", test_support::TEST_COOKIE).await;
    assert_eq!(fragment.status(), StatusCode::FORBIDDEN);
}

/// Every mutation is gated by `settings.manage` and not by `inventory.write`:
/// a principal that holds the inventory gate cannot touch the definition
/// surface, and nothing is written. (Since U1 it cannot reach a definition from
/// any WEB address — `tax_web_definition_administration_is_exclusive_to_settings_
/// manage` in `tax_tests.rs` proves the inventory side is now 404, not 403.
/// The JSON API is out of scope for that test and still admits `inventory.write`.)
#[tokio::test]
async fn settings_tax_administration_is_refused_without_the_settings_manage_permission() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["inventory.write"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let tax = existing_tax(&state, "DENIED", "21").await;

    for uri in [
        "/web/settings/taxes".to_string(),
        format!("/web/settings/taxes/delete-confirm/{}", tax.id),
    ] {
        let response = get(&app, &uri, &cookie).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    for (uri, payload) in [
        (
            "/web/settings/taxes",
            "code=NEW&name=New&rate=5".to_string(),
        ),
        (
            "/web/settings/taxes/edit",
            format!("id={}&code=DENIED&name=Denied&rate=9", tax.id),
        ),
        ("/web/settings/taxes/activate", format!("id={}", tax.id)),
        ("/web/settings/taxes/deactivate", format!("id={}", tax.id)),
        (
            "/web/settings/taxes/delete",
            format!("id={}&confirm=on", tax.id),
        ),
    ] {
        let response = post_form_to(&app, &cookie, &uri, &payload, &htmx()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }

    let stored = state.tax_service.get_tax(tax.id).await.unwrap();
    assert_eq!(stored.code, "DENIED", "a refused mutation writes nothing");
    assert!(stored.is_active, "a refused mutation writes nothing");
    assert!(
        SqliteTaxRepository::new(state.pool.clone())
            .find_by_code("NEW")
            .await
            .unwrap()
            .is_none(),
        "no tax may be created by a refused mutation"
    );
}

/// A rate typed the way the locale writes it (`21,5`) is parsed through the
/// request context, stored as the canonical decimal (`21.5`) and read back as
/// the localized percentage — the one decimal rule the rest of the app uses.
#[tokio::test]
async fn settings_taxes_tab_creates_a_tax_from_a_localized_rate() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    let response = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes",
        "code=IVAWEB&name=IVA+Web&rate=21%2C5",
        &htmx(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = body(response).await;
    assert!(
        response.contains("21,5 %"),
        "the catalogue must show the localized percentage: {response}"
    );

    let stored_rate: String = sqlx::query_scalar("SELECT rate FROM taxes WHERE code = 'IVAWEB'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        stored_rate, "21.5",
        "a localized rate is parsed to a canonical decimal, never persisted as typed"
    );
}

/// Editing re-rates and renames through the same service contract, and a code
/// another tax already owns is refused as a conflict with the stored row
/// untouched.
#[tokio::test]
async fn settings_taxes_tab_edits_a_tax_and_refuses_a_duplicate_code() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let tax = existing_tax(&state, "IVA21", "21").await;
    let other = existing_tax(&state, "IVA105", "10.5").await;

    let response = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/edit",
        &format!("id={}&code=IVA21&name=IVA+21+Editado&rate=22%2C25", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = body(response).await;
    assert!(response.contains("IVA 21 Editado"), "{response}");
    assert!(response.contains("22,25 %"), "{response}");

    let edited = state.tax_service.get_tax(tax.id).await.unwrap();
    assert_eq!(edited.name, "IVA 21 Editado");
    assert_eq!(edited.rate, Decimal::from_str("22.25").unwrap());

    let duplicate = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/edit",
        &format!("id={}&code=IVA105&name=Robado&rate=5", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let duplicate = body(duplicate).await;
    assert!(
        !duplicate.contains("FOREIGN KEY") && !duplicate.contains("UNIQUE"),
        "no raw database text may reach the operator: {duplicate}"
    );

    // The same refusal WITHOUT the HTMX marker is a rendered page carrying the
    // same sentence, never a JSON body a browser would dump on the operator.
    let plain = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/edit",
        &format!("id={}&code=IVA105&name=Robado&rate=5", tax.id),
        &[],
    )
    .await;
    assert_eq!(plain.status(), StatusCode::CONFLICT);
    let plain = body(plain).await;
    assert!(plain.contains("data-notice=\"error\""), "{plain}");
    assert!(
        plain.contains("Ya existe un impuesto con este código."),
        "the rendered page must carry the localized refusal: {plain}"
    );
    assert!(!plain.contains("UNIQUE"), "{plain}");

    assert_eq!(
        state.tax_service.get_tax(tax.id).await.unwrap().code,
        "IVA21",
        "a refused edit writes nothing"
    );
    assert_eq!(
        state.tax_service.get_tax(other.id).await.unwrap().name,
        "Tax IVA105"
    );
}

/// Activation is its own action, not a side effect of an edit: a deactivated
/// tax can be activated again from the tab, and both transitions are audited.
#[tokio::test]
async fn settings_taxes_tab_activates_and_deactivates_a_tax() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let tax = existing_tax(&state, "IVA21", "21").await;
    // The actor is the principal whose session the requests ride, and a
    // lifecycle change that moved a tax without recording WHO moved it (or
    // WHEN) would be unauditable. The probe principal's id is resolved through
    // its own session token rather than guessed from a username, so the
    // assertion names the actor the request really carried.
    let actor: i64 = sqlx::query_scalar("SELECT user_id FROM sessions WHERE token_hash = ?")
        .bind(crate::security::session::hash_token(&token))
        .fetch_one(&state.pool)
        .await
        .unwrap();

    /// Put the row's `updated_at` in the past so the ordering assertion is a
    /// real one and not a tie between two writes in the same millisecond.
    async fn backdate(pool: &sqlx::SqlitePool, id: i64) {
        sqlx::query("UPDATE taxes SET updated_at = '2000-01-01T00:00:00.000Z' WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    backdate(&state.pool, tax.id).await;
    let response = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/deactivate",
        &format!("id={}", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let inactive = state.tax_service.get_tax(tax.id).await.unwrap();
    assert!(!inactive.is_active);
    assert_eq!(
        inactive.updated_by,
        Some(actor),
        "a deactivation must record the actor who performed it"
    );
    assert!(
        inactive.updated_at.to_string().as_str() > "2000-01-01T00:00:00.000Z",
        "a deactivation must move updated_at forward, got {}",
        inactive.updated_at
    );

    backdate(&state.pool, tax.id).await;
    let response = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/activate",
        &format!("id={}", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let active = state.tax_service.get_tax(tax.id).await.unwrap();
    assert!(active.is_active);
    assert_eq!(
        active.updated_by,
        Some(actor),
        "a reactivation must record the actor who performed it"
    );
    assert!(
        active.updated_at.to_string().as_str() > "2000-01-01T00:00:00.000Z",
        "a reactivation must move updated_at forward, got {}",
        active.updated_at
    );
    assert_eq!(
        active.created_by, inactive.created_by,
        "a lifecycle change must not rewrite who created the tax"
    );
}

/// A FAULT is not a refusal an operator can act on, and it must not be dressed
/// as one.
///
/// The catalogue's own refusals (a duplicate code, a referenced tax) are
/// decisions: they answer 409/400 with a sentence that says what to do. A
/// broken request is neither, and the app already has one convention for it —
/// `AppError: IntoResponse` logs a database error and answers 500 with a
/// generic body. This test drives a REAL fault by removing the table the
/// handler writes to, so the mapping is proven against the database and not
/// against a constructed error value.
#[tokio::test]
async fn settings_taxes_faults_answer_500_without_database_text() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    sqlx::query("DROP TABLE taxes")
        .execute(&state.pool)
        .await
        .unwrap();

    for htmx in [htmx().as_slice(), [].as_slice()] {
        let response = post_form_to(
            &app,
            &cookie,
            "/web/settings/taxes",
            "code=BROKEN&name=Broken&rate=5",
            htmx,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a broken request is a 500, never a 400 the operator would act on"
        );
        let response = body(response).await;
        assert_eq!(
            response, "{\"error\":\"database error\"}",
            "the body is the app's generic fault shape, from the same mapping \
             every other route uses"
        );
        for leak in ["no such table", "SQLITE", "sqlite", "taxes"] {
            assert!(
                !response.contains(leak),
                "the fault must not leak database text ({leak}): {response}"
            );
        }
    }
}

/// A hard delete is a two-step action, enforced by the SERVER and not only by
/// the button: the first request asks for a confirmation naming the tax, and a
/// delete that does not carry the explicit confirmation writes nothing.
#[tokio::test]
async fn settings_taxes_tab_hard_delete_requires_an_explicit_confirmation() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let tax = existing_tax(&state, "IVAWEB", "21").await;

    let confirmation = get(
        &app,
        &format!("/web/settings/taxes/delete-confirm/{}", tax.id),
        &cookie,
    )
    .await;
    assert_eq!(confirmation.status(), StatusCode::OK);
    let confirmation = body(confirmation).await;
    assert!(
        confirmation.contains("IVAWEB"),
        "the confirmation must name the tax it would delete: {confirmation}"
    );
    assert!(
        confirmation.contains("hx-post=\"/web/settings/taxes/delete\""),
        "the confirmation must post the real delete: {confirmation}"
    );
    assert!(
        confirmation.contains("name=\"confirm\""),
        "the confirmation must carry the explicit confirmation field: {confirmation}"
    );
    // Both counts are the decision the operator is being asked to make, and
    // they are shown IN THE SESSION'S LANGUAGE, not as bare numbers. This
    // fixture's business locale is es-AR, so the Spanish sentences are the ones
    // an operator here actually reads.
    assert!(
        confirmation.contains("Productos aún vinculados: 0"),
        "the confirmation must report the product references: {confirmation}"
    );
    assert!(
        confirmation.contains("Líneas de documento registradas: 0"),
        "the confirmation must report the document references: {confirmation}"
    );

    let unconfirmed = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/delete",
        &format!("id={}", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(
        unconfirmed.status(),
        StatusCode::BAD_REQUEST,
        "an unconfirmed delete is a refused request: {unconfirmed:?}"
    );
    let unconfirmed = body(unconfirmed).await;
    assert!(
        unconfirmed.contains("Confirmá la eliminación antes de ejecutarla."),
        "the refusal must say, in the session language, that the delete needs \
         its confirmation — and say nothing about the tax's own state: \
         {unconfirmed}"
    );
    assert!(
        state.tax_service.get_tax(tax.id).await.is_ok(),
        "an unconfirmed delete writes nothing"
    );

    // The same refusal without the HTMX marker is a rendered page carrying the
    // same sentence, so neither caller can be handed a different answer.
    let plain = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/delete",
        &format!("id={}", tax.id),
        &[],
    )
    .await;
    assert_eq!(plain.status(), StatusCode::BAD_REQUEST);
    let plain = body(plain).await;
    assert!(
        plain.contains("Confirmá la eliminación antes de ejecutarla."),
        "{plain}"
    );

    let confirmed = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/delete",
        &format!("id={}&confirm=on", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    assert!(
        state.tax_service.get_tax(tax.id).await.is_err(),
        "a confirmed delete of an unreferenced tax removes it"
    );
}

/// A tax still linked to a product is refused with an actionable, localized
/// message that says what to do — and never with database text.
#[tokio::test]
async fn settings_taxes_tab_delete_refusal_names_the_products_to_unlink() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);
    let tax = existing_tax(&state, "IVA21", "21").await;
    linked_product(&state, "TAX-LINKED", tax.id).await;

    let refused = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/delete",
        &format!("id={}&confirm=on", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let refused = body(refused).await;
    for leak in [
        "FOREIGN KEY",
        "UNIQUE constraint",
        "SQLITE",
        "sqlite",
        "taxes.tax_id",
    ] {
        assert!(
            !refused.contains(leak),
            "the refusal must not leak raw database text ({leak}): {refused}"
        );
    }
    assert!(
        refused.contains("producto") || refused.contains("Producto"),
        "the refusal must name what blocks it, in the session locale: {refused}"
    );
    assert!(
        state.tax_service.get_tax(tax.id).await.is_ok(),
        "a refused delete writes nothing"
    );

    // Unlinking the product is what the message tells the operator to do, and it
    // is what actually makes the delete possible.
    let product_id: i64 = sqlx::query_scalar("SELECT id FROM products WHERE sku = 'TAX-LINKED'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    state
        .tax_service
        .unlink_product_tax(product_id, tax.id)
        .await
        .unwrap();
    let deleted = post_form_to(
        &app,
        &cookie,
        "/web/settings/taxes/delete",
        &format!("id={}&confirm=on", tax.id),
        &htmx(),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert!(state.tax_service.get_tax(tax.id).await.is_err());
}

/// The Taxes tab is additive: the business form still submits, still redirects
/// and still persists, and neither tab's markup leaks into the other.
#[tokio::test]
async fn settings_business_settings_submission_is_unchanged_by_the_taxes_tab() {
    let state = configured_state().await;
    let token = test_support::seed_session_with_permissions(&state.pool, &["settings.manage"])
        .await
        .unwrap();
    let app = router(state.clone());
    let cookie = test_support::cookie_for(&token);

    let submission = post_form(
        &app,
        &cookie,
        "business_name=Roya+Market&default_locale_code=en-US&currency_code=USD&timezone=UTC\
         &locale_code_0=es-AR&locale_code_1=en-US\
         &display_name_0=Espa%C3%B1ol+(Argentina)&display_name_1=English+(United+States)\
         &enabled_1=on",
    )
    .await;
    let status = submission.status();
    let submission = body(submission).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{submission}");

    let (settings, locales) = state.settings_service.load().await.unwrap();
    assert_eq!(settings.business_name, "Roya Market");
    assert_eq!(settings.currency_code, "USD");
    assert_eq!(locales.len(), 2, "both profiles are still round-tripped");

    let business = get(&app, "/settings?saved=true", &cookie).await;
    assert_eq!(business.status(), StatusCode::OK);
    let business = body(business).await;
    assert!(business.contains("Settings saved"), "{business}");
    assert!(business.contains("name=\"business_name\""), "{business}");
    assert!(
        !business.contains("id=\"settings-tax-list\""),
        "the saved notice belongs to the business tab: {business}"
    );

    let taxes = get(&app, "/settings?tab=taxes", &cookie).await;
    let taxes = body(taxes).await;
    assert!(
        !taxes.contains("name=\"business_name\""),
        "the taxes tab must not render the business form: {taxes}"
    );
}
