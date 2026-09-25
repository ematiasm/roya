// M5 identity (slice S3 part 2): the users administration screen. `/users`
// list + `<dialog>` create form + dialog-loaded fragments for role assignment
// and the administrator password reset, exactly on the customers/suppliers
// pattern: Askama + HTMX, thin handlers with no SQL, collection endpoints
// with the typed id in the body (`/web/users`, `/web/users/activate`,
// `/web/users/deactivate`, `/web/users/password`, `/web/users/roles`),
// `#[serde(default)]` on every form field, `HX-Trigger` events and the
// notice idiom (`data-action` feeding base.html's success box; refusals
// arrive as the JSON error the global htmx:responseError handler renders
// into the `#notice` box).
//
// This is the first real consumer of the authorization kernel (S2): `GET`
// surfaces declare `Require<IdentityUsersRead>`, every mutation
// `Require<IdentityUsersManage>`; the full-page refusal page answers a
// principal that cannot read, the JSON shape reaches the htmx notice box.
// A refused request writes nothing: the extractor runs before any handler
// code. The acting principal is read here (as `Extension<Principal>`) to
// record `granted_by` and to hide the actions it may not perform — this
// screen is one of the identity screens this slice ships, so reading the
// principal here does not open the S7 department plumbing.
use askama::Template;
use axum::{
    body::Body,
    extract::{Extension, Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{Role, User, UserWithRoles};
use crate::routes::AppState;
use crate::security::authz::{
    IdentityRolesManage, IdentityUsersManage, IdentityUsersRead, Nav, Principal, Require,
};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One grant with its display names resolved (slice S13): the roles list
/// carries the machine names the assignment form uses; the trail renders who
/// granted and when, in the established display idiom (names, never ids).
/// `granted_on` is the recorded instant formatted for the list (the date
/// part); the template cannot format a timestamp itself.
pub struct GrantRowView {
    pub role_name: String,
    pub granted_by_label: String,
    pub granted_on: String,
    pub trail: String,
}

/// One users-list row with the audit attribution resolved (slice S13): the
/// row's creator/last editor and, for each granted role, who granted it and
/// when. The wiring layer resolves every id to a display name here — the one
/// layer the AC20 boundary scan allows to read identity — so the template
/// renders names, never ids. NULL audit columns mean "the system" (the
/// migration's sentinel, the bootstrap administrator) and render as the
/// honest Spanish label, never as a blank or a raw id.
pub struct UserRowView {
    pub user: User,
    pub roles: Vec<Role>,
    pub grants: Vec<GrantRowView>,
    pub created_by_label: String,
    pub updated_by_label: Option<String>,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersTemplate {
    localization: LocalizationContext,
    users: Vec<UserRowView>,
    /// Whether the principal may act on credentials (`identity.users.manage`):
    /// decides which actions the markup offers. The handlers refuse regardless.
    can_manage: bool,
    /// Whether the principal may change anyone's role set
    /// (`identity.roles.manage`): the assignment tier is a distinct tier, so
    /// the Roles button is its own courtesy.
    can_manage_roles: bool,
    /// The acting principal's own id: the role set of nobody's account is
    /// edited through this screen — including the actor's own — so the
    /// markup hides the button on their row; the handler refuses anyway.
    acting_user_id: i64,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/user_list.html")]
struct UserListPartial {
    localization: LocalizationContext,
    users: Vec<UserRowView>,
    can_manage: bool,
    can_manage_roles: bool,
    acting_user_id: i64,
}

/// The role assignment form for the edit dialog: every role with a checkbox,
/// checked when the user holds it.
#[derive(Template)]
#[template(path = "partials/user_roles_form.html")]
struct UserRolesFormPartial {
    localization: LocalizationContext,
    user: UserWithRoles,
    roles: Vec<Role>,
}

/// The administrator password reset form for the edit dialog.
#[derive(Template)]
#[template(path = "partials/user_password_form.html")]
struct UserPasswordFormPartial {
    localization: LocalizationContext,
    user: UserWithRoles,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn render<T: Template>(template: T) -> AppResult<String> {
    template
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))
}

async fn user_rows(
    state: &AppState,
    localization: &LocalizationContext,
) -> AppResult<Vec<UserRowView>> {
    let users = state.identity_service.list_users_with_roles().await?;
    Ok(resolve_user_rows(state, users, localization).await?)
}

/// Resolve the audit attribution of the users list: every actor id (the row's
/// creator/editor and every grant's granter) to a display name, in ONE wiring
/// query. NULL means the system did it — the sentinel, the bootstrap
/// administrator — and renders as the honest Spanish label ("el sistema").
/// The grant instant is shown as its date part; the interface never renders a
/// raw id.
async fn resolve_user_rows(
    state: &AppState,
    users: Vec<UserWithRoles>,
    localization: &LocalizationContext,
) -> AppResult<Vec<UserRowView>> {
    let mut actor_ids: Vec<i64> = Vec::new();
    for row in &users {
        actor_ids.extend(row.user.created_by);
        actor_ids.extend(row.user.updated_by);
        for grant in &row.grants {
            actor_ids.push(grant.granted_by);
        }
    }
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    // created_by/updated_by: NULL means the system performed it — the honest
    // label, not a blank. granted_by is a live foreign key (NOT NULL), so a
    // miss here is only the concurrent-deactivation window the shared read
    // documents: the established explicit marker, never a raw id.
    let system_label = |id: Option<i64>| -> String {
        id.and_then(|actor| names.get(&actor).cloned())
            .unwrap_or_else(|| {
                localization
                    .tr(crate::localization::MessageKey::DocumentsSystem)
                    .to_string()
            })
    };
    let resolved =
        |id: i64| -> String { names.get(&id).cloned().unwrap_or_else(|| "—".to_string()) };
    let mut rows = Vec::with_capacity(users.len());
    for row in users {
        let grants = row
            .grants
            .iter()
            .map(|grant| {
                let granted_by_label = resolved(grant.granted_by);
                let granted_on = localization.format_date(grant.granted_at.date());
                let role_name = localization
                    .seeded_role_display_name(grant.role.code.as_str(), grant.role.name.as_str())
                    .into_owned();
                let trail = format!(
                    "{}: {} {} {} {}",
                    role_name,
                    localization.tr(crate::localization::MessageKey::IdentityGrantedBy),
                    granted_by_label,
                    localization.tr(crate::localization::MessageKey::IdentityGrantOn),
                    granted_on,
                );
                GrantRowView {
                    role_name,
                    granted_by_label,
                    granted_on,
                    trail,
                }
            })
            .collect();
        rows.push(UserRowView {
            created_by_label: system_label(row.user.created_by),
            updated_by_label: row
                .user
                .updated_by
                .and_then(|actor| names.get(&actor).cloned()),
            user: row.user,
            roles: row.roles,
            grants,
        });
    }
    Ok(rows)
}

fn render_list(
    localization: LocalizationContext,
    users: Vec<UserRowView>,
    can_manage: bool,
    can_manage_roles: bool,
    acting_user_id: i64,
) -> AppResult<Html<String>> {
    Ok(Html(render(UserListPartial {
        localization,
        users,
        can_manage,
        can_manage_roles,
        acting_user_id,
    })?))
}

async fn list_response(
    state: &AppState,
    can_manage: bool,
    can_manage_roles: bool,
    acting_user_id: i64,
) -> AppResult<Response> {
    let localization = crate::localization::load_context(&state.pool).await?;
    let users = user_rows(state, &localization).await?;
    Ok(render_list(
        localization,
        users,
        can_manage,
        can_manage_roles,
        acting_user_id,
    )?
    .into_response())
}

// ---------------------------------------------------------------------------
// Pages + fragments
// ---------------------------------------------------------------------------

async fn users_page(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
    Extension(principal): Extension<Principal>,
    _: Require<IdentityUsersRead>,
) -> Result<Html<String>, AppError> {
    let users = user_rows(&state, &localization).await?;
    let tmpl = UsersTemplate {
        localization,
        users,
        can_manage: principal.has_permission::<IdentityUsersManage>(),
        can_manage_roles: principal.has_permission::<IdentityRolesManage>(),
        acting_user_id: principal.user_id,
        nav_key: "users",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(render(tmpl)?))
}

async fn web_user_list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    _: Require<IdentityUsersRead>,
) -> AppResult<Response> {
    list_response(
        &state,
        principal.has_permission::<IdentityUsersManage>(),
        principal.has_permission::<IdentityRolesManage>(),
        principal.user_id,
    )
    .await
}

// ---------------------------------------------------------------------------
// Forms (HTMX, collection endpoints with the typed id in the body)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct CreateUserForm {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct UserIdForm {
    #[serde(default)]
    pub user_id: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct ResetPasswordForm {
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub new_password: String,
}

async fn web_assign_roles_placeholder() {}

async fn web_create_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityUsersManage>,
    Form(form): Form<CreateUserForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .create_user(
            principal.user_id,
            &form.username,
            &form.display_name,
            &form.password,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(
            &state,
            true,
            principal.has_permission::<IdentityRolesManage>(),
            principal.user_id,
        )
        .await?;
        resp.headers_mut()
            .insert("HX-Trigger", "user-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/users").into_response())
}

async fn web_activate_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityUsersManage>,
    Form(form): Form<UserIdForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .set_user_active(principal.user_id, form.user_id, true)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(
            &state,
            true,
            principal.has_permission::<IdentityRolesManage>(),
            principal.user_id,
        )
        .await?;
        resp.headers_mut()
            .insert("HX-Trigger", "user-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/users").into_response())
}

