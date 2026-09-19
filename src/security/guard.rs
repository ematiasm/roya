// Deny-by-default authentication gate (Slice S1b part 2). One middleware in
// front of every route: the origin check for unsafe methods runs first (even
// for the public login route), then the public allowlist, then the session.
// There is exactly one validity opinion in this codebase and it is
// `IdentityService::resolve_session` — the middleware never decides expiry,
// revocation or user state itself. Anything not on the allowlist without a
// valid session is refused, so a forgotten annotation fails closed.
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
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Public allowlist
// ---------------------------------------------------------------------------

/// The public allowlist: every entry carries the one-line reason it is public,
/// and anything not listed requires a valid session — deny by default. A new
/// entry needs the same argument written down here. The JSON session endpoints
/// (`/api/sessions`) are deliberately NOT listed: they arrive with the S1b
/// part 3 API slice, and until then refusing them anonymously is the correct
/// default, not an omission.
const PUBLIC_ROUTES: [(Method, &'static str, &'static str); 4] = [
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
    req: Request,
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
        Ok(Some(_)) => next.run(req).await,
        Ok(None) => refuse(&path, &target, is_htmx(req.headers())),
        // A failed session read (database error) is a 500, never a silent pass.
        Err(e) => AppError::Internal(format!("session check failed: {e}")).into_response(),
    }
}

/// CSRF posture: `POST`/`PUT`/`DELETE` are refused when the browser-declared
/// `Origin` does not match the `Host` header. See `origin_host_matches` for
/// what passes.
fn is_unsafe(method: &Method) -> bool {
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

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Refusal shapes
// ---------------------------------------------------------------------------

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
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "Origen no permitido" })),
    )
        .into_response()
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
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::routes::router;
    use crate::security::test_support;

    /// The full production router over the part-1 test fixture: same
    /// construction a client experiences, with the light test hasher.
    async fn test_app() -> axum::Router {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
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

    // -- allowlist shape ------------------------------------------------------

    #[test]
    fn allowlist_matches_its_entries_and_nothing_else() {
        for (method, path) in [
            ("GET", "/login"),
            ("POST", "/login"),
            ("GET", "/static/htmx.min.js"),
            ("GET", "/static/tailwind.css"),
            ("GET", "/favicon.ico"),
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
        // Deny by default: everything else is refused.
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/sales"),
            ("DELETE", "/api/sessions"),
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
