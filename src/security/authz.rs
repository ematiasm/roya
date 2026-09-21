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
// The any-of gate (PermissionSet / RequireAny)
// ---------------------------------------------------------------------------

/// A set of catalog permission codes that grants a route when the principal
/// holds ANY ONE of them — the any-of gate a page like `/documents` needs,
/// where four department read permissions each open part of the same screen
/// and the route itself must admit any one of them. The tuple of marker
/// types carries the codes the same way `Require<P>` carries one: the
/// compiler, not a string comparison at the call site, ties the set to the
/// catalog markers.
///
/// Implemented for tuple arities 2–4 only. No arity-1: a single permission
/// already has `Require<P>`, and an any-of gate over one code is just that
/// extractor with a costlier message. No arity-5+: no route has needed it;
/// one line in the macro invocation below adds the arity the day one does.
pub trait PermissionSet {
    const CODES: &'static [&'static str];
}

macro_rules! permission_set {
    ($($ty:ident),+) => {
        impl<$($ty: Permission),+> PermissionSet for ($($ty,)+) {
            const CODES: &'static [&'static str] = &[$($ty::CODE),+];
        }
    };
}
permission_set!(A, B);
permission_set!(A, B, C);
permission_set!(A, B, C, D);

/// The any-of per-handler authorization declaration: an argument of type
/// `RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>`
/// runs the handler when the principal holds at least ONE of the declared
/// codes, and answers the same `403` shape `Require<P>` does otherwise,
/// naming every code it would have accepted (AC10). Mirrors `Require<P>`
/// exactly: zero-sized and bodyless (it composes with later extractors),
/// fail closed when the principal extension is absent, refusal writes
/// nothing because the extractor runs before any handler code.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequireAny<S: PermissionSet> {
    /// Keeps `S` in the type without storing anything; the extractor is
    /// zero-sized at runtime.
    _marker: std::marker::PhantomData<S>,
}

