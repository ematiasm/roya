// Deny-by-default authentication gate (Slice S1b part 2; the forced password
// change is slice S3 part 1). One middleware in front of every route: the
// origin check for unsafe methods runs first (even for the public login
// route), then the public allowlist, then the session, then the
// `must_change_password` confinement. There is exactly one validity opinion in
// this codebase and it is `IdentityService::resolve_session` — the middleware
// never decides expiry, revocation or user state itself. Anything not on the
// allowlist without a valid session is refused, so a forgotten annotation
// fails closed. A session whose user still owes the password change is
// confined to the change form: it may not go anywhere else, but it can always
// reach `/password`, the logout endpoints and the public allowlist.
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::models::ResolvedSession;
use crate::repositories::SqlitePermissionRepository;
use crate::routes::AppState;
use crate::security::authz::Principal;

// ---------------------------------------------------------------------------
// Public allowlist
// ---------------------------------------------------------------------------

/// The public allowlist: every entry carries the one-line reason it is public,
/// and anything not listed requires a valid session — deny by default. A new
/// entry needs the same argument written down here.
const PUBLIC_ROUTES: [(Method, &'static str, &'static str); 6] = [
    (
        Method::GET,
        "/login",
        "the login form itself: no session exists yet",
    ),
    (
        Method::POST,
        "/login",
        "submitting credentials is how the first session is earned",
    ),
    (
        Method::GET,
        "/static/*",
        "vendored assets; a test asserts both load unauthenticated",
    ),
    (
        Method::GET,
        "/favicon.ico",
        "reserved for the favicon a browser requests on its own before any session exists; the route is public so that request never needs one (no file is shipped yet, which is deliberate)",
    ),
    (
        Method::POST,
        "/api/sessions",
        "the JSON login (S1b part 3): a machine client earns its first session with credentials through the same IdentityService::login the web form uses; without a session there is nothing to protect yet",
    ),
    (
        Method::DELETE,
        "/api/sessions",
        "the JSON logout (S1b part 3): revoking the session the cookie names must work without one, exactly like the web logout; an unknown or absent token is an idempotent 204",
    ),
];

/// `true` when the request hits the allowlist. `/*` matches the directory
/// itself and everything under it; plain patterns match exactly.
fn is_public(method: &Method, path: &str) -> bool {
    PUBLIC_ROUTES
        .iter()
        .any(|(route_method, pattern, _)| method == route_method && pattern_matches(pattern, path))
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => path == prefix || (path.starts_with(prefix) && path[prefix.len()..].starts_with('/')),
        None => path == pattern,
    }
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// The single enforcement point: refuse every non-public request that carries
/// no valid session, in the shape the caller can read. Registered with
/// `axum::middleware::from_fn_with_state` over the merged router, so the
/// fallback (a routing miss) is behind the gate too.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_owned();
    // The full request target (path plus query) is what a refusal must preserve:
    // a filtered page (`/products?q=a&b=c`) only round-trips if its query is
    // carried along, not just its path.
    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| path.clone());
    let method = req.method().clone();

    // Origin check BEFORE anything else, `/login` included: a forged POST from
    // another site must not even reach the login handler.
    if is_unsafe(&method) && !origin_host_matches(req.headers()) {
        return forbidden_origin();
    }

    if is_public(&method, &path) {
        return next.run(req).await;
    }

    match current_session(&state, req.headers()).await {
        Ok(Some(resolved)) => {
            // S3 part 1: the forced password change. While the flag is set the
            // session is confined to the change form, the logout endpoints and
            // the public allowlist: a change that must happen cannot be walked
            // around by visiting any other route. Checked after authentication
            // (the session is already resolved here) and before the permission
            // read — confinement is not a permission question, it holds for
            // every principal, and an administrator confined by the bootstrap
            // holds the whole catalog anyway.
            if resolved.user.must_change_password && !password_change_allows(&method, &path) {
                return confine(&path, is_htmx(req.headers()));
            }
            // S2: resolve the effective permission set (one query, no cache —
            // a matrix edit applies to the next request) and carry the
            // principal in the extensions, where `Require<P>` reads it. The
            // permission repository is built here, per request, from the pool
            // the state already holds; the identity department owns its
            // tables, the kernel owns the read that answers "may this user do
            // this?".
            let permissions_repo = SqlitePermissionRepository::new(state.pool.clone());
            match state
                .identity_service
                .effective_permissions(&permissions_repo, resolved.user.id)
                .await
            {
                Ok(permissions) => {
                    req.extensions_mut()
                        .insert(Principal::from_user(&resolved.user, permissions));
                    next.run(req).await
                }
                // A failed permission read (database error) is a 500, never a
                // silent pass — the same posture as the session check below.
                Err(e) => {
                    AppError::Internal(format!("permission check failed: {e}")).into_response()
                }
            }
        }
        Ok(None) => refuse(&path, &target, is_htmx(req.headers())),
        // A failed session read (database error) is a 500, never a silent pass.
        Err(e) => AppError::Internal(format!("session check failed: {e}")).into_response(),
    }
}

