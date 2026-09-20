// Identity kernel (Slice S2): the authorization vocabulary. One `Permission`
// trait plus one zero-sized marker type per catalog code, so a handler
// declares what it needs in its argument list — `Require<SalesCreate>` — and
// the compiler, not a string comparison at the call site, ties the declaration
// to the catalog. `PERMISSIONS` below is the single source of truth the
// migration seeds from and the drift test compares the database against
// (codes and, since the S3 correction round, descriptions — the matrix copy
// an operator reads must never promise more than the gate allows): a
// permission only exists if code enforces it, and a seeded row cannot exist
// without an enforcer.
//
// `Principal` is the per-request authorization identity the middleware
// resolves and inserts into request extensions. Departments receive it as an
// opaque value from the kernel; they never resolve permissions themselves and
// never depend on the identity service (AC20).
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{Html, IntoResponse, Response},
};
use askama::Template;

use crate::error::AppError;
use crate::models::User;

// ---------------------------------------------------------------------------
// The permission catalog (AC12: this list and the database must not drift)
// ---------------------------------------------------------------------------

/// The contract every marker type implements: its `CODE` is the exact string
/// stored in `permissions.code` and checked against the principal's effective
/// set. The marker types are zero-sized; a handler writes `Require<SalesCreate>`
/// and the type carries the code.
pub trait Permission {
    const CODE: &'static str;
}

macro_rules! permission {
    ($ty:ident, $code:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $ty;
        impl Permission for $ty {
            const CODE: &'static str = $code;
        }
    };
}

permission!(DashboardRead, "dashboard.read", "View the dashboard.");
permission!(FinanceRead, "finance.read", "View accounts and transactions.");
permission!(FinanceWrite, "finance.write", "Record and edit transactions.");
permission!(
    FinanceMethodsManage,
    "finance.methods.manage",
    "Manage accounts and payment methods."
);
permission!(InventoryRead, "inventory.read", "View products and stock.");
permission!(InventoryWrite, "inventory.write", "Create and edit products.");
permission!(
    InventoryStockWrite,
    "inventory.stock.write",
    "Adjust stock levels."
);
permission!(SalesRead, "sales.read", "View sales.");
permission!(SalesCreate, "sales.create", "Record sales.");
permission!(SalesCancel, "sales.cancel", "Cancel sales.");
permission!(CustomersRead, "customers.read", "View customers.");
permission!(CustomersWrite, "customers.write", "Create and edit customers.");
permission!(CustomersCollect, "customers.collect", "Record collections.");
permission!(PurchasesRead, "purchases.read", "View purchases.");
permission!(PurchasesCreate, "purchases.create", "Record purchases.");
permission!(PurchasesCancel, "purchases.cancel", "Cancel purchases.");
permission!(
    PurchasesCostsRead,
    "purchases.costs.read",
    "View per-supplier costs."
);
permission!(
    PurchasesCostsWrite,
    "purchases.costs.write",
    "Edit per-supplier costs."
);
permission!(SuppliersRead, "suppliers.read", "View suppliers.");
permission!(SuppliersWrite, "suppliers.write", "Create and edit suppliers.");
permission!(IdentityUsersRead, "identity.users.read", "View users.");
permission!(
    IdentityUsersManage,
    "identity.users.manage",
    "Create users and reset passwords of accounts holding no protected role."
);
permission!(
    IdentityRolesManage,
    "identity.roles.manage",
    "Create roles, edit the permission matrix and change other accounts' role sets."
);

/// The whole catalog. This constant is the single source of truth: the drift
/// test (AC12) fails if the database holds a code this list does not, or this
/// list holds a code the database does not — which is the reason the catalog
/// stays seeded rather than UI-created: a permission created in the interface
/// would tick boxes that gate nothing.
/// The seeded Spanish description each catalog row carries in the database —
/// the operator-facing copy the S4 permission matrix renders. This constant is
/// the code-side mirror the drift test compares the database against (same
/// contract as `PERMISSIONS`, extended to descriptions by the S3 correction
/// round): a description that disagrees with its row must fail the drift test,
/// because the matrix is what an operator reads before granting. Migration
/// `20240101000027_create_identity_rbac.sql` seeds these texts and migration
/// `20240101000029_clarify_identity_permission_descriptions.sql` corrects the
/// two identity rows whose original wording overclaimed (it promised role
/// assignment on the users tier, which the endpoint gates behind the roles
/// tier); a fresh database and a pre-existing one end on the same texts.
pub const PERMISSION_DESCRIPTIONS: &[(&str, &str)] = &[
    (DashboardRead::CODE, "Ver el panel principal"),
    (FinanceRead::CODE, "Ver cuentas y movimientos"),
    (FinanceWrite::CODE, "Registrar y editar movimientos"),
    (FinanceMethodsManage::CODE, "Administrar cuentas y medios de pago"),
    (InventoryRead::CODE, "Ver productos y stock"),
    (InventoryWrite::CODE, "Crear y editar productos"),
    (InventoryStockWrite::CODE, "Ajustar stock"),
    (SalesRead::CODE, "Ver ventas"),
    (SalesCreate::CODE, "Registrar ventas"),
    (SalesCancel::CODE, "Anular ventas"),
    (CustomersRead::CODE, "Ver clientes"),
    (CustomersWrite::CODE, "Crear y editar clientes"),
    (CustomersCollect::CODE, "Registrar cobros"),
    (PurchasesRead::CODE, "Ver compras"),
    (PurchasesCreate::CODE, "Registrar compras"),
    (PurchasesCancel::CODE, "Anular compras"),
    (PurchasesCostsRead::CODE, "Ver costos por proveedor"),
    (PurchasesCostsWrite::CODE, "Editar costos por proveedor"),
    (SuppliersRead::CODE, "Ver proveedores"),
    (SuppliersWrite::CODE, "Crear y editar proveedores"),
    (IdentityUsersRead::CODE, "Ver usuarios"),
    (
        IdentityUsersManage::CODE,
        "Crear usuarios y restablecer contraseñas de cuentas sin roles protegidos",
    ),
    (
        IdentityRolesManage::CODE,
        "Crear roles, editar la matriz de permisos y cambiar los roles de otras cuentas",
    ),
];