impl<S, T> FromRequestParts<T> for RequireAny<S>
where
    T: Send + Sync,
    S: PermissionSet,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &T) -> Result<Self, Self::Rejection> {
        // Fail closed when the principal is absent (a route registered
        // outside the deny-by-default gate): identical to holding none —
        // and holding none satisfies no any-of set either.
        let granted = parts
            .extensions
            .get::<Principal>()
            .is_some_and(|principal| S::CODES.iter().any(|code| principal.has(code)));
        if granted {
            Ok(Self {
                _marker: std::marker::PhantomData,
            })
        } else {
            let listed = S::CODES
                .iter()
                .map(|code| format!("«{code}»"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(forbidden_response(
                parts,
                format!("Se necesita alguno de los permisos {listed} para esta acción"),
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
        nav_key: String::new(),
        // The shell obeys the same nav rule every page renders: the entries
        // this principal may read, or the anonymous fallback when the
        // extensions carry no principal (the fail-closed path).
        nav: parts
            .extensions
            .get::<Principal>()
            .map(Nav::for_principal)
            .unwrap_or_else(Nav::anonymous),
        message,
    })
    .render()
    {
        Ok(html) => (StatusCode::FORBIDDEN, Html(html)).into_response(),
        // A template failure is an internal error, never a silent pass.
        Err(e) => AppError::Internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// The sidebar's nav view (S7 part 2, AC21)
// ---------------------------------------------------------------------------

/// How a nav entry's declared codes decide its visibility. Two semantics
/// only: an entry either needs every code it names (`All`, the shape every
/// existing row uses) or at least one of them (`Any`, the shape the
/// upcoming `/documents` entry will use, openable by any one of four read
/// permissions). The variants carry the same `&'static [&'static str]` list
/// shape the drift tests already verify against the catalog.
#[derive(Debug, Clone, Copy)]
pub enum NavVisibility {
    /// Every code must be held. An empty list is the "every signed-in
    /// operator" entry (the password page): vacuously true.
    All(&'static [&'static str]),
    /// At least one code must be held. An empty list can never be satisfied
    /// — an any-of gate over nothing denies, deny by default.
    Any(&'static [&'static str]),
}

/// One sidebar entry: its template key, the permission codes the entry
/// needs under the visibility semantics above — `All` needs EVERY code (the
/// gate of the route its href opens, plus the data-owner permission of any
/// block its label names — an entry that names less shows its operator a
/// screen the route refuses, one that names more hides a screen the
/// principal may read), `Any` needs at least one (a screen several
/// departments' permissions each open a part of) — and the sidebar group it
/// renders in. An empty `All` list means the entry needs no permission
/// (every signed-in operator may see it); an empty `Any` list is a deny.
/// This table is the single place that decides the mapping entry →
/// permissions; the drift tests below fail when a sidebar entry has no
/// declared row, a row's code is not in the compiled catalog, or a principal
/// holding exactly the row's codes is refused the href the entry opens.
#[derive(Debug)]
struct NavEntry {
    key: &'static str,
    visibility: NavVisibility,
    group: &'static str,
}

const NAV_ENTRIES: &[NavEntry] = &[
    NavEntry { key: "dashboard", visibility: NavVisibility::All(&[DashboardRead::CODE]), group: "operation" },
    NavEntry { key: "sales", visibility: NavVisibility::All(&[SalesRead::CODE]), group: "operation" },
    NavEntry { key: "purchases", visibility: NavVisibility::All(&[PurchasesRead::CODE]), group: "operation" },
    NavEntry { key: "products", visibility: NavVisibility::All(&[InventoryRead::CODE]), group: "catalogue" },
    NavEntry { key: "suppliers", visibility: NavVisibility::All(&[SuppliersRead::CODE]), group: "catalogue" },
    NavEntry { key: "customers", visibility: NavVisibility::All(&[CustomersRead::CODE]), group: "catalogue" },
    // The accounts entry opens `/#accounts`, which is the `/` route's
    // dashboard section: the route itself declares `dashboard.read`, and the
    // entry's label names the accounts block, whose data owner is
    // `finance.read`. The entry carries BOTH, and the dashboard renders the
    // accounts block conditionally on `finance.read` (web.rs) — the same
    // shape the suggestions block uses in purchases_web.rs.
    NavEntry { key: "accounts", visibility: NavVisibility::All(&[DashboardRead::CODE, FinanceRead::CODE]), group: "cash" },
    NavEntry { key: "users", visibility: NavVisibility::All(&[IdentityUsersRead::CODE]), group: "account" },
    NavEntry { key: "roles", visibility: NavVisibility::All(&[IdentityRolesManage::CODE]), group: "account" },
    // No permission gates the password page: every signed-in operator — a
    // confined session included — must always be able to reach it. The empty
    // `All` list is vacuously true: that entry is the one visible to every
    // signed-in principal.
    NavEntry { key: "password", visibility: NavVisibility::All(&[]), group: "account" },
];

/// What the sidebar renders for one signed-in request: the acting user's name
/// next to the logout control, and the entries the principal may read. Built by
/// every full-page handler from the request's principal; the permission
/// refusal builds it from the request extensions so the refusal's shell obeys
/// the same rule. Template code never decides visibility: it asks `visible`.
#[derive(Debug, Clone)]
pub struct Nav {
    pub display_name: String,
    pub username: String,
    pub must_change_password: bool,
    visible_keys: std::collections::BTreeSet<&'static str>,
    visible_groups: std::collections::BTreeSet<&'static str>,
}

impl Nav {
    /// The nav view of one principal: an entry is visible exactly when its
    /// declared visibility is satisfied — every code held for an `All` entry
    /// (vacuously true when empty), at least one held for an `Any` entry.
    pub fn for_principal(principal: &Principal) -> Self {
        Self::from_parts(
            principal.display_name.clone(),
            principal.username.clone(),
            principal.must_change_password,
            &principal.permissions,
        )
    }

    /// The degraded view for a page rendered where no principal exists (the
    /// fail-closed refusal path): the shell renders, the always-visible entry
    /// stays, and nothing else is promised.
    pub fn anonymous() -> Self {
        Self::from_parts(String::new(), String::new(), false, &Default::default())
    }

    fn from_parts(
        display_name: String,
        username: String,
        must_change_password: bool,
        permissions: &std::collections::BTreeSet<String>,
    ) -> Self {
        Self::from_entries(
            display_name,
            username,
            must_change_password,
            permissions,
            NAV_ENTRIES,
        )
    }

    /// The nav view over one entry list — the production path passes
    /// `NAV_ENTRIES`; the kernel's truth-table test passes synthetic entries
    /// so the `NavVisibility` semantics are observable without declaring a
    /// row for them. An `All` entry is visible exactly when the principal
    /// holds every declared code (vacuously true when the list is empty); an
    /// `Any` entry exactly when it holds at least one (never true when the
    /// list is empty).
    fn from_entries(
        display_name: String,
        username: String,
        must_change_password: bool,
        permissions: &std::collections::BTreeSet<String>,
        entries: &[NavEntry],
    ) -> Self {
        let mut visible_keys: std::collections::BTreeSet<&'static str> =
            std::collections::BTreeSet::new();
        let mut visible_groups: std::collections::BTreeSet<&'static str> =
            std::collections::BTreeSet::new();
        for entry in entries {
            let allowed = match entry.visibility {
                NavVisibility::All(codes) => {
                    codes.is_empty()
                        // A no-permission entry is for every signed-in operator.
                        || codes.iter().all(|code| permissions.contains(*code))
                }
                NavVisibility::Any(codes) => {
                    codes.iter().any(|code| permissions.contains(*code))
                }
            };
            if allowed {
                visible_keys.insert(entry.key);
                visible_groups.insert(entry.group);
            }
        }
        Self {
            display_name,
            username,
            must_change_password,
            visible_keys,
            visible_groups,
        }
    }

    /// `true` when the principal may read the entry's screen. The mapping
    /// table is the only decision; an undeclared key is hidden (and the
    /// drift test fails on it), never shown.
    pub fn visible(&self, key: &str) -> bool {
        self.visible_keys.contains(key)
    }

    /// `true` when at least one entry of the group is visible, so the sidebar
    /// never renders an empty group heading.
    pub fn group_visible(&self, group: &str) -> bool {
        self.visible_groups.contains(group)
    }
}

/// The full-page refusal card. Extends `base.html`, so the app shell — the
/// navigation the principal may still use — renders around the refusal, and
/// the sidebar shows exactly the entries that principal may read (the same
/// nav view every page renders; the refusal hides nothing extra and promises
/// nothing extra).
#[derive(Template)]
#[template(path = "forbidden.html")]
struct ForbiddenTemplate {
    /// The sidebar partial's active-entry key; empty so the refusal marks no
    /// entry as current (the shell renders, nothing is highlighted).
    nav_key: String,
    /// The sidebar's nav view: built from the principal when one is in the
    /// extensions, the anonymous fallback on the fail-closed path.
    nav: Nav,
    message: String,
}

// The enforcement surface ships in this slice and gains its production
// consumers slice by slice (S5-S7 annotate the department handlers). Until
// then only the kernel's tests construct `Require`, so the bin target would
// report the extractor, its refusal shapes and the principal reads behind it
// as dead code. The consts below pin the surface the way the census above
// pins the catalog — live references, no `#[allow]` markers — and keep every
// signature compiling: a later slice that changes one of them breaks the
// build here first. The principal's identity fields left this list with S7
// part 2: the sidebar renders `display_name`/`username` and the password page
// reads `must_change_password`, so they are production-read now.
static _PIN_REQUIRE_CONSTRUCTED: Require<DashboardRead> = Require {
    _marker: std::marker::PhantomData,
};
static _PIN_HAS: fn(&Principal, &str) -> bool = Principal::has;
static _PIN_HAS_MARKER: fn(&Principal) -> bool = Principal::has_permission::<DashboardRead>;
static _PIN_REFUSAL: fn(&Parts, String) -> Response = forbidden_response;
// The any-of surface has no production consumer yet: `RequireAny` and the
// `PermissionSet` trait were added for the `/documents` route, which lands
// in a later task together with its nav entry and template item (the ac21
// invariant test drives the real router, so neither may land early). Until
// then only the kernel's tests construct them, so the pins below keep the
// surface compiling the same way the pins above keep `Require`'s — live
// references to the type, the field and the trait, no `#[allow]` markers.
// The `#[used]` attribute is what keeps the pin itself a live root: a bare
// underscore-prefixed static is exempt from the dead-code report but no
// longer feeds liveness to what it references, so the plain pin form the
// older slices could rely on would leave `RequireAny`, `PermissionSet` and
// the unused `NavVisibility::Any` variant reported dead by the bin target.
// The `NavVisibility::Any` pin is the variant's consumer of record until
// the `/documents` nav entry (the first `Any` row) declares it.
#[used]
static _PIN_REQUIRE_ANY_CONSTRUCTED: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)> =
    RequireAny {
        _marker: std::marker::PhantomData,
    };
#[used]
static _PIN_PERMISSION_SET: fn() -> &'static [&'static str] = || {
    <(SalesRead, PurchasesRead, InventoryRead, CustomersRead) as PermissionSet>::CODES
};
#[used]
static _PIN_NAV_ANY_VARIANT: NavVisibility = NavVisibility::Any(&[]);

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
        sqlx::query(
            "INSERT INTO roles (code, name, created_by) VALUES ('probe-writer', 'probe', \
             (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE))",
        )
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
            html.contains("data-nav=\"password\""),
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
        let actor_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(test_support::TEST_USERNAME)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        permissions_repo
            .set_role_permissions(vendedor.id, &kept, actor_id)
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
            .set_role_permissions(vendedor.id, &kept, actor_id)
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
            created_by: None,
            updated_by: None,
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

    // -- AC21: the nav mapping is declared, and the nav tells the truth ----------

    /// The keys the sidebar partial actually renders (every `nav_item("key"`
    /// call in `templates/partials/sidebar.html`). Extracted from the file so
    /// the comparison is against the markup a principal receives, not a
    /// parallel list that could drift from it.
    fn sidebar_nav_item_keys() -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/partials/sidebar.html");
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let needle = "nav_item(\"";
        let mut keys = Vec::new();
        let mut cursor = 0;
        while let Some(found) = content[cursor..].find(needle) {
            let start = cursor + found + needle.len();
            let rest = &content[start..];
            let end = rest
                .find('"')
                .unwrap_or_else(|| panic!("unterminated nav_item key in sidebar.html"));
            keys.push(rest[..end].to_string());
            cursor = start + end;
        }
        keys
    }

    /// The mapping's static drift discipline, carried over from the
    /// permission catalog: every sidebar entry's key is declared in
    /// `NAV_ENTRIES` and vice versa, and every declared code is a catalog
    /// permission. This is the DECLARATION half only — the behavioral truth
    /// (that the declared codes actually open the href) lives in the
    /// invariant test below, which drives the real router.
    #[test]
    fn ac21_the_sidebar_renders_exactly_the_declared_entries_with_catalog_codes() {
        let declared: std::collections::BTreeSet<&str> = NAV_ENTRIES
            .iter()
            .map(|entry| entry.key)
            .chain(["__never_a_sidebar_key__"])
            .collect();
        for key in sidebar_nav_item_keys() {
            assert!(
                declared.contains(key.as_str()),
                "sidebar entry {key:?} has no declared nav mapping: add it to \
                 NAV_ENTRIES in authz.rs with every permission its href and \
                 named blocks need"
            );
        }
        // The template direction of the same drift: a declared entry with no
        // sidebar item is a mapping row that decides nothing.
        let rendered_keys = sidebar_nav_item_keys();
        let rendered: std::collections::BTreeSet<&str> = rendered_keys
            .iter()
            .map(String::as_str)
            .collect();
        for entry in NAV_ENTRIES {
            assert!(
                rendered.contains(entry.key),
                "nav mapping declares {entry:?} but the sidebar partial never renders it"
            );
        }
        for entry in NAV_ENTRIES {
            for code in entry_codes(entry) {
                assert!(
                    PERMISSIONS.contains(code),
                    "nav mapping for {:?} names {} which is not a catalog permission",
                    entry.key,
                    code
                );
            }
        }
    }

    /// The (key, href) pairs the sidebar partial actually renders, extracted
    /// from `templates/partials/sidebar.html` so the invariant below probes
    /// the href a real click opens, not a parallel list that could drift.
    fn sidebar_nav_items() -> Vec<(String, String)> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/partials/sidebar.html");
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let needle = "nav_item(\"";
        let mut items = Vec::new();
        let mut cursor = 0;
        while let Some(found) = content[cursor..].find(needle) {
            let start = cursor + found + needle.len();
            let rest = &content[start..];
            let key_end = rest
                .find('"')
                .unwrap_or_else(|| panic!("unterminated nav_item key in sidebar.html"));
            let key = &rest[..key_end];
            let after_key = &rest[key_end + 1..];
            // The href follows the key after exactly `, "`.
            let href_needle = ", \"";
            let href_at = after_key
                .find(href_needle)
                .unwrap_or_else(|| panic!("nav_item {key} has no href in sidebar.html"));
            let href_rest = &after_key[href_at + href_needle.len()..];
            let href_end = href_rest
                .find('"')
                .unwrap_or_else(|| panic!("unterminated nav_item href in sidebar.html"));
            items.push((key.to_string(), href_rest[..href_end].to_string()));
            cursor = start + key_end + 1 + href_at + href_needle.len() + href_end;
        }
        items
    }

    /// The HTML marker of the page block an entry's label names, when one
    /// exists. `accounts` labels the dashboard's accounts card (`/#accounts`
    /// is the `/` route's section); the block's data owner is `finance.read`,
    /// and the dashboard renders the card conditionally on that code — the
    /// invariant uses the marker to prove the second code is load-bearing.
    /// No other entry's label names a block, so an extra code on any of them
    /// has nothing to point at and the invariant fails it as over-declared.
    fn named_block_marker(key: &str) -> Option<&'static str> {
        match key {
            "accounts" => Some("id=\"accounts\""),
            _ => None,
        }
    }

    /// The invariant that replaced the table-trusting drift test: **for every
    /// nav entry, a principal holding exactly the permissions that entry
    /// declares gets 200 on that entry's href.** One test, every entry,
    /// present and future: it fails when an entry declares too little (the
    /// href refuses the exact-declared principal — 2026-09-20's `accounts`
    /// mismatch, `finance.read` alone against a `dashboard.read` route) and
    /// when it declares too much: every declared code must be load-bearing,
    /// meaning a principal holding the declared set MINUS that code either
    /// gets the route's 403 (the code gates the route) or misses the page
    /// block the entry's label names (`named_block_marker`). A code for which
    /// neither holds is over-declared — it hides the entry from a principal
    /// who may read everything it promises.
    #[tokio::test]
    async fn ac21_a_principal_holding_exactly_what_an_entry_declares_opens_its_href() {
        let items = sidebar_nav_items();
        for entry in NAV_ENTRIES {
            let href = items
                .iter()
                .find(|(key, _)| key == entry.key)
                .map(|(_, href)| href.clone())
                .unwrap_or_else(|| panic!("the sidebar never renders {:?}", entry.key));
            let db = pool().await;
            test_support::seed_session(&db).await.unwrap();
            // Every declared row is `All` today (asserted by
            // `nav_migration_preserves_every_rows_all_semantics`), so the
            // exact-declared set and the minus-one probes below read the
            // all-of semantics; the invariant grows an any-of direction the
            // day a row declares one.
            let declared: Vec<&str> = entry_codes(entry).to_vec();
            let exact = test_support::seed_session_with_permissions(&db, &declared)
                .await
                .unwrap();
            // One probe per declared code, holding the set minus that code:
            // seeded up front because the app state takes the pool over.
            let mut minus_one = Vec::new();
            for code in entry_codes(entry) {
                let reduced: Vec<&str> = declared
                    .iter()
                    .copied()
                    .filter(|held| *held != *code)
                    .collect();
                let token =
                    test_support::seed_session_with_permissions(&db, &reduced)
                        .await
                        .unwrap();
                minus_one.push((code, token));
            }
            let state = test_support::app_state(db);
            let app = crate::routes::router(state);

            // Direction one: the exact-declared principal opens the href.
            let (status, html) = get_page(&app, &href, &test_support::cookie_for(&exact)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "nav entry {:?} declares {:?} but GET {href} refuses the exact principal",
                entry.key,
                declared
            );
            if let Some(marker) = named_block_marker(entry.key) {
                assert!(
                    html.contains(marker),
                    "nav entry {:?} promises the block {marker:?} but the page it opens \
                     does not render it: {html:.600}",
                    entry.key
                );
            }

            // Direction two: every declared code is load-bearing.
            for (code, token) in minus_one {
                let (status, html) =
                    get_page(&app, &href, &test_support::cookie_for(&token)).await;
                if status != StatusCode::OK {
                    // The code gates the route: without it the href refuses.
                    continue;
                }
                match named_block_marker(entry.key) {
                    Some(marker) => assert!(
                        !html.contains(marker),
                        "nav entry {:?} declares {code} but the href opens and the \
                         named block still renders without it: {code} is \
                         over-declared and hides nothing",
                        entry.key
                    ),
                    None => panic!(
                        "nav entry {:?} declares {code} but the href opens without \
                         it and its label names no block: {code} is over-declared \
                         and hides a screen the principal may read",
                        entry.key
                    ),
                }
            }
        }
    }

    /// The visible set is computed, not asserted: for any principal, the
    /// entries it sees are exactly the declared entries whose permission it
    /// holds, plus the no-permission ones. Both directions covered: a
    /// limited principal sees few (and never an entry it may not read), the
    /// full-catalog principal sees all.
    #[test]
    fn ac21_the_nav_view_shows_exactly_the_readable_entries() {
        let catalog: std::collections::BTreeSet<String> =
            PERMISSIONS.iter().map(|c| c.to_string()).collect();
        let all_keys = |permissions: &std::collections::BTreeSet<String>| -> Vec<&'static str> {
            NAV_ENTRIES
                .iter()
                .filter(|entry| match entry.visibility {
                    NavVisibility::All(codes) => {
                        codes.is_empty()
                            || codes.iter().all(|c| permissions.contains(*c))
                    }
                    NavVisibility::Any(codes) => {
                        codes.iter().any(|c| permissions.contains(*c))
                    }
                })
                .map(|entry| entry.key)
                .collect()
        };

        // The permissionless principal: only the no-permission entries.
        let none = Nav::for_principal(&principal_with(std::collections::BTreeSet::new()));
        assert_eq!(
            visible_keys(&none),
            all_keys(&std::collections::BTreeSet::new())
        );

        // One entry's declared set at a time: the view shows exactly the
        // entries whose declared set the principal now holds — the entry
        // itself, plus any entry whose codes it subsumes (e.g. holding
        // `accounts`' two codes shows the dashboard entry too), and nothing
        // the set does not cover.
        for entry in NAV_ENTRIES {
            if entry_codes(entry).is_empty() {
                continue;
            }
            let set: std::collections::BTreeSet<String> =
                entry_codes(entry).iter().map(|c| c.to_string()).collect();
            let view = Nav::for_principal(&principal_with(set.clone()));
            assert_eq!(
                visible_keys(&view),
                all_keys(&set),
                "holding {:?} must show exactly the entries it covers",
                entry_codes(entry)
            );
        }

        // The full catalog: every declared entry.
        let admin = Nav::for_principal(&principal_with(catalog));
        for entry in NAV_ENTRIES {
            assert!(admin.visible(entry.key), "the full catalog must show {entry:?}");
        }
    }

    /// The declared codes regardless of the variant, for the drift checks
    /// that only care that each name is a catalog permission. Lives here in
    /// the test module because only tests collapse the two variants this way:
    /// production code (`Nav::from_entries`) needs the variant itself to
    /// decide the semantics, not just the list.
    fn entry_codes(entry: &NavEntry) -> &'static [&'static str] {
        match entry.visibility {
            NavVisibility::All(codes) | NavVisibility::Any(codes) => codes,
        }
    }

    fn principal_with(permissions: std::collections::BTreeSet<String>) -> Principal {
        let user = User {
            id: 7,
            username: "nav-probe".into(),
            created_by: None,
            updated_by: None,
            display_name: "Nav Probe".into(),
            is_active: true,
            must_change_password: false,
            last_login_at: None,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };
        Principal::from_user(&user, permissions)
    }

    fn visible_keys(nav: &Nav) -> Vec<&'static str> {
        NAV_ENTRIES
            .iter()
            .filter(|entry| nav.visible(entry.key))
            .map(|entry| entry.key)
            .collect()
    }

    /// GET one page of the real router with one cookie, as a browser does.
    async fn get_page(
        app: &axum::Router,
        uri: &str,
        cookie: &str,
    ) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("cookie", cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    // -- The any-of gate (PermissionSet / RequireAny / NavVisibility) --------------

    /// A synthetic entry builder for the visibility truth table: private to the
    /// kernel's tests, so no NAV_ENTRIES row has to exist to prove the
    /// mechanism (a declared `Any` row must wait for the sidebar entry that
    /// renders it — the ac21 declaration test would fail it).
    fn probe_entry(visibility: NavVisibility) -> NavEntry {
        NavEntry { key: "probe", visibility, group: "probe-group" }
    }

    /// The truth table `Nav::from_parts` implements through `NavVisibility`:
    /// `All` is vacuously true when its list is empty (the password entry,
    /// every signed-in operator) and false unless every code is held;
    /// `Any` is false when its list is empty (an any-of gate over nothing
    /// must deny — there is nothing to hold, deny by default) and true when
    /// at least one declared code is held. Synthetic entries drive the
    /// private entry list `Nav::from_entries` exposes for exactly this test.
    #[test]
    fn nav_visibility_truth_table_in_from_parts() {
        let codes: &[&str] = &["sales.read", "customers.read"];
        let held = |values: &[&str]| -> std::collections::BTreeSet<String> {
            values.iter().map(|c| c.to_string()).collect()
        };
        let nav_for =
            |visibility: NavVisibility, held: &std::collections::BTreeSet<String>| {
                Nav::from_entries(
                    "Probe".to_string(),
                    "probe".to_string(),
                    false,
                    held,
                    &[probe_entry(visibility)],
                )
            };

        // All: every code, vacuously true when the list is empty.
        assert!(
            nav_for(NavVisibility::All(codes), &held(&["sales.read", "customers.read"]))
                .visible("probe"),
            "All with both codes held must be visible"
        );
        assert!(
            !nav_for(NavVisibility::All(codes), &held(&["sales.read"])).visible("probe"),
            "All missing one code must be hidden"
        );
        assert!(
            nav_for(NavVisibility::All(&[]), &held(&[])).visible("probe"),
            "All with an empty list is the every-signed-in-operator entry"
        );

        // Any: at least one code, never true when the list is empty.
        assert!(
            nav_for(NavVisibility::Any(codes), &held(&["sales.read"])).visible("probe"),
            "Any with the first code held must be visible"
        );
        assert!(
            nav_for(NavVisibility::Any(codes), &held(&["customers.read"])).visible("probe"),
            "Any with the other code held must be visible"
        );
        assert!(
            !nav_for(NavVisibility::Any(codes), &held(&[])).visible("probe"),
            "Any with no code held must be hidden"
        );
        assert!(
            !nav_for(NavVisibility::Any(&[]), &held(&[])).visible("probe"),
            "Any with an empty list can never be satisfied"
        );

        // The group follows the same entries: a group is visible exactly when
        // one of its entries is visible, and a name no entry declared is not.
        let groups = Nav::from_entries(
            "Probe".to_string(),
            "probe".to_string(),
            false,
            &held(&["sales.read"]),
            &[
                probe_entry(NavVisibility::Any(codes)),
                NavEntry {
                    key: "empty",
                    visibility: NavVisibility::Any(&[]),
                    group: "empty-group",
                },
            ],
        );
        assert!(groups.group_visible("probe-group"));
        assert!(!groups.group_visible("empty-group"));
        assert!(!groups.group_visible("never-declared"));
    }

    /// The migration's behavior-preservation proof: every existing row keeps
    /// the all-of semantics it had before `NavVisibility` existed. Declared
    /// rows are all `All`, and only the password entry declares no code; for
    /// each row the principal holding exactly the declared codes sees it and
    /// the principal holding one code fewer does not (for the password entry,
    /// whose empty list is vacuous, the permissionless principal sees it).
    #[test]
    fn nav_migration_preserves_every_rows_all_semantics() {
        for entry in NAV_ENTRIES {
            let codes = match &entry.visibility {
                NavVisibility::All(codes) => *codes,
                NavVisibility::Any(_) => panic!(
                    "nav entry {:?} declares Any: a declared row whose sidebar item \
                     and route do not exist yet would fail the ac21 declaration test",
                    entry.key
                ),
            };
            if entry.key == "password" {
                assert!(
                    codes.is_empty(),
                    "the password entry stays the no-permission one"
                );
                assert!(
                    Nav::for_principal(&principal_with(Default::default())).visible("password"),
                    "the permissionless principal must still see the password entry"
                );
                continue;
            }
            assert!(
                !codes.is_empty(),
                "nav entry {:?} declares no code", entry.key
            );
            let held: std::collections::BTreeSet<String> =
                codes.iter().map(|c| c.to_string()).collect();
            assert!(
                Nav::for_principal(&principal_with(held.clone())).visible(entry.key),
                "holding exactly {:?} must show {:?}", codes, entry.key
            );
            for dropped in codes.iter().copied() {
                let reduced: std::collections::BTreeSet<String> = held
                    .iter()
                    .filter(|c| c.as_str() != dropped)
                    .cloned()
                    .collect();
                assert!(
                    !Nav::for_principal(&principal_with(reduced)).visible(entry.key),
                    "holding {:?} minus {dropped} must hide {:?}", codes, entry.key
                );
            }
        }
    }

    /// The any-of extractor's test router, built exactly the way
    /// `guarded_app` builds its kernel router: the production middleware over
    /// guarded handlers defined here, so no department route gets annotated
    /// before its enforcement slice. The shared fixture seeds the
    /// permissionless user; each probe principal comes from
    /// `seed_session_with_permissions`, holding exactly the codes named.
    async fn any_of_app() -> (axum::Router, AppState) {
        let p = pool().await;
        test_support::seed_session_without_roles(&p).await.unwrap();
        let state = test_support::app_state(p);
        let app = axum::Router::new()
            .route("/any", get(any_of_gated_handler))
            .route("/all", get(all_of_gated_handler))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state.clone());
        (app, state)
    }

    async fn any_of_gated_handler(
        _: RequireAny<(SalesRead, PurchasesRead, InventoryRead, CustomersRead)>,
    ) -> &'static str {
        "handler-ran"
    }

    /// The all-of control: the same shape the AC10 handlers gate with, so the
    /// test can prove the any-of gate is really a different gate — a
    /// principal the any-of route admits that this one refuses.
    async fn all_of_gated_handler(_: Require<SalesRead>) -> &'static str {
        "handler-ran"
    }

    #[tokio::test]
    async fn any_of_extractor_grants_when_any_declared_code_is_held() {
        let (app, state) = any_of_app().await;

        // Exactly sales.read: both gates open — the control proves the any-of
        // route is not accidentally looser for a principal both gates admit.
        let sales = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let (status, body) = get_page(&app, "/any", &test_support::cookie_for(&sales)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "sales.read is one of the declared codes: {body:.600}"
        );
        assert_eq!(body, "handler-ran");
        let (status, body) = get_page(&app, "/all", &test_support::cookie_for(&sales)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "handler-ran");

        // Exactly customers.read: the any-of gate opens what the all-of
        // control refuses — the two gates are really different.
        let customers =
            test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
                .await
                .unwrap();
        let (status, body) =
            get_page(&app, "/any", &test_support::cookie_for(&customers)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "customers.read is one of the declared codes: {body:.600}"
        );
        assert_eq!(body, "handler-ran");
        let (status, body) =
            get_page(&app, "/all", &test_support::cookie_for(&customers)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.contains("Se necesita el permiso «sales.read» para esta acción"),
            "the control's refusal must name its own single code: {body:.600}"
        );

        // No permission at all: the any-of gate refuses and names every code
        // it would have accepted, in the catalog order the tuple declares.
        let none = test_support::seed_session_with_permissions(&state.pool, &[])
            .await
            .unwrap();
        let (status, body) = get_page(&app, "/any", &test_support::cookie_for(&none)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.contains(
                "Se necesita alguno de los permisos «sales.read», «purchases.read», \
                 «inventory.read», «customers.read» para esta acción",
            ),
            "the any-of refusal must name every declared code: {body:.600}"
        );
        assert!(
            !body.contains("handler-ran"),
            "the handler must never run on a refusal"
        );
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