/// CSRF posture: `POST`/`PUT`/`DELETE` are refused when the browser-declared
/// `Origin` does not match the `Host` header. See `origin_host_matches` for
/// what passes.
fn is_unsafe(method: &Method) -> bool {
    // PATCH is deliberately absent: no `patch(` route exists yet. Add it here in
    // the same change that adds the first one, or that route would silently
    // skip the origin check.
    matches!(method.as_str(), "POST" | "PUT" | "DELETE")
}

/// Same-origin check: an `Origin` header's `host:port` must equal the `Host`
/// header. An absent `Origin` (curl, scripts) passes — a non-browser client
/// can forge any header anyway, and it still needs a valid session to get
/// anywhere. Written-down tradeoff (same as the design): a default-port
/// mismatch (`Origin: http://localhost` vs `Host: localhost:80`) reads as
/// cross-origin; browsers always send the explicit port when it is not the
/// scheme default, so a real browser never trips on this.
fn origin_host_matches(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    // An origin carries no path, but tolerate one rather than mis-comparing.
    let origin_host = origin_host.split('/').next().unwrap_or(origin_host);
    origin_host.eq_ignore_ascii_case(host)
}

/// The session check everyone uses: cookie → `IdentityService::resolve_session`.
/// No second validity opinion lives anywhere in Rust; the SQL decides.
pub async fn current_session(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<Option<ResolvedSession>> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| state.identity_service.policy.parse_cookie(h));
    match token {
        Some(token) => state.identity_service.resolve_session(&token).await,
        None => Ok(None),
    }
}

/// The routes a flagged session may still reach, on the methods the flow
/// needs: the change form itself (read and submit), both logout endpoints
/// (a flagged operator can still choose to leave instead of changing —
/// logout must always work), and nothing else. The public allowlist was
/// already honoured above, so `/static/*` and the favicon stay reachable
/// without being listed here.
fn password_change_allows(method: &Method, path: &str) -> bool {
    match path {
        "/password" => matches!(method.as_str(), "GET" | "POST"),
        "/logout" => method == Method::POST,
        "/api/sessions" => method == Method::DELETE,
        _ => false,
    }
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Refusal shapes
// ---------------------------------------------------------------------------

/// The reason the confinement refusal carries: written in Spanish like the
/// rest of the operator-facing copy, because the JSON reaches the interface
/// (the htmx error handler or an API client's log).
const CONFINEMENT_MESSAGE: &str = "Se requiere cambiar la contraseña antes de continuar";

/// The confinement refusal (AC1/AC16), in the same three shapes the gate
/// uses, pointed at `/password` instead of `/login`: a full-page navigation
/// gets a `303`, `/api/*` gets `403` JSON with the reason (an API client must
/// not be redirected into an HTML form), and an `HX-Request` gets the refusal
/// with `HX-Redirect: /password` — htmx 1.9.12 performs that navigation on
/// any status, and the refusal status says the session was valid but may not
/// go there.
fn confine(path: &str, htmx: bool) -> Response {
    if path.starts_with("/api/") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": CONFINEMENT_MESSAGE })),
        )
            .into_response();
    }
    if htmx {
        return (StatusCode::FORBIDDEN, [("HX-Redirect", "/password")]).into_response();
    }
    Redirect::to("/password").into_response()
}

/// AC2's three shapes, decided by the caller: `/api/*` reads JSON, an HTMX
/// request can only navigate, and a full-page navigation gets a redirect that
/// preserves the intended destination when it is local.
fn refuse(path: &str, target: &str, htmx: bool) -> Response {
    if path.starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
    }
    if htmx {
        // htmx 1.9.12 performs the navigation itself on `HX-Redirect`.
        return (StatusCode::UNAUTHORIZED, [("HX-Redirect", "/login")]).into_response();
    }
    let location = match local_next(target) {
        // The validated target is emitted percent-encoded as one query
        // parameter. Interpolated raw, `/products?q=a&b=c` would be parsed back
        // as `next=/products?q=a` plus a stray `b=c`, and every later `&` would
        // truncate the destination the same way.
        Some(local) => format!("/login?next={}", encode_query_value(local)),
        None => "/login".to_owned(),
    };
    Redirect::to(&location).into_response()
}