async fn web_deactivate_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityUsersManage>,
    Form(form): Form<UserIdForm>,
) -> AppResult<Response> {
    state
        .identity_service
        .set_user_active(principal.user_id, form.user_id, false)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(
            &state,
            true,
            principal.has_permission::<IdentityRolesManage>(),
            principal.user_id,
        )
        .await?;
        resp.headers_mut()
            .insert("HX-Trigger", "user-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/users").into_response())
}

async fn web_reset_password(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityUsersManage>,
    Form(form): Form<ResetPasswordForm>,
) -> AppResult<Response> {
    // The tier rule lives in the service (the target's roles decide which
    // permission tier may take over this account); the repository rides in
    // as the argument the service reads the actor's codes from.
    let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
    state
        .identity_service
        .admin_reset_password(
            &permissions,
            principal.user_id,
            form.user_id,
            &form.new_password,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(
            &state,
            true,
            principal.has_permission::<IdentityRolesManage>(),
            principal.user_id,
        )
        .await?;
        resp.headers_mut()
            .insert("HX-Trigger", "user-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/users").into_response())
}

/// The role assignment form. The checkboxes repeat the `role_ids` key, and
/// `Form` (serde_urlencoded) refuses a repeated key as a duplicate field, so
/// this endpoint reads the raw body instead — see `parse_roles_form`.
#[derive(Debug, Default)]
pub struct RolesForm {
    pub user_id: i64,
    pub role_ids: Vec<i64>,
}

