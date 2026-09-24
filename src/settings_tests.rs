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

async fn body(response: axum::response::Response) -> String {
    String::from_utf8_lossy(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .into_owned()
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
    assert!(page.contains("Configuración guardada"));
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