/// Percent-encode a string for use as a URL query-parameter value: every byte
/// outside the RFC 3986 unreserved set (`A-Za-z0-9-._~`) becomes `%XX`. The
/// query parser on the receiving end decodes it back to exactly the bytes
/// validated here, so the redirect round-trips losslessly.
fn encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

fn forbidden_origin() -> Response {
    // The same JSON shape `AppError::Forbidden` produces; going through the
    // error type keeps the one 403 constructor the interface has, and the CSRF
    // refusal text is Spanish like every other operator-facing copy.
    AppError::Forbidden("Origen no permitido".to_string()).into_response()
}

/// A `next` value is honoured only when it is a local path: it must start with
/// exactly one `/` (a second one would make it protocol-relative), carry no
/// backslash, and contain **no control character at all**. A scheme is excluded
/// by construction: a URL with a scheme puts `scheme:` before any path, so a
/// value that starts with `/` cannot name one. This is what keeps `/login` from
/// becoming an open redirect. Shared with the login form, which validates the
/// same value.
///
/// The control-character rule is deliberately `char::is_control()` instead of an
/// enumeration of the characters someone remembers: a browser strips TAB, CR and
/// LF from a URL before handing it to the WHATWG parser, so a value such as
/// `/\t/evil.com` would otherwise collapse to the network-path-relative
/// `//evil.com` and navigate off-origin. `is_control()` covers TAB, CR, LF, NUL,
/// DEL and the whole C1 range in one rule, so a future stripped character cannot
/// slip through a hand-written list.
pub fn local_next(next: &str) -> Option<&str> {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\')
        && !next.chars().any(char::is_control)
    {
        Some(next)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::routes::router;
    use crate::security::test_support;

    /// The full production router over the part-1 test fixture: same
    /// construction a client experiences, with the light test hasher.
    async fn test_app() -> axum::Router {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        test_support::seed_session(&pool).await.unwrap();
        router(test_support::app_state(pool))
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> axum::http::Response<axum::body::Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn cookie() -> [(&'static str, &'static str); 1] {
        [("cookie", test_support::TEST_COOKIE)]
    }

    /// The same production router, but the session's user owes the password
    /// change (`must_change_password = 1`, the bootstrap-generated-password
    /// shape). The user's hash is a placeholder: confinement never verifies a
    /// credential, it reads the flag the session resolution already carried.
    async fn flagged_test_app() -> (axum::Router, String) {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let state = test_support::app_state(pool);
        let (token, _session_id) = test_support::seed_flagged_session(&state.pool)
            .await
            .unwrap();
        (router(state), token)
    }

    // -- allowlist shape ------------------------------------------------------

    #[test]
    fn allowlist_matches_its_entries_and_nothing_else() {
        for (method, path) in [
            ("GET", "/login"),
            ("POST", "/login"),
            ("GET", "/static/htmx.min.js"),
            ("GET", "/static/tailwind.css"),
            ("GET", "/favicon.ico"),
            // S1b part 3: the JSON session surface is public exactly on the
            // two verbs that earn or revoke the first session.
            ("POST", "/api/sessions"),
            ("DELETE", "/api/sessions"),
        ] {
            assert!(
                is_public(&method.parse().unwrap(), path),
                "{method} {path} must be public"
            );
        }
        // The prefix pattern covers the directory itself and the tree below it.
        assert!(is_public(&"GET".parse().unwrap(), "/static"));
        // But not a look-alike sibling or another verb.
        assert!(!is_public(&"GET".parse().unwrap(), "/staticx/evil"));
        assert!(!is_public(&"POST".parse().unwrap(), "/static/htmx.min.js"));
        // Deny by default: everything else is refused. `/api/sessions` is
        // public only for POST (login) and DELETE (logout); a read verb on the
        // same path is not on the allowlist and sessions are never enumerable.
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/sales"),
            ("GET", "/api/sessions"),
            ("PUT", "/api/sessions"),
            ("POST", "/logout"),
            ("GET", "/loginx"),
        ] {
            assert!(!is_public(&method.parse().unwrap(), path), "{method} {path}");
        }
    }

    // -- AC2: the three refusal shapes ------------------------------------------

    #[tokio::test]
    async fn ac2_anonymous_full_page_get_redirects_to_login_with_next() {
        let app = test_app().await;
        let resp = send(&app, "GET", "/products", &[]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fproducts"
        );
    }

    #[tokio::test]
    async fn ac2_anonymous_api_request_answers_401_json_unauthorized() {
        let app = test_app().await;
        let resp = send(&app, "GET", "/api/accounts", &[]).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            json!({ "error": "unauthorized" })
        );
    }

    #[tokio::test]
    async fn ac2_anonymous_htmx_request_answers_401_with_hx_redirect() {
        let app = test_app().await;
        let resp = send(&app, "GET", "/products", &[("HX-Request", "true")]).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(resp.headers().get("HX-Redirect").unwrap(), "/login");
        let bytes = axum::body::to_bytes(resp.into_body(), 16).await.unwrap();
        assert!(bytes.is_empty(), "the htmx refusal body must be empty");
    }

    // -- AC24: the part-1 helper introduces no bypass -----------------------------

    #[tokio::test]
    async fn ac24_the_same_app_refuses_the_request_without_the_cookie() {
        // This app was built through the shared test helper (seeded session
        // included): the refusal below proves enforcement is real, not a
        // test-only bypass.
        let app = test_app().await;
        let anonymous = send(&app, "GET", "/", &[]).await;
        assert_eq!(anonymous.status(), StatusCode::SEE_OTHER);

        let authenticated = send(&app, "GET", "/", &cookie()).await;
        assert_eq!(authenticated.status(), StatusCode::OK);
    }

    // -- AC3: deny-by-default over a representative route list ---------------------

    #[tokio::test]
    async fn ac3_protected_routes_refuse_anonymously_and_answer_with_a_session() {
        let app = test_app().await;
        // Dashboard, products, sales and a JSON endpoint: every one is refused
        // anonymously (the handler never runs) and answers its normal status
        // once the shared cookie is present.
        for uri in ["/", "/products", "/sales", "/customers"] {
            let anonymous = send(&app, "GET", uri, &[]).await;
            assert_eq!(
                anonymous.status(),
                StatusCode::SEE_OTHER,
                "GET {uri} anonymously must redirect to /login"
            );
            assert!(anonymous
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("/login"));

            let authenticated = send(&app, "GET", uri, &cookie()).await;
            assert_eq!(
                authenticated.status(),
                StatusCode::OK,
                "GET {uri} with the shared session must reach its handler"
            );
        }

        // The JSON endpoint refuses anonymously in its own shape.
        let anonymous = send(&app, "GET", "/api/accounts", &[]).await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let authenticated = send(&app, "GET", "/api/accounts", &cookie()).await;
        assert_eq!(authenticated.status(), StatusCode::OK);
    }

    // -- AC1/AC16: the must_change_password confinement --------------------------

    fn flagged_cookie(token: &str) -> String {
        format!("roya_session={token}")
    }

    #[tokio::test]
    async fn ac16_a_flagged_full_page_request_redirects_to_the_password_route() {
        let (app, token) = flagged_test_app().await;
        let cookie = flagged_cookie(&token);
        let resp = send(&app, "GET", "/products", &[("cookie", cookie.as_str())]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/password",
            "a flagged session is confined to the change form"
        );
    }

    #[tokio::test]
    async fn ac16_a_flagged_api_request_is_refused_with_403_json() {
        // An API client must not be redirected into an HTML form: it reads
        // the reason in its own shape.
        let (app, token) = flagged_test_app().await;
        let cookie = flagged_cookie(&token);
        let resp = send(&app, "GET", "/api/accounts", &[("cookie", cookie.as_str())]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.headers().get(header::LOCATION).is_none());
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json.get("error").is_some(),
            "the confinement refusal must carry a reason: {json}"
        );
    }

    #[tokio::test]
    async fn ac16_a_flagged_htmx_request_carries_hx_redirect_to_password() {
        let (app, token) = flagged_test_app().await;
        let cookie = flagged_cookie(&token);
        let resp = send(
            &app,
            "GET",
            "/products",
            &[("cookie", cookie.as_str()), ("HX-Request", "true")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(resp.headers().get("HX-Redirect").unwrap(), "/password");
        let bytes = axum::body::to_bytes(resp.into_body(), 16).await.unwrap();
        assert!(bytes.is_empty(), "the htmx confinement body must be empty");
    }

    #[tokio::test]
    async fn ac16_while_flagged_the_change_form_logout_and_static_stay_reachable() {
        let (app, token) = flagged_test_app().await;
        let cookie = flagged_cookie(&token);

        // The change form itself, read and (elsewhere) submit.
        let form = send(&app, "GET", "/password", &[("cookie", cookie.as_str())]).await;
        assert_eq!(form.status(), StatusCode::OK, "GET /password stays reachable");

        // Logout must always work, confinement included.
        let logout = send(&app, "POST", "/logout", &[("cookie", cookie.as_str())]).await;
        assert_eq!(logout.status(), StatusCode::SEE_OTHER, "POST /logout stays reachable");
        assert_eq!(logout.headers().get(header::LOCATION).unwrap(), "/login");

        // The public allowlist is untouched: static assets load.
        let asset = send(&app, "GET", "/static/tailwind.css", &[]).await;
        assert_eq!(asset.status(), StatusCode::OK);

        // The JSON logout endpoint is public by design and stays reachable.
        let json_logout = send(&app, "DELETE", "/api/sessions", &[("cookie", cookie.as_str())]).await;
        assert_eq!(json_logout.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn ac16_an_unflagged_session_goes_everywhere_as_before() {
        // The confinement must not leak onto unflagged sessions: the shared
        // fixture's user owes no change and reaches every route it could before.
        let app = test_app().await;
        for uri in ["/", "/products", "/api/accounts"] {
            let resp = send(&app, "GET", uri, &cookie()).await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "GET {uri} with an unflagged session must not be confined"
            );
        }
    }

    // -- Origin check ---------------------------------------------------------------

    #[tokio::test]
    async fn cross_origin_post_is_refused_with_403() {
        let app = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("host", "localhost:3000"),
                ("origin", "http://evil.example.com"),
            ],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // The refusal happens before authentication is even considered.
        let bytes = axum::body::to_bytes(resp.into_body(), 256).await.unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes)
            .unwrap()
            .get("error")
            .is_some());
    }

    #[tokio::test]
    async fn post_without_origin_header_passes_the_origin_check() {
        // curl/scripts send no Origin: allowed (the handler still runs its own
        // logic; here the login endpoint answers, not 403).
        let app = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn matching_origin_passes_the_origin_check() {
        let app = test_app().await;
        let resp = send(
            &app,
            "POST",
            "/login",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("host", "localhost:3000"),
                ("origin", "http://localhost:3000"),
            ],
        )
        .await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    // -- next validation ---------------------------------------------------------------

    /// One table, one property: no value that clears the origin, or that
    /// smuggles one in through a character a browser strips or reinterprets,
    /// is ever honoured. Asserting the whole hostile set (rather than the three
    /// characters a hand-written check remembered) is what caught the TAB case.
    #[test]
    fn local_next_refuses_every_hostile_value() {
        for hostile in [
            "//evil.com",
            "///evil.com",
            "/\\/evil.com",
            "\\\\evil.com",
            "http://evil.com",
            "https://evil.com/",
            "/x\r\nLocation: http://evil.com",
            "/\t/evil.com",
            "/\ntest",
            "/test\tfoo",
            " /evil.com",
            "",
        ] {
            let result = local_next(hostile);
            eprintln!("hostile unit next {hostile:?} -> {result:?}");
            assert_eq!(
                result,
                None,
                "hostile next must not be honoured: {hostile:?}"
            );
        }
    }

    #[test]
    fn local_next_honours_plain_local_paths() {
        assert_eq!(local_next("/sales"), Some("/sales"));
        assert_eq!(local_next("/products?q=a&b=c"), Some("/products?q=a&b=c"));
    }

    /// The refusal redirect carries the validated target as one encoded query
    /// parameter, so a filtered page keeps its `&`-joined query instead of
    /// losing everything after the first `&` to the login page's query parser.
    #[tokio::test]
    async fn refusal_redirect_encodes_the_next_and_keeps_its_query() {
        let app = test_app().await;
        let resp = send(&app, "GET", "/products?q=a&b=c", &[]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, "/login?next=%2Fproducts%3Fq%3Da%26b%3Dc");
        assert!(
            !location.chars().any(char::is_control),
            "a redirect must never carry a control character: {location:?}"
        );
    }

    #[tokio::test]
    async fn anonymous_get_on_a_subpath_preserves_the_local_next() {
        let app = test_app().await;
        let resp = send(&app, "GET", "/customers", &[]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fcustomers"
        );
    }
}