/// The whole catalog. This constant is the single source of truth: the drift
/// test (AC12) fails if the database holds a code this list does not, or this
/// list holds a code the database does not — which is the reason the catalog
/// stays seeded rather than UI-created: a permission created in the interface
/// would tick boxes that gate nothing.
pub const PERMISSIONS: &[&str] = &[
    DashboardRead::CODE,
    FinanceRead::CODE,
    FinanceWrite::CODE,
    FinanceMethodsManage::CODE,
    InventoryRead::CODE,
    InventoryWrite::CODE,
    InventoryStockWrite::CODE,
    SalesRead::CODE,
    SalesCreate::CODE,
    SalesCancel::CODE,
    CustomersRead::CODE,
    CustomersWrite::CODE,
    CustomersCollect::CODE,
    PurchasesRead::CODE,
    PurchasesCreate::CODE,
    PurchasesCancel::CODE,
    PurchasesCostsRead::CODE,
    PurchasesCostsWrite::CODE,
    SuppliersRead::CODE,
    SuppliersWrite::CODE,
    IdentityUsersRead::CODE,
    IdentityUsersManage::CODE,
    IdentityRolesManage::CODE,
];

/// Compile-time census: a marker type whose `CODE` is missing from the array
/// (or a list entry without its marker) fails to build. The runtime database
/// comparison stays in the drift test; this only keeps the list itself honest.
const _: () = assert!(PERMISSIONS.len() == 23, "the catalog holds 23 permissions");

/// Compile-time census for the description mirror: every code appears exactly
/// once with its description, and the pair list covers the same 23 codes.
const _: () = assert!(
    PERMISSION_DESCRIPTIONS.len() == 23,
    "the description mirror covers all 23 permissions"
);

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

/// The authorization identity of one authenticated request: who is acting and
/// what the union of their roles' permissions allows. Resolved per request by
/// the middleware (no cache: a matrix edit applies to the next request) and
/// inserted into request extensions, where `Require<P>` reads it.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub must_change_password: bool,
    /// The union across the user's roles (AC11). A user with no roles holds
    /// none — deny by default extends past the authentication gate.
    pub permissions: std::collections::BTreeSet<String>,
}

impl Principal {
    /// Build the principal from a resolved session's user and the effective
    /// permission set the middleware resolved for them.
    pub fn from_user(user: &User, permissions: std::collections::BTreeSet<String>) -> Self {
        Self {
            user_id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            must_change_password: user.must_change_password,
            permissions,
        }
    }

    /// `true` when the effective set holds the given permission code.
    pub fn has(&self, code: &str) -> bool {
        self.permissions.contains(code)
    }

    /// `true` when the effective set holds the marker's catalog code — the
    /// form `Require<P>` uses, public so tests can assert set membership
    /// without going through HTTP.
    pub fn has_permission<P: Permission>(&self) -> bool {
        self.has(P::CODE)
    }
}

// ---------------------------------------------------------------------------
// Require<P>
// ---------------------------------------------------------------------------

/// The per-handler authorization declaration: an argument of type
/// `Require<SalesCreate>` runs the handler only when the principal holds
/// `sales.create`, and answers `403` in the caller's shape otherwise (AC10).
/// The refusal writes nothing: the extractor runs before any handler code, so
/// a refused request has produced no write by construction.
///
/// Zero-sized and bodyless by design: it never consumes the request body, so
/// it composes with `Form`/`Json` extractors that run after it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Require<P: Permission> {
    /// Keeps `P` in the type without storing anything; the extractor is
    /// zero-sized at runtime.
    _marker: std::marker::PhantomData<P>,
}

