use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use sqlx::{sqlite::SqlitePoolOptions, Row};
use tower::ServiceExt;

use crate::models::{
    NewBusinessLocale, NewBusinessSettings, UpdateBusinessLocale, UpdateBusinessSettings,
};
use crate::repositories::{BusinessLocaleRepository, BusinessSettingsRepository};
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
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(payload.to_owned()))
                .unwrap(),
        )
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