/// Parse the assignment form out of the raw urlencoded body, one pass, no new
/// dependency: every missing field stays at its default (the `#[serde(default)]
/// idiom), a malformed id is a form error naming the field, other keys are
/// ignored, and a REPEATED `user_id` is refused — the checkboxes repeat only
/// `role_ids`, so a second `user_id` can only come from a hand-built request,
/// and silently keeping the last value would let it pick a different target
/// than the one the checkboxes were rendered for.
///
/// `role_ids=` with an EMPTY value is the empty set, not a form error: the UI
/// omits the key when nothing is checked, so a well-formed body carrying the
/// key with no value means exactly what the omission means — none. The
/// last-holder backstop (the guard trigger behind `replace_user_roles`) is
/// the real protection, not the parser. A genuinely malformed value
/// (`%`, `%zz`, non-numeric) is still refused.
fn parse_roles_form(body: &[u8]) -> AppResult<RolesForm> {
    let mut form = RolesForm::default();
    let mut user_id_seen = false;
    for pair in body.split(|b| *b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, |b| *b == b'=');
        let key = percent_decode(parts.next().unwrap_or(&[]));
        match key.as_str() {
            "user_id" => {
                let value = percent_decode(parts.next().unwrap_or(&[]));
                if user_id_seen {
                    return Err(AppError::Validation(
                        "El identificador del usuario es inválido.".into(),
                    ));
                }
                user_id_seen = true;
                form.user_id = value.trim().parse().map_err(|_| {
                    AppError::Validation("El identificador del usuario es inválido.".into())
                })?;
            }
            "role_ids" => {
                let value = percent_decode(parts.next().unwrap_or(&[]));
                // A present-but-empty value is the empty set: no ids, none.
                if value.trim().is_empty() {
                    continue;
                }
                let id: i64 = value.trim().parse().map_err(|_| {
                    AppError::Validation("Uno de los roles indicados es inválido.".into())
                })?;
                form.role_ids.push(id);
            }
            _ => {}
        }
    }
    Ok(form)
}

/// Percent-decode a urlencoded component: `%XX` escapes, `+` as space.
/// Malformed escapes pass through as literals rather than failing the form.
fn percent_decode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The role assignment endpoint: the tier that decides who administers the
/// instance. Changing anyone's role set — granting the protected role,
/// stripping it, creating administrators — is `identity.roles.manage`, so a
/// `identity.users.manage`-only principal is refused HERE, at the gate,
/// before the service reads a byte of the form. (Escalation formerly lived
/// in exactly this endpoint: the users tier could assign roles.)
///
/// The body is buffered through the handler's OWN limit, not the extractor's
/// 2 MB default: an oversized form is the app's Spanish JSON refusal (413),
/// not an English plain-text buffering error. The form is tiny — the limit
/// exists to refuse abuse, not to size real submissions.
const ROLES_FORM_BODY_LIMIT: usize = 64 * 1024;
const ROLES_FORM_TOO_LARGE_MESSAGE: &str = "El cuerpo de la petición es demasiado grande.";