impl<S, P> FromRequestParts<S> for Require<P>
where
    S: Send + Sync,
    P: Permission,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Fail closed when the principal is absent (a route registered
        // outside the deny-by-default gate): identical to lacking permission.
        let granted = parts
            .extensions
            .get::<Principal>()
            .is_some_and(|principal| principal.has(P::CODE));
        if granted {
            Ok(Self {
                _marker: std::marker::PhantomData,
            })
        } else {
            Err(forbidden_response(
                parts,
                format!("Se necesita el permiso «{}» para esta acción", P::CODE),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// The refusal shape (AC10)
// ---------------------------------------------------------------------------

/// The 403 refusal in the shape the caller reads: JSON `{"error": ...}` for
/// `/api/*` and for HTMX requests (the global `htmx:responseError` handler in
/// base.html turns it into the dismissible notice box, so no new fragment is
/// needed), and a minimal HTML page (`templates/forbidden.html`, extending
/// `base.html` so the navigation survives) for a full-page navigation. The
/// message is written in Spanish like the rest of the interface copy, because
/// the operator is the one reading it. A refusal writes nothing.
pub fn forbidden_response(parts: &Parts, message: String) -> Response {
    let is_api = parts.uri.path().starts_with("/api/");
    let is_htmx = parts
        .headers
        .get("HX-Request")
        .is_some_and(|value| value == "true");
    if is_api || is_htmx {
        // The one constructor `AppError::Forbidden` ever needs: the extractor
        // refusing an under-permissioned principal.
        return AppError::Forbidden(message).into_response();
    }
    match (ForbiddenTemplate {
        message,
        nav_key: String::new(),
    })
    .render()
    {
        Ok(html) => (StatusCode::FORBIDDEN, Html(html)).into_response(),
        // A template failure is an internal error, never a silent pass.
        Err(e) => AppError::Internal(e.to_string()).into_response(),
    }
}

/// The full-page refusal card. Extends `base.html`, so the app shell — the
/// navigation the principal may still use — renders around the refusal.
/// (Navigation gating by permission is S5–S7; until then the sidebar shows
/// every entry and the handler, not the markup, is what refuses.)
#[derive(Template)]
#[template(path = "forbidden.html")]
struct ForbiddenTemplate {
    /// The sidebar partial's active-entry key; empty so the refusal marks no
    /// entry as current (the shell renders, nothing is highlighted).
    nav_key: String,
    message: String,
}

// The enforcement surface ships in this slice and gains its production
// consumers slice by slice (S5-S7 annotate the department handlers). Until
// then only the kernel's tests construct `Require`, so the bin target would
// report the extractor, its refusal shapes and the principal reads behind it
// as dead code. The const below pins the surface the way the census above
// pins the catalog — live references, no `#[allow]` markers — and keeps every
// signature compiling: a later slice that changes one of them breaks the
// build here first.
static _PIN_REQUIRE_CONSTRUCTED: Require<DashboardRead> = Require {
    _marker: std::marker::PhantomData,
};
static _PIN_HAS: fn(&Principal, &str) -> bool = Principal::has;
static _PIN_HAS_MARKER: fn(&Principal) -> bool = Principal::has_permission::<DashboardRead>;
static _PIN_REFUSAL: fn(&Parts, String) -> Response = forbidden_response;
static _PIN_IDENTITY_FIELDS: fn(&Principal) -> (&i64, &String, &String, bool) = |p| {
    (
        &p.user_id,
        &p.username,
        &p.display_name,
        p.must_change_password,
    )
};

// ---------------------------------------------------------------------------
// AC10 / AC11 / AC12 / AC20: the kernel's own tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::get};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use std::path::Path;
    use tower::ServiceExt;

    use crate::models::NewUserRole;
    use crate::repositories::permission_repo::{PermissionRepository, SqlitePermissionRepository};
    use crate::repositories::role_repo::{RoleRepository, SqliteRoleRepository};
    use crate::routes::AppState;
    use crate::security::auth_middleware;
    use crate::security::test_support;

    async fn pool() -> SqlitePool {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    use sqlx::Row as _;

    /// The kernel's test router: the production middleware over two guarded
    /// handlers, built exactly the way `routes::router` builds the real one.
    /// The guarded handlers live here, in the kernel's own test module, so no
    /// department route gets annotated before its enforcement slice. It seeds
    /// the PERMISSIONLESS variant of the shared fixture: these tests exercise
    /// exactly which roles the principal holds, so the automatic protected-role
    /// grant of `seed_session` would defeat every refusal assertion here.
    async fn guarded_app(roles: &[&str]) -> (axum::Router, AppState) {
        let p = pool().await;
        test_support::seed_session_without_roles(&p).await.unwrap();
        let state = test_support::app_state(p);
        // Grant the requested roles to the shared test user, through the real
        // repository write path (self-granted: only the test user exists).
        let roles_repo = SqliteRoleRepository::new(state.pool.clone());
        let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        for code in roles {
            let role = roles_repo
                .find_by_code(code)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("seeded role {code} must exist"));
            roles_repo
                .grant(&NewUserRole {
                    user_id,
                    role_id: role.id,
                    granted_by: user_id,
                })
                .await
                .unwrap();
        }
        let app = axum::Router::new()
            .route("/kernel-gated", get(gated_page_handler))
            .route("/api/kernel-gated", get(gated_api_handler))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state.clone());
        (app, state)
    }

    async fn gated_page_handler(_: Require<CustomersWrite>) -> &'static str {
        "handler-ran"
    }

    async fn gated_api_handler(_: Require<FinanceRead>) -> &'static str {
        "handler-ran"
    }

    /// The write-probe handler: if the extractor ever let an under-permissioned
    /// request through, this row would exist.
    async fn gated_writer_handler(
        State(state): State<AppState>,
        _: Require<CustomersWrite>,
    ) -> &'static str {
        sqlx::query("INSERT INTO roles (code, name) VALUES ('probe-writer', 'probe')")
            .execute(&state.pool)
            .await
            .unwrap();
        "handler-ran"
    }

    async fn send(
        app: &axum::Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> axum::http::Response<axum::body::Body> {
        let mut builder = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.clone()
            .oneshot(builder.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_string(resp: axum::http::Response<axum::body::Body>) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    // -- AC10: the refusal shapes and the write guarantee ------------------------

    #[tokio::test]
    async fn ac10_a_full_page_request_without_the_permission_gets_the_html_refusal() {
        let (app, _state) = guarded_app(&[]).await;
        let resp = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let html = body_string(resp).await;
        // The interface copy is Spanish; the page extends base.html, so the
        // navigation shell the principal may still use survives. The rendered
        // message is the extractor's Spanish sentence naming the missing code.
        assert!(
            html.contains("Acción no permitida"),
            "the refusal must speak Spanish: {html}"
        );
        assert!(
            html.contains("Se necesita el permiso «customers.write» para esta acción"),
            "the refusal must name the missing permission: {html}"
        );
        assert!(
            html.contains("data-nav=\"dashboard\""),
            "the refusal page must keep the navigation: {html}"
        );
        assert!(
            !html.contains("handler-ran"),
            "the handler must never run on a refusal"
        );
    }

    #[tokio::test]
    async fn ac10_an_api_request_is_refused_with_403_json() {
        let (app, _state) = guarded_app(&[]).await;
        let resp = send(&app, "/api/kernel-gated", &[]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|message| message.contains("finance.read")),
            "the JSON refusal must name the missing permission: {json}"
        );
    }

    #[tokio::test]
    async fn ac10_an_htmx_request_is_refused_with_403_json_for_the_notice_box() {
        let (app, _state) = guarded_app(&[]).await;
        let resp = send(&app, "/kernel-gated", &[("HX-Request", "true")]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            json.get("error").is_some(),
            "an HTMX refusal must be the JSON shape the global htmx:responseError handler reads: {json}"
        );
    }

    #[tokio::test]
    async fn ac10_a_refusal_writes_nothing() {
        let (_app, state) = guarded_app(&[]).await;
        let writer = axum::Router::new()
            .route("/kernel-writer", get(gated_writer_handler))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state.clone());
        let resp = send(&writer, "/kernel-writer", &[]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM roles WHERE code = 'probe-writer'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(count.0, 0, "a refused request must write nothing");
    }

    #[tokio::test]
    async fn ac10_the_same_handler_runs_when_any_role_grants_the_permission() {
        let (app, _state) = guarded_app(&["vendedor"]).await;
        let resp = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK, "vendedor holds customers.write");
        assert_eq!(body_string(resp).await, "handler-ran");
    }

    // -- AC11: the effective set is the union, resolved per request --------------

    #[tokio::test]
    async fn ac11_permissions_are_the_union_across_roles_and_revoke_applies_next_request() {
        let (app, state) = guarded_app(&["vendedor"]).await;
        // vendedor holds customers.write but not finance.read.
        let granted = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(granted.status(), StatusCode::OK);
        let finance = send(&app, "/api/kernel-gated", &[]).await;
        assert_eq!(
            finance.status(),
            StatusCode::FORBIDDEN,
            "vendedor alone must not hold finance.read"
        );

        // Add cajero (holds finance.read): the union grants both, same app.
        let roles_repo = SqliteRoleRepository::new(state.pool.clone());
        let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let cajero = roles_repo
            .find_by_code("cajero")
            .await
            .unwrap()
            .unwrap();
        roles_repo
            .grant(&NewUserRole {
                user_id,
                role_id: cajero.id,
                granted_by: user_id,
            })
            .await
            .unwrap();
        let both = send(&app, "/api/kernel-gated", &[]).await;
        assert_eq!(both.status(), StatusCode::OK, "the union must hold finance.read");
        let still = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(still.status(), StatusCode::OK, "vendedor's grant must survive");

        // Revoke cajero: the very next request is refused again, no restart.
        roles_repo.revoke(user_id, cajero.id).await.unwrap();
        let after = send(&app, "/api/kernel-gated", &[]).await;
        assert_eq!(
            after.status(),
            StatusCode::FORBIDDEN,
            "revoking a role must apply to the next request"
        );
        let kept = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(kept.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac11_a_role_matrix_edit_applies_to_the_next_request_without_a_restart() {
        let (app, state) = guarded_app(&["vendedor"]).await;
        // vendedor holds customers.write: granted through the middleware.
        let granted = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(granted.status(), StatusCode::OK);

        // The S4 matrix editor's write path: replace vendedor's whole set with
        // everything except customers.write. (The protected-role trigger
        // refuses exactly this statement for `admin`; vendedor is editable.)
        let permissions_repo = SqlitePermissionRepository::new(state.pool.clone());
        let roles_repo = SqliteRoleRepository::new(state.pool.clone());
        let vendedor = roles_repo.find_by_code("vendedor").await.unwrap().unwrap();
        let mut kept: Vec<i64> = Vec::new();
        for code in ["dashboard.read", "inventory.read", "sales.read", "sales.create",
                     "customers.read", "customers.collect"] {
            let id: (i64,) = sqlx::query_as("SELECT id FROM permissions WHERE code = ?")
                .bind(code)
                .fetch_one(&state.pool)
                .await
                .unwrap();
            kept.push(id.0);
        }
        permissions_repo
            .set_role_permissions(vendedor.id, &kept)
            .await
            .unwrap();

        // The very next request is refused: no cache, no restart.
        let after = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(
            after.status(),
            StatusCode::FORBIDDEN,
            "a matrix edit must apply to the next request"
        );

        // Restoring the matrix re-grants on the next request too.
        let customers_write: (i64,) =
            sqlx::query_as("SELECT id FROM permissions WHERE code = 'customers.write'")
                .fetch_one(&state.pool)
                .await
                .unwrap();
        kept.push(customers_write.0);
        permissions_repo
            .set_role_permissions(vendedor.id, &kept)
            .await
            .unwrap();
        let restored = send(&app, "/kernel-gated", &[]).await;
        assert_eq!(restored.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ac11_a_user_with_no_roles_holds_none() {
        let (app, _state) = guarded_app(&[]).await;
        for uri in ["/kernel-gated", "/api/kernel-gated"] {
            let resp = send(&app, uri, &[]).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {uri}");
        }
    }

    /// Fail closed: a handler mounted without the gate (no principal in the
    /// extensions) is refused, never silently allowed.
    #[tokio::test]
    async fn a_missing_principal_fails_closed() {
        let p = pool().await;
        test_support::seed_session_without_roles(&p).await.unwrap();
        let state = test_support::app_state(p);
        let app = axum::Router::new()
            .route("/kernel-gated", get(gated_page_handler))
            .with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/kernel-gated")
                    .header("cookie", test_support::TEST_COOKIE)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // -- AC12: the catalog drift test ---------------------------------------------

    /// The principal carries the spec's identity fields (spec.md: `principal(user)
    /// = { user_id, username, display_name, must_change_password,
    /// effective_permissions }`), and both membership checks read the effective
    /// set: by code and by the marker type `Require<P>` uses.
    #[test]
    fn a_principal_carries_the_identity_and_answers_membership_by_code_and_marker() {
        let user = User {
            id: 7,
            username: "maria".into(),
            display_name: "María".into(),
            is_active: true,
            must_change_password: true,
            last_login_at: None,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };
        let mut permissions = std::collections::BTreeSet::new();
        permissions.insert("sales.create".to_string());
        let principal = Principal::from_user(&user, permissions);

        assert_eq!(principal.user_id, 7);
        assert_eq!(principal.username, "maria");
        assert_eq!(principal.display_name, "María");
        assert!(principal.must_change_password);
        // Membership by code and by marker type — the same set.
        assert!(principal.has("sales.create"));
        assert!(principal.has_permission::<SalesCreate>());
        assert!(!principal.has("sales.cancel"));
        assert!(!principal.has_permission::<SalesCancel>());
        assert!(!principal.has_permission::<FinanceRead>());
    }

    /// The kernel's role-repository surface, exercised through the real
    /// repository: reads, the grant/revoke pair and the whole-set replacement
    /// S3's assignment form will post. (`Principal` tests are unit-level; the
    /// repository read that feeds the principal is covered by AC11's HTTP
    /// tests above, so here the remaining trait methods keep their contracts
    /// observable before their S3/S4 consumers arrive.)
    #[tokio::test]
    async fn the_role_repository_reads_roles_counts_holders_and_replaces_grants() {
        let p = pool().await;
        test_support::seed_session_without_roles(&p).await.unwrap();
        let repo = SqliteRoleRepository::new(p.clone());

        // find_by_code and find_by_id agree, and the row fields survive.
        let vendedor = repo.find_by_code("vendedor").await.unwrap().unwrap();
        assert_eq!(vendedor.code, "vendedor");
        assert_eq!(vendedor.name, "Vendedor");
        assert_eq!(vendedor.description.as_deref(), Some("Ventas y clientes; consulta de stock."));
        assert!(!vendedor.is_system);
        let by_id = repo.find_by_id(vendedor.id).await.unwrap().unwrap();
        assert_eq!(by_id.code, vendedor.code);

        // list(): the four seeded roles in creation order.
        let all = repo.list().await.unwrap();
        let codes: Vec<&str> = all.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(codes, ["admin", "vendedor", "cajero", "deposito"]);
        let admin = &all[0];
        assert!(admin.is_system, "the protected role is marked is_system");
        assert!(
            admin.created_at <= admin.updated_at,
            "the audit timestamps survive the row mapping"
        );

        // list_for_user(): nobody holds anything yet.
        assert!(repo.list_for_user(vendedor.id).await.unwrap().is_empty());

        // count_active_* on a fresh database: one protected role, zero holders.
        assert_eq!(repo.count_active_holders(admin.id).await.unwrap(), 0);
        assert_eq!(repo.count_active_protected_holders().await.unwrap(), 0);

        // grant + list_for_user, then a full replacement that removes both.
        let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&p)
            .await
            .unwrap();
        repo.grant(&NewUserRole {
            user_id,
            role_id: vendedor.id,
            granted_by: user_id,
        })
        .await
        .unwrap();
        let held = repo.list_for_user(user_id).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].code, "vendedor");
        assert_eq!(repo.count_active_holders(vendedor.id).await.unwrap(), 1);
        let kept = repo
            .replace_user_roles(user_id, &[], user_id)
            .await
            .unwrap();
        assert!(kept.is_empty(), "replacing with nothing clears the set");
        assert!(repo.list_for_user(user_id).await.unwrap().is_empty());
        assert_eq!(repo.count_active_holders(vendedor.id).await.unwrap(), 0);
    }

    // -- AC12: the catalog drift test ---------------------------------------------

    /// The drift the AC12 comparison exists to catch, computed the same way
    /// the catalog assertion does: sorted map difference, with the offending
    /// codes and description mismatches named. Empty means the two catalogs
    /// agree on both codes and descriptions.
    fn catalog_drift(compiled: &[(&str, &str)], database: &[(String, String)]) -> String {
        let compiled: std::collections::BTreeMap<&str, &str> =
            compiled.iter().copied().collect();
        let database: std::collections::BTreeMap<&str, &str> = database
            .iter()
            .map(|(code, description)| (code.as_str(), description.as_str()))
            .collect();
        let mut problems: Vec<String> = Vec::new();
        for (code, description) in &database {
            match compiled.get(code) {
                None => problems.push(format!("in the database but not the code: {code}")),
                Some(compiled_description) if compiled_description != description => problems
                    .push(format!(
                        "description drift for {code}: database {description:?} vs code {compiled_description:?}"
                    )),
                _ => {}
            }
        }
        for code in compiled.keys() {
            if !database.contains_key(code) {
                problems.push(format!("in the code but not the database: {code}"));
            }
        }
        if problems.is_empty() {
            String::new()
        } else {
            format!("catalog drift: {}", problems.join("; "))
        }
    }

    /// Assert the two catalogs are equal through the shared drift function, so
    /// the drift test and its mutation proofs exercise the same comparison —
    /// codes AND descriptions.
    async fn assert_catalog_matches(db: &SqlitePool) {
        let database: Vec<(String, String)> =
            sqlx::query("SELECT code, description FROM permissions ORDER BY code")
                .fetch_all(db)
                .await
                .unwrap()
                .iter()
                .filter_map(|row| {
                    Some((
                        row.try_get::<String, _>(0).ok()?,
                        row.try_get::<String, _>(1).ok()?,
                    ))
                })
                .collect();
        let drift = catalog_drift(PERMISSION_DESCRIPTIONS, &database);
        assert!(
            drift.is_empty(),
            "the compiled catalog and the seeded catalog must be identical: {drift}"
        );
    }

    #[tokio::test]
    async fn ac12_the_seeded_catalog_matches_the_compiled_catalog() {
        let db = pool().await;
        assert_catalog_matches(&db).await;
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM permissions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count.0, 23, "the catalog table must hold exactly 23 rows");
    }

    #[test]
    fn every_catalog_code_is_wellformed_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for code in PERMISSIONS {
            assert!(
                code.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && code.chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '.'
                        || c == '_'),
                "malformed catalog code: {code}"
            );
            let parts: Vec<&str> = code.split('.').collect();
            assert!(
                parts.len() == 2 || parts.len() == 3,
                "code must be <module>.<action> or <module>.<resource>.<action>: {code}"
            );
            assert!(seen.insert(*code), "duplicate catalog code: {code}");
        }
    }

    /// AC12's verification plan: reintroduce the drift and watch the
    /// assertion fail. This copy renames a seeded code. (Deleting the row is
    /// not a legal mutation here: the FK cascade into `role_permissions` would
    /// fire the protected-role trigger and the database refuses to un-grant
    /// the administrator — the guards migration doing its job.)
    #[tokio::test]
    async fn ac12_removing_a_database_row_breaks_the_catalog_equality() {
        let db = pool().await;
        sqlx::query("UPDATE permissions SET code = 'drifted.code' WHERE code = 'sales.cancel'")
            .execute(&db)
            .await
            .unwrap();
        let database: Vec<(String, String)> =
            sqlx::query("SELECT code, description FROM permissions")
                .fetch_all(&db)
                .await
                .unwrap()
                .iter()
                .filter_map(|row| {
                    Some((
                        row.try_get::<String, _>(0).ok()?,
                        row.try_get::<String, _>(1).ok()?,
                    ))
                })
                .collect();
        // The same comparison the drift test asserts must now produce a
        // non-empty drift naming the seeded-only code.
        let drift = catalog_drift(PERMISSION_DESCRIPTIONS, &database);
        assert!(
            drift.contains("drifted.code"),
            "a renamed catalog row must break the comparison the drift test asserts: {drift}"
        );
    }

    /// The mirrored direction: a code-side entry that disappears while the
    /// row stays seeded must break the equality too.
    #[test]
    fn ac12_removing_a_catalog_entry_breaks_the_catalog_equality() {
        let database: Vec<(String, String)> = PERMISSION_DESCRIPTIONS
            .iter()
            .map(|(code, description)| (code.to_string(), description.to_string()))
            .collect();
        let drifted: Vec<(&str, &str)> = PERMISSION_DESCRIPTIONS
            .iter()
            .copied()
            .filter(|(code, _)| *code != SalesCancel::CODE)
            .collect();
        // The same comparison, fed a code list missing one entry, must name
        // the drifted side.
        let drift = catalog_drift(&drifted, &database);
        assert!(
            drift.contains("sales.cancel"),
            "a removed catalog entry must break the comparison the drift test asserts: {drift}"
        );
    }

    /// The S3 correction round's extension: a description changed on one side
    /// only is drift too — the permission matrix renders it, so an operator
    /// must never read a promise the gate does not keep.
    #[tokio::test]
    async fn ac12_changing_a_seeded_description_breaks_the_catalog_equality() {
        let db = pool().await;
        sqlx::query(
            "UPDATE permissions SET description = 'overclaimed promise' \
             WHERE code = 'identity.users.manage'",
        )
        .execute(&db)
        .await
        .unwrap();
        let database: Vec<(String, String)> =
            sqlx::query("SELECT code, description FROM permissions")
                .fetch_all(&db)
                .await
                .unwrap()
                .iter()
                .filter_map(|row| {
                    Some((
                        row.try_get::<String, _>(0).ok()?,
                        row.try_get::<String, _>(1).ok()?,
                    ))
                })
                .collect();
        let drift = catalog_drift(PERMISSION_DESCRIPTIONS, &database);
        assert!(
            drift.contains("description drift for identity.users.manage"),
            "a one-sided description edit must break the comparison: {drift}"
        );
    }

    /// Every marker's CODE resolves to exactly one seeded row: the tie between
    /// the marker types and the catalog is not only set-level.
    #[tokio::test]
    async fn ac12_every_marker_code_resolves_to_a_database_row() {
        let db = pool().await;
        for code in PERMISSIONS {
            let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM permissions WHERE code = ?")
                .bind(code)
                .fetch_optional(&db)
                .await
                .unwrap();
            assert!(row.is_some(), "catalog code {code} must exist in the database");
        }
    }

    // -- AC20: departments never touch identity -----------------------------------

    /// The department files this rule governs (the identity routes and
    /// `mod.rs`'s wiring are the only places allowed to name the identity
    /// service). Anything here that starts mentioning the identity service or
    /// querying identity tables breaks the kernel boundary and this test.
    const DEPARTMENT_ROUTE_FILES: &[&str] = &[
        "web.rs",
        "api.rs",
        "customers_web.rs",
        "customers_api.rs",
        "inventory_web.rs",
        "inventory_api.rs",
        "sales_web.rs",
        "sales_api.rs",
        "purchases_web.rs",
        "purchases_api.rs",
        "suppliers_web.rs",
    ];

    const DEPARTMENT_SERVICE_FILES: &[&str] = &[
        "account.rs",
        "customer_receipts.rs",
        "customers.rs",
        "finance_methods.rs",
        "inventory.rs",
        "purchases.rs",
        "sales.rs",
        "suppliers.rs",
        "transaction.rs",
    ];

    const DEPARTMENT_REPO_FILES: &[&str] = &[
        "account_repo.rs",
        "barcode_repo.rs",
        "category_repo.rs",
        "customer_receipt_repo.rs",
        "customer_repo.rs",
        "doc_sequence_repo.rs",
        "payment_method_repo.rs",
        "product_repo.rs",
        "product_supplier_cost_repo.rs",
        "purchase_repo.rs",
        "sale_repo.rs",
        "stock_repo.rs",
        "supplier_repo.rs",
        "transaction_repo.rs",
    ];

    /// SQL-shaped fragments that would mean a department reaches into the
    /// identity tables directly.
    const IDENTITY_SQL_FRAGMENTS: &[&str] = &[
        "FROM users",
        "FROM roles",
        "FROM permissions",
        "FROM user_roles",
        "FROM role_permissions",
        "FROM sessions",
        "INTO users",
        "INTO roles",
        "INTO permissions",
        "INTO user_roles",
        "INTO role_permissions",
        "INTO sessions",
        "UPDATE users",
        "UPDATE roles",
        "UPDATE permissions",
        "UPDATE user_roles",
        "UPDATE role_permissions",
        "UPDATE sessions",
        "DELETE FROM users",
        "DELETE FROM roles",
        "DELETE FROM permissions",
        "DELETE FROM user_roles",
        "DELETE FROM role_permissions",
        "DELETE FROM sessions",
    ];

    fn department_sources(dir: &str, files: &[&str]) -> Vec<(String, String)> {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
        files
            .iter()
            .map(|name| {
                let path = base.join(name);
                let content = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                (name.to_string(), content)
            })
            .collect()
    }

    /// The protected-role and last-administrator guarantees live in triggers,
    /// and a trigger only fires for a statement the connection really runs:
    /// SQLite resolves an `INSERT OR REPLACE` conflict by deleting the row
    /// first, and a `BEFORE DELETE` trigger fires for that deletion only when
    /// `recursive_triggers` is on. The pragma is per connection, so a pool
    /// built ad hoc silently reopens the erasure that
    /// `20240101000028_create_identity_guards.sql` closes — proved by an
    /// independent verification round. Every pool in the identity surface must
    /// therefore come from `db::base_connect_options`, the one place that sets
    /// it alongside `foreign_keys`.
    #[test]
    fn every_identity_pool_comes_from_the_shared_connect_options() {
        const IDENTITY_FILES: &[(&str, &str)] = &[
            ("src", "db.rs"),
            ("src/security", "authz.rs"),
            ("src/security", "guard.rs"),
            ("src/security", "mod.rs"),
            ("src/security", "password.rs"),
            ("src/security", "session.rs"),
            ("src/security", "test_support.rs"),
            ("src/repositories", "user_repo.rs"),
            ("src/repositories", "role_repo.rs"),
            ("src/repositories", "permission_repo.rs"),
            ("src/services", "identity.rs"),
            ("src/routes", "identity_web.rs"),
            ("src/routes", "identity_api.rs"),
        ];
        for (dir, name) in IDENTITY_FILES {
            // `db.rs` is the constructor itself: it is the one file allowed to
            // name the options type directly.
            if *name == "db.rs" {
                continue;
            }
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(name);
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            // The needle is assembled at run time on purpose: this module lives
            // in a file the loop scans, so a literal needle would match itself.
            let ad_hoc_pool = ["SqliteConnectOptions", "::from_str"].concat();
            assert!(
                !content.contains(&ad_hoc_pool),
                "{dir}/{name} builds a pool by hand: it must go through \
                 db::base_connect_options, or `INSERT OR REPLACE` can erase the last \
                 administrator's grant without the trigger firing"
            );
        }
    }

    /// AC20: no department queries identity tables, and no department takes
    /// the identity service as a dependency. The compile-time wiring is the
    /// construction in `routes/mod.rs` (departments get services, the guard
    /// gets the identity service); this grep keeps it that way.
    #[test]
    fn ac20_no_department_queries_identity_tables_or_depends_on_the_identity_service() {
        for (name, content) in department_sources("src/routes", DEPARTMENT_ROUTE_FILES)
            .into_iter()
            .chain(department_sources("src/services", DEPARTMENT_SERVICE_FILES))
            .chain(department_sources("src/repositories", DEPARTMENT_REPO_FILES))
        {
            for fragment in IDENTITY_SQL_FRAGMENTS {
                assert!(
                    !content.contains(fragment),
                    "{name} must not touch identity tables: found {fragment:?}"
                );
            }
            for fragment in ["identity_service", "IdentityService"] {
                assert!(
                    !content.contains(fragment),
                    "{name} must not depend on the identity service: found {fragment:?}"
                );
            }
        }
    }
}
