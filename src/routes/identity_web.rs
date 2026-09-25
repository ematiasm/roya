// M5 identity (Slice S1b part 2). The login and logout web surface: `GET /
// POST /login` and `POST /logout`, thin handlers over `IdentityService` (the
// same service the deny-by-default guard resolves sessions through — no second
// credential opinion anywhere). The login page is a plain HTML form (no htmx):
// a failed login re-renders it with localized generic presentation copy, and
// a success sets the session cookie and redirects to the
// validated `next` path or `/`. Logout revokes the session behind the cookie,
// always clears it, and is idempotent for dead or absent tokens.
//
// `next` follows the guard's single validation (`security::guard`): a local
// path only, so the login page cannot be turned into an open redirect.
// The `/password` change form (slice S3 part 1) lives here too: a flagged
// session is confined to it by the guard, a successful change clears the
// flag, revokes the user's other sessions keeping the acting one, and sends
// the operator back to the app.
use askama::Template;
use axum::{
    extract::{Extension, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::{LocalizationContext, MessageKey};
use crate::routes::AppState;
use crate::security::authz::Nav;
use crate::security::guard::{current_session, local_next};

// ---------------------------------------------------------------------------
// Askama template
// ---------------------------------------------------------------------------

/// The login card: rendered anonymously (no error), after a failed attempt
/// (localized generic error, `next` preserved) and never when a session already
/// resolves (those requests redirect to `/` before rendering).
#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    localization: LocalizationContext,
    next: String,
    error: Option<String>,
}

/// The password-change card (slice S3 part 1). A flagged session is confined
/// to it; every signed-in operator can also reach it from the sidebar. A
/// failure re-renders the form with the reason in the shared danger-notice
/// idiom; a success sends the operator back to the app.
#[derive(Template)]
#[template(path = "password.html")]
struct PasswordTemplate {
    localization: LocalizationContext,
    error: Option<String>,
    /// The sidebar partial's active-entry key: the page marks its own entry.
    nav_key: &'static str,
    /// The sidebar's nav view (S7 part 2): the entries this principal may
    /// read, its names, and its `must_change_password` flag — when set, the
    /// page says so: the operator confined by an administrator reset learns
    /// why the app is refusing everything else.
    nav: Nav,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn login_page(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<LoginQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // An already-authenticated visitor has no business on the login form.
    match current_session(&state, &headers).await {
        Ok(Some(_)) => return Ok(Redirect::to("/").into_response()),
        Ok(None) => {}
        Err(e) => return Err(AppError::Internal(format!("session check failed: {e}"))),
    }
    let template = LoginTemplate {
        localization,
        next: next_value(query.next.as_deref()),
        error: None,
    };
    let html = render_login(template)?;
    Ok(Html(html).into_response())
}

async fn login_submit(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    match state
        .identity_service
        .login(form.username.trim(), &form.password)
        .await
    {
        Ok(outcome) => {
            // Opportunistic session-store hygiene: prune expired and revoked
            // rows where the app already touches the session store (S1a rule).
            if let Err(e) = state.identity_service.prune_sessions().await {
                tracing::warn!(error = %e, "session prune failed");
            }
            let cookie = state
                .identity_service
                .policy
                .serialize_cookie(&outcome.token);
            let target = form.next.as_deref().and_then(local_next).unwrap_or("/");
            Ok((
                [(header::SET_COOKIE, cookie.as_str())],
                Redirect::to(target),
            )
                .into_response())
        }
        // The generic failure (unknown user, wrong password, inactive user or
        // throttled attempt): re-render the form with localized presentation
        // copy, preserving the service's generic contract and `next`.
        Err(AppError::Unauthorized(_)) => {
            let error = localization
                .tr(MessageKey::LoginInvalidCredentials)
                .to_owned();
            let template = LoginTemplate {
                localization,
                next: next_value(form.next.as_deref()),
                error: Some(error),
            };
            let html = render_login(template)?;
            Ok((StatusCode::UNAUTHORIZED, Html(html)).into_response())
        }
        Err(other) => Err(other),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| state.identity_service.policy.parse_cookie(h));
    if let Some(token) = token {
        // Revokes when the session is live; unknown, expired or already
        // revoked tokens are a no-op success (AC9). Logout must always work.
        if let Err(e) = state.identity_service.logout(&token).await {
            return AppError::Internal(format!("logout failed: {e}")).into_response();
        }
    }
    let clear = state.identity_service.policy.clear_cookie();
    (
        [(header::SET_COOKIE, clear.as_str())],
        Redirect::to("/login"),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The password change (slice S3 part 1)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PasswordForm {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

async fn password_page(
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Response, AppError> {
    let html = render_password(PasswordTemplate {
        localization,
        error: None,
        nav_key: PASSWORD_NAV_KEY,
        nav: Nav::for_principal(&principal),
    })?;
    Ok(Html(html).into_response())
}

/// The sidebar key the password page marks as current; the sidebar partial
/// carries the matching entry.
const PASSWORD_NAV_KEY: &str = "password";

async fn password_submit(
    State(state): State<AppState>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<PasswordForm>,
) -> Result<Response, AppError> {
    // The guard guarantees a session here; the handler still resolves it
    // itself because the rule it enforces needs what the middleware does not
    // pass down: the acting session row (token digest, expiry, user agent)
    // the service re-seats while revoking every other session of the user.
    let resolved = match current_session(&state, &headers).await {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Ok(Redirect::to("/login").into_response()),
        Err(e) => return Err(AppError::Internal(format!("session check failed: {e}"))),
    };
    // Confirmation matching is the form's own job (two fields, one value);
    // the credential rules live in the service, the one layer that owns them.
    if form.new_password != form.confirm_password {
        return password_refusal(
            &principal,
            localization.clone(),
            localization.tr(MessageKey::PasswordMismatch),
            StatusCode::BAD_REQUEST,
        );
    }
    match state
        .identity_service
        .change_password_keep_only_session(
            &resolved.session,
            &form.current_password,
            &form.new_password,
        )
        .await
    {
        // Back to the app: the flag is cleared and the acting session survived.
        Ok(()) => Ok(Redirect::to("/").into_response()),
        // The current password did not verify: same card, precise reason, and
        // nothing was written (the service verifies before its first write).
        Err(AppError::Unauthorized(_)) => password_refusal(
            &principal,
            localization.clone(),
            localization.tr(MessageKey::PasswordCurrentIncorrect),
            StatusCode::UNAUTHORIZED,
        ),
        // The service remains the validation authority and keeps its existing
        // error contract. This HTML adapter translates only the two known
        // password rules and falls back to the original message for anything
        // new rather than guessing at domain meaning.
        Err(AppError::Validation(message)) => password_refusal(
            &principal,
            localization.clone(),
            &password_validation_message(&localization, &message),
            StatusCode::BAD_REQUEST,
        ),
        Err(other) => Err(other),
    }
}

fn password_validation_message(localization: &LocalizationContext, message: &str) -> String {
    let key = match message {
        "La nueva contraseña debe tener al menos 12 caracteres." => {
            MessageKey::PasswordMinimumLength
        }
        "La nueva contraseña debe ser distinta de la actual." => MessageKey::PasswordMustDiffer,
        _ => return message.to_owned(),
    };
    localization.tr(key).to_owned()
}

fn password_refusal(
    principal: &crate::security::authz::Principal,
    localization: LocalizationContext,
    message: &str,
    status: StatusCode,
) -> Result<Response, AppError> {
    let html = render_password(PasswordTemplate {
        localization,
        error: Some(message.to_owned()),
        nav_key: PASSWORD_NAV_KEY,
        nav: Nav::for_principal(principal),
    })?;
    Ok((status, Html(html)).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/password", get(password_page).post(password_submit))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The hidden `next` value for the form: honoured when local, dropped
/// otherwise — the same validation the guard applies to its own redirects.
fn next_value(next: Option<&str>) -> String {
    next.and_then(local_next).unwrap_or("").to_owned()
}

fn render_login(template: LoginTemplate) -> AppResult<String> {
    template
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))
}

fn render_password(template: PasswordTemplate) -> AppResult<String> {
    template
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::routes::router;
    use crate::security::session::SESSION_COOKIE;
    use crate::security::test_support;

    const ADMIN_PASSWORD: &str = "bootstrap password 1";

    /// The full production router over the part-1 fixture plus an
    /// administrator with a real (light-hashed) credential. `TEST_USERNAME`'s
    /// seeded hash is a placeholder and must never be logged in against.
    async fn test_app() -> (axum::Router, AppState) {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        test_support::seed_session(&pool).await.unwrap();
        let state = test_support::app_state(pool);
        state
            .identity_service
            .bootstrap_admin(Some(ADMIN_PASSWORD))
            .await
            .unwrap();
        (router(state.clone()), state)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> axum::http::Response<axum::body::Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    fn form_headers() -> Vec<(&'static str, &'static str)> {
        vec![("content-type", "application/x-www-form-urlencoded")]
    }

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// Extract the `roya_session=...` pair from a `Set-Cookie` response header.
    fn cookie_pair(resp: &axum::http::Response<Body>) -> String {
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set_cookie.split(';').next().unwrap().trim().to_string()
    }

    // -- the login page ---------------------------------------------------------

    /// S1b part 3a: the login page renders without the app shell. An anonymous
    /// visitor on the login card must not see the whole navigation or a logout
    /// button, while a page served behind a session still carries both.
    #[tokio::test]
    async fn the_login_page_renders_no_sidebar_and_a_signed_in_page_keeps_it() {
        let (app, _state) = test_app().await;
        let login = send(&app, "GET", "/login", &[], "").await;
        assert_eq!(login.status(), StatusCode::OK);
        let html = body_string(login).await;
        assert!(
            !html.contains("action=\"/logout\""),
            "the login page must not render the logout form: {html}"
        );
        assert!(
            !html.contains("data-nav="),
            "the login page must not render a navigation item: {html}"
        );
        // The shell the login dropped is still everywhere else.
        let home = send(
            &app,
            "GET",
            "/",
            &[("cookie", test_support::TEST_COOKIE)],
            "",
        )
        .await;
        assert_eq!(home.status(), StatusCode::OK);
        let html = body_string(home).await;
        assert!(
            html.contains("action=\"/logout\""),
            "an authenticated page must still render the logout form"
        );
        assert!(
            html.contains("data-nav="),
            "an authenticated page must still render navigation"
        );
    }

    #[tokio::test]
    async fn login_page_renders_the_form_anonymously() {
        let (app, _state) = test_app().await;
        let resp = send(&app, "GET", "/login", &[], "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("name=\"username\""), "{html}");
        assert!(html.contains("name=\"password\""), "{html}");
        assert!(html.contains("name=\"next\""), "{html}");
    }

    #[tokio::test]
    async fn login_page_redirects_an_authenticated_visitor_home() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "GET",
            "/login",
            &[("cookie", test_support::TEST_COOKIE)],
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/");
    }

    // -- login round trip ---------------------------------------------------------

    #[tokio::test]
    async fn wrong_password_answers_401_with_the_generic_message_and_no_cookie() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=not-the-password",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers().get(header::SET_COOKIE).is_none(),
            "a failed login must not set a cookie"
        );
        let html = body_string(resp).await;
        assert!(
            html.contains("Incorrect username or password"),
            "generic message required: {html}"
        );
    }

    #[tokio::test]
    async fn correct_login_sets_the_cookie_and_redirects_home() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/");
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.starts_with(&format!("{SESSION_COOKIE}=")),
            "{set_cookie}"
        );
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(set_cookie.contains("Path=/"));
        assert!(
            set_cookie.contains("Max-Age=43200"),
            "absolute TTL: {set_cookie}"
        );

        // The minted cookie authenticates a subsequent page request.
        let resp = send(
            &app,
            "GET",
            "/",
            &[("cookie", cookie_pair(&resp).as_str())],
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn next_is_honoured_for_a_local_path_and_rejected_for_an_external_url() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201&next=/sales",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/sales");

        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201&next=%2Fproducts%3Fq%3Da%26b%3Dc",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/products?q=a&b=c",
            "a local next keeps its query string exactly"
        );

        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201&next=http://evil.example.com",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/",
            "an external next must fall back to /"
        );
    }

    /// FIX-1, end to end: every hostile value is submitted through the real
    /// `POST /login` path with valid credentials. The login succeeds, so the
    /// only thing under test is the redirect target: it must be `/`, and no
    /// `Location` may carry a control character for the browser's URL parser to
    /// strip into an off-origin target.
    #[tokio::test]
    async fn hostile_next_values_fall_back_to_root_end_to_end() {
        let (app, _state) = test_app().await;
        // The value on the left of each pair is the hostile `next`; the right
        // side is the form-encoded body that carries it (serde_urlencoded
        // decodes it back to the hostile value the login handler validates).
        for (hostile, encoded) in [
            ("//evil.com", "%2F%2Fevil.com"),
            ("///evil.com", "%2F%2F%2Fevil.com"),
            ("/\\/evil.com", "%2F%5C%2Fevil.com"),
            ("\\\\evil.com", "%5C%5Cevil.com"),
            ("http://evil.com", "http%3A%2F%2Fevil.com"),
            ("https://evil.com/", "https%3A%2F%2Fevil.com%2F"),
            (
                "/x\r\nLocation: http://evil.com",
                "%2Fx%0D%0ALocation%3A%20http%3A%2F%2Fevil.com",
            ),
            ("/\t/evil.com", "%2F%09%2Fevil.com"),
            ("/\ntest", "%2F%0Atest"),
            ("/test\tfoo", "%2Ftest%09foo"),
            (" /evil.com", "%20%2Fevil.com"),
            ("", ""),
        ] {
            let body = format!("username=admin&password=bootstrap%20password%201&next={encoded}");
            let resp = send(&app, "POST", "/login", &form_headers(), &body).await;
            assert_eq!(
                resp.status(),
                StatusCode::SEE_OTHER,
                "a valid login with hostile next {hostile:?} must still redirect"
            );
            let location = resp
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap();
            eprintln!("hostile login next {hostile:?} -> Location {location:?}");
            assert_eq!(
                location, "/",
                "hostile next {hostile:?} must not reach the attacker value"
            );
            assert!(
                !location.chars().any(char::is_control),
                "hostile next {hostile:?} leaked a control character into Location"
            );
        }
    }

    /// FIX-3 round trip: the guard refuses a filtered page and emits the whole
    /// target percent-encoded as one query parameter; the login page decodes it
    /// back to exactly that target; a successful login lands on it unchanged.
    #[tokio::test]
    async fn a_filtered_path_round_trips_through_the_login_redirect() {
        let (app, _state) = test_app().await;

        let refused = send(&app, "GET", "/products?q=a&b=c", &[], "").await;
        assert_eq!(refused.status(), StatusCode::SEE_OTHER);
        let login_location = refused
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            login_location, "/login?next=%2Fproducts%3Fq%3Da%26b%3Dc",
            "the guard must encode the whole filtered target as one parameter"
        );

        // The login page receives the encoded parameter and decodes it back to
        // the exact destination for the hidden form field.
        let page = send(&app, "GET", &login_location, &[], "").await;
        assert_eq!(page.status(), StatusCode::OK);
        let html = body_string(page).await;
        assert!(
            html.contains("value=\"/products?q=a&amp;b=c\""),
            "the hidden next must decode to the exact filtered target: {html}"
        );

        // And the successful login honours that exact value.
        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201&next=%2Fproducts%3Fq%3Da%26b%3Dc",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/products?q=a&b=c"
        );
    }

    // -- logout ----------------------------------------------------------------

    #[tokio::test]
    async fn logout_clears_the_cookie_and_revokes_the_session() {
        let (app, _state) = test_app().await;
        let login = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201",
        )
        .await;
        let pair = cookie_pair(&login);
        assert_eq!(login.status(), StatusCode::SEE_OTHER);

        let logout = send(&app, "POST", "/logout", &[("cookie", pair.as_str())], "").await;
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        assert_eq!(logout.headers().get(header::LOCATION).unwrap(), "/login");
        let cleared = logout
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");

        // The same cookie afterwards is refused: the session row is revoked.
        let after = send(&app, "GET", "/", &[("cookie", pair.as_str())], "").await;
        assert_eq!(after.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn logout_with_a_dead_token_is_an_idempotent_success() {
        // Reaching the handler directly (no guard): the cookie token is dead,
        // revocation is a no-op and the cookie is still cleared (AC9).
        let (_app, state) = test_app().await;
        let handler_app = super::router().with_state(state);
        let resp = send(
            &handler_app,
            "POST",
            "/logout",
            &[("cookie", "roya_session=never-a-live-token")],
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let cleared = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
    }

    #[tokio::test]
    async fn logout_without_any_cookie_is_a_success() {
        let (_app, state) = test_app().await;
        let handler_app = super::router().with_state(state);
        let resp = send(&handler_app, "POST", "/logout", &[], "").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().get(header::SET_COOKIE).is_some());
    }

    // -- must_change_password is enforced from S3 part 1 -------------------------

    /// The S1b placeholder asserted the opposite (the flag did nothing yet);
    /// slice S3 part 1 ships the confinement, so the same fixture now asserts
    /// the real rule: a flagged session is redirected to the change form on
    /// any other full-page request, and the form itself answers.
    #[tokio::test]
    async fn must_change_password_confines_the_session_to_the_password_change() {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let state = test_support::app_state(pool);
        let (token, _user_id) = test_support::seed_flagged_session(&state.pool)
            .await
            .unwrap();
        let app = router(state.clone());

        let cookie = format!("roya_session={token}");
        let resp = send(&app, "GET", "/products", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "a flagged session must be confined to the change form"
        );
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/password");

        let resp = send(&app, "GET", "/password", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(resp.status(), StatusCode::OK, "the form itself must answer");
    }

    // -- the confined flow (AC1 + AC16) -------------------------------------------

    /// The bootstrap with NO env password: the administrator's credential is
    /// generated (returned here, the one place outside the log) and the user
    /// is flagged, so the first login lands confined to `/password`.
    async fn generated_password_app() -> (axum::Router, String) {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let state = test_support::app_state(pool);
        let outcome = state.identity_service.bootstrap_admin(None).await.unwrap();
        let generated = outcome
            .generated_password
            .expect("the generated-password bootstrap returns it once");
        (router(state), generated)
    }

    #[tokio::test]
    async fn the_generated_bootstrap_login_is_confined_until_the_password_changes() {
        let (app, generated) = generated_password_app().await;

        // The generated token is base64url: urlencoded-safe as-is.
        let login_body = format!("username=admin&password={generated}");
        let login = send(&app, "POST", "/login", &form_headers(), &login_body).await;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookie = cookie_pair(&login);

        // Confined: any full-page request lands on the change form.
        let home = send(&app, "GET", "/", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(
            home.status(),
            StatusCode::SEE_OTHER,
            "AC1: the bootstrap without ROYA_ADMIN_PASSWORD ends confined"
        );
        assert_eq!(home.headers().get(header::LOCATION).unwrap(), "/password");

        let form = send(&app, "GET", "/password", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(form.status(), StatusCode::OK);
        let html = body_string(form).await;
        assert!(html.contains("name=\"current_password\""), "{html}");
        assert!(html.contains("name=\"new_password\""), "{html}");
        assert!(html.contains("name=\"confirm_password\""), "{html}");

        // The change: current verified, new valid and confirmed.
        let change_body = "current_password={generated}&new_password=brand%20new%20password%2012&confirm_password=brand%20new%20password%2012";
        let change_body = change_body.replace("{generated}", &generated);
        let change = send(
            &app,
            "POST",
            "/password",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", cookie.as_str()),
            ],
            &change_body,
        )
        .await;
        assert_eq!(
            change.status(),
            StatusCode::SEE_OTHER,
            "a successful change redirects back to the app"
        );
        assert_eq!(change.headers().get(header::LOCATION).unwrap(), "/");

        // The same session continues into the app, unconfined.
        let home = send(&app, "GET", "/", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(
            home.status(),
            StatusCode::OK,
            "AC16: after the change the same session reaches /"
        );
    }

    /// The password page says why it is confining: a flagged session sees the
    /// Spanish confinement notice, a clean session does not (S7 part 2 — the
    /// principal's `must_change_password` field reaches the interface).
    #[tokio::test]
    async fn the_password_page_says_why_it_confines_a_flagged_session() {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        test_support::seed_session(&pool).await.unwrap();
        let state = test_support::app_state(pool);
        let (token, _user_id) = test_support::seed_flagged_session(&state.pool)
            .await
            .unwrap();
        let app = router(state.clone());
        let cookie = format!("roya_session={token}");

        let form = send(&app, "GET", "/password", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(form.status(), StatusCode::OK);
        let html = body_string(form).await;
        assert!(
            html.contains("Your session is restricted"),
            "the flagged session must see why it is confined: {html:.600}"
        );

        // The unflagged shared principal: no confinement notice.
        let plain = send(
            &app,
            "GET",
            "/password",
            &[("cookie", test_support::TEST_COOKIE)],
            "",
        )
        .await;
        assert_eq!(plain.status(), StatusCode::OK);
        let html = body_string(plain).await;
        assert!(
            !html.contains("Your session is restricted"),
            "an unflagged session must not see the confinement notice: {html:.600}"
        );
    }

    #[tokio::test]
    async fn the_password_change_revokes_the_users_other_sessions_and_keeps_the_actor() {
        let (app, generated) = generated_password_app().await;

        // Two sessions for the flagged user, like two open tabs.
        let login_body = format!("username=admin&password={generated}");
        let first = send(&app, "POST", "/login", &form_headers(), &login_body).await;
        let second = send(&app, "POST", "/login", &form_headers(), &login_body).await;
        let acting = cookie_pair(&first);
        let other = cookie_pair(&second);
        assert_ne!(acting, other);

        let change_body = "current_password={generated}&new_password=brand%20new%20password%2012&confirm_password=brand%20new%20password%2012"
            .replace("{generated}", &generated);
        let change = send(
            &app,
            "POST",
            "/password",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", acting.as_str()),
            ],
            &change_body,
        )
        .await;
        assert_eq!(change.status(), StatusCode::SEE_OTHER);

        // The acting session survives; the other one of the same user is dead
        // (an anonymous-looking refusal, exactly like an absent token).
        let actor = send(&app, "GET", "/", &[("cookie", acting.as_str())], "").await;
        assert_eq!(
            actor.status(),
            StatusCode::OK,
            "the acting session must survive"
        );
        let dead = send(&app, "GET", "/", &[("cookie", other.as_str())], "").await;
        assert_eq!(
            dead.status(),
            StatusCode::SEE_OTHER,
            "the other session must be revoked"
        );
        let location = dead
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            location.starts_with("/login"),
            "a dead session is refused like an absent one: {location}"
        );
    }

    #[tokio::test]
    async fn a_wrong_current_password_changes_nothing_and_says_why() {
        let (app, generated) = generated_password_app().await;
        let login_body = format!("username=admin&password={generated}");
        let login = send(&app, "POST", "/login", &form_headers(), &login_body).await;
        let cookie = cookie_pair(&login);

        let change_body = "current_password=not-the-password&new_password=brand%20new%20password%2012&confirm_password=brand%20new%20password%2012";
        let resp = send(
            &app,
            "POST",
            "/password",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", cookie.as_str()),
            ],
            change_body,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let html = body_string(resp).await;
        assert!(
            html.contains("The current password is incorrect"),
            "the form must say why: {html}"
        );

        // Nothing changed: the session is still confined and the generated
        // credential still works (the stored hash was never touched).
        let home = send(&app, "GET", "/", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(home.status(), StatusCode::SEE_OTHER);
        assert_eq!(home.headers().get(header::LOCATION).unwrap(), "/password");

        let retry_body = "current_password={generated}&new_password=brand%20new%20password%2012&confirm_password=brand%20new%20password%2012"
            .replace("{generated}", &generated);
        let retry = send(
            &app,
            "POST",
            "/password",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", cookie.as_str()),
            ],
            &retry_body,
        )
        .await;
        assert_eq!(
            retry.status(),
            StatusCode::SEE_OTHER,
            "the generated credential must still verify after the failed attempt"
        );
    }

    /// The service's credential rules, exercised through the form: a too-short
    /// new password, an unchanged one and a mismatched confirmation are each
    /// refused with their reason, and none of them moves anything (the session
    /// stays confined, so the flag still holds).
    #[tokio::test]
    async fn invalid_new_passwords_are_refused_with_their_reason_and_change_nothing() {
        let (app, generated) = generated_password_app().await;
        let login_body = format!("username=admin&password={generated}");
        let login = send(&app, "POST", "/login", &form_headers(), &login_body).await;
        let cookie = cookie_pair(&login);
        let form_headers_with_cookie = vec![
            ("content-type", "application/x-www-form-urlencoded"),
            ("cookie", cookie.as_str()),
        ];

        // Too short (below the 12-character minimum).
        let resp = send(
            &app,
            "POST",
            "/password",
            &form_headers_with_cookie,
            &format!(
                "current_password={generated}&new_password=short%20pw&confirm_password=short%20pw"
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let html = body_string(resp).await;
        assert!(html.contains("at least 12 characters"), "{html}");

        // Identical to the current one.
        let resp = send(
            &app,
            "POST",
            "/password",
            &form_headers_with_cookie,
            &format!("current_password={generated}&new_password={generated}&confirm_password={generated}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let html = body_string(resp).await;
        assert!(
            html.contains("must differ from the current password"),
            "{html}"
        );

        // Confirmation mismatch (the form's own check).
        let resp = send(
            &app,
            "POST",
            "/password",
            &form_headers_with_cookie,
            &format!("current_password={generated}&new_password=brand%20new%20password%2012&confirm_password=another%20password%209"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let html = body_string(resp).await;
        assert!(html.contains("do not match"), "{html}");

        // Every refusal left the world unchanged: still confined, and the
        // generated credential still verifies.
        let home = send(&app, "GET", "/", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(home.status(), StatusCode::SEE_OTHER);
        assert_eq!(home.headers().get(header::LOCATION).unwrap(), "/password");
        let change_body = "current_password={generated}&new_password=brand%20new%20password%2012&confirm_password=brand%20new%20password%2012"
            .replace("{generated}", &generated);
        let change = send(
            &app,
            "POST",
            "/password",
            &form_headers_with_cookie,
            &change_body,
        )
        .await;
        assert_eq!(change.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn an_env_password_bootstrap_is_not_confined() {
        // The e2e harness rides this branch: ROYA_ADMIN_PASSWORD set means the
        // operator chose the credential and owes no change.
        let (app, _state) = test_app().await;
        let login = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=bootstrap%20password%201",
        )
        .await;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookie = cookie_pair(&login);
        let home = send(&app, "GET", "/", &[("cookie", cookie.as_str())], "").await;
        assert_eq!(
            home.status(),
            StatusCode::OK,
            "a bootstrap with ROYA_ADMIN_PASSWORD set must not be confined"
        );
    }

    #[tokio::test]
    async fn the_sidebar_links_to_the_password_page_and_the_page_marks_itself_active() {
        let (app, _state) = test_app().await;
        let home = send(
            &app,
            "GET",
            "/",
            &[("cookie", test_support::TEST_COOKIE)],
            "",
        )
        .await;
        assert_eq!(home.status(), StatusCode::OK);
        let html = body_string(home).await;
        assert!(
            html.contains("href=\"/password\"") && html.contains("data-nav=\"password\""),
            "the sidebar must carry the password entry: {html}"
        );

        let page = send(
            &app,
            "GET",
            "/password",
            &[("cookie", test_support::TEST_COOKIE)],
            "",
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK);
        let html = body_string(page).await;
        assert!(
            html.contains("data-nav-active=\"true\""),
            "the password page must mark its own entry active: {html}"
        );
    }

    // -- a failed render must never answer an empty 200 ---------------------------

    #[tokio::test]
    async fn login_failure_keeps_the_hidden_next() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &form_headers(),
            "username=admin&password=wrong&next=/purchases",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let html = body_string(resp).await;
        assert!(
            html.contains("name=\"next\" value=\"/purchases\""),
            "next must survive the failure re-render: {html}"
        );
    }
}
