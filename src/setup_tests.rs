use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    response::Response,
};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

use crate::routes::{router, AppState};
use crate::security::password::PasswordHashing;
use crate::security::PasswordHasher;

const PASSWORD: &str = "correct horse battery staple";

async fn fresh_state() -> AppState {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(crate::db::base_connect_options("sqlite::memory:").unwrap())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let state = AppState::new(pool, false, true);
    state.refresh_setup_requirement().await.unwrap();
    state
}

fn valid_form() -> String {
    "business_name=Acme+Store&default_locale_code=es-AR&currency_code=ARS&timezone=America%2FArgentina%2FBuenos_Aires&username=setup.admin&display_name=Setup+Admin&password=correct+horse+battery+staple".into()
}

fn form_with(username: &'static str, password: &'static str) -> String {
    format!(
        "business_name=Acme+Store&default_locale_code=es-AR&currency_code=ARS&timezone=America%2FArgentina%2FBuenos_Aires&username={username}&display_name=Setup+Admin&password={password}"
    )
}

async fn send(app: &axum::Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn get(app: &axum::Router, uri: &str) -> Response {
    send(
        app,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn post_form(app: &axum::Router, uri: &str, body: String) -> Response {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

async fn count(state: &AppState, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn fresh_installation_exposes_setup_blocks_normal_routes_and_keeps_static_assets() {
    let state = fresh_state().await;
    let app = router(state);

    let setup = get(&app, "/setup").await;
    assert_eq!(setup.status(), StatusCode::OK);
    let body = axum::body::to_bytes(setup.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("Configuración inicial"));
    assert!(body.contains("value=\"es-AR\" selected"));

    let dashboard = get(&app, "/").await;
    assert_eq!(dashboard.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(dashboard.headers()[header::LOCATION], "/setup");

    let api = get(&app, "/api/accounts").await;
    assert_eq!(api.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(api.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "application setup required");

    assert_eq!(
        get(&app, "/static/htmx.min.js").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn successful_setup_creates_configuration_hashed_admin_and_protected_role_grant() {
    let state = fresh_state().await;
    let app = router(state.clone());

    let response = post_form(&app, "/setup", valid_form()).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");

    let settings: (i64, String, String, String, String) = sqlx::query_as(
        "SELECT id, business_name, default_locale_code, currency_code, timezone FROM business_settings",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        settings,
        (
            1,
            "Acme Store".into(),
            "es-AR".into(),
            "ARS".into(),
            "America/Argentina/Buenos_Aires".into()
        )
    );

    let locale: (String, String, String, bool) = sqlx::query_as(
        "SELECT locale_code, language_code, display_name, is_enabled FROM business_locales",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        locale,
        (
            "es-AR".into(),
            "es".into(),
            "Español (Argentina)".into(),
            true
        )
    );

    let (password_hash, active, must_change, created_by): (String, bool, bool, Option<i64>) =
        sqlx::query_as(
            "SELECT password_hash, is_active, must_change_password, created_by FROM users WHERE username = 'setup.admin'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(password_hash.starts_with("$argon2id$"));
    assert!(PasswordHasher::light().verify(PASSWORD, &password_hash));
    assert_ne!(password_hash, PASSWORD);
    assert!(active);
    assert!(!must_change);
    assert_eq!(created_by, None);

    assert_eq!(count(&state, "SELECT COUNT(*) FROM user_roles ur JOIN roles r ON r.id = ur.role_id JOIN users u ON u.id = ur.user_id WHERE u.username = 'setup.admin' AND r.code = 'admin'").await, 1);
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM users WHERE username = 'sistema'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn invalid_password_and_username_are_refused_without_writes() {
    for form in [
        form_with("setup.admin", "too-short"),
        form_with("A", PASSWORD),
    ] {
        let state = fresh_state().await;
        let app = router(state.clone());

        let response = post_form(&app, "/setup", form).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            count(&state, "SELECT COUNT(*) FROM business_settings").await,
            0
        );
        assert_eq!(
            count(&state, "SELECT COUNT(*) FROM business_locales").await,
            0
        );
        assert_eq!(
            count(
                &state,
                "SELECT COUNT(*) FROM users WHERE username != 'sistema'"
            )
            .await,
            0
        );
        assert_eq!(count(&state, "SELECT COUNT(*) FROM user_roles").await, 0);
    }
}

#[tokio::test]
async fn repeated_setup_is_refused_without_changing_the_initial_admin() {
    let state = fresh_state().await;
    let app = router(state.clone());
    assert_eq!(
        post_form(&app, "/setup", valid_form()).await.status(),
        StatusCode::SEE_OTHER
    );

    let response = post_form(&app, "/setup", valid_form()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(get(&app, "/setup").await.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        get(&app, "/setup").await.headers()[header::LOCATION],
        "/login"
    );
    assert_eq!(
        count(&state, "SELECT COUNT(*) FROM business_settings").await,
        1
    );
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM users WHERE username = 'setup.admin'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn setup_rolls_back_every_write_when_the_final_grant_fails() {
    let state = fresh_state().await;
    sqlx::query(
        r#"CREATE TRIGGER force_setup_grant_failure
           BEFORE INSERT ON user_roles
           BEGIN
               SELECT RAISE(ABORT, 'forced setup grant failure');
           END"#,
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let result = state
        .setup_service
        .create(&crate::services::setup::SetupInput {
            business_name: "Acme Store".into(),
            default_locale_code: "es-AR".into(),
            currency_code: "ARS".into(),
            timezone: "America/Argentina/Buenos Aires".into(),
            username: "setup.admin".into(),
            display_name: "Setup Admin".into(),
            password: PASSWORD.into(),
        })
        .await;

    assert!(result.is_err());
    assert_eq!(
        count(&state, "SELECT COUNT(*) FROM business_settings").await,
        0
    );
    assert_eq!(
        count(&state, "SELECT COUNT(*) FROM business_locales").await,
        0
    );
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM users WHERE username != 'sistema'"
        )
        .await,
        0
    );
    assert_eq!(count(&state, "SELECT COUNT(*) FROM user_roles").await, 0);
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM users WHERE username = 'sistema'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn administrator_can_use_the_existing_login_and_session_flow_after_setup() {
    let state = fresh_state().await;
    let app = router(state);
    assert_eq!(
        post_form(&app, "/setup", valid_form()).await.status(),
        StatusCode::SEE_OTHER
    );

    let login = post_form(
        &app,
        "/login",
        "username=setup.admin&password=correct+horse+battery+staple".into(),
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();

    let dashboard = send(
        &app,
        Request::builder()
            .uri("/")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(dashboard.status(), StatusCode::OK);
}