async fn web_assign_roles(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    _: Require<IdentityRolesManage>,
    body: Body,
) -> AppResult<Response> {
    let bytes = axum::body::to_bytes(body, ROLES_FORM_BODY_LIMIT)
        .await
        .map_err(|e| {
            if e.to_string().contains("length limit") {
                AppError::PayloadTooLarge(ROLES_FORM_TOO_LARGE_MESSAGE.into())
            } else {
                AppError::Internal(e.to_string())
            }
        })?;
    let form = parse_roles_form(&bytes)?;
    let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
    state
        .identity_service
        .assign_roles(
            &permissions,
            principal.user_id,
            form.user_id,
            &form.role_ids,
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(
            &state,
            principal.has_permission::<IdentityUsersManage>(),
            true,
            principal.user_id,
        )
        .await?;
        resp.headers_mut()
            .insert("HX-Trigger", "user-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/users").into_response())
}

// ---------------------------------------------------------------------------
// Dialog fragments (id-final, like the customer edit-form fragment)
// ---------------------------------------------------------------------------

/// The role assignment form fragment: the `identity.roles.manage` tier, the
/// same gate the submit endpoint answers, so a principal that cannot assign
/// roles never sees the form it could not use.
async fn web_user_roles_form(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    _: Require<IdentityRolesManage>,
) -> AppResult<Html<String>> {
    let users = state.identity_service.list_users_with_roles().await?;
    let user = users
        .into_iter()
        .find(|u| u.user.id == id)
        .ok_or_else(|| AppError::NotFound("El usuario no existe.".into()))?;
    let roles: Vec<Role> = state.identity_service.role_list().await?;
    Ok(Html(render(UserRolesFormPartial {
        localization,
        user,
        roles,
    })?))
}

async fn web_user_password_form(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    _: Require<IdentityUsersManage>,
) -> AppResult<Html<String>> {
    let users = state.identity_service.list_users_with_roles().await?;
    let user = users
        .into_iter()
        .find(|u| u.user.id == id)
        .ok_or_else(|| AppError::NotFound("El usuario no existe.".into()))?;
    Ok(Html(render(UserPasswordFormPartial {
        localization,
        user,
    })?))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(users_page))
        .route("/web/users", get(web_user_list).post(web_create_user))
        .route("/web/users/activate", post(web_activate_user))
        .route("/web/users/deactivate", post(web_deactivate_user))
        .route("/web/users/password", post(web_reset_password))
        .route("/web/users/roles", post(web_assign_roles))
        .route("/web/users/roles-form/{id}", get(web_user_roles_form))
        .route("/web/users/password-form/{id}", get(web_user_password_form))
}

