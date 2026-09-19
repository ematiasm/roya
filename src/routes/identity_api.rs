// M5 identity (Slice S1b part 3a). The JSON session surface for machine
// clients: `POST /api/sessions` (login with a JSON body, 204 + session
// `Set-Cookie`) and `DELETE /api/sessions` (logout, 204 + clearing cookie).
// Thin handlers over `IdentityService` — no second credential opinion, no
// throttle bypass: every failure is the service's one generic `Unauthorized`
// message, so a wrong password and an unknown username are indistinguishable.
//
// Both routes are on the guard's public allowlist (see `security::guard`):
// login is how the first session is earned, and logout must work without one.
// JSON clients send no `Origin`, which the guard's same-origin check already
// allows — there is deliberately no extra bypass here. A failed login carries
// NO `Set-Cookie` at all, and `DELETE` is idempotent for unknown, expired or
// absent tokens, exactly like the web logout.
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::AppResult;
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Request DTOs (JSON, English names)
// ---------------------------------------------------------------------------

/// `POST /api/sessions`: credentials. Absent fields default to empty strings, so
/// a body that *is* JSON but omits a field reaches the service and fails with
/// the same generic 401 a wrong password gets, instead of leaking how the
/// request was shaped.
///
/// A body that is not JSON at all is a different kind of failure and never
/// reaches the service: axum's `Json` extractor answers `400` (malformed) or
/// `415` (wrong `Content-Type`) before the handler runs. Neither mints a
/// session, and neither says anything about whether the account exists.
#[derive(Debug, Deserialize, Default)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// JSON login: verify the credentials through the one `IdentityService::login`
/// and mint the session cookie on success. On failure the service's generic
/// `Unauthorized` propagates (401, `{"error": "Usuario o contraseña
/// incorrectos"}`) with no cookie, no matter which of the indistinguishable
/// causes it was.
async fn create_session(
    State(state): State<AppState>,
    Json(payload): Json<CreateSessionRequest>,
) -> AppResult<Response> {
    match state
        .identity_service
        .login(payload.username.trim(), &payload.password)
        .await
    {
        Ok(outcome) => {
            // Opportunistic session-store hygiene, same rule the web login
            // applies wherever the app touches the session store.
            if let Err(e) = state.identity_service.prune_sessions().await {
                tracing::warn!(error = %e, "session prune failed");
            }
            let cookie = state.identity_service.policy.serialize_cookie(&outcome.token);
            Ok((
                [(header::SET_COOKIE, cookie.as_str())],
                StatusCode::NO_CONTENT,
            )
                .into_response())
        }
        // Wrong password, unknown username, inactive user or throttled
        // attempt: the service's generic `Unauthorized` (401 + one message)
        // propagates unchanged, and no cookie header is set on any failure.
        Err(e) => Err(e),
    }
}