// ---------------------------------------------------------------------------
// Tests (AC10 / AC13 / AC14 / AC15 / the administration round trips)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::repositories::role_repo::{RoleRepository, SqliteRoleRepository};
    use crate::repositories::user_repo::UserRepository;
    use crate::routes::router;
    use crate::security::test_support;

    const ADMIN_PASSWORD: &str = "bootstrap password 1";

    async fn test_pool() -> sqlx::SqlitePool {
        let opts = crate::db::base_connect_options("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES ('es-ES', 'es', 'Español (España)', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO business_settings (id, business_name, default_locale_code, currency_code, timezone) VALUES (1, 'Test', 'es-ES', 'USD', 'UTC')")
            .execute(&pool)
            .await
            .unwrap();
        // Fixture wiring (S5): the permissionless variant. The shared principal
        // this screen's tests build on holds NO roles — the refusal fixtures
        // and the read-only principal depend on that premise, and the
        // happy-path tests grant their own permission sets through
        // app_with_permissions. The full-permission seed (seed_session) is for
        // the department fixtures, not this one.
        test_support::seed_session_without_roles(&pool)
            .await
            .unwrap();
        pool
    }

    /// The full production router over the shared fixture (TEST_COOKIE's
    /// user). `with_users_role` grants that user a custom role carrying the
    /// identity.users permissions through the real grant path, so the
    /// bootstrap administrator stays the only protected-role holder and the
    /// AC14 arithmetic stays observable.
    async fn app_with_permissions(permissions: &[&str]) -> (axum::Router, crate::routes::AppState) {
        let pool = test_pool().await;
        let state = test_support::app_state(pool.clone());
        if !permissions.is_empty() {
            sqlx::query(
                "INSERT INTO roles (code, name, created_by) VALUES ('usuarios', 'Usuarios', \
                 (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE))",
            )
            .execute(&pool)
            .await
            .unwrap();
            for code in permissions {
                sqlx::query(
                    "INSERT INTO role_permissions (role_id, permission_id) \
                     SELECT r.id, p.id FROM roles r JOIN permissions p ON p.code = ? \
                     WHERE r.code = 'usuarios'",
                )
                .bind(code)
                .execute(&pool)
                .await
                .unwrap();
            }
            let roles = SqliteRoleRepository::new(pool.clone());
            let role = roles.find_by_code("usuarios").await.unwrap().unwrap();
            let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
                .bind(test_support::TEST_USERNAME)
                .fetch_one(&pool)
                .await
                .unwrap();
            roles
                .grant(&crate::models::NewUserRole {
                    user_id,
                    role_id: role.id,
                    granted_by: user_id,
                })
                .await
                .unwrap();
        }
        (router(state.clone()), state)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> axum::http::Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = to_bytes(resp.into_body(), 1024 * 256).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn form_headers() -> Vec<(&'static str, &'static str)> {
        vec![
            ("content-type", "application/x-www-form-urlencoded"),
            ("HX-Request", "true"),
            ("cookie", test_support::TEST_COOKIE),
        ]
    }

    fn cookie() -> [(&'static str, &'static str); 1] {
        [("cookie", test_support::TEST_COOKIE)]
    }

    async fn use_english(pool: &sqlx::SqlitePool) {
        sqlx::query("INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES ('en-US', 'en', 'English (United States)', 1)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE business_settings SET default_locale_code = 'en-US' WHERE id = 1")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn user_id_by_username(state: &crate::routes::AppState, username: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    async fn user_count(state: &crate::routes::AppState) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap()
    }

    /// The role codes one user holds, through the real roles join — the
    /// takeover tests' "nothing was written" proof.
    async fn held_role_codes(pool: &sqlx::SqlitePool, user_id: i64) -> Vec<String> {
        let roles = SqliteRoleRepository::new(pool.clone());
        roles
            .list_for_user(user_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.code)
            .collect()
    }

    // -- AC10: the permission gate on the real screen ---------------------------

    #[tokio::test]
    async fn ac10_a_full_page_visit_without_the_permission_gets_the_html_refusal() {
        // The shared fixture user holds no roles at all.
        let (app, _state) = app_with_permissions(&[]).await;
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let html = body_string(resp).await;
        assert!(html.contains("Acción no permitida"), "{html:.400}");
        assert!(
            html.contains("identity.users.read"),
            "the refusal names the missing permission: {html:.400}"
        );
    }

    #[tokio::test]
    async fn ac10_mutations_without_the_permission_are_refused_and_write_nothing() {
        let (app, state) = app_with_permissions(&[]).await;
        let before = user_count(&state).await;

        // HTMX create: the JSON shape the notice box renders.
        let resp = send(
            &app,
            "POST",
            "/web/users",
            &form_headers(),
            "username=ghost&display_name=Ghost&password=initial password 1",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("identity.users.manage")),
            "{json}"
        );

        // The full-page shape answers the refusal page.
        let resp = send(
            &app,
            "POST",
            "/web/users/deactivate",
            &cookie(),
            "user_id=1",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Nothing was written.
        assert_eq!(user_count(&state).await, before);
    }

    // -- the screen round trips -------------------------------------------------

    #[tokio::test]
    async fn the_page_lists_users_roles_and_state_and_the_actions_the_principal_may_perform() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let target = state
            .identity_service
            .create_user(actor_id, "teller", "Teller", "initial password 1")
            .await
            .unwrap();
        let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state
            .identity_service
            .role_list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.code == "vendedor")
            .unwrap();
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        state
            .identity_service
            .assign_roles(&permissions, actor_id, target.id, &[vendedor.id])
            .await
            .unwrap();

        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        for expected in [
            "test-admin",
            "Test Admin",
            "Teller",
            "Activo",
            "Vendedor",
            "Nuevo usuario",
            "Desactivar",
            "debe cambiar la contraseña",
            "data-nav=\"users\"",
        ] {
            assert!(
                html.contains(expected),
                "page must show {expected}: {html:.600}"
            );
        }

        // The fragment: same list, smaller body.
        let resp = send(&app, "GET", "/web/users", &cookie(), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("caja") || html.contains("teller") || html.contains("test-admin"));
        assert!(html.contains("Vendedor"), "{html:.400}");
    }

    #[tokio::test]
    async fn seeded_role_names_render_in_english_while_persistence_stays_spanish() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let target = state
            .identity_service
            .create_user(actor_id, "teller", "Teller", "initial password 1")
            .await
            .unwrap();
        let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state
            .identity_service
            .role_list()
            .await
            .unwrap()
            .into_iter()
            .find(|role| role.code == "vendedor")
            .unwrap();
        state
            .identity_service
            .assign_roles(&permissions, actor_id, target.id, &[vendedor.id])
            .await
            .unwrap();
        use_english(&state.pool).await;

        let html = body_string(send(&app, "GET", "/users", &cookie(), "").await).await;
        assert!(html.contains("Salesperson"), "{html:.800}");
        assert!(html.contains("granted by"), "{html:.800}");

        let html = body_string(
            send(
                &app,
                "GET",
                &format!("/web/users/roles-form/{}", target.id),
                &cookie(),
                "",
            )
            .await,
        )
        .await;
        assert!(html.contains("Salesperson"), "{html:.800}");
        assert!(html.contains("vendedor"), "{html:.800}");

        let stored = SqliteRoleRepository::new(state.pool)
            .find_by_id(vendedor.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.name, "Vendedor");
        assert_eq!(
            stored.description.as_deref(),
            Some("Ventas y clientes; consulta de stock.")
        );
    }

    #[tokio::test]
    async fn a_read_only_principal_sees_the_list_but_not_the_actions() {
        let (app, _state) = app_with_permissions(&["identity.users.read"]).await;
        let resp = send(&app, "GET", "/users", &cookie(), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("test-admin"), "{html:.400}");
        let listing = html
            .split("<div id=\"user-list-inner\">")
            .nth(1)
            .expect("users list is rendered");
        for absent in [
            "Nuevo usuario",
            "Desactivar",
            ">Roles</button>",
            "Contraseña",
        ] {
            assert!(
                !listing.contains(absent),
                "a read-only principal must not see {absent}: {html:.600}"
            );
        }
        // The mutation endpoint refuses them anyway.
        let resp = send(
            &app,
            "POST",
            "/web/users",
            &form_headers(),
            "username=ghost&display_name=G&password=initial password 1",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_activate_and_deactivate_round_trip_through_the_screen() {
        let (app, state) =
            app_with_permissions(&["identity.users.read", "identity.users.manage"]).await;

        // Create through the collection endpoint.
        let resp = send(
            &app,
            "POST",
            "/web/users",
            &form_headers(),
            "username=caja1&display_name=Caja+Uno&password=initial%20password%201",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let trigger = resp
            .headers()
            .get("HX-Trigger")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let html = body_string(resp).await;
        assert!(html.contains("caja1"), "{html:.400}");
        assert_eq!(trigger, "user-created", "the create answers with its event");
        let id = user_id_by_username(&state, "caja1").await;
        let created = state
            .identity_service
            .users
            .find_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            created.must_change_password,
            "the created user owes the change"
        );

        // A duplicate username is refused with the Spanish conflict.
        let resp = send(
            &app,
            "POST",
            "/web/users",
            &form_headers(),
            "username=CAJA1&display_name=Other&password=initial password 1",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("ya existe")),
            "{json}"
        );

        // Deactivate through the collection endpoint...
        let resp = send(
            &app,
            "POST",
            "/web/users/deactivate",
            &form_headers(),
            &format!("user_id={id}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let user = state
            .identity_service
            .users
            .find_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert!(!user.is_active);

        // ... and activate again.
        let resp = send(
            &app,
            "POST",
            "/web/users/activate",
            &form_headers(),
            &format!("user_id={id}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let user = state
            .identity_service
            .users
            .find_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert!(user.is_active);
    }

    // -- AC13/AC14: the last administrator, through the screen -------------------

    #[tokio::test]
    async fn ac14_the_last_administrator_cannot_be_deactivated_through_the_screen_and_a_second_makes_it_succeed(
    ) {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        state
            .identity_service
            .bootstrap_admin(Some(ADMIN_PASSWORD))
            .await
            .unwrap();
        let admin_id = user_id_by_username(&state, "admin").await;
        let admin_role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // The refusal: 409 with the Spanish reason, nothing written.
        let resp = send(
            &app,
            "POST",
            "/web/users/deactivate",
            &form_headers(),
            &format!("user_id={admin_id}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let message = json.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(message.contains("último"), "{json}");
        assert!(message.contains("rol protegido"), "{json}");
        assert!(!message.contains("cannot deactivate"), "{json}");
        let admin = state
            .identity_service
            .users
            .find_by_id(admin_id)
            .await
            .unwrap()
            .unwrap();
        assert!(admin.is_active, "the refused deactivation writes nothing");

        // A second administrator is created and granted the role THROUGH the
        // screen; the same deactivation then succeeds.
        let created = state
            .identity_service
            .create_user(admin_id, "second", "Second", "initial password 1")
            .await
            .unwrap();
        let resp = send(
            &app,
            "POST",
            "/web/users/roles",
            &form_headers(),
            &format!("user_id={}&role_ids={}", created.id, admin_role_id),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = send(
            &app,
            "POST",
            "/web/users/deactivate",
            &form_headers(),
            &format!("user_id={admin_id}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let admin = state
            .identity_service
            .users
            .find_by_id(admin_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!admin.is_active);
    }

    // -- the administrator password reset (spec Rules) ---------------------------

    #[tokio::test]
    async fn the_admin_reset_flags_the_target_and_not_the_actor_through_the_screen() {
        // The actor holds both tiers: resetting a protected holder's password
        // is a decision about the administration (identity.roles.manage).
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        state
            .identity_service
            .bootstrap_admin(Some(ADMIN_PASSWORD))
            .await
            .unwrap();
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let admin_id = user_id_by_username(&state, "admin").await;

        // Resetting oneself is refused: that change is /password.
        let resp = send(
            &app,
            "POST",
            "/web/users/password",
            &form_headers(),
            &format!("user_id={actor_id}&new_password=temp password 12"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("Cambiar contraseña")),
            "{json}"
        );

        // The real reset: the target is flagged, the actor is not.
        let resp = send(
            &app,
            "POST",
            "/web/users/password",
            &form_headers(),
            &format!("user_id={admin_id}&new_password=temp password 12"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let target = state
            .identity_service
            .users
            .find_by_id(admin_id)
            .await
            .unwrap()
            .unwrap();
        assert!(target.must_change_password, "the target owes the change");
        let actor = state
            .identity_service
            .users
            .find_by_id(actor_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!actor.must_change_password, "the actor's flag stays clear");
    }

    // -- the dialog fragments ------------------------------------------------------

    #[tokio::test]
    async fn the_dialog_forms_render_the_roles_and_the_reset_target() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let target = state
            .identity_service
            .create_user(actor_id, "teller", "Teller", "initial password 1")
            .await
            .unwrap();
        let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state
            .identity_service
            .role_list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.code == "vendedor")
            .unwrap();
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        state
            .identity_service
            .assign_roles(&permissions, actor_id, target.id, &[vendedor.id])
            .await
            .unwrap();

        let resp = send(
            &app,
            "GET",
            &format!("/web/users/roles-form/{}", target.id),
            &cookie(),
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("name=\"role_ids\""), "{html:.600}");
        assert!(
            html.contains("checked") && html.contains("Vendedor"),
            "the held role is pre-checked: {html:.600}"
        );
        assert!(html.contains("hx-post=\"/web/users/roles\""), "{html:.600}");

        let resp = send(
            &app,
            "GET",
            &format!("/web/users/password-form/{}", target.id),
            &cookie(),
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            html.contains("hx-post=\"/web/users/password\""),
            "{html:.600}"
        );
        assert!(
            html.contains(&format!("name=\"user_id\" value=\"{}\"", target.id)),
            "{html:.600}"
        );

        // An unknown id is a 404, not a 500.
        let resp = send(&app, "GET", "/web/users/roles-form/999999", &cookie(), "").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -- the corrected takeover finding, through the real endpoints ------------

    /// The gate: a principal holding ONLY `identity.users.manage` is refused
    /// every role-set change — on itself, on an account it created, and on
    /// another administrator — before the service reads a byte of the form.
    /// Escalation formerly lived in exactly this endpoint.
    #[tokio::test]
    async fn a_users_manage_principal_is_refused_the_role_set_endpoint_at_the_gate() {
        let (app, state) =
            app_with_permissions(&["identity.users.read", "identity.users.manage"]).await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        state
            .identity_service
            .bootstrap_admin(Some(ADMIN_PASSWORD))
            .await
            .unwrap();
        let created = state
            .identity_service
            .create_user(actor_id, "protege", "Protege", "initial password 1")
            .await
            .unwrap();
        let admin_role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'admin'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let admin_id = user_id_by_username(&state, "admin").await;

        for (label, target_id, role_ids) in [
            ("self-grant", actor_id, format!("role_ids={admin_role_id}")),
            (
                "grant to a created account",
                created.id,
                format!("role_ids={admin_role_id}"),
            ),
            (
                "strip another administrator",
                admin_id,
                "role_ids=".to_string(),
            ),
        ] {
            let resp = send(
                &app,
                "POST",
                "/web/users/roles",
                &form_headers(),
                &format!("user_id={target_id}&{role_ids}"),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{label}");
            let body = body_string(resp).await;
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                json.get("error")
                    .and_then(|e| e.as_str())
                    .is_some_and(|m| m.contains("identity.roles.manage")),
                "{label}: {json}"
            );
        }

        // Nothing was written anywhere.
        assert_eq!(
            held_role_codes(&state.pool, actor_id).await,
            vec!["usuarios".to_string()],
            "the actor's own set is untouched"
        );
        assert!(held_role_codes(&state.pool, created.id).await.is_empty());
        assert_eq!(
            held_role_codes(&state.pool, admin_id).await,
            vec!["admin".to_string()]
        );
    }

    /// The service's own-account rule, through the real endpoint: a
    /// `identity.roles.manage` principal is refused changing ITS OWN role
    /// set — one rule closes self-escalation and the self-lockout.
    #[tokio::test]
    async fn a_roles_manage_principal_cannot_change_their_own_roles_through_the_endpoint() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let vendedor_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE code = 'vendedor'")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        let resp = send(
            &app,
            "POST",
            "/web/users/roles",
            &form_headers(),
            &format!("user_id={actor_id}&role_ids={vendedor_id}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("propios roles")),
            "{json}"
        );
        // The refusal changed nothing.
        let roles = SqliteRoleRepository::new(state.pool.clone());
        let held: Vec<String> = roles
            .list_for_user(actor_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.code)
            .collect();
        assert_eq!(held, vec!["usuarios".to_string()], "own set untouched");
    }

    /// The reset tier rule, through the real endpoint: a
    /// `identity.users.manage`-only principal cannot reset a protected
    /// holder's password at all (the takeover path is gone), while the
    /// ordinary user's reset still succeeds for the same actor.
    #[tokio::test]
    async fn the_reset_tier_rule_through_the_endpoint_refuses_protected_targets_and_the_ordinary_still_succeeds(
    ) {
        let (app, state) =
            app_with_permissions(&["identity.users.read", "identity.users.manage"]).await;
        let boot = state
            .identity_service
            .bootstrap_admin(Some(ADMIN_PASSWORD))
            .await
            .unwrap();
        let admin_id = boot.user.unwrap().id;
        let teller = state
            .identity_service
            .create_user(admin_id, "teller", "Teller", "initial password 1")
            .await
            .unwrap();

        // (d) the protected holder: 403 in the app's Spanish shape, nothing
        // written.
        let resp = send(
            &app,
            "POST",
            "/web/users/password",
            &form_headers(),
            &format!("user_id={admin_id}&new_password=stolen password 1"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("identity.roles.manage")),
            "{json}"
        );
        let admin = state
            .identity_service
            .users
            .find_by_id(admin_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!admin.must_change_password);

        // (e) the ordinary user: the same actor's reset succeeds.
        let resp = send(
            &app,
            "POST",
            "/web/users/password",
            &form_headers(),
            &format!("user_id={}&new_password=temp password 12", teller.id),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let teller_after = state
            .identity_service
            .users
            .find_by_id(teller.id)
            .await
            .unwrap()
            .unwrap();
        assert!(teller_after.must_change_password);
    }

    /// An oversized roles-form body is the app's Spanish refusal (413), not
    /// the extractor's English plain-text buffering error.
    #[tokio::test]
    async fn an_oversized_roles_form_body_answers_413_in_the_app_shape() {
        let (app, _state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let oversized = format!("user_id=1&junk={}", "x".repeat(128 * 1024));
        let resp = send(
            &app,
            "POST",
            "/web/users/roles",
            &form_headers(),
            &oversized,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("demasiado grande")),
            "{json}"
        );
        assert!(
            !body.contains("Failed to buffer"),
            "the English plain-text shape must be gone: {body:.200}"
        );
    }

    /// A hand-built request repeating `user_id` is refused: silently keeping
    /// the last value would let it pick a different target than the one the
    /// checkboxes were rendered for.
    #[tokio::test]
    async fn a_duplicated_user_id_in_the_roles_form_is_refused() {
        let (app, _state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let resp = send(
            &app,
            "POST",
            "/web/users/roles",
            &form_headers(),
            "user_id=1&user_id=2",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|m| m.contains("identificador del usuario")),
            "{json}"
        );
    }

    /// A well-formed empty form (`role_ids=` present with no value) is the
    /// empty set, not a 400: the UI omits the key when nothing is checked,
    /// and both spellings mean none. The replacement actually strips the
    /// user's roles, and the genuinely malformed values stay refused.
    #[tokio::test]
    async fn a_present_but_empty_role_ids_value_is_an_empty_set_not_a_400() {
        let (app, state) = app_with_permissions(&[
            "identity.users.read",
            "identity.users.manage",
            "identity.roles.manage",
        ])
        .await;
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        let target = state
            .identity_service
            .create_user(actor_id, "teller", "Teller", "initial password 1")
            .await
            .unwrap();
        let permissions = crate::repositories::SqlitePermissionRepository::new(state.pool.clone());
        let vendedor = state
            .identity_service
            .role_list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.code == "vendedor")
            .unwrap();
        let actor_id = user_id_by_username(&state, test_support::TEST_USERNAME).await;
        state
            .identity_service
            .assign_roles(&permissions, actor_id, target.id, &[vendedor.id])
            .await
            .unwrap();

        // The empty value replaces the held set with none.
        let resp = send(
            &app,
            "POST",
            "/web/users/roles",
            &form_headers(),
            &format!("user_id={}&role_ids=", target.id),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            held_role_codes(&state.pool, target.id).await.is_empty(),
            "an empty role_ids= form must assign none"
        );

        // A genuinely malformed value is still a form error.
        for value in ["%", "%zz", "not-a-number"] {
            let resp = send(
                &app,
                "POST",
                "/web/users/roles",
                &form_headers(),
                &format!("user_id={}&role_ids={value}", target.id),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "role_ids={value}");
            let body = body_string(resp).await;
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                json.get("error")
                    .and_then(|e| e.as_str())
                    .is_some_and(|m| m.contains("roles indicados")),
                "role_ids={value}: {json}"
            );
        }
    }
}