/// JSON logout: revoke the session the cookie names and clear the cookie with
/// the same flags the web logout clears it with. Idempotent (AC9): an unknown,
/// expired, revoked or absent token is still a `204`.
async fn delete_session(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| state.identity_service.policy.parse_cookie(h));
    if let Some(token) = token {
        // Revokes when the session is live; unknown, expired or already
        // revoked tokens are a no-op success.
        state.identity_service.logout(&token).await?;
    }
    let clear = state.identity_service.policy.clear_cookie();
    Ok(([(header::SET_COOKIE, clear.as_str())], StatusCode::NO_CONTENT).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sessions", post(create_session).delete(delete_session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::Row;
    use tower::ServiceExt;

    use crate::routes::router;
    use crate::security::session::SESSION_COOKIE;
    use crate::security::test_support;

    const ADMIN_PASSWORD: &str = "bootstrap password 1";

    /// The full production router over the part-1 fixture plus an
    /// administrator with a real (light-hashed) credential, mirroring the
    /// `identity_web` construction: `TEST_USERNAME`'s seeded hash is a
    /// placeholder and must never be logged in against.
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

    fn json_headers() -> Vec<(&'static str, &'static str)> {
        vec![("content-type", "application/json")]
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
        set_cookie
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_string()
    }

    // -- POST /api/sessions: the happy path ------------------------------------

    #[tokio::test]
    async fn a_json_login_answers_204_with_a_session_cookie_that_authenticates() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/api/sessions",
            &json_headers(),
            r#"{"username":"admin","password":"bootstrap password 1"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.starts_with(&format!("{SESSION_COOKIE}=")), "{set_cookie}");
        assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
        assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
        assert!(set_cookie.contains("Path=/"), "{set_cookie}");
        // The minted cookie authenticates a subsequent page request — the
        // machine client is now indistinguishable from a browser session.
        let pair = cookie_pair(&resp);
        // 204 means an empty body: nothing but headers.
        let bytes = to_bytes(resp.into_body(), 16).await.unwrap();
        assert!(bytes.is_empty(), "a 204 login must carry no body");

        
        let resp = send(&app, "GET", "/", &[("cookie", pair.as_str())], "").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // -- POST /api/sessions: request-shape rejections --------------------------

    /// The credential contract is one generic 401 for a wrong password, an
    /// unknown user and an inactive user alike. A malformed *request* is a
    /// different failure and is answered before the service sees it: `400` for
    /// a body that is not JSON, `415` for a body that is not declared as JSON.
    /// The assertion that matters is the second one of each pair — a shape
    /// error must never mint a session — and the valid-credentials-sent-as-text
    /// case is what proves the rejection happens before verification rather
    /// than after it.
    #[tokio::test]
    async fn a_malformed_body_is_rejected_before_the_service_and_mints_no_session() {
        let (app, _state) = test_app().await;

        let truncated = send(&app, "POST", "/api/sessions", &json_headers(), r#"{"username":"#).await;
        assert_eq!(truncated.status(), StatusCode::BAD_REQUEST);
        assert!(
            truncated.headers().get(header::SET_COOKIE).is_none(),
            "a malformed body must not mint a session"
        );

        let wrong_content_type = send(
            &app,
            "POST",
            "/api/sessions",
            &[("content-type", "text/plain")],
            r#"{"username":"admin","password":"bootstrap password 1"}"#,
        )
        .await;
        assert_eq!(wrong_content_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            wrong_content_type.headers().get(header::SET_COOKIE).is_none(),
            "valid credentials sent as text/plain must still mint nothing"
        );
    }

    // -- POST /api/sessions: failures never leak, never set a cookie ----------

    #[tokio::test]
    async fn a_wrong_password_and_an_unknown_user_answer_the_same_401_with_no_cookie() {
        let (app, _state) = test_app().await;
        for (label, body) in [
            (
                "wrong password",
                r#"{"username":"admin","password":"not-the-password"}"#,
            ),
            (
                "unknown username",
                r#"{"username":"no-such-admin","password":"whatever"}"#,
            ),
        ] {
            let resp = send(&app, "POST", "/api/sessions", &json_headers(), body).await;
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{label} must answer the generic 401"
            );
            assert!(
                resp.headers().get(header::SET_COOKIE).is_none(),
                "a failed {label} login must not set a cookie"
            );
            let json = body_string(resp).await;
            assert!(
                json.contains("Usuario o contraseña incorrectos"),
                "{label} must carry the same generic message the web login uses: {json}"
            );
        }
    }

    // -- DELETE /api/sessions: revoke, clear, stay idempotent -------------------

    #[tokio::test]
    async fn a_delete_with_a_valid_cookie_revokes_the_session_and_clears_it() {
        let (app, state) = test_app().await;
        let login = send(
            &app,
            "POST",
            "/api/sessions",
            &json_headers(),
            r#"{"username":"admin","password":"bootstrap password 1"}"#,
        )
        .await;
        let pair = cookie_pair(&login);

        let delete = send(&app, "DELETE", "/api/sessions", &[("cookie", pair.as_str())], "").await;
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);
        let cleared = delete
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("HttpOnly"), "{cleared}");
        assert!(cleared.contains("SameSite=Lax"), "{cleared}");
        assert!(cleared.contains("Path=/"), "{cleared}");

        // The session row is actually revoked in the store, not just expired.
        let token = pair.split('=').nth(1).unwrap();
        let row = sqlx::query(
            "SELECT revoked_at FROM sessions WHERE token_hash = ?",
        )
        .bind(crate::security::session::hash_token(token))
        .fetch_one(&state.pool)
        .await
        .unwrap();
        let revoked_at: Option<String> = row.get(0);
        assert!(revoked_at.is_some(), "the session row must carry revoked_at");

        // And the same cookie afterwards is refused: the gate sees a dead
        // session.
        let after = send(&app, "GET", "/", &[("cookie", pair.as_str())], "").await;
        assert_eq!(after.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn a_delete_with_a_dead_token_is_an_idempotent_204() {
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "DELETE",
            "/api/sessions",
            &[("cookie", &format!("{SESSION_COOKIE}=never-a-live-token"))],
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let cleared = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
    }

    #[tokio::test]
    async fn a_delete_without_any_cookie_is_still_a_204() {
        let (app, _state) = test_app().await;
        let resp = send(&app, "DELETE", "/api/sessions", &[], "").await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(resp.headers().get(header::SET_COOKIE).is_some());
    }

    // -- the guard: anonymous reachability, no session enumeration --------------

    #[tokio::test]
    async fn an_anonymous_post_reaches_the_handler_not_the_gate() {
        // With bad credentials the SERVICE answers, not the gate: 401 with the
        // generic Spanish message — the gate's refusal shape would be
        // `{"error":"unauthorized"}` and a redirect for a page, so this body
        // proves the handler ran without a session.
        let (app, _state) = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/api/sessions",
            &json_headers(),
            r#"{"username":"admin","password":"wrong"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_string(resp).await;
        assert!(
            json.contains("Usuario o contraseña incorrectos"),
            "the handler must answer, not the gate: {json}"
        );
        assert!(!json.contains("unauthorized\"}"), "{json}");
    }

    #[tokio::test]
    async fn get_on_the_session_path_is_not_a_registered_public_method() {
        // Observed behaviour, not an assumption: an anonymous GET on
        // /api/sessions is refused by the gate (no session, not on the
        // allowlist) with the JSON unauthorized shape — never 200/204, so no
        // session listing is reachable through it.
        let (app, _state) = test_app().await;
        let resp = send(&app, "GET", "/api/sessions", &[], "").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_string(resp).await;
        assert!(json.contains("unauthorized"), "gate shape required: {json}");
    }
}
